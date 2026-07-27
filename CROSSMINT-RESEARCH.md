# Cross-mint hop — research report (worker:mobee-crossmint, 2026-07-27)

Charter: `/srv/forge/projects/mobee-seller-node/CROSSMINT-CHARTER.md` · parent: keeper:mobee-orchestrator
Ground truth read: cdk `=0.17.2` (the pinned version, vendored at
`~/.cargo/registry/src/index.crates.io-*/cdk-0.17.2` + `cdk-common-0.17.2`), agicash @ `23788b7c`,
mobee @ `origin/dev` `7463cbe`.

⚠ Do NOT read `/srv/forge/projects/cdk` for this work — it is the MakePrisms fork at **v0.16.0-48**,
not the version mobee compiles against.

## Verdict

**No cross-mint primitive exists in cdk 0.17.2 — we build the hop.** But both halves already exist
*in mobee* as separate CLI ops, and cdk gives us persisted, quote-id-addressable crash recovery for
each half. So this is a **composition over existing money machinery**, not new money machinery. The
agicash pattern gives us the fee decomposition, and its loop **inverts** for our invariant 1 (below —
this is the sharpest finding).

## 1. cdk 0.17.2 — what's actually there

**Cross-mint transfer primitive: NO.**
- `cdk-common-0.17.2/src/error.rs:410` declares `Error::TransferTimeout { source_mint, target_mint, amount }`
  — but it is a **dead variant**: never constructed anywhere in `cdk` or `cdk-common` (only declared
  and classified in a match arm at `error.rs:755`). It is a leftover surface, not a feature.
- No `MultiMintWallet` in 0.17.2.
- The fork's `cdk-cli/src/sub_commands/transfer.rs` "transfer" is `source.prepare_send()` →
  `target.receive(token)` — that is a same-mint move mislabelled; receiving a mint-A token at a
  mint-B wallet is rejected (`Error::IncorrectMint` / `MultiMintTokenNotSupported`). **Not a model to copy.**

**Multi-mint wallet management: YES — `WalletRepository`** (`cdk/src/wallet/wallet_repository.rs`).
One shared `localstore` + one seed, N wallets keyed by `WalletKey { mint_url, unit }`:
`get_wallet(mint_url, unit)`, `add_wallet`, `has_mint`, `get_balances`, `total_balance`.
This is how we hold mint A and mint B at once without two databases.

**Quote lifecycle (both halves).**
- Mint side (NUT-04), `cdk/src/wallet/issue/mod.rs`:
  `mint_quote(PaymentMethod, Option<Amount>, description, extra) -> MintQuote` (`:106`) — the
  `MintQuote.request` field IS the bolt11 we need; `mint(...)` (`:416`); `check_mint_quote_status(quote_id)` (`:286`).
- Melt side (NUT-05), `cdk/src/wallet/melt/mod.rs`:
  `melt_quote(method, request, options, extra) -> MeltQuote` (`:1156`); `check_melt_quote_status(quote_id)` (`:1260`).

**Fee reserve reporting: YES, as a first-class field.**
`cdk-common/src/wallet/mod.rs:215` — `MeltQuote { amount, fee_reserve, state, used_by_operation, version, .. }`.
`amount` and `fee_reserve` are **separate** — exactly the two numbers invariant 3 must add up.
Mint (input/keyset) fee is the third: `cdk/src/fees.rs::calculate_fee`.

**Saga machinery (this is the good news).** Every wallet money op is a typed saga with a persisted
record and a resume path: `wallet/{melt,issue,send,receive,swap}/saga/{mod,state,resume,compensation}.rs`.
`cdk-common/src/wallet/saga/mod.rs:162` — `WalletSaga { id, kind, state, amount, mint_url, unit,
quote_id: Option<String>, data, version, .. }`. `version` is optimistic-locking; the doc says a
version conflict means "someone else handled it — skip", which is our multi-process safety net.

## 2. Parent's question: is melt quote state recoverable by quote id after process death?

**Yes — recoverable by quote id, from a cold process, without a live wallet handle.** Mechanism:

