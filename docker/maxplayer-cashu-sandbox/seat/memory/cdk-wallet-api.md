# cdk-wallet-api — CDK 0.17.2 wallet calls that exist

Verified by compiling against `=0.17.2`. Names in this family are easy to guess wrong; these are
the ones that are real.

## Construction

```rust
let store = Arc::new(cdk_sqlite::wallet::memory::empty().await?);   // or a file store
let wallet = Wallet::new(mint_url, CurrencyUnit::Sat, store, seed_bytes /* [u8; 64] */, None)?;
```

## Issue

```rust
let quote = wallet.mint_quote(PaymentMethod::BOLT11, Some(Amount::from(64)), None, None).await?;
let proofs = wallet
    .wait_and_mint_quote(quote, Default::default(), Default::default(), Duration::from_secs(30))
    .await?;
let minted = proofs.total_amount()?;         // needs `use cdk::nuts::nut00::ProofsMethods;`
```

Against the fakewallet mint the quote auto-settles, so `wait_and_mint_quote` returns promptly.

## Send / receive

```rust
let prepared = wallet.prepare_send(Amount::from(21), SendOptions::default()).await?;
let fee = prepared.fee();
let token = prepared.confirm(None).await?;   // consumes `prepared`
let amount = other_wallet.receive(&token.to_string(), ReceiveOptions::default()).await?;
```

`prepare_send` reserves proofs; the debit is not final until `confirm`.

## Melt

```rust
let quote = wallet.melt_quote(PaymentMethod::BOLT11, invoice_string, None, None).await?;
let prepared = wallet.prepare_melt(&quote.id, HashMap::new()).await?;
let finalized = prepared.confirm().await?;                 // or .confirm_prefer_async() -> MeltOutcome
finalized.state(); finalized.amount(); finalized.fee_paid();
```

`confirm_prefer_async()` returns `MeltOutcome::{Paid, Pending}`; a `Pending` can be awaited
directly, or resolved later with `wallet.finalize_pending_melts()` /
`wallet.check_melt_quote_status(&id)`.

## Balance — three separate pools, not two

Read from cdk 0.17.2 `src/wallet/balance.rs:7-40`. These are different queries over different
proof states, and conflating any two of them will make an accounting assertion pass while value is
stranded:

- `wallet.total_balance()` — **spendable only**: `get_balance(..., Some(vec![State::Unspent]))`.
- `wallet.total_pending_balance()` — total of `get_pending_proofs()`.
- `wallet.total_reserved_balance()` — total of `get_reserved_proofs()`. A **distinct** pool from
  pending. A conservation check that looks at spendable and pending but not reserved cannot see
  value stranded in reserve, which is exactly what an abandoned `prepare_send` leaves behind.

## Recovery — what actually returns value, and what does not

`wallet.check_all_pending_proofs()` is **not** a recovery call. Read the body
(cdk 0.17.2 `src/wallet/proofs.rs:112-185`): it skips saga-managed proofs, asks the mint about the
rest, deletes the ones the mint reports **spent**, and returns the **total still pending**. It does
not move survivors back to `Unspent`, and its return value is a residual, not a reclaim. Asserting
`check_all_pending_proofs() > 0` proves nothing was recovered; it proves the opposite.

The real path, three calls:

- `wallet.recover_incomplete_sagas()` (`src/wallet/recovery.rs:302`) — resumes persisted sagas.
  Returns `RecoveryReport { recovered, compensated, skipped, failed }` with `is_empty()`. An
  interrupted operation is either driven forward (`recovered`) or rolled back (`compensated`); both
  are success, and which one happens depends on how far the saga got.
- `wallet.get_pending_sends()` — operation ids for sends sitting in `SendSagaState::TokenCreated`,
  i.e. a token was minted for a recipient who never redeemed it.
- `wallet.revoke_send(operation_id)` (`src/wallet/send/mod.rs:275`) — **this** is the reclaim: it
  swaps those proofs back and returns the reclaimed `Amount`.

So after a send that was never redeemed: `recover_incomplete_sagas()`, then for each id from
`get_pending_sends()` call `revoke_send(id)`. Verify by expected **value**, not by "an error did not
happen": the funded total must come back whole net of fees actually charged, with pending **and**
reserved both at zero. A partial reclaim satisfies `reclaimed > 0` and is still a loss.

Scope note, so this is not read as more than it is: the seat's gate proves recovery across a
**wallet-object and store reopen** — the wallet is dropped and the same sqlite file is reopened in
the same process. That is not a proof of recovery after an OS-level process crash, after seed-only
restore with no local database, of an interrupted **melt**, or of a crash at every intermediate
saga stage.

- Also available: `wallet.get_pending_proofs()`, `wallet.get_pending_spent_proofs()`,
  `wallet.get_reserved_proofs()`.

## Mint info — the name trap

It is **`wallet.fetch_mint_info()`** (returns `Result<Option<MintInfo>, Error>`), not
`get_mint_info` — that name exists only on `AuthWallet`. There is also `wallet.load_mint_info()`
returning `MintInfo` directly.

There is no `reclaim_unspent_proofs()`. Do not substitute `check_all_pending_proofs()` for it — that
returns a residual and reclaims nothing (see the recovery section above). The reclaim is
`revoke_send(operation_id)`.

## Failure signatures worth recognising

- Receiving an already-spent token: `Error` displaying **`Token Already Spent`**.
- A melt whose backend refuses: the wallet call errors with **`Payment failed`**. A melt can also
  come back `Ok` with a non-`Paid` state — assert on the state, not merely on `is_ok()`.
