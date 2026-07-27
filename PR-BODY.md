# Cross-mint payment: the buyer can pay a seller on a mint it holds no ecash at

A buyer funded only at mint A can pay a seller who accepts only mint B. The buyer's own wallet does
the hop: request a NUT-04 mint quote at **the seller's mint** (yields a bolt11) → NUT-05 **melt** from
the buyer's funded mint to pay that invoice → receive fresh ecash at the seller's mint → hand it to the
**existing, unchanged** send path.

Lightning is connective tissue *between mints* only. The wire keeps exactly one settlement shape, so
pays-once, the co-signed receipt, and amount-from-the-buyer-signed-offer are untouched.

## The shape, and why it is not the obvious one

agicash's `receive-cashu-token` solves the mirror problem — "fees eat the received amount" — with a
bounded convergence loop: the token amount is fixed, so the **delivered** amount floats *down* until
`melt.amount + fee_reserve` fits.

Copying that direction here would break the strongest money invariant in the system. The amount is
fixed by the **buyer-signed offer**, so delivery is pinned and the **buyer's cost** floats *up*
instead:

1. mint quote at **B** for exactly `offer.amount` → the bolt11
2. melt quote at **A** for that bolt11 → `amount` + `fee_reserve`
3. input fee at A (existing bounded, fail-closed reader)
4. `planned_cost = melt.amount + melt.fee_reserve + input_fee`
5. cap check on `planned_cost` **before** any melt — refuse ⇒ zero money moved
6. journal the pairing, **then** melt at A → mint at B → existing send from B

With delivery pinned, cost is a single forward computation and the convergence loop disappears. The
amount the seller receives is never a function of a fee reading.

## Invariant 1 — entry points to the spend gate

The hop adds no path by which a seller- or mint-supplied number becomes the paid amount. The mint
amount at B is `offer.amount`; every fee reading feeds `planned_cost` (what the *buyer* spends),
never the delivered amount.

The hop does, however, create **a new entry point to money**, found while building this slice and
called out explicitly:

- **The hop target mint.** A hop ends with the buyer holding ecash at the target. Fencing only the
  buyer's source mint would therefore be a **real-mint back door with `allow_real_mints` off** — the
  buyer could come to hold real-sats ecash at an unfenced mint that merely appeared in the seller's
  accepted list. `plan_payment` fences **both** legs and refuses fail-closed ("nowhere permitted to
  land") when no accepted mint is admissible.
  Toothed by `hop_refuses_when_no_accepted_mint_passes_the_fence` and
  `hop_skips_unfenced_entries_and_lands_on_the_first_admissible_one`; both confirmed to bite (below).

## Recovery: the trap this design is built around

`Wallet::recover_incomplete_sagas()` (cdk 0.17.2, `wallet/recovery.rs:302`) filters
`saga.mint_url == self.mint_url && saga.unit == self.unit`. A hop has a **melt saga at A** and an
**issue saga at B**, so recovery run on one wallet **silently skips** the other half — no error, just
an unrecovered leg. Hop recovery therefore runs on **both** wallets, plus the repo-wide unissued-quote
sweep, and a melted-at-A/unminted-at-B strand is surfaced **loud** rather than swept.

cdk journals each half itself (`WalletSaga` carries `quote_id`, and `check_melt_quote_status(quote_id)`
resumes off the persisted store from a cold process). The only thing cdk cannot know is that the two
quotes are one logical hop, so that pairing — and nothing more — is what we journal, before the melt.

## Deliberately not in this PR

- **Fee-reserve reconciliation** → #186. The cap is charged the full `fee_reserve` before the melt; the
  melt returns change worth `reserve − actual`, which is not credited back. The cap therefore
  under-reports remaining allowance — the safe direction (refuses too early, never overspends). Netting
  it would reshape `budget.rs`'s append-only fold, i.e. put a spend-gate change behind a feature review.
  Marked `// interim:` at the charge site.
- **The general fee-dust behaviour of `authorize_pay`** is untouched — that is #185 proper. The direct
  path still charges `amount` exactly as before; only the hop path charges `planned_cost`.
- **One mint list per side** → #187. Today `mint_allowed` admits exactly `DEFAULT_MINT_URL` when
  `allow_real_mints` is off, so multi-mint is structurally unreachable in the safe posture, and
  `extra_mints` is dead config the fence never consults.

## Gate evidence — read this with eyes open

The newest rust available on the build box is **1.95.0**; this repo is formatted and linted by a newer
stable. Two of the charter's six gates therefore measure toolchain skew rather than this diff:

- `cargo fmt --all -- --check` on the **pristine tip `7463cbe`, before any edit in this branch**:
  **rc=1, 733 diffs**, in files this PR never opens.
- `cargo clippy --no-default-features --features gateway,git-delivery,wallet --all-targets -- -D warnings`:
  **57 errors across 20 files, all pre-existing** (authorize_pay, budget, buyer/*, gateway,
  seller_node/*, wallet_ops, …). The run found exactly **one** lint in this PR's own file
  (`assert_eq!` against a bool literal); it is fixed, taking the count 58 → 57.
- This PR's own files are clean by both: `rustfmt --edition 2024 --check crossmint.rs` → **rc=0**, and
  **0** clippy hits naming `crossmint.rs`.
- **CI runs neither fmt nor clippy.** `.github/workflows/ci.yml` is build + test only, on
  `dtolnay/rust-toolchain@stable`. The charter's fmt/clippy gates are stricter than CI.

Per-file evidence was accepted for this slice specifically so a feature slice does not carry a
fleet-wide toolchain change; the whole-tree question is tracked separately as a standing gate condition
on this box.

Note when reproducing: `rustfmt --check lib.rs` recurses the entire crate through `mod` declarations
and will hand back the tip's 733 diffs — check the individual file.

## Teeth

Decision seam, 9 tests, all green. Both mutations were run **after** committing, and bite specifically
rather than blanket:

```
clean:       test result: ok. 9 passed; 0 failed   (rc=0)

mutation 1 — overlap no longer short-circuits to Direct        -> rc=101
  buyer_mint_in_accepted_set_pays_direct_without_hopping ... FAILED
  buyer_mint_listed_but_not_first_still_pays_direct ... FAILED
  (the other 7 still ok)

mutation 2 — hop-target fence dropped, .find(fenced) -> .next() -> rc=101
  hop_refuses_when_no_accepted_mint_passes_the_fence ... FAILED
  hop_skips_unfenced_entries_and_lands_on_the_first_admissible_one ... FAILED
  (the other 7 still ok)
```

Invariant 2 is toothed on the **decision** (`PayPlan::Direct`), not on a downstream outcome — a hop
that happened and then coincidentally produced the right amount would still be a bug.

Also note a gate-hygiene trap worth not repeating: `cargo test -p mobee-core crossmint::` returns
**rc=0 with zero tests run**, because `wallet`/`gateway` are not default features and the module is
cfg-gated out. Assert the test **count**, not the rc. Correct invocation:
`cargo test -p mobee-core --features wallet crossmint::`.

## Live smoke

Test ecash structurally cannot hop — fake mints cannot pay each other's invoices over real Lightning —
so the live proof is a small **real-sats** cross-mint trade. That is a real-money spend and a
money-gate config change, so it is gudnuf-gated and not executed as part of this PR. The smoke script
is written to be ready-to-run the moment authorization arrives.
