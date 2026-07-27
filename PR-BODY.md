# Cross-mint hop: pay a seller at a mint the buyer holds nothing at

A buyer funded only at mint A could not pay a seller who accepts only mint B. Now it can: the buyer's own wallet raises a NUT-04 mint quote at B, melts at A to pay that invoice, and holds fresh ecash at B — where the existing send path takes over completely unchanged. Lightning is connective tissue between the two mints and nothing more, so the wire still carries exactly one settlement shape, and pays-once, the co-signed receipt, and amount-from-the-buyer-signed-offer are untouched by it.

## Delivery is pinned; the buyer's cost floats

The reference pattern for this (agicash's `receive-cashu-token`) solves the fee problem the other way round: it pins the token amount and lets the **delivered** amount float DOWN through a bounded convergence loop until the fees fit. That is precisely what our strongest money invariant forbids — the seller receives the amount they signed for.

So the direction is inverted here. Delivery is pinned at `offer.amount` and the **buyer's cost** floats up:

```
planned_cost = melt.amount + melt.fee_reserve + input_fee
```

Two consequences worth stating: the convergence loop disappears entirely (cost is one forward computation), and the fixed-delivery invariant holds *by construction* — the amount the seller receives is never a function of any fee reading.

## ⚠ A documented refusal disappears

**`mint_unreachable_pay` is unreachable on the pay path as of this slice.** "The single-mint buyer wallet holds no balance at any accepted mint" was a refusal; it is now a hop. That is the feature, but it is also a safety property being removed, so here is what replaces it.

**The covering refusal is the target fence.** A hop ends with the buyer holding ecash at the target, so an unfenced target would be a real-mint back door with `allow_real_mints` off — a NEW entry point to money created by this slice. `plan_payment` fences the source AND the target and refuses fail-closed when no accepted mint is admissible. Both halves of the boundary are pinned by tests sitting next to each other:

- `pay_plan_hops_when_the_configured_mint_is_not_listed_but_the_target_is_admissible` — the old refusal's fixture, now asserting the decision that replaced it.
- `pay_plan_refuses_when_no_overlap_and_no_accepted_mint_is_admissible` — no admissible landing still refuses.
- `pay_plan_refuses_a_no_overlap_hop_under_the_default_posture` — with the flag off the fence admits exactly one mint, so a hop is **structurally unreachable in the default posture**. The operator's opt-in is what makes two distinct admissible mints possible at all.
- `authorize_pay_refuses_an_inadmissible_hop_with_zero_spend_and_no_pairing` — the same refusal end-to-end through the real pay path: zero budget burned, no pairing left on disk for a later run to resume.

### The replacement invariant: the hop fails closed at every leg

With the membership refusal gone, the hop is what now stands between "cannot settle at the seller's mint" and a wrong or partial spend. Every leg refuses without spending:

| leg | what fails | what happens |
|---|---|---|
| plan | fence rejects source or target | refuse before the gate — **zero charge** |
| plan | mint quote / melt quote / fee read fails or times out | refuse before the gate — **zero charge** |
| plan | source wallet cannot cover `planned_cost` | refuse before the gate — **zero charge** |
| gate | cap below `planned_cost` | the effect never runs, so **no melt is attempted at all** |
| melt | source mint reports `Pending` | refuse, never melt again (money may be in flight) |
| melt | source mint reports `Failed` | refuse; nothing left the wallet (see #194) |
| mint | target unreachable or refuses to issue | refuse; no completion record, no ecash claimed |
| mint | target issues an amount ≠ the pinned delivery | refuse rather than carry a short amount into the send |

`no_failing_leg_leaves_a_completion_record_behind` drives all five reachable failure legs and asserts, for each, that no completion record was written and no ecash was claimed.

## The decision seam turned out to be three seams

`resolve_realized_mint` had **three** production callers, and wiring only the pay path would have shipped this feature dead — the other two refuse a no-overlap claim before the pay path ever sees it:

1. `job_lifecycle::accept` — sealed the realized mint and refused no-overlap ("never accepted into an unpayable bind"), so such a job never reached `authorize_pay`.
2. `buyer::lifecycle::claim_is_payable` — the award filter. Its own comment claimed "the SAME resolution the pay path performs", so the buyer would never even award a claim it can now settle. That comment is rewritten to the new truth.

All three now call `crossmint::plan_payment`; `resolve_realized_mint` is deleted.

### Why the accept-bind still seals the buyer's own mint

The seal stays the buyer's **selected (funded)** mint, not the hop target. Sealing the target would break the feature: at pay time the sealed value would already be in the accepted set, `plan_payment` would say `Direct`, and the hop would never fire — a direct payment from a mint the buyer holds nothing at.

Attempt-id stability is preserved without storing a second field. On the direct path the sealed value is byte-identical to before (the old resolver returned the buyer's own mint in every `Ok` branch). For a hop, the realized mint is re-derived deterministically from two values that are *both* frozen at accept — the sealed selection and the accepted set beside it — so a config-default change after accept still cannot shift the attempt id. `the_sealed_source_replans_into_the_same_plan` pins exactly that.

## Pays-once across two mints

cdk journals each leg on its own: a melt quote's state is recoverable from a cold process by quote id, and a mint quote that was paid but never issued can still be issued later. The one thing nothing in cdk knows is that the two quotes are **one hop** — so that pairing, and nothing more, is what gets journalled, before the melt.

That ordering is what makes the melt leg safe to re-enter, and it is stronger than marking a melt "initiated" once the mint reports it pending — that leaves a window where money is in flight and the flag says otherwise.

**On a resumed attempt the persisted quote ids WIN over freshly planned ones.** Raising a second melt quote for one attempt id is exactly the double-pay the journal exists to prevent; a pairing that disagrees with the persisted one is refused outright rather than reconciled.

The resume decision is taken from what the mints say, never from what we infer:

| melt at source | mint at target | action |
|---|---|---|
| `Unpaid` | — | nothing left; melt (the mint's own answer, not a guess) |
| `Pending` | — | money in flight; refuse, never melt again |
| `Paid` | not issued | **the strand** — issue at the target, and say so LOUDLY |
| `Paid` | issued | complete without touching either mint |

The strand row is the one that must never pass in silence: a buyer whose sats left the source but whose ecash never arrived has money that is neither spent nor held.

## Recovery runs on both mints, or refuses to call it recovery

cdk's `recover_incomplete_sagas` filters to its own wallet's mint. A two-mint operation recovered on one wallet **silently skips the other mint's saga** — it reports success having examined half the problem. The buyer daemon's startup sweep opens both mints and recovers each, and `require_both_mints_recovered` turns a half-covered sweep into a refusal rather than a false "swept".

The sweep exists because a hop interrupted by a crash is not something the next pay attempt necessarily re-drives: that attempt may never be retried, and the sats would sit melted at the source with no ecash anywhere and nothing looking for them. It reports unconditionally, including the pass that found nothing — silence would be indistinguishable from a sweep that has stopped running.

## Teeth, and the mutations that prove they bite

**577 workspace tests pass (`cargo test --workspace`, rc=0)**, up from 546 on the merge base. Seven mutations were each reverted after proving a *specific* tooth fails — not a blanket failure:

| mutation | tooth that fails |
|---|---|
| overlap no longer plans `Direct` | the two decision teeth |
| target fence dropped | the two fence teeth |
| input fee dropped from the cap total | the cap-arithmetic tooth |
| persisted pairing no longer wins | `a_second_pairing_for_one_attempt_is_refused_rather_than_melted` |
| strand completed but not reported | `kill_between_melt_and_mint_pays_once_on_restart_and_reports_the_strand` |
| recovery guard checks only the source | `a_sweep_that_covered_one_mint_refuses_to_call_the_hop_swept` |
| cap charged `delivered_sats`, not `planned_cost` | `the_cap_is_charged_the_hop_cost_and_the_direct_amount` |

`cap_charge` exists as a named function precisely so that last mutation has somewhere to bite; inline, only a live trade would have noticed it change.

The kill-between-legs tooth is the strong form of pays-once: the run dies after the melt lands, a restart resumes from the journal, and the assertion is that the second run melts **zero** times, issues exactly once, and reports the strand.

## Scoped out, named rather than hidden

- **#186 — the fee reserve is charged in full.** A hop's cap charge includes the whole Lightning fee reserve, never reconciled against the fee actually paid, so a hop can leave the cap reporting *less* remaining budget than the buyer really spent. That is the safe direction. Reconciling reserve-versus-actual reshapes the spend ledger, which is money-gate machinery and its own slice. Marked `// interim:` at the charge site.
- **The input fee is an upper bound.** The melt selects its inputs when it runs, so the exact count is not knowable before the cap check; it is priced at every unspent proof at the source — the most the melt could possibly select. Over-states rather than under-states, because under-stating would put a fee on the wire the cap never saw, which is the #185 defect class.
- **#194 — a `Failed` melt dead-ends the attempt.** Recovering it means a superseding melt quote against the still-unpaid target invoice, which turns the melt leg into a sequence and requires every pays-once argument to be re-made over that sequence. Marked `// interim:` in code.
- **#187 — one mint list per side.** The fence hardcodes a single admissible mint and `extra_mints` is dead config, which is why a hop needs `allow_real_mints` to be reachable at all.
- **"Both wallets recovered" has no hermetic tooth.** Proving it would mean faking a cdk `Wallet` faithfully enough to reproduce its per-mint saga filtering. Instead the property is a runtime refusal, and the guard that enforces it *is* toothed. That is the honest form when the dependency cannot be faked: enforce it at runtime and test the enforcement.

## No live cross-mint trade has been run

Test ecash **structurally cannot hop**: fake mints cannot pay each other's invoices over real Lightning. So control flow, the journal, budget accounting and recovery are proven hermetically; **real Lightning routing between two mints is not.** A live cross-mint smoke is a real-money spend plus a money-gate config change, and is gated on the repo owner. The smoke script is written ready-to-run for the moment that authorization arrives.

## ⚠ Gate evidence — this box's toolchain is older than the tree's

The newest Rust available here is **1.95.0**; the tree is formatted and linted by a newer stable. On the **pristine** merge base `7463cbe`, before a single edit:

- `cargo fmt --all -- --check` → **rc=1, 733 diffs**
- money-combo clippy `--all-targets -- -D warnings` → **57 errors across 20 files**

**CI runs neither fmt nor clippy** — `.github/workflows/ci.yml` is build + test only, on `dtolnay/rust-toolchain@stable`. Whole-tree fmt and clippy on this box therefore measure toolchain skew, not this diff, so the evidence below is per-file against that same merge base:

| file | fmt hunks (base → branch) | clippy hits (base → branch) |
|---|---|---|
| `crossmint.rs` | new file → **0** | new file → **0** |
| `crossmint_hop.rs` | new file → **0** | new file → **0** |
| `authorize_pay.rs` | 28 → 26 | 13 → 4 |
| `job_lifecycle.rs` | 53 → 53 | 5 → 5 |
| `payment_wallet.rs` | 29 → 29 | 0 → 0 |
| `wallet_ops.rs` | 13 → 13 | 1 → 1 |
| `buyer/mod.rs` | 135 → 135 | 3 → 3 |
| `buyer/lifecycle.rs` | 33 → 33 | 1 → 1 |

Both new modules are fmt-clean and clippy-clean. No touched file gains a single fmt hunk or clippy hit; `authorize_pay.rs` loses several because the superseded resolver went with them. Whole-crate clippy goes **57 → 48**. Provisioning a matching toolchain fleet-wide is a separate call and deliberately does not ride this slice.

Counting is per-hunk rather than per-byte on purpose: rustfmt prints the absolute path in every hunk header, so byte totals differ between two checkouts for reasons that have nothing to do with the code.

## Gates run

```
cargo test --workspace                                                        rc=0   577 lib tests
cargo test -p mobee-core --no-default-features \
           --features gateway,git-delivery,wallet                             rc=0   577 lib tests
cargo test -p mobee-core --features acp                                       rc=0   170 tests
cargo build -p mobee --release                                                rc=0
rustfmt --edition 2024 --check   (per file, table above)                      rc=0 on both new modules
cargo clippy  (money combo, --all-targets -D warnings)                        0 hits on either new module
```

Every feature combo the slice ships in is compiled, not just the money set — a cfg-gated module is invisible to a gate that never enables its cfg.

One gate-hygiene trap worth not repeating: `cargo test -p mobee-core crossmint::` returns **rc=0 with zero tests run**, because `wallet`/`gateway` are not default features and the module is cfg-gated out. Assert the test **count**, not the rc.