1. The quote itself is **persisted in the wallet DB** (sqlite), not in-memory:
   `check_melt_quote_status(quote_id)` (`melt/mod.rs:1260`) starts with
   `self.localstore.get_melt_quote(quote_id)` → `Error::UnknownQuote` if absent.
2. The quote row carries `used_by_operation: Option<String>` → the saga UUID. The function then does
   `localstore.get_saga(&operation_id)` and, if a saga exists, calls `resume_melt_saga(&saga)` — i.e.
   **asking for the quote's status is itself the recovery trigger.**
3. `resume_melt_saga` (`melt/saga/resume.rs:20`) queries the mint and either finalizes or compensates.
   Its contract: `Ok(Some(FinalizedMelt))` = finalized or compensated; **`Ok(None)` = still pending or
   mint unreachable → skipped** (retry later); `Err` = recovery error.
4. Cold-start sweep: `Wallet::recover_incomplete_sagas()` (`wallet/recovery.rs:302`) reads
   `localstore.get_incomplete_sagas()` and dispatches per kind. Also
   `get_pending_melt_quotes()` / `get_active_melt_quotes()` (`melt/mod.rs:970,983`) read from the store,
   and `finalize_pending_melts()` (`:749`).

**So the primitive's shape, before we design the journal:** we do NOT need to journal melt state
ourselves — cdk already journals it. What we MUST journal is the **pairing** (which melt quote at A
belongs to which mint quote at B, for which job/attempt), because nothing in cdk knows the two quotes
are one logical hop. That's the whole content of our journal: `(attempt_id, mint_a, melt_quote_id,
mint_b, mint_quote_id, planned_cost)` written **before** the melt fires.

**⚠ Trap that hangs directly off this** (and it is why per-wallet recovery is not enough):
`recover_incomplete_sagas()` filters `sagas.retain(|s| s.mint_url == self.mint_url && s.unit == self.unit)`
(`recovery.rs:308-311`). A hop has one saga at **mint A** (melt) and one at **mint B** (issue). Recovery
run on the mint-A wallet **silently skips** the mint-B saga and vice versa. A single-wallet startup
sweep therefore leaves half the hop unrecovered, with no error — the exact "absent signal from a
changed source" shape. ⇒ **the hop's recovery must run on both wallets** (or over
`WalletRepository::get_wallets()`).

**The paid-but-unminted strand has a purpose-built sweeper.** `MintQuote` tracks `amount_paid` vs
`amount_issued` with `amount_mintable()` (`cdk-common/src/wallet/mod.rs:303`; for BOLT11 it is
all-or-nothing on `state == Paid`). `Wallet::mint_unissued_quotes()` (`issue/mod.rs:340`) +
`get_unissued_mint_quotes()` (`:408`) find and complete them, and
**`WalletRepository::check_all_mint_quotes(None)` (`wallet_repository.rs:822`) iterates every wallet and
calls `mint_unissued_quotes()` on each** — repo-wide, which is what the cross-mint case needs. So
"melted at A, paid at B, crashed before minting at B" is recoverable and must be surfaced loudly, not
silently swept.

## 3. agicash `receive-cashu-token` — the fee pattern

Files (main tree, ignore `worktrees/*` copies):
`apps/web-wallet/app/features/receive/cashu-token-melt-data.ts`,
`receive-cashu-token-quote-service.ts`, `cashu-receive-quote-service.ts`, `cashu-receive-quote-hooks.ts`.

**Fee decomposition** (`cashu-token-melt-data.ts`) — their persisted per-hop record. Four money fields:
- `cashuReceiveFee` — mint input fee for spending the source proofs as melt inputs
  (`sourceAccount.wallet.getFeesForProofs(token.proofs)`)
- `lightningFeeReserve` — `meltQuote.fee_reserve`
- `lightningFee` (optional) — the **actual** LN fee, known only after the melt completes;
  `reserve - actual = change`. Their comment: "For cashu token receives over lightning, we are
  currently not returning the change to the user."
- plus `sourceMintUrl`, `tokenAmount`, `tokenProofs`, `meltQuoteId`, `meltInitiated: boolean`

**Fee sizing = a bounded convergence loop** (`receive-cashu-token-quote-service.ts:205-273`). The
circularity is real: fee reserve needs a melt quote, which needs an invoice, which needs an amount.
Their resolution:

```
cashuReceiveFee = getFeesForProofs(token.proofs)
targetAmount    = tokenAmount - cashuReceiveFee        // hard ceiling
amountToMelt    = targetAmount
loop (attempts < 5):
    amountToMint = convert(amountToMelt)
    if amountToMint < 1        -> DomainError "too small to cover the fees"
    invoice   = destination.mintQuote(amountToMint).request     // NUT-04 at DESTINATION
    meltQuote = source.createMeltQuoteBolt11(invoice)           // NUT-05 at SOURCE
    required  = meltQuote.amount + meltQuote.fee_reserve
    diff      = required - targetAmount
    if diff <= 0 -> DONE
    amountToMelt -= diff                                       // shrink, retry
throw "Failed to find valid quotes after 5 attempts"
```

Properties worth keeping: **bounded** (5, no unbounded retry), **fail-closed** (throws rather than
melting something that doesn't fit), and the fit predicate is exactly
`melt.amount + melt.fee_reserve <= budgeted`.

**Pays-once discipline** (`cashu-receive-quote-service.ts:127`, `cashu-receive-quote-hooks.ts:775-820`).
`markMeltInitiated` is idempotent (no-op if set) and state-gated (must be `UNPAID`). Recovery is a
decision table driven by the **melt quote state at the source mint**, looked up **by melt quote id**:
- `Unpaid` + `meltInitiated` ⇒ the melt **failed** → fail the hop. **Never re-melt.**
- `Unpaid` + `!meltInitiated` ⇒ safe to initiate.
- `Pending` ⇒ mark initiated (money in flight).
- `Expired` ⇒ expire.

⚠ One honest weakness to NOT copy: they set `meltInitiated` **reactively**, on observing `Pending`.
A crash between sending the melt request and observing `Pending` leaves the flag false with money
possibly in flight; they are then relying on the mint rejecting a second melt on the same quote. We
should instead **write the journal before the melt call** (write-before-effect) — which is already
mobee's own discipline for the budget ledger (`budget.rs:6-14`, "appended (never rewritten) **before**
the pay effect"), and is strictly stronger.

## 4. Mobee-side seams (what we compose)

- `crates/mobee-core/src/wallet_ops.rs:354` — `mint_quote(PaymentMethod::BOLT11, Some(amount), ..)` (deposit path)
- `crates/mobee-core/src/wallet_ops.rs:651` — `melt_quote(PaymentMethod::BOLT11, bolt11, ..)` (withdraw path)
  ⇒ **both halves of the hop already exist and are exercised.**
- `crates/mobee-core/src/payment_wallet.rs:246` — `prepare_send(terms.amount, options)`; `:577` — "Gate on
  the **realized** locked token after prepare_send/confirm — never input face." This is the send path the
  hop must feed **unchanged**.
- `crates/mobee-core/src/payment_wallet.rs:521` — `require_fee_safe_amount(wallet, amount)` via
  `mint_input_fee_bounded(wallet, 1, MINT_TOUCH_TIMEOUT)` returning
  `BoundedFee::{Fee, Failed, Unreachable}`. **This is our invariant-5 precedent: a timeout-bounded,
  fail-closed mint touch.** Reuse it for quote request/poll rather than inventing a timeout.
- `crates/mobee-core/src/authorize_pay.rs:385` — `require_fee_safe_amount(&wallet, terms.amount)`
  (pre-budget dust check); `:415` — `gate.authorize_then_attempt(attempt_id, amount, ..)`.
  **⇒ the #185 seam is visible right here:** the gate is charged `amount` (the buyer-signed number)
  while the fee is only *dust-checked* (`require_amount_covers_fee`: `amount > fee`), never *added to
  the cap*. For the hop, `amount + fee_reserve + input_fee` must be what the cap sees.
- `crates/mobee-core/src/budget.rs` — append-only `spent.jsonl`, fold-at-read, cross-process `flock`,
  `attempt_id`-keyed idempotence. The hop charges through this, not around it.

## 5. Design consequence — the loop INVERTS (headline)

agicash solves "**receive**: fees eat the amount" — the token amount is fixed by whoever sent it, so
the **delivered** amount floats *down* to absorb fees.

Our charter's invariant 1 forbids exactly that: the amount is fixed by the **buyer-signed offer** and
no mint- or seller-supplied number may become the paid amount. So we must **not** shrink the mint
amount. We pin the delivery and let the **buyer's cost** float *up*:

1. `mint_quote` at seller's mint **B** for **exactly `offer.amount`** → `MintQuote.request` (bolt11)
2. `melt_quote` at buyer's funded mint **A** for that bolt11 → `amount`, `fee_reserve`
3. input fee at A for the proofs to be spent (mobee's bounded reader)
4. `planned_cost = melt.amount + melt.fee_reserve + input_fee`
5. **budget check on `planned_cost` BEFORE any melt** — insufficient ⇒ abort, zero money moved
6. journal `(attempt_id, A, melt_quote_id, B, mint_quote_id, planned_cost)` **before** the melt
7. melt at A → mint at B → hand fresh mint-B ecash to the **unchanged** send path

No convergence loop is needed: with the delivery pinned, cost is a single forward computation. That is
both simpler than agicash's loop **and** satisfies invariant 1 by construction — the seller-received
amount is never a function of any fee reading. (A retry loop would only be needed if we allowed the
delivered amount to float, which is precisely what we must not do.)

Invariant map: 1 → step 1 pins `offer.amount`, nothing downstream can rewrite it. 2 → decision tooth
before step 1 (overlap ⇒ existing path, byte-identical). 3 → step 4+5. 4 → step 6 + both-wallet
recovery + `check_all_mint_quotes`. 5 → every mint touch through the bounded/fail-closed reader; abort
before melt on any quote failure.

## 6. Traps found (each would bite)

1. **Per-wallet recovery filter** — `recover_incomplete_sagas()` skips the other mint's saga silently. Run on both.
2. **`TransferTimeout` is bait** — a dead cdk error variant suggesting a transfer feature that isn't there.
3. **The fork is the wrong cdk** — `/srv/forge/projects/cdk` is v0.16; mobee pins `=0.17.2`.
4. **`cdk-cli transfer` is not a cross-mint hop** — naive send→receive; copying it produces `IncorrectMint`.
5. **Reactive `meltInitiated`** (agicash) leaves a crash window; use write-before-effect.
6. **Fee reserve ≠ fee charged** — `reserve - actual` comes back as melt change, so the *planned* cost
   is an upper bound on the *realized* cost (see open question).

## 7. Open questions for parent (2)

**Q1 — budget reconciliation of the unused fee reserve.** We charge `planned_cost` (with the full
`fee_reserve`) before the melt, per invariant 3. The melt then returns change worth
`fee_reserve - actual_ln_fee`. Options: (a) leave the conservative overcharge standing — safe,
fail-closed, but the cap under-reports remaining allowance; (b) append a reconciliation record keyed to
the same `attempt_id` so the fold reflects true spend. **Recommendation: (a) for this slice**, and note
it in code as interim — (b) touches the ledger fold, which is money-gate machinery I should not
reshape inside a feature slice. Your call.

**Q2 — second test mint.** Charter names `testnut.cashudevkit.org` as our default and the
`testnut.cashu.space` family as candidates for mint B. I'll probe candidates for NUT-04/NUT-05 support
and report which pair actually works. Expect an honest caveat: if both are fake-wallet mints, the LN
leg auto-settles and the smoke proves the *hop's control flow, journal, budget accounting and
recovery* — but **not** real LN routing or a real fee reserve being consumed. Real-LN proof stays
gudnuf-gated; I won't attempt it.

Nothing here needs a money-gate config change. No budget/mint/`allow_real_mints` edit is implied by
the design above.

---

# Part II — rulings + implementation design (post-research)

## Rulings received (keeper:mobee-orchestrator)

- **Q1 = (a)** conservative overcharge, `// interim:` at the charge site linking a filed follow-up.
  ⇒ **MakePrisms/mobee#186**.
- **Q2 = (iii)** hermetic teeth this slice; live cross-mint smoke deferred (gudnuf holds the
  second-mint money-gate ask). Fence defect filed separately ⇒ **MakePrisms/mobee#187**
  (testnut allow-LIST + `extra_mints` dead-config, one fence one review).
- **Req 1** — the per-wallet saga-sweep trap is the heart of invariant 4: recovery on BOTH wallets +
  the mint-quote sweep; a melted-at-A/unminted-at-B strand must surface LOUD.
- **Req 2** — the hop's cap check sees `planned_cost`; do NOT touch `authorize_pay`'s general
  fee-dust behaviour (that is #185 proper, stays separate).
- **Req 3** — CLEARED, no migration; recommendation accepted (below).

## Req 3 resolution: no WalletRepository, no migration

Mobee already has WalletRepository's model. `open_wallet_at_mint_async(home, mint_url)`
(`buyer_fund.rs:85-106`) uses `sqlite_path(&home.wallet_dir)` — **one store per home, independent of
mint** — then `Wallet::new(mint_url, Sat, Arc::new(store), seed, None)`. Its own docstring: *"The
store (sqlite) is shared across mints; only the `Wallet`'s bound mint differs."*

⇒ Call that helper **twice** (mint A, mint B). Three reasons it beats introducing `WalletRepository`:
1. Zero schema/layout change to any existing money home.
2. The **real-mint fence lives inside the helper**, so both legs of the hop are fenced automatically
   rather than depending on us remembering to fence the second one.
3. The shared store makes req-1 recovery work for free: `get_incomplete_sagas()` returns sagas for
   every mint off the one store, and each `Wallet` filters to its own `mint_url` — so opening both
   wallets and recovering each covers the whole hop.

## The decision seam (invariant 2)

`resolve_realized_mint(buyer_mint_url, accepted_mints, allow_real_mints)` (`authorize_pay.rs:471`)
already **is** the decision, and its no-overlap branch already names the gap it errors on:

> `mint_unreachable_pay: buyer mint {buyer_mint} is not in the creq mint list {listed:?}; the
> single-mint buyer wallet holds no balance at any accepted mint`

That error is the hop's trigger. Shape the seam as a plan, not a bool:

```
enum PayPlan {
    Direct { mint: MintUrl },                        // buyer mint ∈ accepted_mints
    Hop    { source: MintUrl, target: MintUrl },     // no overlap: melt at source, mint at target
}
```

- **Overlap ⇒ `Direct`** and every downstream byte is what it is today. The tooth asserts on the
  *decision* (`PayPlan::Direct`), per charter — not on the outcome.
- **No overlap ⇒ `Hop`**, `target` = the first admissible entry of `accepted_mints` (seller's list
  order is their preference; **deterministic**, because replay/pays-once depends on re-deriving the
  same target).
- **Realized mint** sealed into `terms.mint` = `Direct.mint` or `Hop.target`. That keeps
  `wallet_open_mint_url` (`:462`) honest — after the hop the ecash *is* at `target`, so the send, the
  attempt id, and the co-signed receipt all stay on one mint. One settlement shape on the wire.

## Order of operations in `authorize_pay_async`

Today: verify delivery (pre-budget) → `require_fee_safe_amount` → `gate.authorize_then_attempt(attempt_id, amount, effect)`.

With a hop, the pre-budget region grows the *planning* (no money moves) and the effect grows the
*hop execution* (money moves, after the durable append):

1. verify delivery + bind-check — **unchanged**, pre-budget, zero spend on failure
2. **plan** (pre-budget, no money moves, all reads timer-bounded/fail-closed):
   a. `mint_quote(BOLT11, Some(offer.amount))` at **B** → `MintQuote.request` (bolt11) — amount pinned
      to the buyer-signed offer, never a mint- or seller-supplied number
   b. `melt_quote(BOLT11, bolt11)` at **A** → `amount`, `fee_reserve`
   c. input fee at A via the existing bounded reader
   d. `planned_cost = melt.amount + melt.fee_reserve + input_fee`
3. `gate.authorize_then_attempt(attempt_id, planned_cost, effect)` — cap sees the whole hop
   **before** any melt; refuse ⇒ zero money moved (the gate never calls `effect` on refuse)
4. inside `effect`: journal the pairing → melt at A → mint at B → **existing send path from B, unchanged**

`authorize_then_attempt` already gives us write-before-effect and cross-process `flock` (budget.rs
`reserve`), so the hop inherits the TOCTOU closure rather than re-inventing it. On the `Direct` path
we pass `amount` exactly as today — req 2 honoured, general fee-dust behaviour untouched.

## The journal (only what cdk does not already know)

cdk journals each half itself (`WalletSaga` + `used_by_operation` + quote rows). The one thing nothing
in cdk knows is that the two quotes are **one logical hop**, so that is the entire journal payload,
written **before** the melt:

```
(attempt_id, source_mint, melt_quote_id, target_mint, mint_quote_id, planned_cost)
```

Recovery then reads the pairing and asks cdk for each half by quote id:
`check_melt_quote_status(melt_quote_id)` at A, `check_mint_quote_status(mint_quote_id)` /
`amount_mintable()` at B. Decision table (agicash's, keyed by quote id, but with our
write-before-effect ordering so the "initiated" flag can never be false while money is in flight):

| melt @ A | mint @ B | action |
|---|---|---|
| Unpaid, journal present | — | melt never landed → safe to retry the melt (quote still Unpaid at the mint) |
| Pending | — | money in flight → wait, timer-bounded; never re-melt |
| Paid | not issued | **the strand** → mint at B (recoverable), and surface LOUD |
| Paid | issued | complete → hand to send path; exactly-once |

## Invariant → tooth map

1. **Amount from the buyer-signed offer only** — step 2a pins `offer.amount`; nothing downstream can
   rewrite it. Tooth: enumerate the entry points to the spend gate and show the hop adds none — the
   hop's only new gate call passes `planned_cost` as the *cost*, while the *delivered* amount stays
   `offer.amount`. Red-on-revert: make the mint amount a function of any mint/seller number → tooth bites.
2. **Hop only when needed** — decision tooth on `PayPlan`, not the outcome: overlap ⇒ `Direct`.
3. **Budget covers the WHOLE hop** — `planned_cost` includes melt amount + fee reserve + input fee,
   charged before the melt. Strong-form bite: shrink the cap to just under `planned_cost` and assert
   *no melt is attempted at all* (not merely that it fails afterwards).
4. **Pays-once across the hop** — kill between melt and mint, restart, assert exactly-one payment
   **and** the loud strand line; recovery runs on both wallets + the repo-wide mint-quote sweep.
5. **Fail-closed, timer-bounded** — every mint touch through the bounded/fail-closed reader
   (`BoundedFee::{Fee,Failed,Unreachable}`, `MINT_TOUCH_TIMEOUT`); any quote failure aborts before the
   melt; no timer-less await anywhere on the hop.

## Build environment (this workspace)

Clone `/srv/forge/workspaces/mobee-crossmint`, branch `crossmint/hop` off `origin/dev` @ `7463cbe`.
Toolchain is NOT on the default PATH — rust 1.95.0 + gcc-wrapper 15.2.0 from the nix store; edition
2024 needs ≥1.85 so 1.95.0 is fine. `CARGO_TARGET_DIR=/srv/forge/workspaces/.crossmint-target`
(separate, per the charter). Gate runner writes `<label>.log` + `<label>.rc` with no trailing
pipeline, so pasted rcs are real rcs.

⚠ Shell gotcha that already bit once: `cat` is aliased to `bat` here, so `$(cat file)` expands to
**empty** silently — it filed issue #186 with an empty body before I caught it. Use `--body-file`,
`command cat`, or `$(<file)`.
