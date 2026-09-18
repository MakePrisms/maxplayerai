//! Paying the accrued platform fee to the platform's Lightning address — **the one remit path in
//! the product**, and its first seller-fee payment code (operator melts existed before it).
//!
//! ## Who calls this, and when
//!
//! [`remit`] has exactly three callers, and all three are named here so a grep confirms it:
//!
//! 1. **The seller node's collect path** (`seller_node::run`, the `Collected::New` arm of the
//!    receipt write), through [`remit_live_best_effort`] → [`remit_best_effort`] under
//!    [`RemitTrigger::Collect`]. This is the mechanism, and the fast path: a fee the seller had to
//!    remember to pay would not be a fee, so the node remits as a consequence of collecting, normally
//!    within a second of the sale. It fires only when a receipt was journaled **New** — never on a
//!    replayed wrap (`Duplicate`), never on an error — and it is **best-effort**: whatever happens
//!    here is logged and journaled; it cannot fail the collect, delay the job being marked paid, or
//!    change what the seller received.
//! 2. **The seller node's retry tick** (`seller_node::run`, an arm of the live loop's `select!`),
//!    through the same two functions under [`RemitTrigger::Retry`]. This is the safety net behind
//!    the fast path (stage 2a, addendum 2): a failed remittance retries on the node's own clock — base
//!    30 s, doubling to a 30-minute cap, full jitter — for as long as the node runs, and stops with
//!    the loop. [`RemitBackoff`] is the pacing; [`RemitFlight`] keeps the two node paths to one
//!    attempt in flight.
//! 3. **`maxplayer seller fees remit`** (`crates/maxplayer/src/seller_fees.rs`): inspection (the
//!    default dry run resolves, quotes, prints the plan and the recent attempts, and moves nothing),
//!    recovery (`--confirm` forces an attempt now, for an operator whose automatic path has been
//!    failing), and reconciliation (an interrupted attempt is settled or released against the mint).
//!
//! The `[platform_fee] auto_remit` switch ([`crate::home::PlatformFeeConfig`]) turns BOTH node
//! paths off — the collect path's attempt and the retry tick — and does not touch accrual;
//! `--confirm` pays regardless. There is no startup sweep and no other call site. The node's two
//! attempts run on threads the node owns and drains on shutdown (bounded); see `seller_node::run`.
//!
//! ## What one attempt does
//!
//! Reconcile any in-flight attempt first; read the unremitted balance; resolve
//! [`PLATFORM_FEE_ADDRESS`] over LNURL-pay ([`crate::lnurl_pay`], fail-closed) and refuse below its
//! minimum — the expected steady state for small sellers, not an error; probe the mint's melt fee
//! reserve on the gross and invoice the **net**, so the fee comes OUT of the accrued amount; journal
//! the plan (which pins the receipts, records this process as the row's owner under a lease, and
//! refuses a duplicate — the idempotency that makes two concurrent collects pay at most once); raise
//! the **payment quote** and check its invoice + reserve alone against the hard [`MeltCeiling`]
//! (the reserve-only precheck; refused ⇒ the planned row is NOT written: it stays planned, unbound
//! and ours, receipts pinned, nothing spent, and the next attempt's reconciliation releases it —
//! addendum 10 §2); if the live reserve differs from the estimate, optionally **re-plan** ONCE onto a
//! new invoice; **prepare** the melt of that quote through
//! [`crate::wallet_ops::prepare_melt_payment_blocking`] — the wallet selects and reserves proofs in
//! its own database (it may fetch mint metadata/keysets, a GET; it posts nothing proof-bearing or
//! fee-bearing) and states the SDK's PREPARED proof-input fee and pre-melt swap fee; run the SDK's
//! **post-swap arithmetic** on those figures — the bound is the ACTUAL input fee recomputed on the
//! swapped split, not the prepared display: invoice + reserve + actual input fee + swap fee must fit
//! the gross (over, or the SDK would refuse after its swap ⇒ the prepared melt is cancelled locally
//! and the same before-fence refusal: row stays planned, receipts pinned, nothing posted); pass the
//! **pre-spend gate** — ONE compare-and-set
//! in the store advances the row `planned → spending` and BINDS that quote to it (only if still
//! planned, still ours, with more than [`SPEND_MARGIN`] of lease left at the clock read INSIDE the
//! store call; zero rows changed is a refusal, and the prepared melt is cancelled); then **confirm**
//! the prepared melt — the swap if one is required, then the melt request for exactly that quote, by
//! id — the same gated wallet `maxplayer wallet melt` uses (it honours `allow_real_mints`); it never
//! raises a second quote; settle. Every attempt that meant to pay is journaled with its outcome
//! (`fee_remit_attempts`), so a payout that keeps failing is visible in the read-out rather than
//! silent.
//!
//! ## The two invariants (stage 2a, addendum 3; the fence of addendum 4; the hold of addendum 6; the total bound of addendum 8)
//!
//! **Money hold (§1):** the seller never pays more than the fee it accrued — gross is the ceiling,
//! every cost of the payment comes out of it, and the ceiling is enforced at the moment of spending,
//! not estimated beforehand or regretted afterwards. The estimate at plan time is a plan; the quote
//! the wallet actually pays under is checked against `gross`, and a reserve that grew in between is
//! a refused, journaled, failed attempt with the balance intact. Since addendum 8 the bound is on the
//! **entire wallet debit**: the pinned CDK 0.17.2 wallet charges, on top of invoice + fee reserve, a
//! proof-input fee on the proofs it sends (`input_fee_ppk`, NUT-02) and — when its proofs do not fit
//! — the fee of a pre-melt swap it performs inside `confirm`; the delivered ceiling of rounds 1–6
//! saw neither (verdict B4 at 19f30d3). Now the melt is PREPARED before the fence — `prepare_melt`
//! selects and reserves proofs in the wallet's own store and exposes `input_fee`, `swap_fee` and
//! `requires_swap` (`melt/mod.rs:428–450`) while posting no proof-bearing or fee-bearing request
//! (it may fetch mint metadata/keysets, a GET; the swap `melt/saga/mod.rs:687–697` and the melt
//! request `:907–911` both live inside `confirm`) — and [`MeltCeiling::admits_confirmable`] is taken on
//! those four figures; over ⇒ `PreparedMelt::cancel` (`:817–831`, best-effort local compensation:
//! proofs back to Unspent, quote released) and the ordinary before-fence refusal (the planned row
//! is NOT written: it stays planned, unbound and ours, its receipts pinned; ONE `REFUSED before
//! spending` line naming the total, its parts and the ceiling; backoff continues and the next
//! attempt's reconciliation releases the row as our own earlier attempt and re-plans — addendum
//! 10 §2) — a fee refusal never leaves a bound Spending row held.
//!
//! **The fee model is CDK's, not the prepared display's (addendum 9).** The prepared `input_fee` is
//! an ESTIMATE — the fee on the split of invoice + reserve before that fee is added
//! (`melt/saga/mod.rs:383–387`). `confirm` swaps to a target of invoice + reserve + that estimate
//! (`:678`), receives exactly the target's denomination split, RECOMPUTES the input fee on that
//! split (`:704`) and refuses — after the swap has been paid — when the target does not cover
//! invoice + reserve + the actual fee (`:706–712`). So this module runs that arithmetic itself,
//! twice, before any fee-bearing effect: **planning** ([`plan_confirmable_invoice`]) searches down
//! from gross − reserve for the largest invoice whose post-swap arithmetic holds and whose worst
//! case (invoice + reserve + actual input fee + swap fee) fits the gross — none ⇒
//! [`Refusal::FeesDoNotFit`] — and re-checks the quote actually raised; **before the fence**
//! ([`confirm_would_succeed`]) the same check runs on the prepared figures and the keyset's
//! `input_fee_ppk` ([`crate::wallet_ops::MeltPreparation::input_fee_ppk`]) and refuses — prepared
//! melt cancelled, row left planned with its receipts pinned, one line — when the SDK would refuse
//! after its swap or the actual worst case exceeds the gross. The PREPARED total is not the bound:
//! a prepared total over the gross whose actual debit fits (19/2/1000/[32] ⇒ invoice 13, prepared
//! 20 > 19, actual 19 ≤ 19) is admitted. After payment the SDK's `FinalizedMelt::fee_paid` (`:139–148`:
//! proofs sent − invoice − change) already CONTAINS the actual input fee beside the Lightning fee;
//! the actual debit is invoice + `fee_paid` + swap fee, counted once — the prepared estimate is
//! printed, never added. A failed post-payment balance read prints `unknown`, never a computed
//! figure.
//!
//! **Bound, disclosed not solved (addendum 9 §1.5):** the mint's fee metadata can change between
//! prepare and confirm, and the SDK takes no caller maximum into `confirm`. A change there can make
//! `confirm` fail after the swap (the swap fee is gone, the row is bound Spending and HELD by the
//! PAID-only rule below) or cost more than the arithmetic predicted (the `WARNING` line after
//! payment). No exploit is claimed or reproduced; no changing-fee mint was measured. Likewise
//! **owed**: a cancelled preparation whose SDK compensation itself failed can leave a local proof
//! reservation — `open_wallet_async` only constructs the wallet and CDK `recover_incomplete_sagas`
//! is not run on this path (a blanket recovery could replay a still-live payer's saga); a supported
//! recovery path is owed, not wired.
//!
//! The fee-bearing fake wallet — whose `confirm` models the swap output split, the swap fee charged
//! even when the melt then fails, the recomputed input fee, the post-swap refusal and the inclusive
//! `fee_paid` — and the regressions
//! `a_fee_bearing_payment_whose_prepared_input_fee_differs_from_the_actual_pays_once_and_the_wallet_loses_at_most_the_gross`
//! (one swap, one melt, the wallet measured),
//! `a_fee_bearing_schedule_whose_prepared_figures_fit_but_post_swap_arithmetic_does_not_is_refused_before_the_fence`
//! (wallet delta 0, no swap, no melt, row left planned and pinned),
//! `a_fee_bearing_total_that_exceeds_the_gross_by_the_fee_is_refused_before_any_swap_or_melt`,
//! `fee_aware_planning_sizes_the_invoice_so_a_fee_bearing_payment_fits_without_reserve_slack`
//! (invoice 13, not 14), `fees_that_can_never_fit_are_refused_at_planning_not_at_payment` and
//! `a_failed_balance_read_after_a_fee_bearing_payment_prints_unknown_and_no_false_warning` hold
//! it; the reserve-grew case `a_reserve_that_grows_between_estimate_and_payment_is_refused_before_spending`
//! stands.
//!
//! **Ownership (§2), as the tests in this module prove it:** a row is paid only by the process that
//! planned it, only through the fence — [`SellerStore::admit_remittance_spend`], which two
//! processes cannot both pass — and only under the ONE quote the fence bound to it: the payer never
//! raises a second quote for a row it holds, and a quote it has not raised cannot be spoken for.
//! While that row is in flight — `planned` or `spending` — no second remittance against the same
//! balance can be planned by anyone: [`SellerStore::plan_remittance`] refuses inside its own
//! transaction, and a partial unique index refuses underneath it; neither predicate has a time
//! term. Every release is a conditional transition carrying its reason's predicate
//! ([`ReleaseOn`]): zero rows changed means the row moved under the releasing process, which then
//! holds. A `planned` row (fence not yet passed: nothing spent against it, and once released its
//! owner's fence changes zero rows) is released by another process only when its invoice's quote
//! is FAILED or UNPAID and expired, or its owner's lease of [`REMIT_LEASE`] has run out — never on
//! a live UNPAID alone, because UNPAID means "not yet", not "abandoned"; a planned row whose lease
//! ran down while its owner paused is refused by the owner's own fence, which reads the clock
//! inside the store's lock (`an_owner_whose_lease_ran_down_while_it_paused_is_refused_by_its_own_fence`;
//! released-then-refused: `an_owner_that_outlives_its_lease_is_released_and_its_fence_then_changes_zero_rows`,
//! `an_owner_paused_after_its_quote_and_past_its_lease_is_released_and_never_pays_that_quote`);
//! and a release decided on a stale planned snapshot changes zero rows once the owner's fence has
//! landed (`a_release_decided_on_a_stale_planned_snapshot_cannot_revoke_a_later_admission`).
//!
//! A `spending` row (fence passed, a quote bound: its owner may be mid-melt) **is released by
//! nobody, on no clock.** It settles when the mint reports its BOUND quote, asked by id, PAID; on
//! anything else — UNPAID however long past its expiry, FAILED, PENDING, UNKNOWN, a quote the wallet
//! does not know — it is HELD, and its receipts with it, until the mint says PAID or an operator
//! decides (no override exists in this round). **We do not infer terminality from a clock**, and
//! not from the mint's FAILED either, because the inspected CDK 0.17.2 mint implementation
//! (checksum-pinned source, read in the round-4 and round-5 verdicts — the implementation this
//! wallet is built on, not a measurement of whichever server a configured mint URL reaches) pays an
//! UNPAID *or FAILED* quote with no expiry check, and the wallet's own request, once past
//! `prepare_melt`, re-checks nothing: a
//! payment prepared before the quote expired can land after any observation a second process makes.
//! A release on "expired" or "FAILED" would therefore make the same gross payable twice
//! (`a_payment_prepared_before_expiry_cannot_be_doubled_by_a_release_after_it` schedules exactly
//! that ordering — A paused inside its payment after its last local check, the quote expiring, B
//! held with funds for a second payment in the same wallet — and counts one debit;
//! `a_bound_quote_expired_past_the_margin_is_held_and_its_owner_refuses_to_pay_it`,
//! `a_spending_row_is_not_released_when_its_lease_expires_and_its_owner_pays_exactly_once`,
//! `a_spending_row_is_reconciled_by_its_bound_quote_not_by_an_expired_estimate`,
//! `a_spending_rows_bound_quote_decides_its_release_on_the_full_path` — whose FAILED arm has the
//! mint pay the FAILED quote — and the table
//! `a_spending_row_is_never_released_by_reconciliation_only_settled`). What "at most one debit"
//! rests on is the exclusion: a second attempt is never admitted while a bound spending row
//! exists — at two boundaries. The ordinary one is RECONCILIATION: every `remit` run first finds the
//! in-flight row and, when it is SPENDING with a quote bound and the mint's answer about that quote
//! is anything but PAID, returns [`Refusal::SpendingHeld`] before it plans anything (this is where
//! B stops in (b1), (b2), (d) and the delayed-confirm test; a PLANNED row in flight is a different
//! case — (a)'s B returns [`Refusal::HeldByOwner`], (c)'s B releases the expired-lease row and pays
//! its own quote, (B2)'s B returns [`Refusal::RowChangedUnderMe`]). Behind it is the STORE:
//! [`SellerStore::plan_remittance`] refuses a second plan with `PlanRefused::InFlight` inside its own
//! `IMMEDIATE` transaction while any planned-or-spending row exists — the race-closing layer, reached
//! when two runs both saw no row (`two_racing_attempts_against_the_same_balance_record_exactly_one_remittance`)
//! and exercised directly, on B's own connection while A is paused inside its payment, in
//! `a_payment_prepared_before_expiry_cannot_be_doubled_by_a_release_after_it`. It does not rest on
//! when a quote dies. The cost is named, not hidden: a melt the mint
//! genuinely failed leaves the row held and every later remittance refused until an operator acts
//! (owed as later work; the CLI exits 3 and prints one `HELD:` line naming the row, the quote, the
//! mint's answer and the pinned sats). The owner's own reconciliation of its own `planned` row may
//! release on UNPAID: a process runs at most one attempt at a time ([`RemitFlight`] in the node; one
//! shot for the command), so its earlier attempt is over and, the fence never having been passed,
//! spent nothing. Its own `spending` row gets no such exception.
//!
//! **What the two-process tests prove, and their bound:** two processes, each on its own store
//! connection, debit an accrued balance at most once under every interleaving they schedule —
//! pauses after the plan, after the quote, after the fence, inside the payment after the wallet's
//! last local check, and between a release decision and its write; the lease and the quote expiring
//! while paused; distinct invoices; actual melts counted; the fake mint accepting UNPAID or FAILED
//! quotes regardless of expiry, as the inspected CDK 0.17.2 implementation does. What each shares
//! is stated per test, not assumed. One fake mint (the quote registry): (a), (b1), (b2), (c), (B2),
//! (d) and the delayed-confirm test. One effects clock handed from A to B: (b2), (c), (B2) and the
//! delayed-confirm test — (a), (b1) and (d) leave each process its own clock. One fake wallet (one
//! proof pool both processes select from): the delayed-confirm test only; the single-process
//! fee-bearing tests of addendum 8 each own a pool of their own. The older (b)/(c) cases and the
//! node's 2b test script the mint's answer on one process instead of reading a shared registry.
//! They do not run a real mint or a real wallet, and
//! `a_release_decided_on_a_stale_planned_snapshot…` moves its command clock (401) independently of
//! its effects clock (100) to force the SQL ordering — a synthetic time model, not a claim about
//! how a mint's clock behaves.
//!
//! Every effect on the world goes through [`RemitEffects`], so the decision logic is tested against
//! scripted effects without a network or a mint. Exactly one method of that trait spends:
//! [`RemitEffects::confirm_melt`] (on a melt [`RemitEffects::prepare_melt`] prepared and bounded).

use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::home::MaxplayerHome;
use crate::lnurl_pay::{self, HttpsFetch, LightningAddress, PayRequest, ResolvedInvoice};
use crate::platform_fee::PLATFORM_FEE_ADDRESS;
use crate::seller_node::store::{
    FeeRemittance, OwnershipLost, PlanRefused, ReleaseOn, RemitAttempt, RemitAttemptOutcome,
    RemitAttemptTrigger, RemitSettlement, RemittancePlan, RemittanceReplan, RemittanceState,
    SellerStore, SettledBy,
};
use crate::wallet_ops::{
    self, MeltCeiling, MeltEstimate, MeltOutcome, MeltPreparation, MeltQuoteState, MeltQuoteStatus,
    WalletOpsError,
};

/// How many journaled attempts the command prints, newest first.
pub const RECENT_ATTEMPTS_SHOWN: usize = 5;

/// How long a process's claim on a `planned` row stands (addendum 3 §2). Another process may
/// release the row on UNPAID / no-quote only after this has passed since the plan. Five minutes is
/// far longer than a melt needs to leave UNPAID: a melt that reaches the mint turns its quote
/// PENDING or PAID within seconds, and one that never reaches it errors out and ends the attempt.
pub const REMIT_LEASE: Duration = Duration::from_secs(5 * 60);

/// How much of its lease an owner must still hold to be ADMITTED to a payment. Another process is
/// entitled to release a PLANNED row once its lease ends (a spending row it never releases), so an
/// admission that landed with less than this margin would race that release. A payer also refuses
/// to pay its bound quote inside this margin of the quote's expiry — to avoid a pointless attempt
/// the mint would likely turn down, NOT as a safety bound: nothing about a spending row's release
/// is inferred from this margin, or from any clock (addendum 6 §0–§1). Processes share one host
/// clock (the store is a local file); the margin covers scheduling pauses on that clock.
pub const SPEND_MARGIN: Duration = Duration::from_secs(60);

/// Why [`RemitEffects::prepare_melt`] or [`RemitEffects::confirm_melt`] did not pay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeltFailure {
    /// The payment was refused by the wallet layer with NOTHING posted to the mint: the quote's
    /// stored amount and reserve did not fit the [`MeltCeiling`], or — after `prepare_melt`
    /// selected and reserved proofs in the wallet's own database — the ACTUAL worst-case debit
    /// (invoice + fee reserve + the input fee the SDK recomputes on the swapped split + swap fee)
    /// did not fit it, or the SDK would refuse after its swap, and the prepared melt was cancelled
    /// (addendum 9 §1.1). Raised by [`RemitEffects::prepare_melt`], which the remittance calls
    /// BEFORE the fence, so the row is still Planned: it is not written, its receipts stay pinned,
    /// and the next attempt's reconciliation releases it (addendum 10 §2). Nothing left the wallet.
    RefusedBeforeSpending(String),
    /// The payment failed somewhere the caller cannot see: from `prepare_melt` (unknown or expired
    /// quote, proofs short — still nothing posted, see the caller) or from `confirm_melt` (proofs may
    /// or may not have reached the mint, or the mint refused the bound quote). After the fence the
    /// spending row stays for reconciliation of its bound quote against the mint; the payer never
    /// re-quotes.
    Failed(String),
}

/// A melt PREPARED by [`RemitEffects::prepare_melt`] and not yet decided: proofs selected and
/// reserved in the wallet's own store, the SDK's fee figures known and already admitted by the
/// ceiling, nothing posted to the mint. The remittance holds it across the fence, then
/// [`RemitEffects::confirm_melt`]s it (the payment) or [`RemitEffects::cancel_melt`]s it (nothing
/// to undo at the mint).
#[derive(Debug)]
pub struct PreparedMelt {
    pub preparation: wallet_ops::MeltPreparation,
    token: PreparedToken,
}

#[derive(Debug)]
enum PreparedToken {
    Live(wallet_ops::PreparedMeltPayment),
    #[cfg(test)]
    Fake(u64),
}

impl PreparedMelt {
    fn live(payment: wallet_ops::PreparedMeltPayment) -> Self {
        Self {
            preparation: payment.preparation.clone(),
            token: PreparedToken::Live(payment),
        }
    }

    #[cfg(test)]
    pub(crate) fn fake(preparation: wallet_ops::MeltPreparation, token: u64) -> Self {
        Self {
            preparation,
            token: PreparedToken::Fake(token),
        }
    }

    #[cfg(test)]
    pub(crate) fn fake_token(&self) -> Option<u64> {
        match self.token {
            PreparedToken::Fake(token) => Some(token),
            PreparedToken::Live(_) => None,
        }
    }
}

impl fmt::Display for MeltFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RefusedBeforeSpending(reason) => write!(formatter, "{reason}"),
            Self::Failed(error) => write!(formatter, "{error}"),
        }
    }
}

/// The remit path's effects on the world, behind a trait so the decision logic — what is paid,
/// when, and what is refused — is tested without a network or a mint. Exactly one method moves
/// money: [`Self::confirm_melt`]. Everything else reads or writes the wallet's own database
/// (raising a quote spends nothing; preparing a melt reserves proofs locally and posts nothing).
pub trait RemitEffects {
    /// This process's opaque owner token for the rows it plans (addendum 3 §2): stable for the
    /// life of the process, distinct across processes. The node's attempts and the command's run
    /// each speak as one owner.
    fn owner(&self) -> &str;
    /// LNURL step 1–2: the destination's payRequest (callback + sendable bounds).
    fn pay_request(&mut self, address: &LightningAddress) -> Result<PayRequest, String>;
    /// LNURL step 3–4: an invoice for exactly `amount_sats`.
    fn invoice(&mut self, pay: &PayRequest, amount_sats: u64) -> Result<ResolvedInvoice, String>;
    /// A melt quote for the invoice — the mint's fee reserve — WITHOUT paying. The ESTIMATE, at
    /// plan time: it sizes the net invoice and is journaled on the planned row as `melt_quote_id`.
    fn melt_estimate(&mut self, bolt11: &str) -> Result<MeltEstimate, String>;
    /// **The payment quote** for the planned invoice (addendum 5 §1, rule 1 step 1): raised after
    /// the plan is journaled, checked against the ceiling, then BOUND to the row by the fence — the
    /// one quote [`Self::prepare_melt`] prepares and [`Self::confirm_melt`] pays. Raising it spends
    /// nothing. The same wallet call as [`Self::melt_estimate`], distinguished so the two quotes'
    /// roles are told apart in the ledger.
    fn melt_quote(&mut self, bolt11: &str) -> Result<MeltEstimate, String>;
    /// **Prepare the payment, bounded on its total** (addendum 8 §1.1): re-checks the quote's stored
    /// amount and reserve against the ceiling, then `prepare_melt(quote_id)` — the wallet selects
    /// and RESERVES proofs in its own database and states the proof-input fee and any pre-melt swap
    /// fee — then refuses unless invoice + reserve + input fee + swap fee ≤ the ceiling, cancelling
    /// the prepared melt on refusal. Posts nothing to the mint; spends nothing. Called BEFORE the
    /// fence, so a refusal leaves a Planned row, never a held one.
    fn prepare_melt(
        &mut self,
        quote_id: &str,
        ceiling: &MeltCeiling,
    ) -> Result<PreparedMelt, MeltFailure>;
    /// **The payment.** `confirm` on the prepared melt: the pre-melt swap if one is required, then
    /// the melt request for the bound quote, BY ID. Never raises a quote. The only method here that
    /// spends.
    fn confirm_melt(&mut self, prepared: PreparedMelt) -> Result<MeltOutcome, MeltFailure>;
    /// Release a prepared melt the fence (or the pay-time margin) refused: proofs back to Unspent
    /// in the wallet's database, quote released. Nothing was posted, so nothing is undone at the
    /// mint.
    fn cancel_melt(&mut self, prepared: PreparedMelt) -> Result<(), MeltFailure>;
    /// What the mint says about the melt quote(s) this wallet raised for the invoice, if any —
    /// used to reconcile a PLANNED row (no quote bound yet) and a spending row admitted before
    /// quotes were bound.
    fn melt_status(&mut self, bolt11: &str) -> Result<Option<MeltQuoteStatus>, String>;
    /// What the mint says about ONE quote, by id — the quote a SPENDING row's admission bound
    /// (addendum 5 §1, rule 2). `None` when this wallet never raised it.
    fn melt_status_for_quote(&mut self, quote_id: &str) -> Result<Option<MeltQuoteStatus>, String>;
    /// Observation point: called once the plan is journaled, before the payment quote is raised.
    /// The live effects do nothing here; tests pause here to interleave a second process against
    /// the planned row (addendum 3 §2.2).
    fn after_plan(&mut self, _planned: &FeeRemittance) {}
    /// Observation point: called once the payment quote is raised and has passed the ceiling, before
    /// the fence (addendum 5 §2, `AfterQuote`). The row is still `planned`; tests pause here.
    fn after_quote(&mut self, _planned: &FeeRemittance, _quote: &MeltEstimate) {}
    /// Observation point: called once the compare-and-set has admitted the melt (the row is
    /// `spending`, bound to its quote) and before the payment. The live effects do nothing here;
    /// tests pause here to interleave a second process against a SPENDING row (addendum 4 §1).
    fn after_admit(&mut self, _admitted: &FeeRemittance) {}
    /// Observation point: called with reconciliation's decision about the in-flight row, AFTER the
    /// decision is taken and BEFORE the release / settle is written (addendum 5 §2,
    /// `AfterDecision`). Tests pause here so another process can move the row under a decided
    /// release, which must then change zero rows.
    fn after_decision(&mut self, _row: &FeeRemittance, _decision: &Reconcile) {}
    /// **The clock, read now.** The pre-spend fence compares the row's lease against the time at
    /// the instant of admission — never the attempt's entry time, which may be arbitrarily stale by
    /// then (addendum 4 §1.1). The live effects read the host clock; tests inject one so "the clock
    /// advanced while A was paused" is real arithmetic in the store.
    fn now_unix(&self) -> i64 {
        host_now_unix()
    }
}

/// The host's unix clock in whole seconds; `0` if the clock is before the epoch (it is not).
fn host_now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// This process's owner token: pid plus a boot nonce from the OS RNG, fixed for the life of the
/// process. Every [`LiveEffects`] in the process speaks as this one owner.
fn process_owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| {
        let mut nonce = [0u8; 8];
        let nonce = match getrandom::fill(&mut nonce) {
            Ok(()) => u64::from_le_bytes(nonce),
            Err(_) => 0,
        };
        format!("pid{}-{nonce:016x}", std::process::id())
    })
}

/// The shipped effects: LNURL over https, the packaged CDK wallet at `home`, the home's default
/// mint (the first accepted mint — where a seller's receipts land). Every wallet call goes through
/// the `*_blocking` wrappers in [`crate::wallet_ops`], which refuse to run inside a Tokio runtime —
/// so the seller node drives this from a plain thread of its own, never from a task.
pub struct LiveEffects {
    home: MaxplayerHome,
    fetch: HttpsFetch,
}

impl LiveEffects {
    pub fn new(home: MaxplayerHome) -> Result<Self, String> {
        let fetch = HttpsFetch::new().map_err(|error| error.to_string())?;
        Ok(Self { home, fetch })
    }
}

impl RemitEffects for LiveEffects {
    fn owner(&self) -> &str {
        process_owner()
    }

    fn pay_request(&mut self, address: &LightningAddress) -> Result<PayRequest, String> {
        lnurl_pay::fetch_pay_request(&self.fetch, address).map_err(|error| error.to_string())
    }

    fn invoice(&mut self, pay: &PayRequest, amount_sats: u64) -> Result<ResolvedInvoice, String> {
        lnurl_pay::request_invoice(&self.fetch, pay, amount_sats).map_err(|error| error.to_string())
    }

    fn melt_estimate(&mut self, bolt11: &str) -> Result<MeltEstimate, String> {
        wallet_ops::melt_quote_blocking(&self.home, bolt11, None).map_err(|error| error.to_string())
    }

    fn melt_quote(&mut self, bolt11: &str) -> Result<MeltEstimate, String> {
        wallet_ops::melt_quote_blocking(&self.home, bolt11, None).map_err(|error| error.to_string())
    }

    fn prepare_melt(
        &mut self,
        quote_id: &str,
        ceiling: &MeltCeiling,
    ) -> Result<PreparedMelt, MeltFailure> {
        wallet_ops::prepare_melt_payment_blocking(&self.home, quote_id, None, ceiling)
            .map(PreparedMelt::live)
            .map_err(|error| match error {
                // The two typed refusals: the quote's own figures over the ceiling (before a proof
                // was selected) or the actual worst-case debit — invoice + reserve + the input fee
                // recomputed on the swapped split + swap fee — over it (prepared melt cancelled).
                // Both: nothing fee-bearing posted, nothing left the wallet. Every other error from
                // this step also posted nothing fee-bearing (prepare writes the wallet's own
                // database and at most fetches mint metadata) but is opaque as to why.
                refused @ (WalletOpsError::MeltExceedsCeiling { .. }
                | WalletOpsError::MeltTotalExceedsCeiling { .. }) => {
                    MeltFailure::RefusedBeforeSpending(refused.to_string())
                }
                other => MeltFailure::Failed(other.to_string()),
            })
    }

    fn confirm_melt(&mut self, prepared: PreparedMelt) -> Result<MeltOutcome, MeltFailure> {
        match prepared.token {
            PreparedToken::Live(payment) => payment
                .confirm()
                .map_err(|error| MeltFailure::Failed(error.to_string())),
            #[cfg(test)]
            PreparedToken::Fake(_) => Err(MeltFailure::Failed(
                "a fake prepared melt reached the live effects".to_owned(),
            )),
        }
    }

    fn cancel_melt(&mut self, prepared: PreparedMelt) -> Result<(), MeltFailure> {
        match prepared.token {
            PreparedToken::Live(payment) => payment
                .cancel()
                .map_err(|error| MeltFailure::Failed(error.to_string())),
            #[cfg(test)]
            PreparedToken::Fake(_) => Err(MeltFailure::Failed(
                "a fake prepared melt reached the live effects".to_owned(),
            )),
        }
    }

    fn melt_status(&mut self, bolt11: &str) -> Result<Option<MeltQuoteStatus>, String> {
        wallet_ops::melt_status_for_invoice_blocking(&self.home, bolt11, None)
            .map_err(|error| error.to_string())
    }

    fn melt_status_for_quote(&mut self, quote_id: &str) -> Result<Option<MeltQuoteStatus>, String> {
        wallet_ops::melt_status_for_quote_blocking(&self.home, quote_id, None)
            .map_err(|error| error.to_string())
    }
}

/// Who is running the attempt, and therefore whether it pays and how it is journaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemitTrigger {
    /// `maxplayer seller fees remit` without `--confirm`: resolve, quote, print, move nothing.
    DryRun,
    /// `maxplayer seller fees remit --confirm`: the operator's recovery path. Pays.
    Command,
    /// The seller node, after a receipt was journaled `Collected::New`. Pays.
    Collect,
    /// The seller node's retry tick (addendum 2): the loop's own clock, paced by [`RemitBackoff`].
    /// Pays.
    Retry,
}

impl RemitTrigger {
    /// Whether this run may journal a plan and melt.
    pub fn pays(self) -> bool {
        !matches!(self, Self::DryRun)
    }

    /// How the attempt is journaled — a dry run is not an attempt.
    fn journal_as(self) -> Option<RemitAttemptTrigger> {
        match self {
            Self::DryRun => None,
            Self::Command => Some(RemitAttemptTrigger::Command),
            Self::Collect => Some(RemitAttemptTrigger::Collect),
            Self::Retry => Some(RemitAttemptTrigger::Retry),
        }
    }
}

/// Why an attempt declined and moved nothing. [`Self::is_threshold`] separates the expected steady
/// state (nothing owed yet, or not enough to clear the destination's minimum) — which is accrued
/// silently and never journaled as an attempt — from the refusals an operator should see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The unremitted balance is zero.
    NothingUnremitted,
    /// The unremitted balance is below the destination's resolved `minSendable`.
    BelowMinimum { unremitted: u64, min_sats: u64 },
    /// The unremitted balance is above the destination's resolved `maxSendable`; this path remits
    /// the whole balance or nothing.
    AboveMaximum { unremitted: u64, max_sats: u64 },
    /// The mint's melt fee reserve leaves nothing, leaves less than the minimum, or would take more
    /// out of the wallet than was accrued.
    ReserveDoesNotFit { gross: u64, reserve: u64 },
    /// The mint's melt fee reserve on the net invoice PLUS the proof fees the SDK is expected to
    /// charge this wallet (proof-input fee, and a pre-melt swap fee when its proofs do not fit)
    /// would take more out of the wallet than was accrued (addendum 8 §1.3): a plan that can never
    /// fit is refused here, at planning, not at payment.
    FeesDoNotFit {
        gross: u64,
        reserve: u64,
        expected_fees: u64,
    },
    /// An earlier attempt's PLANNED row (or a legacy spending row with no quote bound) has a melt
    /// quote the mint reports PENDING or unknown: a payment may be settling — hold. A bound
    /// spending row in the same state is [`Refusal::SpendingHeld`] instead, so that it renders the
    /// one `HELD:` line every bound non-PAID observation renders (addendum 7 §2).
    Settling { remittance_id: String },
    /// An earlier attempt's planned row belongs to another process whose lease has not run out, and
    /// the mint does not report its quote terminal (addendum 3 §2): UNPAID means "not yet", not
    /// "abandoned", so this run may not release it and may not plan on top of it.
    HeldByOwner {
        remittance_id: String,
        owner: String,
        lease_until_unix: i64,
    },
    /// An earlier attempt's row is SPENDING — its owner's compare-and-set admitted the melt and
    /// bound a quote — and the mint does not report that quote PAID (addendum 6 §1.2). A payment
    /// prepared under the bound quote may still reach the mint after ANY local observation — the
    /// mint pays an UNPAID or FAILED quote however long ago it expired (CDK 0.17.2, verdict at
    /// 6fc77e1 §4) — so no run releases this row on FAILED, on expiry, on the clock, or on the
    /// wallet not knowing the quote: it is held, and its receipts with it, until the mint reports
    /// the quote PAID. Whoever asks, however late. A stuck row is an operator's decision (no
    /// override exists in this round), never a timeout's. Also the hold for a legacy spending row
    /// with no quote bound (`quote_id: None`) while its invoice's quotes are UNPAID / absent.
    SpendingHeld {
        remittance_id: String,
        owner: String,
        spending_since_unix: i64,
        /// The quote the fence bound (`None` only for a row a v12 binary admitted).
        quote_id: Option<String>,
        /// What the mint (or the wallet) said about that quote when this run asked.
        observed: String,
        /// The receipts pinned to the row: the row's gross, held from remittance while it stands.
        held_sats: u64,
    },
    /// The pre-spend gate refused: between journaling the plan and paying it, this process lost its
    /// claim on the row (another process reconciled it), or too little lease remained to start a
    /// payment safely. Nothing was spent.
    OwnershipLost {
        remittance_id: String,
        reason: String,
    },
    /// Reconciliation decided to release the in-flight row, and the conditional release then
    /// changed ZERO rows: the row moved under this run between the decision and the write (its
    /// owner was admitted, or another process resolved it). Nothing written; re-run to reconcile
    /// against the row as it now stands (addendum 5 §1, rule 2).
    RowChangedUnderMe { remittance_id: String },
    /// The store refused to journal the plan (a row already in flight, the balance moved under us,
    /// an invoice already used).
    PlanRefused(String),
}

impl Refusal {
    /// The two refusals that are the expected steady state for a small seller: not an error, not an
    /// attempt, nothing to journal — the balance accumulates until it clears the minimum.
    pub fn is_threshold(&self) -> bool {
        matches!(self, Self::NothingUnremitted | Self::BelowMinimum { .. })
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NothingUnremitted => write!(formatter, "nothing unremitted"),
            Self::BelowMinimum {
                unremitted,
                min_sats,
            } => write!(
                formatter,
                "unremitted {unremitted} sats is below the destination's minimum of {min_sats} sats"
            ),
            Self::AboveMaximum {
                unremitted,
                max_sats,
            } => write!(
                formatter,
                "unremitted {unremitted} sats exceeds the destination's maximum of {max_sats} sats"
            ),
            Self::ReserveDoesNotFit { gross, reserve } => write!(
                formatter,
                "the mint's melt fee reserve ({reserve} sats) does not fit inside the {gross} sats accrued"
            ),
            Self::FeesDoNotFit {
                gross,
                reserve,
                expected_fees,
            } => write!(
                formatter,
                "the mint's melt fee reserve ({reserve} sats) plus the expected proof fees ({expected_fees} sats) do not fit inside the {gross} sats accrued"
            ),
            Self::Settling { remittance_id } => write!(
                formatter,
                "remittance {remittance_id} is still settling at the mint"
            ),
            Self::HeldByOwner {
                remittance_id,
                owner,
                lease_until_unix,
            } => write!(
                formatter,
                "remittance {remittance_id} is planned by another live process ({owner}, lease until unix {lease_until_unix}) and its quote is not terminal; not releasing a live payer's intent"
            ),
            Self::SpendingHeld {
                remittance_id,
                owner,
                spending_since_unix,
                quote_id,
                observed,
                held_sats,
            } => write!(
                formatter,
                "HELD: remittance {remittance_id} is SPENDING (admitted by {owner} at unix {spending_since_unix}), {}; {observed}; {held_sats} sats of receipts stay pinned to it — a spending row is released by nobody and on no clock; it settles only when the mint reports that quote PAID; an operator decision, not a timeout, resolves it",
                match quote_id {
                    Some(quote_id) => format!("bound to melt quote {quote_id}"),
                    None => "with no quote bound (admitted before v13)".to_owned(),
                }
            ),
            Self::OwnershipLost {
                remittance_id,
                reason,
            } => write!(
                formatter,
                "refused before spending: remittance {remittance_id} — {reason}"
            ),
            Self::RowChangedUnderMe { remittance_id } => write!(
                formatter,
                "remittance {remittance_id} changed under this run between the release decision and the release itself; nothing written"
            ),
            Self::PlanRefused(reason) => write!(formatter, "plan refused: {reason}"),
        }
    }
}

/// How one run of [`remit`] ended. `Err` from [`remit`] is the other ending: an effect failed before
/// or during reconciliation and nothing was paid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemitOutcome {
    /// The plan was printed and nothing moved.
    DryRun,
    /// The melt settled and the remittance is journaled settled.
    Paid {
        remittance_id: String,
        net_sats: u64,
        melt_fee_sats: u64,
    },
    /// Declined; nothing moved.
    Refused(Refusal),
    /// The plan was journaled, the fence admitted the melt and the payment of the bound quote then
    /// failed (or was refused locally, the quote being inside its margin of expiry): the row stays
    /// `spending`, bound to its quote, and the next attempt asks the mint about that quote —
    /// settled if it reports PAID, otherwise HELD, with its receipts, until it does (addendum 6
    /// §1.2). No later observation releases it: a payment prepared under the quote may still
    /// reach the mint, and the mint pays an UNPAID or FAILED quote regardless of its expiry.
    MeltFailed {
        remittance_id: String,
        error: String,
    },
    /// The plan was journaled and the payment was REFUSED before the fence, nothing spent — the
    /// payment quote would have taken more than the accrued gross (addendum 3 §1), or expires
    /// inside the spending margin. The row was still planned and this process's own, so it
    /// released it: the balance is unremitted again, the attempt is journaled failed, and the
    /// backoff escalates. The next attempt plans a fresh row and raises fresh quotes.
    MeltRefused {
        remittance_id: String,
        reason: String,
    },
    /// The plan was journaled and the payment quote could not be raised (mint unreachable, or it
    /// quoted a different amount). Nothing spent; the planned row, this process's own, was
    /// released; journaled failed; backoff escalates.
    QuoteFailed {
        remittance_id: String,
        error: String,
    },
}

/// What reconciliation decides about the one in-flight row, from the mint's answer about its
/// quote and the row's state and ownership (addendum 3 §2.1, addendum 5 §1 rule 2). Pure, so the
/// rule is tested as a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconcile {
    /// The mint reports the quote PAID: settle, keep the receipts discharged, never melt again.
    Settle,
    /// Release the receipts back to unremitted — by the conditional transition `on`, which changes
    /// zero rows if the row is no longer as this decision found it. `reason` is printed and
    /// journaled.
    Release { reason: String, on: ReleaseOn },
    /// Leave the row exactly as it is and refuse this run; the reason is printed and journaled.
    Hold(Refusal),
}

/// The release rule. `status` is the mint's answer about the row's **bound quote, by id**, when the
/// row is `spending` with a quote bound (addendum 5 §1, rule 2) — and about the invoice's quote(s)
/// otherwise (a `planned` row has no quote bound yet; a spending row admitted before v13 has none).
///
/// The in-flight row is **settled** when the mint reports the quote **PAID**, whatever its state
/// or owner.
///
/// A **`spending` row bound to a quote** — every row this binary admits — has **no other
/// transition here** (addendum 6 §1.2): on UNPAID at any age, FAILED, PENDING, UNKNOWN, or the
/// wallet not knowing the quote, it is **held**, and its receipts with it, until the mint reports
/// the quote PAID — every one of those five observations as [`Refusal::SpendingHeld`], whose
/// one-line read-out names the row, the bound quote, what the mint said and the held receipts
/// (addendum 6 §1.3; addendum 7 §2); nothing written. Nothing about it is inferred from a clock:
/// `now_unix` is not consulted for a bound spending row. The reason is the mint itself: a payment its owner prepared under the
/// bound quote may still be on its way, and the mint (CDK 0.17.2 as the verdict at 6fc77e1 §4
/// read it) pays an UNPAID **or FAILED** quote with no expiry check — so "FAILED" and "expired"
/// are not cancellation, and a release on either could make the same gross payable twice. The
/// hold is therefore permanent until PAID or an operator's decision (none automated here); the
/// exclusion that keeps a second attempt from planning on top of the held row is
/// [`SellerStore::plan_remittance`]'s in-flight refusal, which has no time term either.
///
/// A **`planned` row** (never admitted: nothing spent against it, and once released its owner's
/// fence changes zero rows) is **released** — always by the conditional transition that carries
/// the reason ([`ReleaseOn`]), never unconditionally — when the invoice's quote is **FAILED** or
/// **UNPAID and expired** ([`ReleaseOn::TerminalQuotePlanned`]); or when the mint reports
/// **UNPAID** / the wallet never raised a quote AND either the row is **this process's own** (a
/// process runs one attempt at a time, so its earlier attempt is over — [`ReleaseOn::OwnPlanned`])
/// or the owner's **lease has run out** (the owner is provably not spending: its fence refuses
/// inside [`SPEND_MARGIN`] of the lease's end and, once past it, changes zero rows —
/// [`ReleaseOn::LeaseExpired`]). It is **held** when its quote is PENDING or UNKNOWN, and when it
/// is UNPAID / absent but another process's lease still stands (UNPAID means "not yet").
///
/// A **legacy `spending` row with no quote bound** (a v12 admission; out of this round's scope,
/// addendum 6 §1.5) keeps the v12 rule as delivered: released through
/// [`ReleaseOn::TerminalUnboundSpending`] on FAILED or UNPAID-and-expired, held otherwise.
pub fn reconcile_decision(
    row: &FeeRemittance,
    status: Option<&MeltQuoteStatus>,
    my_owner: &str,
    now_unix: i64,
) -> Reconcile {
    let spending = row.state == RemittanceState::Spending;
    let bound = if spending {
        row.spending_quote_id.as_deref()
    } else {
        None
    };
    // PAID settles the row whatever its state or owner.
    if status.is_some_and(|status| status.state == MeltQuoteState::Paid) {
        return Reconcile::Settle;
    }
    let observed = match status {
        None => "this wallet holds no such melt quote".to_owned(),
        Some(status) => format!(
            "mint {} reports melt quote {} {} (expiry unix {})",
            status.mint_url, status.quote_id, status.state, status.expiry_unix
        ),
    };
    let hold_spending = |quote_id: Option<&str>| {
        Reconcile::Hold(Refusal::SpendingHeld {
            remittance_id: row.remittance_id.clone(),
            owner: row.owner.clone().unwrap_or_default(),
            spending_since_unix: row.spending_since_unix.unwrap_or(row.created_at_unix),
            quote_id: quote_id.map(str::to_owned),
            observed: observed.clone(),
            held_sats: row.gross_sats,
        })
    };
    // A bound spending row: held on everything but PAID — UNPAID, FAILED, PENDING, UNKNOWN, or the
    // wallet not knowing the quote — as ONE refusal, so every such observation renders the same
    // single `HELD:` line naming the row, the quote, what the mint said and the held receipts
    // (addendum 6 §1.3; addendum 7 §2). No clock, no terminality inference.
    if let (true, Some(quote_id)) = (spending, bound) {
        return hold_spending(Some(quote_id));
    }
    // From here: a PLANNED row, or a legacy spending row with no quote bound.
    // PENDING / UNKNOWN: a payment may be settling — hold, whatever the row (its own refusal, so
    // the read-out says "settling", not "held": nothing is pinned to a spending mark here).
    if status.is_some_and(|status| {
        matches!(
            status.state,
            MeltQuoteState::Pending | MeltQuoteState::Unknown
        )
    }) {
        return Reconcile::Hold(Refusal::Settling {
            remittance_id: row.remittance_id.clone(),
        });
    }
    let unpaid_reason = match status {
        None => "the wallet never raised a melt quote for its invoice — no sats left the wallet"
            .to_owned(),
        Some(status) => format!(
            "mint {} reports melt quote {} {} — no sats left the wallet",
            status.mint_url, status.quote_id, status.state
        ),
    };
    let terminal_release = |reason: String| {
        let on = if spending {
            ReleaseOn::TerminalUnboundSpending
        } else {
            ReleaseOn::TerminalQuotePlanned
        };
        Reconcile::Release { reason, on }
    };
    match status.map(|status| status.state) {
        Some(MeltQuoteState::Paid) => Reconcile::Settle,
        Some(MeltQuoteState::Pending) | Some(MeltQuoteState::Unknown) => {
            Reconcile::Hold(Refusal::Settling {
                remittance_id: row.remittance_id.clone(),
            })
        }
        Some(MeltQuoteState::Failed) => terminal_release(format!(
            "{unpaid_reason}; FAILED at the mint, and the row was never admitted to spend under this binary's fence — nothing was spent against it"
        )),
        Some(MeltQuoteState::Unpaid)
            if status.is_some_and(|status| status.expired_at(now_unix)) =>
        {
            terminal_release(format!(
                "{unpaid_reason}; the quote expired at unix {}, and the row was never admitted to spend under this binary's fence — nothing was spent against it",
                status.map(|status| status.expiry_unix).unwrap_or_default()
            ))
        }
        Some(MeltQuoteState::Unpaid) | None => {
            if spending {
                hold_spending(None)
            } else if row.owner.as_deref() == Some(my_owner) {
                Reconcile::Release {
                    reason: format!(
                        "{unpaid_reason}; the row is this process's own earlier attempt, which is over"
                    ),
                    on: ReleaseOn::OwnPlanned {
                        owner: my_owner.to_owned(),
                    },
                }
            } else if row.lease_expired(now_unix) {
                Reconcile::Release {
                    reason: format!(
                        "{unpaid_reason}; its owner's lease ran out at unix {} (owner {})",
                        row.lease_until_unix.unwrap_or(row.created_at_unix),
                        row.owner.as_deref().unwrap_or("none recorded")
                    ),
                    on: ReleaseOn::LeaseExpired { now_unix },
                }
            } else {
                Reconcile::Hold(Refusal::HeldByOwner {
                    remittance_id: row.remittance_id.clone(),
                    owner: row.owner.clone().unwrap_or_default(),
                    lease_until_unix: row.lease_until_unix.unwrap_or(row.created_at_unix),
                })
            }
        }
    }
}

/// What the run learned before it ended, for the attempt journal.
#[derive(Default)]
struct AttemptTrace {
    unremitted: Option<u64>,
    remittance_id: Option<String>,
}

/// **The remit entry point.** Runs one attempt against `store` through `effects`, printing every
/// step to `out` in the seller's words, and — when the trigger pays — journals the attempt and its
/// outcome (`fee_remit_attempts`) so a failing payout is visible. Refusals at the threshold
/// ([`Refusal::is_threshold`]) are the expected steady state and are not journaled.
///
/// `Err` is an effect failure (LNURL host, mint quote, reconciliation query) before any plan was
/// journaled, or the store failing to write; nothing was paid. A melt that fails AFTER the plan is
/// [`RemitOutcome::MeltFailed`], not `Err`, because there is a row to reconcile.
///
/// Order of operations, and why:
/// 1. Reconcile any in-flight row first — a payment may be in flight from an interrupted run, and
///    nothing may be planned on top of it. PAID ⇒ settle. A `spending` row bound to a quote ⇒
///    otherwise HOLD, whatever the mint says and however old the quote (addendum 6 §1.2). A
///    `planned` row: FAILED / expired ⇒ release; UNPAID / no quote ⇒ release only if the row is
///    this process's own or its owner's lease has run out, else refuse; PENDING ⇒ refuse this run
///    ([`reconcile_decision`]).
/// 2. Read the unremitted total. Zero ⇒ refuse (nothing to do), before any network.
/// 3. Resolve the destination; refuse below its minimum with the shortfall (expected for small
///    sellers, not an error).
/// 4. Probe the melt fee reserve on an invoice for the GROSS, then invoice for `gross − reserve` so
///    the fee comes out of the accrued amount — a seller never pays more than it accrued — and check
///    the second quote still fits.
/// 5. Print the plan. A dry run stops here.
/// 6. Journal the plan (pins the receipts, records this process as owner under [`REMIT_LEASE`];
///    refuses a duplicate). Then, in this order (addendum 5 §1 rule 1): raise the PAYMENT quote
///    and check the ceiling against its amount and reserve — refused ⇒ release the row (still
///    planned, ours, nothing spent) and journal failed; pass the fence — one compare-and-set that
///    marks the row spending and BINDS that quote, with the clock read inside the store call and
///    more than [`SPEND_MARGIN`] of lease left; pay that quote BY ID, re-checking the ceiling
///    immediately before `prepare_melt`; settle. A payment ERROR after the fence leaves the row
///    `spending`, bound to its quote, for step 1 of the next run — which asks the mint about THAT
///    quote, never raises another for the row, settles on PAID and otherwise holds.
pub fn remit(
    store: &SellerStore,
    effects: &mut dyn RemitEffects,
    trigger: RemitTrigger,
    now_unix: i64,
    out: &mut dyn Write,
) -> Result<RemitOutcome, String> {
    let mut trace = AttemptTrace::default();
    let result = remit_inner(store, effects, trigger, now_unix, out, &mut trace);
    let attempt = trigger.journal_as().and_then(|journal_trigger| {
        attempt_record(store, journal_trigger, now_unix, &trace, &result)
    });
    if let Some(attempt) = attempt
        && let Err(error) = store.record_remit_attempt(&attempt)
    {
        let _ = writeln!(
            out,
            "WARNING: could not journal this attempt ({error}); the outcome above stands."
        );
    }
    result
}

/// The attempt row for a finished run, or `None` when the run is not an attempt (a dry run, or a
/// refusal at the threshold).
fn attempt_record(
    store: &SellerStore,
    trigger: RemitAttemptTrigger,
    now_unix: i64,
    trace: &AttemptTrace,
    result: &Result<RemitOutcome, String>,
) -> Option<RemitAttempt> {
    let (outcome, detail, remittance_id) = match result {
        Ok(RemitOutcome::DryRun) => return None,
        Ok(RemitOutcome::Refused(refusal)) if refusal.is_threshold() => return None,
        Ok(RemitOutcome::Paid {
            remittance_id,
            net_sats,
            melt_fee_sats,
        }) => (
            RemitAttemptOutcome::Paid,
            format!(
                "paid {net_sats} sats to {PLATFORM_FEE_ADDRESS} (melt fee {melt_fee_sats} sats)"
            ),
            Some(remittance_id.clone()),
        ),
        Ok(RemitOutcome::Refused(refusal)) => (
            RemitAttemptOutcome::Refused,
            refusal.to_string(),
            trace.remittance_id.clone(),
        ),
        Ok(RemitOutcome::MeltFailed {
            remittance_id,
            error,
        }) => (
            RemitAttemptOutcome::Failed,
            format!("melt failed: {error}"),
            Some(remittance_id.clone()),
        ),
        Ok(RemitOutcome::MeltRefused {
            remittance_id,
            reason,
        }) => (
            RemitAttemptOutcome::Failed,
            format!("refused before spending: {reason}"),
            Some(remittance_id.clone()),
        ),
        Ok(RemitOutcome::QuoteFailed {
            remittance_id,
            error,
        }) => (
            RemitAttemptOutcome::Failed,
            format!("payment quote failed: {error}"),
            Some(remittance_id.clone()),
        ),
        Err(error) => (
            RemitAttemptOutcome::Failed,
            error.clone(),
            trace.remittance_id.clone(),
        ),
    };
    // A run that failed before it read the ledger still journals the balance it was attempting.
    let unremitted = trace.unremitted.or_else(|| {
        store
            .accrued_fees()
            .ok()
            .map(|accrued| accrued.unremitted_fee_sats)
    });
    Some(RemitAttempt {
        attempt_id: 0,
        started_at_unix: now_unix,
        trigger,
        unremitted_sats: unremitted.unwrap_or(0),
        outcome,
        detail,
        remittance_id,
    })
}

fn remit_inner(
    store: &SellerStore,
    effects: &mut dyn RemitEffects,
    trigger: RemitTrigger,
    now_unix: i64,
    out: &mut dyn Write,
    trace: &mut AttemptTrace,
) -> Result<RemitOutcome, String> {
    // 0. For the operator: what the recent attempts did. The collect path skips this — its output is
    //    the node's log, and the journal is what the command prints.
    if trigger != RemitTrigger::Collect {
        print_recent_attempts(store, out)?;
    }

    // 1. Reconcile an in-flight attempt before anything else — under the release rule
    //    (`reconcile_decision`): never a live payer's intent.
    if let Some(active) = store
        .in_flight_remittance()
        .map_err(|error| format!("read remittances: {error}"))?
    {
        trace.remittance_id = Some(active.remittance_id.clone());
        let _ = writeln!(
            out,
            "Reconciling in-flight remittance {} (planned at unix {} by {}, lease until unix {}: {} sats to {}, gross {} sats){}",
            active.remittance_id,
            active.created_at_unix,
            active.owner.as_deref().unwrap_or("nobody recorded"),
            active
                .lease_until_unix
                .map(|until| until.to_string())
                .unwrap_or_else(|| "none recorded".to_owned()),
            active.net_sats,
            active.destination,
            active.gross_sats,
            match (active.state, active.spending_quote_id.as_deref()) {
                (RemittanceState::Spending, Some(quote_id)) => format!(
                    " — SPENDING since unix {}, bound to melt quote {quote_id}: asking the mint about that quote by id",
                    active.spending_since_unix.unwrap_or(active.created_at_unix)
                ),
                (RemittanceState::Spending, None) => format!(
                    " — SPENDING since unix {} with no quote bound (admitted before quotes were bound): asking the mint about its invoice's quotes",
                    active.spending_since_unix.unwrap_or(active.created_at_unix)
                ),
                _ => String::new(),
            }
        );
        // A spending row bound to a quote is reconciled against THAT quote, by id — never against
        // "the most alive quote for the invoice", which cannot see a quote nobody has raised and
        // must not speak for the one the owner is paying (addendum 5 §1, rule 2).
        let status = match (active.state, active.spending_quote_id.as_deref()) {
            (RemittanceState::Spending, Some(quote_id)) => {
                effects.melt_status_for_quote(quote_id)?
            }
            _ => effects.melt_status(&active.bolt11)?,
        };
        let decision = reconcile_decision(&active, status.as_ref(), effects.owner(), now_unix);
        effects.after_decision(&active, &decision);
        match decision {
            Reconcile::Settle => {
                let status = status.expect("Settle is decided only on a PAID status");
                let settlement = RemitSettlement {
                    net_paid_sats: Some(status.amount_sats),
                    melt_fee_sats: None,
                    melt_fee_reserve_sats: Some(status.fee_reserve_sats),
                    melt_quote_id: Some(status.quote_id.clone()),
                    settled_by: SettledBy::Reconciliation,
                };
                store
                    .settle_remittance(&active.remittance_id, &settlement, now_unix)
                    .map_err(|error| format!("record settled remittance: {error}"))?;
                let _ = writeln!(
                    out,
                    "  mint {} reports melt quote {} PAID — recorded as settled by reconciliation: {} sats reached {}; Lightning fee at most {} sats (the quote's reserve); the inclusive melt fee (Lightning + actual proof input fee) is recorded as not observed — the mint reports PAID, not what it kept",
                    status.mint_url,
                    status.quote_id,
                    status.amount_sats,
                    active.destination,
                    status.fee_reserve_sats
                );
            }
            Reconcile::Release { reason, on } => {
                // The release is CONDITIONAL on the row still being as the decision found it. Zero
                // rows changed ⇒ the row moved under us (its owner was admitted, or another process
                // resolved it): HOLD — nothing written, this run refused, re-run to reconcile.
                match store
                    .release_remittance(&active.remittance_id, &on, now_unix)
                    .map_err(|error| format!("record failed remittance: {error}"))?
                {
                    Some(_) => {
                        let _ = writeln!(
                            out,
                            "  {reason}; released {} sats back to unremitted (release condition: {})",
                            active.gross_sats,
                            on.describe()
                        );
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "  {reason} — but the row changed under me between that decision and the release (condition: {}): nothing written. REFUSED — nothing moved by this run; re-run to reconcile the row as it now stands.",
                            on.describe()
                        );
                        return Ok(RemitOutcome::Refused(Refusal::RowChangedUnderMe {
                            remittance_id: active.remittance_id,
                        }));
                    }
                }
            }
            Reconcile::Hold(refusal) => {
                let _ = writeln!(
                    out,
                    "  {}. REFUSED — nothing moved by this run; re-run later to reconcile.",
                    match &refusal {
                        Refusal::Settling { .. } => format!(
                            "mint {} reports melt quote {} {}: the payment is still settling",
                            status.as_ref().map(|s| s.mint_url.as_str()).unwrap_or("?"),
                            status.as_ref().map(|s| s.quote_id.as_str()).unwrap_or("?"),
                            status
                                .as_ref()
                                .map(|s| s.state.to_string())
                                .unwrap_or_default()
                        ),
                        other => other.to_string(),
                    }
                );
                return Ok(RemitOutcome::Refused(refusal));
            }
        }
        trace.remittance_id = None;
    }

    // 2. What is owed.
    let accrued = store
        .accrued_fees()
        .map_err(|error| format!("read receipts: {error}"))?;
    let gross = accrued.unremitted_fee_sats;
    trace.unremitted = Some(gross);
    let _ = writeln!(
        out,
        "Accrued platform fee: {} sats all-time — {} sats remitted, {} sats unremitted",
        accrued.total_fee_sats, accrued.remitted_fee_sats, gross
    );
    if gross == 0 {
        let _ = writeln!(out, "Nothing to remit. REFUSED — nothing moved.");
        return Ok(RemitOutcome::Refused(Refusal::NothingUnremitted));
    }

    // 3. The destination and its bounds.
    let address =
        LightningAddress::parse(PLATFORM_FEE_ADDRESS).map_err(|error| error.to_string())?;
    let pay = effects.pay_request(&address)?;
    let min_sats = pay.min_sendable_sats();
    let max_sats = pay.max_sendable_sats();
    let _ = writeln!(
        out,
        "Destination: {address} (LNURL-pay; accepts {min_sats} to {max_sats} sats)"
    );
    if gross < min_sats {
        let _ = writeln!(
            out,
            "REFUSED — unremitted {gross} sats is below the destination's minimum of {min_sats} sats ({} sats short). The balance accumulates until it clears the minimum. Nothing moved.",
            min_sats - gross
        );
        return Ok(RemitOutcome::Refused(Refusal::BelowMinimum {
            unremitted: gross,
            min_sats,
        }));
    }
    if gross > max_sats {
        let _ = writeln!(
            out,
            "REFUSED — unremitted {gross} sats exceeds the destination's maximum of {max_sats} sats; this path remits the whole balance or nothing. Nothing moved."
        );
        return Ok(RemitOutcome::Refused(Refusal::AboveMaximum {
            unremitted: gross,
            max_sats,
        }));
    }

    // 4. The melt fee comes OUT of the gross. Probe the reserve on the gross, then invoice the net.
    let probe = effects.invoice(&pay, gross)?;
    let probe_estimate = effects.melt_estimate(&probe.bolt11)?;
    if probe_estimate.amount_sats != gross {
        return Err(format!(
            "mint {} quoted {} sats for a {gross}-sat invoice; refusing",
            probe_estimate.mint_url, probe_estimate.amount_sats
        ));
    }
    let reserve = probe_estimate.fee_reserve_sats;
    // Addendum 11 §1 (B/N1): the ONLY refusal taken on the gross probe is the reserve alone — the
    // helper below searches invoices in 1..=gross − reserve, so it needs gross − reserve > 0. The
    // proof fees the SDK is expected to charge THIS wallet (input fee on the proofs it would send,
    // plus a pre-melt swap fee when its proofs do not fit) are NOT a gate here: the probe's
    // `expected_fees_sats` is an estimate on the GROSS-sized layout, and round 9's veto on
    // `reserve + expected ≥ gross` refused remittances a smaller invoice pays (verdict 1ee5cb2 §4.2:
    // 3/0/1000/[32] ⇒ probe expects 2 ≥ 3 − 0 … refused; invoice 1 pays with a 3-sat debit). Fees
    // are checked by the exact search over candidate invoices (`plan_confirmable_invoice`), then
    // re-checked on the quote actually raised; `FeesDoNotFit` is returned only when NO candidate
    // fits. So a payment that can fit within the gross IS planned (the largest such invoice), and
    // one that never can is refused here at planning, not at payment. The probe's figure is kept
    // for the printed lines only.
    let expected = probe_estimate.expected_fees_sats;
    let expected_clause = |expected: u64| {
        if expected > 0 {
            format!(" plus {expected} sats of expected proof fees")
        } else {
            String::new()
        }
    };
    if reserve >= gross {
        let _ = writeln!(
            out,
            "REFUSED — mint {} needs a melt fee reserve of {reserve} sats to pay {gross} sats, which leaves nothing for the destination. The balance accumulates. Nothing moved.",
            probe_estimate.mint_url
        );
        return Ok(RemitOutcome::Refused(Refusal::ReserveDoesNotFit {
            gross,
            reserve,
        }));
    }
    // Addendum 9 §1.2: the invoice is the largest one `confirm` can actually pay within the gross —
    // the SDK's swap target must cover invoice + reserve + the input fee it RECOMPUTES on the
    // swapped proofs, and that plus the swap fee must fit the gross. Searched locally on the
    // probe's reserve, swap fee and keyset ppk; re-checked below on the quote actually raised.
    let net = match plan_confirmable_invoice(
        gross,
        reserve,
        probe_estimate.expected_swap_fee_sats,
        probe_estimate.input_fee_ppk,
    ) {
        Some(net) => net,
        None => {
            let _ = writeln!(
                out,
                "REFUSED — no invoice fits: at mint {}'s {} ppk proof fee, no amount up to {} sats ({gross} sats less the {reserve} sats melt fee reserve) can be paid for at most {gross} sats once the SDK recomputes its proof input fee on the swapped proofs{}. The balance accumulates. Nothing moved.",
                probe_estimate.mint_url,
                probe_estimate.input_fee_ppk,
                gross - reserve,
                expected_clause(expected)
            );
            return Ok(RemitOutcome::Refused(Refusal::FeesDoNotFit {
                gross,
                reserve,
                expected_fees: expected,
            }));
        }
    };
    if net < min_sats {
        let _ = writeln!(
            out,
            "REFUSED — after the mint's melt fee reserve ({reserve} sats){} the {gross} sats unremitted leaves {net} sats, below the destination's minimum of {min_sats} sats ({} sats short). The balance accumulates. Nothing moved.",
            expected_clause(expected),
            min_sats - net
        );
        return Ok(RemitOutcome::Refused(if expected > 0 {
            Refusal::FeesDoNotFit {
                gross,
                reserve,
                expected_fees: expected,
            }
        } else {
            Refusal::ReserveDoesNotFit { gross, reserve }
        }));
    }
    let (invoice, estimate) = if reserve == 0 && expected == 0 {
        (probe, probe_estimate)
    } else {
        let invoice = effects.invoice(&pay, net)?;
        let estimate = effects.melt_estimate(&invoice.bolt11)?;
        if estimate.amount_sats != net {
            return Err(format!(
                "mint {} quoted {} sats for a {net}-sat invoice; refusing",
                estimate.mint_url, estimate.amount_sats
            ));
        }
        (invoice, estimate)
    };
    let reserve_ceiling = net.saturating_add(estimate.fee_reserve_sats);
    if reserve_ceiling > gross {
        let _ = writeln!(
            out,
            "REFUSED — mint {} quotes a {} sats fee reserve on {net} sats, so up to {reserve_ceiling} sats would leave the wallet against {gross} sats accrued. A seller never pays more than it accrued. Nothing moved.",
            estimate.mint_url, estimate.fee_reserve_sats
        );
        return Ok(RemitOutcome::Refused(Refusal::ReserveDoesNotFit {
            gross,
            reserve: estimate.fee_reserve_sats,
        }));
    }
    // Addendum 10 §1.1 re-check on the quote actually raised (its reserve, and this wallet's SDK
    // figures for THIS amount) — the SAME bound the planner searched with and the wallet's prepared-
    // melt gate takes: `confirm` must succeed (target ≥ need + the input fee it RECOMPUTES) and the
    // worst-case debit — invoice + reserve + ACTUAL input fee + swap fee — must fit the gross. The
    // PREPARED total (invoice + reserve + estimated input fee + swap fee) is NOT a gate: it is an
    // estimate that can exceed the final debit, and round 8's early return on it refused fitting
    // remittances (verdict 4714623 §3.2: 19/2/1000/[32] ⇒ invoice 13, prepared total 20 > 19, actual
    // debit 19 ≤ 19).
    let planned_estimated_input = estimate
        .expected_fees_sats
        .saturating_sub(estimate.expected_swap_fee_sats);
    let planned = match confirm_bound(
        net,
        estimate.fee_reserve_sats,
        Some(planned_estimated_input),
        estimate.expected_swap_fee_sats,
        estimate.input_fee_ppk,
        estimate.input_fee_ppk > 0,
        gross,
    ) {
        Ok(bound) => bound,
        Err(ConfirmShortfall::TargetShort {
            bound,
            needed_after_swap_sats,
        }) => {
            let _ = writeln!(
                out,
                "REFUSED — mint {} quotes a {} sats fee reserve on {net} sats; the wallet would swap to {} sats and the SDK's actual proof input fee on those proofs is {} sats (estimate {planned_estimated_input} sats), so the payment would need {needed_after_swap_sats} sats and the SDK would refuse after its swap. A seller never pays more than it accrued. Nothing moved.",
                estimate.mint_url,
                estimate.fee_reserve_sats,
                bound.target_sats,
                bound.actual_input_fee_sats
            );
            return Ok(RemitOutcome::Refused(Refusal::FeesDoNotFit {
                gross,
                reserve: estimate.fee_reserve_sats,
                expected_fees: estimate.expected_fees_sats,
            }));
        }
        Err(ConfirmShortfall::OverCeiling { bound, .. }) => {
            let _ = writeln!(
                out,
                "REFUSED — mint {} quotes a {} sats fee reserve on {net} sats; with the SDK's actual proof input fee of {} sats on the swapped proofs (estimate {planned_estimated_input} sats) and a {} sats swap fee the payment would cost up to {} sats against {gross} sats accrued. A seller never pays more than it accrued. Nothing moved.",
                estimate.mint_url,
                estimate.fee_reserve_sats,
                bound.actual_input_fee_sats,
                bound.swap_fee_sats,
                bound.worst_debit_sats
            );
            return Ok(RemitOutcome::Refused(Refusal::FeesDoNotFit {
                gross,
                reserve: estimate.fee_reserve_sats,
                expected_fees: estimate.expected_fees_sats,
            }));
        }
        Err(ConfirmShortfall::DifferentInvoice { .. }) => {
            unreachable!("confirm_bound without a ceiling never compares invoices")
        }
    };
    let planned_actual_input = planned.actual_input_fee_sats;
    let worst_debit = planned.worst_debit_sats;

    // 5. The plan, in the seller's words.
    let _ = writeln!(
        out,
        "Plan:\n  unremitted platform fee (gross): {gross} sats\n  mint melt fee reserve (bounds the Lightning fee): {} sats — taken out of the gross, never on top\n  invoice amount ({address} receives): {net} sats\n  leaves your wallet: at most {worst_debit} sats (≤ {gross}); unused reserve returns as change",
        estimate.fee_reserve_sats
    );
    if estimate.expected_fees_sats > 0 {
        let _ = writeln!(
            out,
            "  expected proof fees (SDK estimate, bounded exactly at payment): {} sats = estimated proof input fee {planned_estimated_input} sats + swap fee {} sats; actual proof input fee the SDK recomputes on the swapped proofs: {planned_actual_input} sats ⇒ worst case {worst_debit} sats leaves the wallet (≤ {gross}) = invoice {net} + reserve {} (bounds the Lightning fee) + actual proof input fee {planned_actual_input} + swap fee {}; the inclusive melt fee (Lightning + actual proof input fee) is known only at payment",
            estimate.expected_fees_sats,
            estimate.expected_swap_fee_sats,
            estimate.fee_reserve_sats,
            estimate.expected_swap_fee_sats
        );
    }
    if let Some(note) = &estimate.expected_fees_note {
        let _ = writeln!(
            out,
            "  proof fees not estimable now ({note}); the ceiling still bounds them exactly at payment"
        );
    }
    let _ = writeln!(
        out,
        "  mint: {} (melt quote {})\n  invoice payment hash: {}",
        estimate.mint_url, estimate.quote_id, invoice.payment_hash
    );
    if !trigger.pays() {
        let _ = writeln!(
            out,
            "DRY RUN — nothing moved. Re-run with --confirm to pay {net} sats to {address}."
        );
        return Ok(RemitOutcome::DryRun);
    }

    // 6. Journal (as this process, under a lease), pass the pre-spend gate, pay under the ceiling,
    //    settle.
    let plan = RemittancePlan {
        payment_hash: invoice.payment_hash.clone(),
        gross_sats: gross,
        net_sats: net,
        melt_fee_reserve_sats: estimate.fee_reserve_sats,
        destination: address.to_string(),
        bolt11: invoice.bolt11.clone(),
        melt_quote_id: Some(estimate.quote_id.clone()),
    };
    let lease_until_unix = now_unix.saturating_add(lease_secs(REMIT_LEASE));
    let planned = match store.plan_remittance(&plan, effects.owner(), lease_until_unix, now_unix) {
        Ok(planned) => planned,
        Err(PlanRefused::Store(error)) => return Err(format!("journal remittance: {error}")),
        Err(refused) => {
            let _ = writeln!(out, "REFUSED — {refused}. Nothing moved.");
            if let PlanRefused::InFlight(active) = &refused {
                trace.remittance_id = Some(active.remittance_id.clone());
            }
            return Ok(RemitOutcome::Refused(Refusal::PlanRefused(
                refused.to_string(),
            )));
        }
    };
    trace.remittance_id = Some(planned.remittance_id.clone());
    let _ = writeln!(
        out,
        "Journaled remittance {} covering {} receipt{} (owner {}, lease until unix {lease_until_unix}); paying...",
        planned.remittance_id,
        planned.receipts,
        if planned.receipts == 1 { "" } else { "s" },
        effects.owner()
    );
    effects.after_plan(&planned);

    // The spend, in the order addendum 5 §1 rule 1 fixes, with addendum 9 §1.1's actual-arithmetic
    // bound inserted BEFORE the fence (one model, addendum 10 §2 / addendum 11):
    //   1. raise the PAYMENT quote Q (spends nothing) and check the ceiling against Q's amount and
    //      reserve ALONE — the reserve-only precheck, before any proof is selected; refused ⇒
    //      `refuse_before_fence`: the row is NOT written (stays planned, unbound, ours, receipts
    //      pinned), the attempt is journaled failed, nothing spent; the next attempt's
    //      reconciliation releases it as our own earlier attempt and re-plans;
    //   1a. if Q's live reserve differs from the estimate and the planned invoice would not confirm,
    //      re-plan ONCE onto a new invoice and a live quote for it (one conditional update on the
    //      still-planned row); a second mismatch is refused as in step 1;
    //   1b. PREPARE the melt of Q: the wallet selects and reserves proofs in its OWN database and
    //      states the SDK's PREPARED proof-input fee and pre-melt swap fee. Prepare may fetch mint
    //      metadata/keysets (a GET); it posts nothing proof-bearing or fee-bearing — pinned CDK
    //      0.17.2 melt/saga/mod.rs:286–460 writes only the local store; the swap :687–697 and the
    //      melt request :907–911 both live inside `confirm`. The PREPARED total is not a gate;
    //   1c. run the SDK's post-swap arithmetic on the prepared figures: the ACTUAL input fee
    //      recomputed on the swapped split, and invoice + reserve + actual input fee + swap fee
    //      against the gross. Over, or the SDK would refuse after its swap ⇒ the prepared melt is
    //      cancelled (proofs back to Unspent, locally) and this is refused exactly like step 1: row
    //      stays planned and pinned, nothing spent, no bound Spending row is ever held for a fee;
    //   2. the fence — ONE compare-and-set in the store advances the row planned → spending AND
    //      BINDS Q to it, only if it is still planned, still ours, and its lease ends more than
    //      SPEND_MARGIN after the clock as read INSIDE the store call, after its lock (however long
    //      we paused between the plan and this line, that time counts, and no pause between reading
    //      the clock and the write can make it stale). Zero rows changed ⇒ cancel the prepared melt,
    //      refuse, no spend. Once admitted, the row is released by nobody on time: only the mint's
    //      verdict on Q resolves it;
    //   3. CONFIRM the prepared melt of Q — never a second quote for a row we hold; the swap (if
    //      any) and the melt request happen here and nowhere else.
    let ceiling = MeltCeiling {
        max_debit_sats: gross,
        invoice_sats: net,
        planned_quote_id: Some(estimate.quote_id.clone()),
    };
    let effects_owner_for_release = effects.owner().to_owned();
    let release_own_planned = |store: &SellerStore, out: &mut dyn Write| -> Result<bool, String> {
        // Nothing was spent and the row is ours and still planned: release it for the next attempt.
        // Conditional like every release: if the row is not as we left it, hold and say so.
        let released = store
            .release_remittance(
                &planned.remittance_id,
                &ReleaseOn::OwnPlanned {
                    owner: effects_owner_for_release.clone(),
                },
                now_unix,
            )
            .map_err(|error| format!("release remittance: {error}"))?;
        if released.is_none() {
            let _ = writeln!(
                out,
                "  (the row changed under me before it could be released — nothing written; the next attempt reconciles it)"
            );
        }
        Ok(released.is_some())
    };

    // 1. The payment quote, and the ceiling against IT.
    let quote = match effects.melt_quote(&invoice.bolt11) {
        Ok(quote) => quote,
        Err(error) => {
            let released = release_own_planned(store, out)?;
            let _ = writeln!(
                out,
                "payment quote failed: {error}. Nothing left the wallet{}.",
                if released {
                    format!(
                        "; released {} sats back to unremitted for the next attempt",
                        planned.gross_sats
                    )
                } else {
                    String::new()
                }
            );
            return Ok(RemitOutcome::QuoteFailed {
                remittance_id: planned.remittance_id,
                error,
            });
        }
    };
    let refuse_before_fence = |reason: String,
                               store: &SellerStore,
                               out: &mut dyn Write|
     -> Result<RemitOutcome, String> {
        // Addendum 10 §2: an arithmetic refusal before the fence writes NOTHING to the row. It stays
        // Planned, unbound (no admission mark, no bound quote), ours, under its lease, its receipts
        // pinned; the attempt journal records the failure. The next attempt's reconciliation
        // releases it as our own earlier attempt (`OwnPlanned`) and re-plans on a fresh quote —
        // backoff continues, one line here. (`release_own_planned` remains for a failed payment
        // quote and a lost fence, which are not arithmetic refusals.)
        let _ = store;
        let _ = writeln!(
            out,
            "REFUSED before spending — {reason}; ceiling {gross} sats; nothing left the wallet; the row stays planned and the next attempt re-quotes."
        );
        Ok(RemitOutcome::MeltRefused {
            remittance_id: planned.remittance_id.clone(),
            reason,
        })
    };
    if quote.amount_sats != net {
        return refuse_before_fence(
            format!(
                "melt refused before spending: mint {} quoted {} sats for the {net}-sat invoice; nothing left the wallet",
                quote.mint_url, quote.amount_sats
            ),
            store,
            out,
        );
    }
    if !ceiling.admits(quote.amount_sats, quote.fee_reserve_sats) {
        return refuse_before_fence(
            format!(
                "melt refused before spending: mint {} quote {} would debit {} sats ({} sats invoice + {} sats fee reserve; planned invoice {net} sats) against a ceiling of {gross} sats; nothing left the wallet",
                quote.mint_url,
                quote.quote_id,
                quote.amount_sats.saturating_add(quote.fee_reserve_sats),
                quote.amount_sats,
                quote.fee_reserve_sats
            ),
            store,
            out,
        );
    }

    // 1a. Addendum 10 §1.4 — the LIVE reserve differs from the estimate the invoice was planned
    //     on. A reserve that still lets the planned invoice confirm pays with slack; one that does
    //     not is re-planned ONCE, in this attempt, BEFORE anything is prepared: a new (re-planned)
    //     invoice — smaller or larger, whatever the search finds at the live reserve (record 40:
    //     12 → 15) — and a live quote for it, the planned row re-pointed at them by one conditional update
    //     (still ours, still planned, still unbound — otherwise nothing is written and this is a
    //     pre-fence refusal). A next-attempt re-quote could not do this: it would plan from the
    //     probe estimate again and meet the same drift (§2.1). Gross, receipts, owner and lease do
    //     not move. The re-planned quote is checked by the same bound; a second mismatch is NOT
    //     re-planned again. Only a reserve that SHRANK is re-planned (addendum 10 §1.4's words); a
    //     reserve that grew past the plan is refused as before, by the prepared-melt gate below
    //     (record 29): growth is the mint asking for more than was planned, and a seller that
    //     planned on the estimate does not chase it within the same attempt.
    let (quote, ceiling) = if quote.fee_reserve_sats >= estimate.fee_reserve_sats {
        (quote, ceiling)
    } else {
        let live_reserve = quote.fee_reserve_sats;
        let requires_swap = estimate.input_fee_ppk > 0;
        match confirm_bound(
            net,
            live_reserve,
            None,
            estimate.expected_swap_fee_sats,
            estimate.input_fee_ppk,
            requires_swap,
            gross,
        ) {
            Ok(_) => (quote, ceiling),
            Err(shortfall) => {
                let why_not = match &shortfall {
                    ConfirmShortfall::TargetShort {
                        bound,
                        needed_after_swap_sats,
                    } => format!(
                        "the wallet would swap to {} sats and the SDK's actual proof input fee on those proofs is {} sats, so the payment would need {needed_after_swap_sats} sats and the SDK would refuse after its swap",
                        bound.target_sats, bound.actual_input_fee_sats
                    ),
                    ConfirmShortfall::OverCeiling { bound, .. } => format!(
                        "invoice + reserve + actual proof input fee {} sats + swap fee {} sats = {} sats would exceed the {gross} sats accrued",
                        bound.actual_input_fee_sats, bound.swap_fee_sats, bound.worst_debit_sats
                    ),
                    ConfirmShortfall::DifferentInvoice { .. } => {
                        unreachable!("confirm_bound without a ceiling never compares invoices")
                    }
                };
                let net2 = match plan_confirmable_invoice(
                    gross,
                    live_reserve,
                    estimate.expected_swap_fee_sats,
                    estimate.input_fee_ppk,
                ) {
                    Some(net2) if net2 >= min_sats => net2,
                    _ => {
                        return refuse_before_fence(
                            format!(
                                "melt refused before spending: mint {} quote {} carries a {live_reserve} sats fee reserve on the {net}-sat invoice (planned on {} sats); {why_not}, and no invoice of at least {min_sats} sats fits {gross} sats at that reserve; nothing left the wallet",
                                quote.mint_url, quote.quote_id, estimate.fee_reserve_sats
                            ),
                            store,
                            out,
                        );
                    }
                };
                let invoice2 = match effects.invoice(&pay, net2) {
                    Ok(invoice2) => invoice2,
                    Err(error) => {
                        return refuse_before_fence(
                            format!(
                                "melt refused before spending: re-planning from invoice {net} sats to {net2} sats (live fee reserve {live_reserve} sats, planned on {} sats) — the {net2}-sat invoice could not be raised: {error}; nothing left the wallet",
                                estimate.fee_reserve_sats
                            ),
                            store,
                            out,
                        );
                    }
                };
                let quote2 = match effects.melt_quote(&invoice2.bolt11) {
                    Ok(quote2) => quote2,
                    Err(error) => {
                        return refuse_before_fence(
                            format!(
                                "melt refused before spending: re-planning from invoice {net} sats to {net2} sats (live fee reserve {live_reserve} sats, planned on {} sats) — the payment quote for the {net2}-sat invoice failed: {error}; nothing left the wallet",
                                estimate.fee_reserve_sats
                            ),
                            store,
                            out,
                        );
                    }
                };
                if quote2.amount_sats != net2 {
                    return refuse_before_fence(
                        format!(
                            "melt refused before spending: mint {} quoted {} sats for the re-planned {net2}-sat invoice; nothing left the wallet",
                            quote2.mint_url, quote2.amount_sats
                        ),
                        store,
                        out,
                    );
                }
                // The re-planned quote must itself confirm under the bound — with ITS reserve. A
                // second mismatch is a refusal, not another re-plan.
                let bound2 = match confirm_bound(
                    net2,
                    quote2.fee_reserve_sats,
                    None,
                    estimate.expected_swap_fee_sats,
                    estimate.input_fee_ppk,
                    requires_swap,
                    gross,
                ) {
                    Ok(bound2) => bound2,
                    Err(_) => {
                        return refuse_before_fence(
                            format!(
                                "melt refused before spending: re-planned to invoice {net2} sats on a {live_reserve} sats fee reserve, but mint {} quote {} carries a {} sats fee reserve on it and the payment would not confirm within {gross} sats; not re-planned a second time; nothing left the wallet",
                                quote2.mint_url, quote2.quote_id, quote2.fee_reserve_sats
                            ),
                            store,
                            out,
                        );
                    }
                };
                let replanned = store
                    .replan_remittance(
                        &planned.remittance_id,
                        effects.owner(),
                        &RemittanceReplan {
                            net_sats: net2,
                            payment_hash: invoice2.payment_hash.clone(),
                            bolt11: invoice2.bolt11.clone(),
                            melt_fee_reserve_sats: quote2.fee_reserve_sats,
                            melt_quote_id: Some(quote2.quote_id.clone()),
                        },
                    )
                    .map_err(|error| format!("re-plan remittance: {error}"))?;
                if replanned.is_none() {
                    return refuse_before_fence(
                        format!(
                            "melt refused before spending: the planned row changed under me while re-planning from invoice {net} sats to {net2} sats (live fee reserve {live_reserve} sats) — nothing written; nothing left the wallet"
                        ),
                        store,
                        out,
                    );
                }
                let _ = writeln!(
                    out,
                    "Re-planned: the payment quote's fee reserve is {live_reserve} sats (planned on {} sats); invoice {net} sats would not confirm ({why_not}), invoice {net2} sats will (at most {} sats leaves the wallet, ≤ {gross}); payment quote {} raised at mint {} for {net2} sats (fee reserve {} sats); invoice payment hash: {}",
                    estimate.fee_reserve_sats,
                    bound2.worst_debit_sats,
                    quote2.quote_id,
                    quote2.mint_url,
                    quote2.fee_reserve_sats,
                    invoice2.payment_hash
                );
                let ceiling2 = MeltCeiling {
                    max_debit_sats: gross,
                    invoice_sats: net2,
                    planned_quote_id: Some(quote2.quote_id.clone()),
                };
                (quote2, ceiling2)
            }
        }
    };
    // From here `quote` and `ceiling` are the (possibly re-planned) figures — the invoice paid is
    // `ceiling.invoice_sats`, the quote's; `planned` keeps the row as first journaled and only its
    // unchanged fields (id, gross, receipts) are read below.
    let margin_secs = lease_secs(SPEND_MARGIN);
    let quote_inside_margin = |now_unix: i64| {
        u64::try_from(now_unix.saturating_add(margin_secs))
            .is_ok_and(|bound| quote.expiry_unix <= bound)
    };
    let quote_now_unix = effects.now_unix();
    if quote_inside_margin(quote_now_unix) {
        return refuse_before_fence(
            format!(
                "melt refused before spending: mint {} quote {} expires at unix {}, within {margin_secs} s of now (unix {quote_now_unix}); a quote this close to expiry is not paid; nothing left the wallet",
                quote.mint_url, quote.quote_id, quote.expiry_unix
            ),
            store,
            out,
        );
    }
    let _ = writeln!(
        out,
        "Payment quote {} raised at mint {} for {} sats (fee reserve {} sats, expires unix {}); fits the ceiling of {gross} sats",
        quote.quote_id,
        quote.mint_url,
        quote.amount_sats,
        quote.fee_reserve_sats,
        quote.expiry_unix
    );

    // 1b. Prepare, and bound the TOTAL. Either refusal here happened in the wallet's own database
    //     (prepare may have fetched mint metadata — a GET): no proof-bearing or fee-bearing request
    //     was posted, nothing left the wallet, the row is still ours and planned.
    let prepared = match effects.prepare_melt(&quote.quote_id, &ceiling) {
        Ok(prepared) => prepared,
        Err(MeltFailure::RefusedBeforeSpending(reason)) => {
            return refuse_before_fence(reason, store, out);
        }
        Err(MeltFailure::Failed(reason)) => {
            return refuse_before_fence(
                format!(
                    "wallet could not prepare the melt of quote {}: {reason}; nothing was posted to the mint and nothing left the wallet",
                    quote.quote_id
                ),
                store,
                out,
            );
        }
    };
    let preparation = prepared.preparation.clone();
    // 1c. Confirmability (addendum 9 §1.1): the SDK's confirm recomputes the input fee on the
    //     proofs its swap yields and refuses AFTER the swap when they do not cover it. Run that
    //     arithmetic now, before the fence: a refusal here cancels the prepared melt (local) and
    //     takes `refuse_before_fence` — the row is not written, it stays planned with its receipts
    //     pinned for the next attempt's reconciliation; no fee-bearing request, no bound row.
    let actual_input_fee_sats = match confirm_would_succeed(&preparation, gross) {
        Ok(actual) => actual,
        Err(reason) => {
            if let Err(error) = effects.cancel_melt(prepared) {
                let _ = writeln!(
                    out,
                    "  (cancelling the prepared melt failed: {error}; no fee-bearing request was posted; its local proof reservation may remain until a supported recovery path — owed — releases it)"
                );
            }
            return refuse_before_fence(reason, store, out);
        }
    };
    let _ = writeln!(
        out,
        "Prepared melt of quote {}: proof input fee {} sats (estimate; actual on the swapped proofs {actual_input_fee_sats} sats), swap fee {} sats{}; total debit {} sats ({} invoice + {} reserve + fees) fits the ceiling of {gross} sats; proofs reserved in this wallet only, nothing posted yet",
        preparation.quote_id,
        preparation.input_fee_sats,
        preparation.swap_fee_sats,
        if preparation.requires_swap {
            " (the wallet's proofs do not fit: a pre-melt swap will be performed)"
        } else {
            ""
        },
        preparation.total_debit_sats,
        preparation.invoice_sats,
        preparation.fee_reserve_sats
    );
    effects.after_quote(&planned, &quote);

    // 2. The fence: clock read inside the store call, Q bound.
    let mut admit_now_unix: Option<i64> = None;
    let owner = effects_owner_for_release.clone();
    let admitted = {
        let effects_ref: &dyn RemitEffects = &*effects;
        store
            .admit_remittance_spend(
                &planned.remittance_id,
                &owner,
                &quote.quote_id,
                margin_secs,
                &mut || {
                    let now = effects_ref.now_unix();
                    admit_now_unix = Some(now);
                    now
                },
            )
            .map_err(|error| format!("admit remittance {}: {error}", planned.remittance_id))?
    };
    let admit_now_unix = admit_now_unix.unwrap_or(now_unix);
    let admitted = match admitted {
        Ok(admitted) => admitted,
        Err(lost) => {
            // The prepared melt first: cancel it — the SDK's best-effort local compensation. It
            // posted no fee-bearing request, so a failed cancel changes nothing at the mint; a
            // local proof reservation may remain (opening the wallet runs no saga recovery on this
            // path; a supported recovery path is owed) — say so.
            if let Err(error) = effects.cancel_melt(prepared) {
                let _ = writeln!(
                    out,
                    "  (cancelling the prepared melt failed: {error}; no fee-bearing request was posted; its local proof reservation may remain until a supported recovery path — owed — releases it)"
                );
            }
            // Ours, still planned, but too little lease left: nothing was spent, so release our own
            // row (conditionally). Not ours, gone, or no longer planned: another process holds or
            // resolved it — touch nothing.
            let released = if matches!(lost, OwnershipLost::LeaseTooShort { .. }) {
                release_own_planned(store, out)?
            } else {
                false
            };
            let reason = lost.to_string();
            let _ = writeln!(
                out,
                "REFUSED before spending — {reason} (checked at unix {admit_now_unix}). Nothing moved by this run{}.",
                if released {
                    format!(
                        "; released {} sats back to unremitted for the next attempt",
                        planned.gross_sats
                    )
                } else {
                    String::new()
                }
            );
            return Ok(RemitOutcome::Refused(Refusal::OwnershipLost {
                remittance_id: planned.remittance_id,
                reason,
            }));
        }
    };
    let _ = writeln!(
        out,
        "Admitted to spend at unix {admit_now_unix}: remittance {} is now spending, bound to melt quote {} (lease until unix {lease_until_unix}); from here only the mint's verdict on that quote resolves it",
        admitted.remittance_id, quote.quote_id
    );
    effects.after_admit(&admitted);

    // 3. Confirm the prepared melt of Q. First the local refusal of a quote inside its margin of
    //    expiry, on a fresh clock — to avoid a pointless attempt, not as a safety bound: the row is
    //    spending and stays so, held until the mint reports Q PAID, and this process never
    //    re-quotes for it. The prepared melt is cancelled (best-effort local compensation; no
    //    fee-bearing request was posted).
    let pay_now_unix = effects.now_unix();
    if quote_inside_margin(pay_now_unix) {
        let error = format!(
            "bound melt quote {} expires at unix {}, within {margin_secs} s of now (unix {pay_now_unix}); not paid",
            quote.quote_id, quote.expiry_unix
        );
        let cancel_note = match effects.cancel_melt(prepared) {
            Ok(()) => String::new(),
            Err(cancel_error) => format!(
                " (cancelling the prepared melt failed: {cancel_error}; no fee-bearing request was posted; its local proof reservation may remain until a supported recovery path — owed — releases it)"
            ),
        };
        let _ = writeln!(
            out,
            "not paid: {error}.{cancel_note}\n  remittance {} stays journaled as spending, bound to that quote; this process raises no other quote for it. The next attempt asks the mint about that quote: settled if it shows PAID, otherwise HELD with its receipts — no clock releases a spending row. Nothing else was attempted.",
            planned.remittance_id
        );
        return Ok(RemitOutcome::MeltFailed {
            remittance_id: planned.remittance_id,
            error,
        });
    }
    match effects.confirm_melt(prepared) {
        Ok(outcome) => {
            let settlement = RemitSettlement {
                net_paid_sats: Some(outcome.paid_sats),
                melt_fee_sats: Some(outcome.fee_sats),
                melt_fee_reserve_sats: Some(outcome.fee_reserve_sats),
                melt_quote_id: Some(outcome.quote_id.clone()),
                settled_by: SettledBy::Melt,
            };
            let settled = store
                .settle_remittance(&planned.remittance_id, &settlement, now_unix)
                .map_err(|error| {
                    format!(
                        "PAID {} sats (melt fee {} sats, quote {}) but could not record the settlement: {error}. \
                         Remittance {} stays spending; the next attempt reconciles it with the mint before paying anything else.",
                        outcome.paid_sats, outcome.fee_sats, outcome.quote_id, planned.remittance_id
                    )
                })?;
            // Actual debit (addendum 9 §2.1): invoice + the SDK's `fee_paid` (the mint's Lightning
            // fee PLUS the actual proof input fee on the melt's proofs, inclusive — pinned CDK
            // `melt/saga/mod.rs:139–148`) + the swap fee charged at the swap. The PREPARED input
            // fee is an estimate the SDK replaced inside `fee_paid`; it is printed, not added.
            let debit = outcome
                .paid_sats
                .saturating_add(outcome.fee_sats)
                .saturating_add(outcome.swap_fee_sats);
            let balance_now = match outcome.balance_after_sats {
                Some(balance) => format!("{balance} sats"),
                None => "unknown (the balance read after the payment failed; the payment stands)"
                    .to_owned(),
            };
            let _ = writeln!(
                out,
                "PAID — remittance {} settled\n  gross discharged: {} sats\n  melt fee taken by the mint: {} sats (quote {} reserved {} sats; ceiling {gross} sats held at the moment of spending; this is the SDK's fee_paid = Lightning fee + actual proof input fee)\n  estimated proof input fee (prepared): {} sats — replaced by the actual fee inside the melt fee above, not added again; swap fee (charged at swap): {} sats\n  actual debit: {debit} sats = net + melt fee + swap fee\n  net paid to {}: {} sats\n  stays in your wallet (unused reserve): {} sats\n  wallet balance now: {} at {}\n  receipts discharged: {}",
                settled.remittance_id,
                settled.gross_sats,
                outcome.fee_sats,
                outcome.quote_id,
                outcome.fee_reserve_sats,
                outcome.input_fee_sats,
                outcome.swap_fee_sats,
                settled.destination,
                outcome.paid_sats,
                gross.saturating_sub(debit),
                balance_now,
                outcome.mint_url,
                settled.receipts
            );
            if debit > gross {
                // Belt behind the braces: before the fence the confirmability check bounded
                // invoice + reserve + ACTUAL input fee (recomputed on the swapped split) + swap fee
                // under the ceiling — the prepared display is not the bound, a prepared total over
                // the gross with a fitting actual debit is admitted — and the mint's Lightning fee
                // is at most its reserve, so this line should never print under fixed fee metadata. If it does,
                // the mint's fee metadata changed between prepare and confirm (§1.5 bound).
                let _ = writeln!(
                    out,
                    "WARNING: the wallet lost {debit} sats ({} net + {} melt fee incl. actual proof input fee + {} swap fee) against {gross} sats accrued — above the ceiling the melt was admitted under. Recorded as settled; report this.",
                    outcome.paid_sats, outcome.fee_sats, outcome.swap_fee_sats
                );
            }
            Ok(RemitOutcome::Paid {
                remittance_id: settled.remittance_id,
                net_sats: outcome.paid_sats,
                melt_fee_sats: outcome.fee_sats,
            })
        }
        Err(MeltFailure::RefusedBeforeSpending(reason)) => {
            // Since addendum 8 the ceiling is taken at `prepare_melt`, before the fence, so a
            // confirm cannot refuse on it: this arm is unreachable through the live effects and
            // kept only so a future effects impl that does refuse here is handled the safe way —
            // nothing left the wallet, but the row is SPENDING and bound: it is not released on a
            // typed promise — reconciliation asks the mint about its bound quote, settles on PAID
            // and otherwise holds; this process never re-quotes for it.
            let error = format!("refused before spending: {reason}");
            let _ = writeln!(
                out,
                "REFUSED before spending — {reason}.\n  Nothing left the wallet. remittance {} stays journaled as spending, bound to melt quote {}; the next attempt asks the mint about that quote: settled if PAID, otherwise held with its receipts. Nothing else was attempted.",
                planned.remittance_id, quote.quote_id
            );
            Ok(RemitOutcome::MeltFailed {
                remittance_id: planned.remittance_id,
                error,
            })
        }
        Err(MeltFailure::Failed(error)) => {
            let _ = writeln!(
                out,
                "melt failed: {error}\n  remittance {} stays journaled as spending, bound to melt quote {}: proofs may have reached the mint. The next attempt (automatic, or `maxplayer seller fees remit`) asks the mint about THAT QUOTE: settled if it is PAID, otherwise HELD with its receipts — UNPAID, FAILED, PENDING, unknown or expired, no clock releases it; an operator decision does. This process raises no other quote for the row. Nothing else was attempted.",
                planned.remittance_id, quote.quote_id
            );
            Ok(RemitOutcome::MeltFailed {
                remittance_id: planned.remittance_id,
                error,
            })
        }
    }
}

/// A `Duration` as whole unix seconds, for lease arithmetic.
fn lease_secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

fn print_recent_attempts(store: &SellerStore, out: &mut dyn Write) -> Result<(), String> {
    let attempts = store
        .recent_remit_attempts(RECENT_ATTEMPTS_SHOWN)
        .map_err(|error| format!("read remit attempts: {error}"))?;
    if attempts.is_empty() {
        let _ = writeln!(out, "Recent attempts: none journaled yet");
        return Ok(());
    }
    let _ = writeln!(
        out,
        "Recent attempts (newest first, last {}):",
        RECENT_ATTEMPTS_SHOWN
    );
    for attempt in &attempts {
        let _ = writeln!(
            out,
            "  unix {}: {} attempt saw {} sats unremitted — {}: {}{}",
            attempt.started_at_unix,
            match attempt.trigger {
                RemitAttemptTrigger::Collect => "automatic (after collect)",
                RemitAttemptTrigger::Retry => "automatic (retry tick)",
                RemitAttemptTrigger::Command => "operator (--confirm)",
            },
            attempt.unremitted_sats,
            attempt.outcome.as_str().to_uppercase(),
            attempt.detail,
            match &attempt.remittance_id {
                Some(id) => format!(" [remittance {id}]"),
                None => String::new(),
            }
        );
    }
    Ok(())
}

/// What the collect path gets back: the outcome and every line the attempt printed, for the node's
/// log. Never an error the caller has to handle — that is the point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemitReport {
    pub outcome: Result<RemitOutcome, String>,
    pub lines: Vec<String>,
}

impl RemitReport {
    /// A refusal at the threshold — the expected steady state; the caller may log it briefly.
    pub fn is_quiet(&self) -> bool {
        matches!(&self.outcome, Ok(RemitOutcome::Refused(refusal)) if refusal.is_threshold())
    }

    /// One line saying how the attempt ended.
    pub fn summary(&self) -> String {
        match &self.outcome {
            Ok(RemitOutcome::Paid {
                remittance_id,
                net_sats,
                melt_fee_sats,
            }) => format!(
                "PAID {net_sats} sats to {PLATFORM_FEE_ADDRESS} (melt fee {melt_fee_sats} sats), remittance {remittance_id}"
            ),
            Ok(RemitOutcome::DryRun) => "dry run; nothing moved".to_owned(),
            Ok(RemitOutcome::Refused(refusal)) if refusal.is_threshold() => format!(
                "nothing moved ({refusal}); the balance accumulates until it clears the destination's minimum"
            ),
            Ok(RemitOutcome::Refused(refusal)) => {
                format!("REFUSED, nothing moved: {refusal}; the balance stays unremitted")
            }
            Ok(RemitOutcome::MeltFailed {
                remittance_id,
                error,
            }) => format!(
                "melt FAILED ({error}); remittance {remittance_id} stays spending and is reconciled against the mint on the next attempt"
            ),
            Ok(RemitOutcome::MeltRefused {
                remittance_id,
                reason,
            }) => format!(
                "melt REFUSED before spending ({reason}); remittance {remittance_id} stays planned and unbound, nothing spent; the next attempt reconciles it and re-quotes"
            ),
            Ok(RemitOutcome::QuoteFailed {
                remittance_id,
                error,
            }) => format!(
                "payment quote FAILED ({error}); remittance {remittance_id} released, nothing spent, the balance stays unremitted and the next attempt re-quotes"
            ),
            Err(error) => format!(
                "attempt FAILED ({error}); the balance stays unremitted and the node retries with backoff while it runs"
            ),
        }
    }

    /// Whether this attempt counts as a FAILURE for pacing ([`RemitBackoff::observe`]): it meant to
    /// pay and did not, for a reason that is not the steady state. `Err` (an effect failed),
    /// `MeltFailed`, `MeltRefused`, `QuoteFailed`, and every refusal that is not at the threshold —
    /// the balance stays owed and hammering the same host or mint every 30 s would not change that.
    /// A threshold refusal, a payment and a dry run are not failures.
    pub fn is_failure(&self) -> bool {
        match &self.outcome {
            Err(_)
            | Ok(RemitOutcome::MeltFailed { .. })
            | Ok(RemitOutcome::MeltRefused { .. })
            | Ok(RemitOutcome::QuoteFailed { .. }) => true,
            Ok(RemitOutcome::Refused(refusal)) => !refusal.is_threshold(),
            Ok(RemitOutcome::Paid { .. }) | Ok(RemitOutcome::DryRun) => false,
        }
    }
}

/// **The node's attempt** — [`remit`] under a paying node trigger ([`RemitTrigger::Collect`] or
/// [`RemitTrigger::Retry`]), with every error caught into the report. Nothing here can fail the
/// caller: on the collect path the receipt is already journaled and the job already marked paid
/// before this runs; a failure leaves the balance unremitted for the retry tick (and the next
/// collect) to try again.
pub fn remit_best_effort(
    store: &SellerStore,
    effects: &mut dyn RemitEffects,
    trigger: RemitTrigger,
    now_unix: i64,
) -> RemitReport {
    debug_assert!(
        matches!(trigger, RemitTrigger::Collect | RemitTrigger::Retry),
        "the node's best-effort attempt runs under a node trigger, never the operator's"
    );
    let mut out = Vec::new();
    let outcome = remit(store, effects, trigger, now_unix, &mut out);
    let lines = String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_owned)
        .collect();
    RemitReport { outcome, lines }
}

/// [`remit_best_effort`] over the shipped [`LiveEffects`] — what the seller node runs, on a thread
/// of its own, after a receipt is journaled `Collected::New` and on each retry tick. A failure to
/// build the https client is itself journaled as a failed attempt, so even that is visible in the
/// read-out.
pub fn remit_live_best_effort(
    store: &SellerStore,
    home: MaxplayerHome,
    trigger: RemitTrigger,
    now_unix: i64,
) -> RemitReport {
    match LiveEffects::new(home) {
        Ok(mut effects) => remit_best_effort(store, &mut effects, trigger, now_unix),
        Err(error) => {
            let error = format!("build https client for LNURL: {error}");
            let unremitted = store
                .accrued_fees()
                .map(|accrued| accrued.unremitted_fee_sats)
                .unwrap_or(0);
            let journaled = store.record_remit_attempt(&RemitAttempt {
                attempt_id: 0,
                started_at_unix: now_unix,
                trigger: trigger.journal_as().unwrap_or(RemitAttemptTrigger::Collect),
                unremitted_sats: unremitted,
                outcome: RemitAttemptOutcome::Failed,
                detail: error.clone(),
                remittance_id: None,
            });
            let mut lines = vec![error.clone()];
            if let Err(journal_error) = journaled {
                lines.push(format!("could not journal the attempt: {journal_error}"));
            }
            RemitReport {
                outcome: Err(error),
                lines,
            }
        }
    }
}

// ---- retry pacing (stage 2a, addendum 2) ------------------------------------------------------

/// The retry tick's base delay: the first attempt after boot waits at least this long
/// ([`RemitBackoff::boot_delay`]), and a streak of failures doubles it from here.
pub const RETRY_BASE: Duration = Duration::from_secs(30);
/// The ceiling the doubling stops at. A node whose payout host is down keeps trying at most this
/// often, for as long as it runs.
pub const RETRY_CAP: Duration = Duration::from_secs(30 * 60);

/// How one observed attempt moved the pacing — what the loop logs, and how loudly (addendum 2 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pacing {
    /// The steady state: nothing owed, or not enough to clear the destination's minimum. Not a
    /// failure — the streak is untouched and nothing is logged at full volume.
    Idle,
    /// A payment with no failure streak behind it.
    Paid,
    /// The first failure of a streak — the one to log in full, with its error.
    FirstFailure,
    /// Another failure in the same streak. `entered_cap` marks the transition into the 30-minute
    /// ceiling — a line an operator wants once, not every half hour.
    RepeatFailure { streak: u32, entered_cap: bool },
    /// A payment that ended a streak: how many attempts failed first, and for how long the fee sat
    /// owed while they did. The line an operator wants when they ask "did it ever go out?".
    Recovered {
        failed_attempts: u32,
        owed_for_secs: i64,
    },
}

/// The retry tick's backoff: **base 30 s, doubling on consecutive failures, capped at 30 minutes,
/// with full jitter**; reset to base by a successful remittance and by nothing else.
///
/// One instance per node, shared by the loop's tick and the collect path's thread, so a success on
/// either path resets it and a failure on either escalates it: both back off against the same LNURL
/// host and the same mint.
///
/// **Why full jitter, and why nobody may "simplify" it away:** every seller's node backs off against
/// the same payout host and the same mint. If they all slept the computed delay, an outage would end
/// with every node in the fleet retrying in the same second — the correlated burst that turns a
/// recovered host back into a failed one. So the delay actually slept is a uniform random value in
/// `[0, computed_delay]`, not the delay plus a small wobble ([`Self::next_delay`]). The first
/// attempt after boot is the one exception to "from zero" (addendum 3 RULING 1): it waits the full
/// base and THEN a jitter in `[0, base]` — `[30 s, 60 s]` — so a fleet restarting together neither
/// attempts at once nor attempts at startup ([`Self::boot_delay`]). Zero is never a legal first
/// delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemitBackoff {
    base: Duration,
    cap: Duration,
    /// Consecutive failures observed since the last success.
    streak: u32,
    /// When the current streak began (unix seconds), for the recovery line.
    streak_since_unix: Option<i64>,
}

impl Default for RemitBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl RemitBackoff {
    /// The shipped bounds: [`RETRY_BASE`] doubling to [`RETRY_CAP`].
    pub fn new() -> Self {
        Self::with_bounds(RETRY_BASE, RETRY_CAP)
    }

    /// Explicit bounds — for tests that must not sleep 30 minutes. `cap` below `base` is clamped
    /// to `base`.
    pub fn with_bounds(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap: cap.max(base),
            streak: 0,
            streak_since_unix: None,
        }
    }

    /// Consecutive failures so far (0 = healthy).
    pub fn streak(&self) -> u32 {
        self.streak
    }

    /// The base delay: the floor under the first retry after boot (RULING 1), whatever re-arms it.
    pub fn base(&self) -> Duration {
        self.base
    }

    /// The delay the current streak computes to, BEFORE jitter: `base × 2^streak`, capped.
    pub fn computed_delay(&self) -> Duration {
        let mut delay = self.base;
        for _ in 0..self.streak {
            if delay >= self.cap {
                break;
            }
            delay = delay.saturating_mul(2);
        }
        delay.min(self.cap)
    }

    /// Whether the doubling has reached the cap.
    pub fn at_cap(&self) -> bool {
        self.computed_delay() >= self.cap
    }

    /// The delay to actually sleep before the next attempt: full jitter over
    /// [`Self::computed_delay`], drawn from the OS RNG. Never above the computed delay, never below
    /// zero. If the RNG is unavailable (it should never be), sleeps the full computed delay — later
    /// is the safe direction.
    pub fn next_delay(&self) -> Duration {
        jittered(self.computed_delay(), os_entropy())
    }

    /// The delay before the FIRST attempt after boot (addendum 3 RULING 1): one full base delay,
    /// plus an additive jitter in `[0, base]` — `[base, 2 × base]`, never less than the base, never
    /// zero. See [`boot_delay_for`] for the pure form.
    pub fn boot_delay(&self) -> Duration {
        boot_delay_for(self.base, os_entropy())
    }

    /// Fold one finished attempt into the pacing and say what changed. Failures
    /// ([`RemitReport::is_failure`]) lengthen the streak; a payment resets it to base; the steady
    /// state ([`Pacing::Idle`]) leaves it exactly as it was — a balance under the threshold neither
    /// escalates nor resets.
    pub fn observe(&mut self, report: &RemitReport, now_unix: i64) -> Pacing {
        if report.is_failure() {
            let was_at_cap = self.at_cap();
            self.streak = self.streak.saturating_add(1);
            if self.streak == 1 {
                self.streak_since_unix = Some(now_unix);
                return Pacing::FirstFailure;
            }
            return Pacing::RepeatFailure {
                streak: self.streak,
                entered_cap: !was_at_cap && self.at_cap(),
            };
        }
        match &report.outcome {
            Ok(RemitOutcome::Paid { .. }) => {
                let failed_attempts = self.streak;
                let owed_for_secs = self
                    .streak_since_unix
                    .map(|since| now_unix.saturating_sub(since).max(0))
                    .unwrap_or(0);
                self.streak = 0;
                self.streak_since_unix = None;
                if failed_attempts == 0 {
                    Pacing::Paid
                } else {
                    Pacing::Recovered {
                        failed_attempts,
                        owed_for_secs,
                    }
                }
            }
            _ => Pacing::Idle,
        }
    }
}

/// Eight bytes from the OS RNG as a `u64`. If the RNG is unavailable (it should never be), the
/// maximum — which every caller maps to the LONGEST delay: later is the safe direction.
fn os_entropy() -> u64 {
    let mut bytes = [0u8; 8];
    match getrandom::fill(&mut bytes) {
        Ok(()) => u64::from_le_bytes(bytes),
        Err(_) => u64::MAX,
    }
}

/// The boot delay's pure form: `base + jittered(base, entropy)`, so `[base, 2 × base]` — `entropy
/// = 0` gives exactly the base, never less. Saturates rather than overflowing.
pub fn boot_delay_for(base: Duration, entropy: u64) -> Duration {
    base.saturating_add(jittered(base, entropy))
}

/// Full jitter: a uniform point in `[0, computed]` chosen by `entropy` (`0` ⇒ zero, `u64::MAX` ⇒
/// the whole computed delay). Pure, so the bound is tested without a clock or an RNG.
pub fn jittered(computed: Duration, entropy: u64) -> Duration {
    // Integer arithmetic, scaled in two parts so nothing overflows even at `Duration::MAX`:
    // `secs × entropy` and `subsec_nanos × entropy` each fit u128 (u64 × u64), and the remainder
    // of the seconds part becomes nanoseconds.
    let scale = u128::from(u64::MAX);
    let entropy = u128::from(entropy);
    let secs_scaled = u128::from(computed.as_secs()) * entropy;
    let whole_secs = secs_scaled / scale;
    let carry_nanos = (secs_scaled % scale) * 1_000_000_000 / scale;
    let subsec_nanos = u128::from(computed.subsec_nanos()) * entropy / scale;
    let nanos = carry_nanos + subsec_nanos;
    let secs = Duration::from_secs(u64::try_from(whole_secs).unwrap_or(u64::MAX));
    secs.checked_add(Duration::from_nanos(
        u64::try_from(nanos).unwrap_or(u64::MAX),
    ))
    .unwrap_or(computed)
    .min(computed)
}

/// **Single-flight for the node's two paths** (addendum 2 §3): one remittance attempt in flight per
/// process, ever. The collect thread and the loop's tick can reach the entry point at the same time;
/// whichever cannot take the permit **skips and returns** — it does not queue, block or fail. This is
/// a liveness device, not the correctness argument: the store's one-`planned`-row rule is what makes
/// a double payment impossible, including against `maxplayer seller fees remit --confirm` in another
/// process, which this guard cannot see.
#[derive(Debug, Clone, Default)]
pub struct RemitFlight(Arc<AtomicBool>);

/// Held by the one attempt in flight; the slot frees when it drops (including on a panic).
#[derive(Debug)]
pub struct RemitPermit(Arc<AtomicBool>);

impl RemitFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the slot if it is free. `None` means an attempt is already in flight: skip.
    pub fn try_acquire(&self) -> Option<RemitPermit> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RemitPermit(Arc::clone(&self.0)))
    }

    /// Whether an attempt holds the slot right now.
    pub fn in_flight(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Drop for RemitPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// The SDK fee arithmetic (`fee_for`, `binary_split`, `post_swap_figures`, `confirm_bound`) lives in
// `wallet_ops` since addendum 10 §1.1, so the wallet's prepared-melt gate, this module's planner and
// its pre-fence check are one function; re-exported here for this module and its tests.
pub(crate) use crate::wallet_ops::{ConfirmShortfall, binary_split, confirm_bound};

/// **Fee-aware planning (addendum 9 §1.2): the largest invoice `confirm` can actually pay within
/// `gross`.** Searches DOWN from `gross − reserve` for the first invoice `n` for which, with
/// `need = n + reserve`, the post-swap arithmetic holds — target ≥ need + actual input fee — AND
/// need + actual input fee + `swap_fee_sats` ≤ `gross`. `reserve` and `swap_fee_sats` are the
/// probe's (the mint's reserve policy and the wallet's swap fee at the gross); the quote raised for
/// the chosen invoice is checked again with its own figures. `None`: no invoice fits — refuse at
/// planning. A fee-free mint (`input_fee_ppk == 0`, no swap) yields `gross − reserve`, as before.
/// Verdict da0ee92 §4.4: gross 20, reserve 2, 1000 ppk, one 32-sat proof ⇒ 13 (need 15, target 19 =
/// [16, 2, 1], actual 3, worst 19 ≤ 20), not 14 (need 16, target 17 = [16, 1], actual 2, 17 < 18).
pub(crate) fn plan_confirmable_invoice(
    gross: u64,
    reserve: u64,
    swap_fee_sats: u64,
    input_fee_ppk: u64,
) -> Option<u64> {
    let ceiling = gross.checked_sub(reserve)?;
    (1..=ceiling).rev().find(|&invoice| {
        confirm_bound(
            invoice,
            reserve,
            None,
            swap_fee_sats,
            input_fee_ppk,
            true,
            gross,
        )
        .is_ok()
    })
}

/// **Will `confirm` succeed, under the fee metadata the preparation saw?** Addendum 9 §1.1, from
/// pinned CDK 0.17.2 `MeltSaga::request_melt_with_options` (`melt/saga/mod.rs:647–760`): on a
/// swap layout the wallet swaps to a target of invoice + reserve + the PREPARED input fee (`:678`),
/// receives exactly that target's binary split (`swap/saga/mod.rs:285–301`), RECOMPUTES the input
/// fee on that split (`:704`) and refuses — after the swap has been paid — when the target does not
/// cover invoice + reserve + that actual fee (`:706–712`). The prepared `input_fee` is an estimate
/// on the split of invoice + reserve BEFORE the fee is added (`:383–387`), so the two can differ.
/// This is that arithmetic, run BEFORE the fence and before any fee-bearing effect: the target's
/// split × the keyset's `input_fee_ppk`, ceil — deterministic for a power-of-two keyset. On an
/// exact-fit layout (no swap) the melt sends the selected proofs and the recomputed fee is the
/// prepared one (same proofs, same metadata).
///
/// Also requires the worst-case debit under the ACTUAL fee — invoice + reserve + actual input fee
/// + swap fee — to fit `gross`. Returns the actual input fee; `Err` is the one printed line.
///
/// Bound, disclosed not solved (§1.5): fee metadata can change between prepare and confirm and the
/// SDK takes no caller maximum; a change there is what the bound-Spending hold after the fence
/// covers.
pub(crate) fn confirm_would_succeed(
    preparation: &MeltPreparation,
    gross: u64,
) -> Result<u64, String> {
    match confirm_bound(
        preparation.invoice_sats,
        preparation.fee_reserve_sats,
        Some(preparation.input_fee_sats),
        preparation.swap_fee_sats,
        preparation.input_fee_ppk,
        preparation.requires_swap,
        gross,
    ) {
        Ok(bound) => Ok(bound.actual_input_fee_sats),
        Err(ConfirmShortfall::TargetShort {
            bound,
            needed_after_swap_sats,
        }) => Err(format!(
            "melt refused before spending: the wallet would swap to {} sats ({:?}) for quote {} and the mint's actual proof input fee on those proofs is {} sats (prepared estimate {} sats at {} ppk), so {} sats invoice + {} sats fee reserve + {} sats would need {needed_after_swap_sats} sats and the SDK would refuse AFTER paying the {} sats swap fee; the prepared melt was cancelled before any fee-bearing request",
            bound.target_sats,
            binary_split(bound.target_sats),
            preparation.quote_id,
            bound.actual_input_fee_sats,
            preparation.input_fee_sats,
            preparation.input_fee_ppk,
            preparation.invoice_sats,
            preparation.fee_reserve_sats,
            bound.actual_input_fee_sats,
            preparation.swap_fee_sats
        )),
        Err(ConfirmShortfall::OverCeiling { bound, .. }) => Err(format!(
            "melt refused before spending: quote {} would debit up to {} sats under the mint's actual proof input fee ({} sats invoice + {} sats fee reserve + {} sats actual proof input fee + {} sats swap fee) against a ceiling of {gross} sats; the prepared melt was cancelled before any fee-bearing request",
            preparation.quote_id,
            bound.worst_debit_sats,
            preparation.invoice_sats,
            preparation.fee_reserve_sats,
            bound.actual_input_fee_sats,
            preparation.swap_fee_sats
        )),
        Err(ConfirmShortfall::DifferentInvoice { .. }) => {
            unreachable!("confirm_bound without a ceiling never compares invoices")
        }
    }
}

/// Scripted effects for tests, shared with `seller_node::run`'s collect-path tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    use super::{MeltFailure, PreparedMelt, Reconcile, RemitEffects, host_now_unix};
    use crate::lnurl_pay::{LightningAddress, PayRequest, ResolvedInvoice, Url};
    use crate::seller_node::store::FeeRemittance;
    use crate::wallet_ops::{
        ConfirmShortfall, MeltCeiling, MeltEstimate, MeltOutcome, MeltPreparation, MeltQuoteState,
        MeltQuoteStatus, WalletOpsError,
    };

    /// A rendezvous a test uses to PAUSE one attempt at a chosen point (after the plan is journaled,
    /// or inside the melt) while another attempt runs against the same store — the deterministic
    /// interleaving addendum 3 §2.2 asks for. The paused side calls [`Self::arrive_and_wait`]; the
    /// test waits for [`Self::wait_arrived`], does what it wants, then [`Self::release`]s.
    pub(crate) struct Gate {
        arrived: AtomicBool,
        released: Mutex<bool>,
        cv: Condvar,
    }

    impl Gate {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                arrived: AtomicBool::new(false),
                released: Mutex::new(false),
                cv: Condvar::new(),
            })
        }

        pub(crate) fn arrive_and_wait(&self) {
            self.arrived.store(true, Ordering::SeqCst);
            let mut released = self.released.lock().unwrap_or_else(|e| e.into_inner());
            while !*released {
                released = self.cv.wait(released).unwrap_or_else(|e| e.into_inner());
            }
        }

        pub(crate) fn arrived(&self) -> bool {
            self.arrived.load(Ordering::SeqCst)
        }

        /// Spin (bounded) until the paused side has arrived.
        pub(crate) fn wait_arrived(&self, bound: std::time::Duration) {
            let deadline = std::time::Instant::now() + bound;
            while !self.arrived() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the paused attempt never reached the gate"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }

        pub(crate) fn release(&self) {
            let mut released = self.released.lock().unwrap_or_else(|e| e.into_inner());
            *released = true;
            self.cv.notify_all();
        }
    }

    /// One melt quote at the fake mint, as BOTH sides of a test see it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct FakeQuote {
        pub(crate) bolt11: String,
        pub(crate) state: MeltQuoteState,
        pub(crate) amount_sats: u64,
        pub(crate) fee_reserve_sats: u64,
        pub(crate) expiry_unix: u64,
    }

    /// The fake mint's quote registry, SHARED between the Fakes of one test (one `Arc`, addendum 5
    /// §2): a quote raised by one side is visible to the other; a test moves a quote's state or
    /// expiry in place and both sides read the change; a payment marks its quote PAID for everyone.
    /// A Fake without a registry answers status queries from its scripted `status` instead.
    pub(crate) type QuoteRegistry = Arc<Mutex<BTreeMap<String, FakeQuote>>>;

    /// A melt the fake wallet has PREPARED: the quote as loaded, the proofs taken out of the pool
    /// and held for it. Confirm spends them; cancel returns them.
    #[derive(Debug, Clone)]
    pub(crate) struct FakePrepared {
        pub(crate) quote_id: String,
        pub(crate) quote: FakeQuote,
        pub(crate) selected: Vec<u64>,
        pub(crate) input_fee_sats: u64,
        pub(crate) swap_fee_sats: u64,
        pub(crate) requires_swap: bool,
    }

    /// What the fake wallet would use to pay `need` sats and what the SDK would charge for it, in
    /// the shape of pinned CDK 0.17.2 `MeltSaga::prepare` (`melt/saga/mod.rs:286–460`).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct FakeLayout {
        /// The proofs taken out of the pool (empty for a Fake without a pool).
        pub(crate) picked: Vec<u64>,
        /// `PreparedMelt::input_fee`.
        pub(crate) input_fee_sats: u64,
        /// `PreparedMelt::swap_fee`.
        pub(crate) swap_fee_sats: u64,
        pub(crate) requires_swap: bool,
    }

    pub(crate) use super::binary_split;
    pub(crate) use crate::wallet_ops::fee_for;

    /// One pre-melt swap the fake wallet performed inside `confirm` (CDK `melt/saga/mod.rs:
    /// 678–697` → `swap_no_reserve`): what it sent, what the mint kept as swap fee, what came back
    /// as the melt's proofs (the binary split of the target) and as change.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct FakeSwap {
        pub(crate) quote_id: String,
        pub(crate) sent: Vec<u64>,
        pub(crate) target_sats: u64,
        pub(crate) swap_fee_sats: u64,
        pub(crate) received: Vec<u64>,
        pub(crate) change: Vec<u64>,
    }

    /// The SDK's two prepare branches, on a snapshot of the pool (nothing removed):
    /// - `input_fee_ppk == 0`: the exact-fit branch (`:312–375`) — exact denominations for `need`,
    ///   no swap, no fees (with a positive ppk `select_proofs(…, include_fees = true)` targets
    ///   `need` + the selection's own fee, so the exact-fit test `proofs_total == need` at `:321`
    ///   does not hold and the swap branch is taken — modelled here as "ppk > 0 ⇒ swap branch");
    /// - otherwise the swap branch (`:377–459`): `estimated_output_count` = the number of proofs
    ///   in the binary split of `need` (`:383`, = popcount for a power-of-two keyset),
    ///   `input_fee` = fee on that count (`:384–387`), selection target `need + input_fee`
    ///   (`:389`), largest-first proofs until they cover the target plus their own input fee
    ///   (`select_proofs(…, true)`, `:391–397`), `swap_fee` = fee on the proofs picked (`:403`).
    ///
    /// A Fake without a pool has unbounded funds: one notional proof is swapped.
    pub(crate) fn layout(
        input_fee_ppk: u64,
        available: Option<&[u64]>,
        need: u64,
    ) -> Result<FakeLayout, String> {
        if input_fee_ppk == 0 {
            let picked = match available {
                None => Vec::new(),
                Some(available) => {
                    let mut scratch = available.to_vec();
                    select_exact(&mut scratch, need).ok_or_else(|| {
                        format!("no exact proofs for {need} sats among {available:?}")
                    })?
                }
            };
            return Ok(FakeLayout {
                picked,
                input_fee_sats: 0,
                swap_fee_sats: 0,
                requires_swap: false,
            });
        }
        let input_fee_sats = fee_for(input_fee_ppk, need.count_ones() as usize);
        let selection = need + input_fee_sats;
        let picked = match available {
            None => Vec::new(),
            Some(available) => {
                let mut sorted = available.to_vec();
                sorted.sort_unstable_by(|a, b| b.cmp(a));
                let mut picked = Vec::new();
                let mut sum = 0u64;
                for denomination in sorted {
                    picked.push(denomination);
                    sum += denomination;
                    if sum >= selection + fee_for(input_fee_ppk, picked.len()) {
                        break;
                    }
                }
                if sum < selection + fee_for(input_fee_ppk, picked.len()) {
                    return Err(format!(
                        "proofs {available:?} do not cover {selection} sats (amount + reserve + {input_fee_sats} sats estimated input fee) plus their own input fee"
                    ));
                }
                picked
            }
        };
        let swap_fee_sats = fee_for(input_fee_ppk, picked.len().max(1));
        Ok(FakeLayout {
            picked,
            input_fee_sats,
            swap_fee_sats,
            requires_swap: true,
        })
    }

    /// Remove exactly `picked` from `available` (each denomination once).
    fn take(available: &mut Vec<u64>, picked: &[u64]) {
        for denomination in picked {
            let index = available
                .iter()
                .position(|candidate| candidate == denomination)
                .expect("picked from available");
            available.swap_remove(index);
        }
    }

    pub(crate) fn quote_registry() -> QuoteRegistry {
        Arc::new(Mutex::new(BTreeMap::new()))
    }

    /// The fake WALLET's proofs — denominations in sats — SHARED between the Fakes of one test
    /// (one `Arc`, addendum 6 §2.1): every payment selects exact denominations summing to
    /// `amount + fee reserve` (as CDK's proof selection does) and removes them, so two payments
    /// from one wallet spend DISJOINT proofs and a test can assert that a second payment was
    /// refused by the STORE, not for want of funds. A Fake without proofs has unbounded funds.
    pub(crate) type FakeProofs = Arc<Mutex<Vec<u64>>>;

    pub(crate) fn fake_proofs(denominations: &[u64]) -> FakeProofs {
        Arc::new(Mutex::new(denominations.to_vec()))
    }

    /// Exact-denomination selection, largest first: the proofs (removed from `available`) that sum
    /// to exactly `need`, or `None` — nothing removed — when no such subset exists among the
    /// largest-first picks.
    fn select_exact(available: &mut Vec<u64>, need: u64) -> Option<Vec<u64>> {
        let mut sorted = available.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        let mut picked = Vec::new();
        let mut remaining = need;
        for denomination in sorted {
            if denomination <= remaining {
                picked.push(denomination);
                remaining -= denomination;
                if remaining == 0 {
                    break;
                }
            }
        }
        if remaining != 0 {
            return None;
        }
        for denomination in &picked {
            let index = available
                .iter()
                .position(|candidate| candidate == denomination)
                .expect("picked from available");
            available.swap_remove(index);
        }
        Some(picked)
    }

    /// Scripted effects. `reserve_for(amount)` is the mint's fee reserve policy at ESTIMATE time;
    /// `live_reserve_for`, when set, is the reserve the PAYMENT quote carries (the two can differ —
    /// addendum 3 §1); `melt_results` are consumed in order by payments; `status` answers the
    /// reconciliation queries when no `registry` is set; `pay_request_error` makes the LNURL host
    /// unreachable. Every call is logged so a test can assert what was — and was not — touched;
    /// `melt_counter`, when set, counts ACTUAL debits across Fakes on different threads (a payment
    /// refused at the ceiling or by the mint is not a debit and is logged in `ceiling_refusals` /
    /// `pay_refusals` instead). `owner` is the process this Fake speaks as; `invoice_tag` makes one
    /// Fake's invoices distinct from another's, as two real LNURL calls would be.
    ///
    /// Quote ids: the estimate for `bolt11` is `quote-{bolt11}`, the payment quote is
    /// `paid-quote-{bolt11}` — two quotes, two ids, as the wallet raises them. With a `registry`
    /// every quote raised is recorded there (UNPAID, expiring at `quote_expiry_unix`) and every
    /// status query reads it. [`Self::prepare_melt`] + [`Self::confirm_melt`] are shaped like the
    /// checksum-pinned CDK 0.17.2 path the verdict at 6fc77e1 read (§4 B3), in two halves around
    /// `melt_gate`: the WALLET half (ceiling, funds, and `prepare_melt`'s `expiry > now` check on
    /// the wallet's clock) is `prepare_melt`; the MINT half, which accepts an UNPAID **or FAILED**
    /// quote with NO expiry check and rejects PENDING / PAID / UNKNOWN, is `confirm_melt`, which
    /// pauses at `melt_gate` first. A fake mint stricter than the dependency is what let the
    /// round-4 defect through; this one is not.
    pub(crate) struct Fake {
        pub(crate) owner: String,
        pub(crate) invoice_tag: String,
        pub(crate) min_msat: u64,
        pub(crate) max_msat: u64,
        pub(crate) reserve_for: Box<dyn Fn(u64) -> u64 + Send>,
        pub(crate) live_reserve_for: Option<Box<dyn Fn(u64) -> u64 + Send>>,
        pub(crate) melt_results: Vec<Result<(u64, u64), String>>,
        pub(crate) status: Result<Option<MeltQuoteStatus>, String>,
        pub(crate) registry: Option<QuoteRegistry>,
        /// The wallet's proofs, shared with the other Fake of a two-owner test; `None` = unbounded.
        pub(crate) proofs: Option<FakeProofs>,
        /// The mint's keyset `input_fee_ppk` (NUT-02). `0` = a fee-free mint (every test written
        /// before addendum 8); positive ⇒ every payment takes the SDK's swap branch and pays a
        /// proof-input fee and a swap fee on top of amount + reserve (see [`layout`]).
        pub(crate) input_fee_ppk: u64,
        /// When set, the proof input fee `prepare_melt` REPORTS on its selection is this figure
        /// instead of the layout's — the fee-metadata drift model (round 9 plan §5 §1.4(b)): the
        /// SDK computes the prepared fee from the selected proofs' keyset, the swap lands on the
        /// active keyset, and the two can disagree; the actual fee `confirm` recomputes on the
        /// swapped proofs is unaffected. Only the PREPARED figure moves.
        pub(crate) prepared_input_override: Option<u64>,
        /// The exact proofs each debit spent, in order (empty inner vec when `proofs` is `None`).
        pub(crate) proofs_spent: Vec<Vec<u64>>,
        /// When set, the observational balance read after a confirmed melt fails (addendum 9 §2.3):
        /// `MeltOutcome::balance_after_sats` is `None`; the payment itself is unaffected.
        pub(crate) balance_read_fails: bool,
        /// Every pre-melt swap performed inside `confirm_melt`, in order — a swap is a mint effect
        /// that charges its fee whether or not the melt after it succeeds (addendum 9 §1.3).
        pub(crate) swaps: Vec<FakeSwap>,
        /// Melts prepared (proofs selected and held) and not yet confirmed or cancelled, by token.
        pub(crate) prepared: BTreeMap<u64, FakePrepared>,
        pub(crate) next_token: u64,
        /// Prepared melts the caller cancelled (quote ids), in order — proofs went back to the pool.
        pub(crate) cancels: Vec<String>,
        /// Expiry stamped on every quote this Fake raises (registry or not). Far future by default.
        pub(crate) quote_expiry_unix: u64,
        pub(crate) pay_request_error: Option<String>,
        pub(crate) pay_requests: usize,
        pub(crate) invoices: Vec<u64>,
        pub(crate) estimates: Vec<String>,
        /// Payment quotes raised (bolt11s), in order.
        pub(crate) quotes: Vec<String>,
        /// Actual payments (bolt11s), in order — the debits.
        pub(crate) melts: Vec<String>,
        pub(crate) ceiling_refusals: Vec<String>,
        /// Payments refused after the ceiling and before any debit: by the WALLET (the quote had
        /// expired at `prepare_melt`, or the proofs did not cover amount + reserve) or by the MINT
        /// (the quote was PENDING, PAID or UNKNOWN — never for expiry, never for FAILED).
        pub(crate) pay_refusals: Vec<String>,
        pub(crate) status_calls: Vec<String>,
        /// Reconciliation queries BY QUOTE ID (a spending row's bound quote).
        pub(crate) quote_status_calls: Vec<String>,
        pub(crate) melt_counter: Option<Arc<AtomicUsize>>,
        pub(crate) plan_gate: Option<Arc<Gate>>,
        /// Pause point AFTER the payment quote is raised and has passed the ceiling, BEFORE the fence
        /// (addendum 5 §2, `AfterQuote`): the row is still `planned` while the paused side waits.
        pub(crate) quote_gate: Option<Arc<Gate>>,
        /// Pause point AFTER the compare-and-set admitted the melt and BEFORE the payment (addendum
        /// 4 §1): the row is `spending`, bound to its quote, while the paused side waits here.
        pub(crate) admit_gate: Option<Arc<Gate>>,
        /// Pause point inside the payment itself: AFTER the wallet's last local check (ceiling,
        /// funds, `prepare_melt`'s expiry check) and BEFORE the request reaches the mint — the
        /// suspension the verdict at 6fc77e1 traced (§4 B3, `AfterPrepare`). A quote that expires
        /// while the payer waits here is still paid by the mint when the payer resumes.
        pub(crate) melt_gate: Option<Arc<Gate>>,
        /// Pause point AFTER reconciliation decided and BEFORE it writes (addendum 5 §2,
        /// `AfterDecision`): a test moves the row under a decided release here.
        pub(crate) decision_gate: Option<Arc<Gate>>,
        pub(crate) planned_seen: Vec<FeeRemittance>,
        pub(crate) admitted_seen: Vec<FeeRemittance>,
        pub(crate) decisions_seen: Vec<Reconcile>,
        /// The injectable clock [`RemitEffects::now_unix`] reads at the fence and before paying.
        /// Shared between the Fakes of one test (`Arc`) so that "the clock advanced while A was
        /// paused" is a fact A reads FRESH — inside the store's lock — and the store compares in
        /// SQL. Unset (`i64::MIN`) the Fake reads the host clock, like the live effects.
        pub(crate) clock: Arc<AtomicI64>,
    }

    impl Fake {
        pub(crate) fn new(reserve_for: impl Fn(u64) -> u64 + Send + 'static) -> Self {
            Self {
                owner: "fake-owner".to_owned(),
                invoice_tag: String::new(),
                min_msat: 1000,
                max_msat: 1_000_000_000,
                reserve_for: Box::new(reserve_for),
                live_reserve_for: None,
                prepared_input_override: None,
                melt_results: Vec::new(),
                status: Ok(None),
                registry: None,
                proofs: None,
                input_fee_ppk: 0,
                proofs_spent: Vec::new(),
                balance_read_fails: false,
                swaps: Vec::new(),
                prepared: BTreeMap::new(),
                next_token: 1,
                cancels: Vec::new(),
                quote_expiry_unix: u64::MAX,
                pay_request_error: None,
                pay_requests: 0,
                invoices: Vec::new(),
                estimates: Vec::new(),
                quotes: Vec::new(),
                melts: Vec::new(),
                ceiling_refusals: Vec::new(),
                pay_refusals: Vec::new(),
                status_calls: Vec::new(),
                quote_status_calls: Vec::new(),
                melt_counter: None,
                plan_gate: None,
                quote_gate: None,
                admit_gate: None,
                melt_gate: None,
                decision_gate: None,
                planned_seen: Vec::new(),
                admitted_seen: Vec::new(),
                decisions_seen: Vec::new(),
                clock: Arc::new(AtomicI64::new(i64::MIN)),
            }
        }

        /// Set the clock this Fake (and every Fake sharing its `clock`) reads at the fence.
        pub(crate) fn set_clock(&self, now_unix: i64) {
            self.clock.store(now_unix, Ordering::SeqCst);
        }

        pub(crate) fn bolt11_for(amount_sats: u64, sequence: usize) -> String {
            format!("lnbc-fake-{amount_sats}-{sequence}")
        }

        pub(crate) fn hash_for(amount_sats: u64, sequence: usize) -> String {
            format!("hash-{amount_sats}-{sequence}")
        }

        /// The payment quote's id for an invoice, as this Fake raises it.
        pub(crate) fn pay_quote_id(bolt11: &str) -> String {
            format!("paid-quote-{bolt11}")
        }

        fn amount_in(bolt11: &str) -> u64 {
            bolt11
                .split('-')
                .nth(2)
                .and_then(|raw| raw.parse().ok())
                .expect("fake bolt11 carries its amount")
        }

        fn live_reserve(&self, amount_sats: u64) -> u64 {
            match &self.live_reserve_for {
                Some(live) => live(amount_sats),
                None => (self.reserve_for)(amount_sats),
            }
        }

        fn register(&self, quote_id: &str, quote: FakeQuote) {
            if let Some(registry) = &self.registry {
                registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(quote_id.to_owned(), quote);
            }
        }

        /// The pool's value in sats — the wallet measured, not the melt counter. `None` for a
        /// Fake with unbounded funds.
        pub(crate) fn pool_value(&self) -> Option<u64> {
            self.proofs.as_ref().map(|proofs| {
                proofs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .sum()
            })
        }

        /// The fees the SDK would charge this wallet on top of `need` right now (nothing removed):
        /// the estimate's `expected_fees_sats`, or `0` and the reason.
        fn expected_fees(&self, need: u64) -> (u64, u64, Option<String>) {
            let snapshot: Option<Vec<u64>> = self
                .proofs
                .as_ref()
                .map(|proofs| proofs.lock().unwrap_or_else(|e| e.into_inner()).clone());
            match layout(self.input_fee_ppk, snapshot.as_deref(), need) {
                Ok(layout) => (
                    layout.input_fee_sats + layout.swap_fee_sats,
                    layout.swap_fee_sats,
                    None,
                ),
                Err(reason) => (0, 0, Some(reason)),
            }
        }

        /// Reserved proofs go back to the shared wallet when a payment is refused after selection.
        fn return_proofs(&self, selected: &[u64]) {
            if let Some(proofs) = &self.proofs {
                proofs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(selected);
            }
        }

        fn registered(&self, quote_id: &str) -> Option<FakeQuote> {
            self.registry.as_ref().and_then(|registry| {
                registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(quote_id)
                    .cloned()
            })
        }

        fn status_of(quote_id: &str, quote: &FakeQuote) -> MeltQuoteStatus {
            MeltQuoteStatus {
                mint_url: "https://mint.example".to_owned(),
                quote_id: quote_id.to_owned(),
                state: quote.state,
                amount_sats: quote.amount_sats,
                fee_reserve_sats: quote.fee_reserve_sats,
                expiry_unix: quote.expiry_unix,
            }
        }

        /// How "alive" a quote is, for the by-invoice query (the shipped helper's ranking): a paid
        /// quote outranks a pending one, which outranks a live unpaid one; expired and failed last.
        fn liveness(quote: &FakeQuote, now_unix: i64) -> u8 {
            match quote.state {
                MeltQuoteState::Paid => 0,
                MeltQuoteState::Pending => 1,
                MeltQuoteState::Unpaid
                    if !u64::try_from(now_unix).is_ok_and(|now| now > quote.expiry_unix) =>
                {
                    2
                }
                MeltQuoteState::Unknown => 3,
                MeltQuoteState::Unpaid => 4,
                MeltQuoteState::Failed => 5,
            }
        }
    }

    impl RemitEffects for Fake {
        fn owner(&self) -> &str {
            &self.owner
        }

        fn after_plan(&mut self, planned: &FeeRemittance) {
            self.planned_seen.push(planned.clone());
            // A Fake nobody set a clock on reads the plan's own timestamp — the run's entry time —
            // so the single-process tests that pass a literal `now` keep their arithmetic. A test
            // that moves time sets the clock (before the run, or while the run is paused here).
            if self.clock.load(Ordering::SeqCst) == i64::MIN {
                self.set_clock(planned.created_at_unix);
            }
            if let Some(gate) = &self.plan_gate {
                gate.arrive_and_wait();
            }
        }

        fn after_quote(&mut self, _planned: &FeeRemittance, _quote: &MeltEstimate) {
            if let Some(gate) = &self.quote_gate {
                gate.arrive_and_wait();
            }
        }

        fn after_admit(&mut self, admitted: &FeeRemittance) {
            self.admitted_seen.push(admitted.clone());
            if let Some(gate) = &self.admit_gate {
                gate.arrive_and_wait();
            }
        }

        fn after_decision(&mut self, _row: &FeeRemittance, decision: &Reconcile) {
            self.decisions_seen.push(decision.clone());
            if let Some(gate) = &self.decision_gate {
                gate.arrive_and_wait();
            }
        }

        fn now_unix(&self) -> i64 {
            match self.clock.load(Ordering::SeqCst) {
                // Never set and no plan seen yet (the fence always follows a plan, so this is
                // unreachable on the paying path): the host clock, like the live effects.
                i64::MIN => host_now_unix(),
                set => set,
            }
        }

        fn pay_request(&mut self, address: &LightningAddress) -> Result<PayRequest, String> {
            assert_eq!(address.to_string(), "maxplayer@strike.me");
            self.pay_requests += 1;
            if let Some(error) = &self.pay_request_error {
                return Err(error.clone());
            }
            Ok(PayRequest {
                callback: Url::parse("https://agi.cash/cb").unwrap(),
                min_sendable_msat: self.min_msat,
                max_sendable_msat: self.max_msat,
            })
        }

        fn invoice(
            &mut self,
            pay: &PayRequest,
            amount_sats: u64,
        ) -> Result<ResolvedInvoice, String> {
            pay.invoice_url(amount_sats)
                .map_err(|error| error.to_string())?;
            self.invoices.push(amount_sats);
            let sequence = self.invoices.len();
            Ok(ResolvedInvoice {
                bolt11: format!(
                    "{}{}",
                    Self::bolt11_for(amount_sats, sequence),
                    self.invoice_tag
                ),
                payment_hash: format!(
                    "{}{}",
                    Self::hash_for(amount_sats, sequence),
                    self.invoice_tag
                ),
                amount_sats,
                amount_msat: amount_sats * 1000,
            })
        }

        fn melt_estimate(&mut self, bolt11: &str) -> Result<MeltEstimate, String> {
            self.estimates.push(bolt11.to_owned());
            let amount_sats = Self::amount_in(bolt11);
            let fee_reserve_sats = (self.reserve_for)(amount_sats);
            let quote_id = format!("quote-{bolt11}");
            self.register(
                &quote_id,
                FakeQuote {
                    bolt11: bolt11.to_owned(),
                    state: MeltQuoteState::Unpaid,
                    amount_sats,
                    fee_reserve_sats,
                    expiry_unix: self.quote_expiry_unix,
                },
            );
            let (expected_fees_sats, expected_swap_fee_sats, expected_fees_note) =
                self.expected_fees(amount_sats.saturating_add(fee_reserve_sats));
            Ok(MeltEstimate {
                mint_url: "https://mint.example".to_owned(),
                quote_id,
                amount_sats,
                fee_reserve_sats,
                expiry_unix: self.quote_expiry_unix,
                expected_fees_sats,
                expected_swap_fee_sats,
                input_fee_ppk: self.input_fee_ppk,
                expected_fees_note,
            })
        }

        /// The PAYMENT quote, as the shipped wallet raises it: a fresh quote for the invoice whose
        /// reserve is `live_reserve_for` (or the estimate's policy when unset). Spends nothing.
        fn melt_quote(&mut self, bolt11: &str) -> Result<MeltEstimate, String> {
            self.quotes.push(bolt11.to_owned());
            let amount_sats = Self::amount_in(bolt11);
            let fee_reserve_sats = self.live_reserve(amount_sats);
            let quote_id = Self::pay_quote_id(bolt11);
            self.register(
                &quote_id,
                FakeQuote {
                    bolt11: bolt11.to_owned(),
                    state: MeltQuoteState::Unpaid,
                    amount_sats,
                    fee_reserve_sats,
                    expiry_unix: self.quote_expiry_unix,
                },
            );
            let (expected_fees_sats, expected_swap_fee_sats, expected_fees_note) =
                self.expected_fees(amount_sats.saturating_add(fee_reserve_sats));
            Ok(MeltEstimate {
                mint_url: "https://mint.example".to_owned(),
                quote_id,
                amount_sats,
                fee_reserve_sats,
                expiry_unix: self.quote_expiry_unix,
                expected_fees_sats,
                expected_swap_fee_sats,
                input_fee_ppk: self.input_fee_ppk,
                expected_fees_note,
            })
        }

        /// The WALLET half of the payment, BY QUOTE ID, in the shape of the shipped path
        /// (`wallet_ops::prepare_melt_payment_blocking` over CDK 0.17.2, as the verdict at 6fc77e1
        /// §4 read `prepare_melt`) — nothing has left the wallet; a refusal here is not a debit:
        /// the quote must be one this wallet raised; its STORED amount and reserve are re-checked
        /// against the ceiling; the proofs must cover amount + reserve (exact denominations are
        /// selected and HELD, as CDK reserves them); `prepare_melt` refuses a quote whose `expiry`
        /// has passed on the WALLET's clock, read now; then the prepared melt's TOTAL is bounded
        /// (addendum 8 §1.1). The prepared melt waits in `self.prepared` for the verdict.
        fn prepare_melt(
            &mut self,
            quote_id: &str,
            ceiling: &MeltCeiling,
        ) -> Result<PreparedMelt, MeltFailure> {
            let quote = match self.registered(quote_id) {
                Some(quote) => quote,
                None => {
                    // No registry: the quote is the one this Fake raised for the invoice its id
                    // names, with the reserve the payment quote carries.
                    let bolt11 = quote_id
                        .strip_prefix("paid-quote-")
                        .unwrap_or_else(|| {
                            panic!("the payer must pay the PAYMENT quote it raised, not {quote_id}")
                        })
                        .to_owned();
                    let amount_sats = Self::amount_in(&bolt11);
                    FakeQuote {
                        fee_reserve_sats: self.live_reserve(amount_sats),
                        bolt11,
                        state: MeltQuoteState::Unpaid,
                        amount_sats,
                        expiry_unix: self.quote_expiry_unix,
                    }
                }
            };
            if !ceiling.admits(quote.amount_sats, quote.fee_reserve_sats) {
                let reason = format!(
                    "melt refused before spending: mint https://mint.example quote {quote_id} would debit {} sats ({} sats invoice + {} sats fee reserve; planned invoice {} sats) against a ceiling of {} sats; nothing left the wallet",
                    quote.amount_sats.saturating_add(quote.fee_reserve_sats),
                    quote.amount_sats,
                    quote.fee_reserve_sats,
                    ceiling.invoice_sats,
                    ceiling.max_debit_sats
                );
                self.ceiling_refusals.push(reason.clone());
                return Err(MeltFailure::RefusedBeforeSpending(reason));
            }
            let need = quote.amount_sats.saturating_add(quote.fee_reserve_sats);
            // Selection and reservation, under the pool's lock (a shared pool is read and taken
            // from atomically, as the wallet's own store is).
            let layout = {
                let mut pool = self
                    .proofs
                    .as_ref()
                    .map(|proofs| proofs.lock().unwrap_or_else(|e| e.into_inner()));
                match layout(
                    self.input_fee_ppk,
                    pool.as_deref().map(|available| available.as_slice()),
                    need,
                ) {
                    Ok(layout) => {
                        if let Some(pool) = pool.as_mut() {
                            take(pool, &layout.picked);
                        }
                        layout
                    }
                    Err(reason) => {
                        drop(pool);
                        let reason =
                            format!("wallet refuses to prepare melt quote {quote_id}: {reason}");
                        self.pay_refusals.push(reason.clone());
                        return Err(MeltFailure::Failed(reason));
                    }
                }
            };
            let FakeLayout {
                picked: selected,
                input_fee_sats,
                swap_fee_sats,
                requires_swap,
            } = layout;
            // Fee-metadata drift knob: the PREPARED figure the SDK reports may differ from the
            // layout's; the actual fee on the swapped proofs (recomputed by the bound) does not.
            let input_fee_sats = self.prepared_input_override.unwrap_or(input_fee_sats);
            let prepare_now = self.now_unix();
            if u64::try_from(prepare_now).is_ok_and(|now| now > quote.expiry_unix) {
                // CDK wallet `initialize_melt`: `expiry > unix_time()` at prepare — the wallet's
                // clock, the LAST expiry check on the path; nothing after it looks at expiry.
                self.return_proofs(&selected);
                let reason = format!(
                    "wallet refuses to prepare melt quote {quote_id}: it expired at unix {} (now {prepare_now})",
                    quote.expiry_unix
                );
                self.pay_refusals.push(reason.clone());
                return Err(MeltFailure::Failed(reason));
            }
            // The one gate on the prepared figures (addendum 10 §1.1) — the SDK's figures through
            // the same `confirm_bound` the shipped path takes, same refusal wording.
            let total_debit_sats = match ceiling.admits_confirmable(
                quote.amount_sats,
                quote.fee_reserve_sats,
                Some(input_fee_sats),
                swap_fee_sats,
                self.input_fee_ppk,
                requires_swap,
            ) {
                Ok(bound) => bound.worst_debit_sats,
                Err(shortfall) => {
                    self.return_proofs(&selected);
                    let refusal = match shortfall {
                        ConfirmShortfall::DifferentInvoice {
                            invoice_sats,
                            planned_invoice_sats,
                        } => WalletOpsError::MeltExceedsCeiling {
                            mint_url: "https://mint.example".to_owned(),
                            quote_id: quote_id.to_owned(),
                            invoice_sats,
                            fee_reserve_sats: quote.fee_reserve_sats,
                            planned_invoice_sats,
                            max_debit_sats: ceiling.max_debit_sats,
                        },
                        ConfirmShortfall::TargetShort { bound, .. } => {
                            WalletOpsError::MeltWouldNotConfirm {
                                mint_url: "https://mint.example".to_owned(),
                                quote_id: quote_id.to_owned(),
                                invoice_sats: quote.amount_sats,
                                fee_reserve_sats: quote.fee_reserve_sats,
                                input_fee_sats,
                                actual_input_fee_sats: bound.actual_input_fee_sats,
                                target_sats: bound.target_sats,
                                swap_fee_sats,
                                input_fee_ppk: self.input_fee_ppk,
                            }
                        }
                        ConfirmShortfall::OverCeiling {
                            bound,
                            max_debit_sats,
                        } => WalletOpsError::MeltTotalExceedsCeiling {
                            mint_url: "https://mint.example".to_owned(),
                            quote_id: quote_id.to_owned(),
                            invoice_sats: quote.amount_sats,
                            fee_reserve_sats: quote.fee_reserve_sats,
                            input_fee_sats: bound.actual_input_fee_sats,
                            swap_fee_sats,
                            total_sats: bound.worst_debit_sats,
                            max_debit_sats,
                        },
                    };
                    let reason = refusal.to_string();
                    self.ceiling_refusals.push(reason.clone());
                    return Err(MeltFailure::RefusedBeforeSpending(reason));
                }
            };
            let token = self.next_token;
            self.next_token += 1;
            let preparation = MeltPreparation {
                mint_url: "https://mint.example".to_owned(),
                quote_id: quote_id.to_owned(),
                invoice_sats: quote.amount_sats,
                fee_reserve_sats: quote.fee_reserve_sats,
                input_fee_sats,
                swap_fee_sats,
                requires_swap,
                input_fee_ppk: self.input_fee_ppk,
                total_debit_sats,
                expiry_unix: quote.expiry_unix,
            };
            self.prepared.insert(
                token,
                FakePrepared {
                    quote_id: quote_id.to_owned(),
                    quote,
                    selected,
                    input_fee_sats,
                    swap_fee_sats,
                    requires_swap,
                },
            );
            Ok(PreparedMelt::fake(preparation, token))
        }

        /// `confirm`, in the shape of pinned CDK 0.17.2 `MeltSaga::request_melt_with_options`
        /// (`melt/saga/mod.rs:647–760`), then the MINT half (CDK mint `setup_melt`).
        ///
        /// Swap branch first (`requires_swap`, the positive-ppk layout): the selected proofs are
        /// POSTED to the mint's swap (`:678–697` → `swap_no_reserve`, `swap/saga/mod.rs:223–227`)
        /// for a target of invoice + reserve + the PREPARED input fee (`:678`); the mint keeps the
        /// swap fee — recomputed on the proofs actually sent (`swap/saga/mod.rs:108–145`), which
        /// under fixed fee metadata equals the prepared `swap_fee` — and hands back the target in
        /// its binary split (`swap/saga/mod.rs:285–301`) plus the rest as change
        /// (`swap/mod.rs:146–175`, `include_fees = false`). **The swap fee is charged at this point
        /// whether or not the melt after it succeeds.** Then the wallet RECOMPUTES the input fee on
        /// the proofs it now holds for the melt (`:704`) and refuses with `InsufficientFunds`
        /// (`:706–712`) when they do not cover invoice + reserve + that ACTUAL input fee: no melt
        /// is posted, the swapped proofs stay in the wallet, the swap fee is gone. Only then does
        /// the payer pause at `melt_gate` — after its last local check, before the melt request
        /// reaches the mint — and the mint accepts the request when the quote is UNPAID **or
        /// FAILED** — with NO expiry check, however long ago the quote expired — and rejects it
        /// when PENDING, PAID or UNKNOWN (the melt's proofs return to the wallet). Accepted ⇒ the
        /// debit is counted, the quote is PAID for everyone reading the registry, and the mint
        /// returns as change what the proofs exceeded invoice + its Lightning fee + the actual
        /// input fee by. `fee_sats` is CDK `FinalizedMelt::fee_paid` (`melt/saga/mod.rs:139–148`):
        /// proofs sent − invoice − change returned = Lightning fee + ACTUAL input fee, inclusive; it
        /// does not contain the swap fee. The scripted `(paid, fee)` is the mint's (invoice,
        /// Lightning fee) — the Lightning fee alone, never above the reserve.
        ///
        /// Exact-fit branch (a fee-free mint, every test written before addendum 8): no swap, the
        /// established conservative model stands — the exact set for amount + reserve is spent
        /// whole and the unused reserve is NOT returned (overstating the loss, never understating
        /// it), so the two-process tests' proof-set assertions hold.
        fn confirm_melt(&mut self, prepared: PreparedMelt) -> Result<MeltOutcome, MeltFailure> {
            let token = prepared
                .fake_token()
                .expect("the Fake confirms only melts it prepared");
            let FakePrepared {
                quote_id,
                quote,
                selected,
                input_fee_sats,
                swap_fee_sats: prepared_swap_fee_sats,
                requires_swap,
            } = self
                .prepared
                .remove(&token)
                .expect("a prepared melt is confirmed or cancelled once");
            let quote_id = quote_id.as_str();
            let ppk = self.input_fee_ppk;
            // What the melt request will carry, and what the swap (if any) charged.
            let (melt_proofs, melt_total_sats, actual_input_fee_sats, swap_fee_sats) =
                if requires_swap {
                    let target_sats = quote.amount_sats + quote.fee_reserve_sats + input_fee_sats;
                    // `swap_fee` recomputed on the proofs actually sent; `selected` is empty only for
                    // a pool-less Fake, whose one notional proof pays the prepared figure.
                    let swap_fee_sats = if selected.is_empty() {
                        prepared_swap_fee_sats
                    } else {
                        fee_for(ppk, selected.len())
                    };
                    let sent_sats = selected.iter().sum::<u64>();
                    if !selected.is_empty() && sent_sats < target_sats + swap_fee_sats {
                        // CDK `swap/mod.rs:146–157`: `checked_sub` refuses BEFORE the swap POST.
                        // Unreachable from `layout` (its selection covers target + own fee); kept
                        // so the fake never fabricates value.
                        self.return_proofs(&selected);
                        let reason = format!(
                            "wallet refuses to swap for melt quote {quote_id}: {sent_sats} sats of proofs do not cover the {target_sats} sats target plus {swap_fee_sats} sats swap fee"
                        );
                        self.pay_refusals.push(reason.clone());
                        return Err(MeltFailure::Failed(reason));
                    }
                    let received = binary_split(target_sats);
                    let change = if selected.is_empty() {
                        Vec::new()
                    } else {
                        binary_split(sent_sats - target_sats - swap_fee_sats)
                    };
                    // The swap is POSTED: the selected proofs are spent at the mint, the swap fee is
                    // gone, the target and the change are the wallet's new proofs.
                    self.swaps.push(FakeSwap {
                        quote_id: quote_id.to_owned(),
                        sent: selected.clone(),
                        target_sats,
                        swap_fee_sats,
                        received: received.clone(),
                        change: change.clone(),
                    });
                    self.proofs_spent.push(selected);
                    self.return_proofs(&change);
                    // `:704–712`: the ACTUAL input fee on the proofs the melt will send.
                    let actual_input_fee_sats = fee_for(ppk, received.len());
                    let needed_sats =
                        quote.amount_sats + quote.fee_reserve_sats + actual_input_fee_sats;
                    if target_sats < needed_sats {
                        // `InsufficientFunds` after the swap: compensations revert proof STATES
                        // (the swapped proofs stay spendable in the wallet); nothing reaches the
                        // melt endpoint; the swap fee is not refunded.
                        self.return_proofs(&received);
                        let reason = format!(
                            "wallet refuses to melt quote {quote_id} after its swap: {target_sats} sats of proofs ({received:?}) do not cover {} sats invoice + {} sats fee reserve + {actual_input_fee_sats} sats actual proof input fee (prepared estimate {input_fee_sats} sats); the {swap_fee_sats} sats swap fee was charged",
                            quote.amount_sats, quote.fee_reserve_sats
                        );
                        self.pay_refusals.push(reason.clone());
                        return Err(MeltFailure::Failed(reason));
                    }
                    (received, target_sats, actual_input_fee_sats, swap_fee_sats)
                } else {
                    let total = selected.iter().sum::<u64>();
                    (selected, total, 0, 0)
                };
            if let Some(gate) = &self.melt_gate {
                gate.arrive_and_wait();
            }
            // The quote's state as the mint holds it NOW (the registry), not as the wallet loaded
            // it before the pause: another process may have moved it meanwhile.
            let state_at_mint = self
                .registered(quote_id)
                .map(|current| current.state)
                .unwrap_or(quote.state);
            if !matches!(
                state_at_mint,
                MeltQuoteState::Unpaid | MeltQuoteState::Failed
            ) {
                self.return_proofs(&melt_proofs);
                let reason = format!(
                    "mint https://mint.example refuses to pay melt quote {quote_id}: it is {state_at_mint}"
                );
                self.pay_refusals.push(reason.clone());
                return Err(MeltFailure::Failed(reason));
            }
            self.melts.push(quote.bolt11.clone());
            if let Some(counter) = &self.melt_counter {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            let (paid, lightning_fee) = self.melt_results.remove(0).map_err(MeltFailure::Failed)?;
            // Swap branch: the mint consumes the melt's proofs, pays the invoice, keeps its
            // Lightning fee and the actual input fee, and returns the rest as change (CDK
            // `melt/saga/mod.rs:136–148`: `fee_paid` = proofs − invoice − change). Exact-fit
            // branch: no change is modelled (see above); `fee_paid` is the scripted Lightning fee,
            // the input fee being 0 on a fee-free mint.
            let fee_paid_sats = if requires_swap {
                let change_sats = melt_total_sats
                    .saturating_sub(paid)
                    .saturating_sub(lightning_fee)
                    .saturating_sub(actual_input_fee_sats);
                if self.proofs.is_some() && change_sats > 0 {
                    self.return_proofs(&binary_split(change_sats));
                }
                melt_total_sats
                    .saturating_sub(paid)
                    .saturating_sub(change_sats)
            } else {
                self.proofs_spent.push(melt_proofs);
                lightning_fee
            };
            if let Some(registry) = &self.registry
                && let Some(entry) = registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(quote_id)
            {
                entry.state = MeltQuoteState::Paid;
            }
            Ok(MeltOutcome {
                mint_url: "https://mint.example".to_owned(),
                paid_sats: paid,
                fee_sats: fee_paid_sats,
                balance_sats: 1_000,
                balance_after_sats: if self.balance_read_fails {
                    None
                } else {
                    Some(1_000)
                },
                quote_id: quote_id.to_owned(),
                fee_reserve_sats: quote.fee_reserve_sats,
                input_fee_sats,
                swap_fee_sats,
            })
        }

        /// Release a prepared melt: its held proofs go back to the pool; nothing was posted.
        fn cancel_melt(&mut self, prepared: PreparedMelt) -> Result<(), MeltFailure> {
            let token = prepared
                .fake_token()
                .expect("the Fake cancels only melts it prepared");
            let FakePrepared {
                quote_id, selected, ..
            } = self
                .prepared
                .remove(&token)
                .expect("a prepared melt is confirmed or cancelled once");
            self.return_proofs(&selected);
            self.cancels.push(quote_id);
            Ok(())
        }

        /// By invoice: the most alive of the quotes raised for it (registry), else the script.
        fn melt_status(&mut self, bolt11: &str) -> Result<Option<MeltQuoteStatus>, String> {
            self.status_calls.push(bolt11.to_owned());
            let Some(registry) = &self.registry else {
                return self.status.clone();
            };
            let now = self.now_unix();
            let registry = registry.lock().unwrap_or_else(|e| e.into_inner());
            Ok(registry
                .iter()
                .filter(|(_, quote)| quote.bolt11 == bolt11)
                .min_by_key(|(_, quote)| Self::liveness(quote, now))
                .map(|(quote_id, quote)| Self::status_of(quote_id, quote)))
        }

        /// By id: exactly that quote (registry), else the script.
        fn melt_status_for_quote(
            &mut self,
            quote_id: &str,
        ) -> Result<Option<MeltQuoteStatus>, String> {
            self.quote_status_calls.push(quote_id.to_owned());
            if self.registry.is_none() {
                return self.status.clone();
            }
            Ok(self
                .registered(quote_id)
                .map(|quote| Self::status_of(quote_id, &quote)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use super::test_support::{
        Fake, FakeLayout, Gate, QuoteRegistry, fake_proofs, fee_for, layout, quote_registry,
    };
    use super::*;
    use crate::seller_node::STATE_DB_FILE;
    use crate::seller_node::store::{ReceiptFees, RemittanceState};
    use crate::wallet_ops::post_swap_figures;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Addendum 10 §2: a pre-fence arithmetic refusal is ONE line and writes nothing to the row —
    /// it stays Planned, unbound, with its receipts pinned and its planned quote on it; nothing of
    /// the old stanza ("A seller never pays…", "released … back to unremitted") follows.
    fn assert_refused_before_fence_row_stays_planned(
        out: &str,
        row: &FeeRemittance,
        gross: u64,
        planned_quote_id: &str,
    ) {
        let refusals: Vec<&str> = out
            .lines()
            .filter(|line| line.starts_with("REFUSED before spending — "))
            .collect();
        assert_eq!(refusals.len(), 1, "exactly one refusal line:\n{out}");
        assert!(
            refusals[0].ends_with(&format!(
                "; ceiling {gross} sats; nothing left the wallet; the row stays planned and the next attempt re-quotes."
            )),
            "{}",
            refusals[0]
        );
        assert_eq!(
            out.matches("REFUSED before spending").count(),
            1,
            "the refusal is stated once:\n{out}"
        );
        assert!(
            !out.contains("A seller never pays more than it accrued")
                && !out.contains("back to unremitted"),
            "nothing of the old stanza follows the one line:\n{out}"
        );
        assert_eq!(
            row.state,
            RemittanceState::Planned,
            "the row is not written"
        );
        assert_eq!(row.spending_since_unix, None, "never admitted");
        assert_eq!(row.spending_quote_id, None, "no quote bound");
        assert_eq!(row.gross_sats, gross);
        assert_eq!(row.receipts, 2, "receipts stay pinned to the planned row");
        assert_eq!(row.melt_quote_id.as_deref(), Some(planned_quote_id));
    }

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "maxplayer-fee-remit-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        root
    }

    fn store_with_fees(label: &str, fees: &[u64]) -> (SellerStore, PathBuf) {
        let root = temp_home(label);
        let store = SellerStore::open(root.join(STATE_DB_FILE)).expect("open store");
        for (index, fee) in fees.iter().enumerate() {
            store
                .collect_receipt(
                    &format!("receipt-{index}"),
                    &format!("job-{index}"),
                    fee * 10,
                    ReceiptFees {
                        mint_fee_sats: 1,
                        fee_bps: 1000,
                        fee_sats: *fee,
                    },
                    index as i64 + 1,
                )
                .expect("collect");
        }
        (store, root)
    }

    fn run_remit(
        store: &SellerStore,
        fake: &mut Fake,
        trigger: RemitTrigger,
        now: i64,
    ) -> (RemitOutcome, String) {
        // The fence reads the clock fresh; a single-process test's clock is the time it runs at.
        fake.set_clock(now);
        let mut out = Vec::new();
        let outcome = remit(store, fake, trigger, now, &mut out).expect("remit runs");
        (outcome, String::from_utf8(out).expect("utf8"))
    }

    fn is_paid(outcome: &RemitOutcome) -> bool {
        matches!(outcome, RemitOutcome::Paid { .. })
    }

    // §3.2: no flag ⇒ dry run. It resolves, quotes, prints every figure, and MOVES NOTHING: no melt,
    // no journal row, no attempt row, the unremitted balance untouched.
    #[test]
    fn dry_run_prints_the_plan_and_moves_nothing() {
        let (store, root) = store_with_fees("dry-run", &[10, 5]);
        let mut fake = Fake::new(|_| 2);
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::DryRun, 100);
        assert_eq!(outcome, RemitOutcome::DryRun, "{out}");
        for needle in [
            "Recent attempts: none journaled yet",
            "Accrued platform fee: 15 sats all-time — 0 sats remitted, 15 sats unremitted",
            "Destination: maxplayer@strike.me (LNURL-pay; accepts 1 to 1000000 sats)",
            "unremitted platform fee (gross): 15 sats",
            "mint melt fee reserve (bounds the Lightning fee): 2 sats — taken out of the gross, never on top",
            "invoice amount (maxplayer@strike.me receives): 13 sats",
            "leaves your wallet: at most 15 sats (≤ 15)",
            "mint: https://mint.example (melt quote quote-lnbc-fake-13-2)",
            "invoice payment hash: hash-13-2",
            "DRY RUN — nothing moved. Re-run with --confirm to pay 13 sats to maxplayer@strike.me.",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
        assert!(fake.melts.is_empty(), "a dry run never melts");
        assert_eq!(
            fake.invoices,
            vec![15, 13],
            "probe on the gross, then invoice the net"
        );
        assert_eq!(fake.estimates.len(), 2);
        assert!(
            store.remittances().expect("rows").is_empty(),
            "a dry run journals nothing"
        );
        assert!(
            store
                .recent_remit_attempts(10)
                .expect("attempts")
                .is_empty(),
            "a dry run is not an attempt"
        );
        assert_eq!(store.accrued_fees().expect("read").unremitted_fee_sats, 15);
        let _ = std::fs::remove_dir_all(&root);
    }

    // §3.2 + §3.3 + §3.4: `--confirm` pays ONCE, the fee comes out of the gross, the settlement is
    // journaled with gross / melt fee / net / destination literal / payment hash / quote id, the
    // receipts are discharged, the attempt is journaled PAID — and a second `--confirm` pays nothing.
    #[test]
    fn confirm_pays_once_takes_the_fee_out_of_the_gross_and_is_idempotent() {
        let (store, root) = store_with_fees("confirm", &[10, 5]);
        let mut fake = Fake::new(|_| 2);
        fake.melt_results = vec![Ok((13, 1))];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        assert_eq!(
            outcome,
            RemitOutcome::Paid {
                remittance_id: "hash-13-2".to_owned(),
                net_sats: 13,
                melt_fee_sats: 1,
            },
            "{out}"
        );
        assert_eq!(
            fake.melts,
            vec!["lnbc-fake-13-2".to_owned()],
            "exactly one melt, of the NET invoice"
        );
        for needle in [
            "Journaled remittance hash-13-2 covering 2 receipts (owner fake-owner, lease until unix 400); paying...",
            "PAID — remittance hash-13-2 settled",
            "gross discharged: 15 sats",
            "melt fee taken by the mint: 1 sats",
            "net paid to maxplayer@strike.me: 13 sats",
            "stays in your wallet (unused reserve): 1 sats",
            "wallet balance now: 1000 sats at https://mint.example",
            "receipts discharged: 2",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
        assert!(!out.contains("WARNING"), "{out}");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.state, RemittanceState::Settled);
        assert_eq!(row.remittance_id, "hash-13-2");
        assert_eq!(row.payment_hash, "hash-13-2");
        assert_eq!(row.bolt11, "lnbc-fake-13-2");
        assert_eq!(
            (row.gross_sats, row.melt_fee_sats, row.net_sats),
            (15, Some(1), 13)
        );
        assert_eq!(
            row.destination, "maxplayer@strike.me",
            "the literal paid is journaled"
        );
        assert_eq!(
            row.melt_quote_id,
            Some("paid-quote-lnbc-fake-13-2".to_owned())
        );
        assert_eq!((row.created_at_unix, row.settled_at_unix), (100, Some(100)));
        assert_eq!(row.receipts, 2);
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(accrued.unremitted_fee_sats, 0);
        assert_eq!(accrued.remitted_fee_sats, 15);
        assert!(
            accrued
                .by_job
                .iter()
                .all(|r| r.remittance_id.as_deref() == Some("hash-13-2"))
        );
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            (
                attempts[0].trigger,
                attempts[0].outcome,
                attempts[0].unremitted_sats,
                attempts[0].remittance_id.as_deref(),
                attempts[0].started_at_unix,
            ),
            (
                RemitAttemptTrigger::Command,
                RemitAttemptOutcome::Paid,
                15,
                Some("hash-13-2"),
                100
            )
        );
        assert_eq!(
            attempts[0].detail,
            "paid 13 sats to maxplayer@strike.me (melt fee 1 sats)"
        );

        // Idempotent: a second --confirm finds nothing unremitted, touches no network, pays nothing,
        // and — a threshold refusal — journals no attempt.
        let pay_requests_before = fake.pay_requests;
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 101);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::NothingUnremitted),
            "{out}"
        );
        assert!(
            out.contains("Nothing to remit. REFUSED — nothing moved."),
            "{out}"
        );
        assert!(
            out.contains("unix 100: operator (--confirm) attempt saw 15 sats unremitted — PAID: paid 13 sats to maxplayer@strike.me (melt fee 1 sats) [remittance hash-13-2]"),
            "the command prints the journaled attempts:\n{out}"
        );
        assert_eq!(
            fake.pay_requests, pay_requests_before,
            "no LNURL round trip"
        );
        assert_eq!(fake.melts.len(), 1, "still exactly one melt, ever");
        assert_eq!(store.remittances().expect("rows").len(), 1);
        assert_eq!(store.recent_remit_attempts(10).expect("attempts").len(), 1);

        // A new receipt after the settlement is the only thing the next remittance covers.
        store
            .collect_receipt(
                "receipt-late",
                "job-late",
                200,
                ReceiptFees {
                    mint_fee_sats: 2,
                    fee_bps: 1000,
                    fee_sats: 20,
                },
                102,
            )
            .expect("collect late");
        fake.melt_results = vec![Ok((18, 2))];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 103);
        assert!(is_paid(&outcome), "{out}");
        assert!(out.contains("gross discharged: 20 sats"), "{out}");
        assert_eq!(fake.melts.len(), 2);
        assert_eq!(store.accrued_fees().expect("read").remitted_fee_sats, 35);
        let _ = std::fs::remove_dir_all(&root);
    }

    // The mint's melt fee is zero (some mints charge none): one invoice, one quote, the whole gross
    // goes to the destination.
    #[test]
    fn a_zero_fee_reserve_invoices_the_gross_once() {
        let (store, root) = store_with_fees("zero-reserve", &[10]);
        let mut fake = Fake::new(|_| 0);
        fake.melt_results = vec![Ok((10, 0))];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        assert!(is_paid(&outcome), "{out}");
        assert_eq!(
            fake.invoices,
            vec![10],
            "no second invoice when the reserve is zero"
        );
        assert_eq!(fake.melts, vec!["lnbc-fake-10-1".to_owned()]);
        assert!(
            out.contains("net paid to maxplayer@strike.me: 10 sats"),
            "{out}"
        );
        assert!(
            out.contains("stays in your wallet (unused reserve): 0 sats"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // §3.2 / addendum rule 4: below the resolved minSendable ⇒ refuse with the shortfall printed, no
    // invoice requested, nothing journaled — not a remittance row, not an attempt row. This is the
    // steady state for small sellers, not an error, from every trigger.
    #[test]
    fn below_the_resolved_minimum_refuses_with_the_shortfall_and_journals_nothing() {
        // Two 10-sat jobs at 10% owe 1 sat each ⇒ gross 2; the destination wants 5000 msat = 5 sats.
        let (store, root) = store_with_fees("below-min", &[1, 1]);
        let mut fake = Fake::new(|_| 0);
        fake.min_msat = 5000;
        for trigger in [
            RemitTrigger::DryRun,
            RemitTrigger::Command,
            RemitTrigger::Collect,
        ] {
            let (outcome, out) = run_remit(&store, &mut fake, trigger, 100);
            assert_eq!(
                outcome,
                RemitOutcome::Refused(Refusal::BelowMinimum {
                    unremitted: 2,
                    min_sats: 5,
                }),
                "{out}"
            );
            assert!(
                out.contains("REFUSED — unremitted 2 sats is below the destination's minimum of 5 sats (3 sats short)."),
                "{out}"
            );
            assert!(
                out.contains("accumulates until it clears the minimum"),
                "{out}"
            );
        }
        assert!(
            fake.invoices.is_empty(),
            "no invoice is requested below the minimum"
        );
        assert!(fake.melts.is_empty());
        assert!(store.remittances().expect("rows").is_empty());
        assert!(
            store
                .recent_remit_attempts(10)
                .expect("attempts")
                .is_empty(),
            "the threshold is the steady state, not an attempt"
        );
        assert_eq!(store.accrued_fees().expect("read").unremitted_fee_sats, 2);

        // A minimum that is not a whole sat rounds UP: 1500 msat ⇒ 2 sats; gross 2 clears it, gross 1 does not.
        fake.min_msat = 1500;
        fake.melt_results = vec![Ok((2, 0))];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 101);
        assert!(is_paid(&outcome), "{out}");
        let (store1, root1) = store_with_fees("below-min-1", &[1]);
        let (outcome, out) = run_remit(&store1, &mut fake, RemitTrigger::Command, 102);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::BelowMinimum {
                unremitted: 1,
                min_sats: 2,
            }),
            "{out}"
        );
        assert!(
            out.contains("below the destination's minimum of 2 sats (1 sats short)"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root1);
    }

    // §3.3: the fee reserve is taken OUT of the gross — and when what is left is below the minimum,
    // or nothing at all, the attempt refuses rather than paying more than the seller accrued. These
    // refusals ARE journaled: an operator should see a mint whose fees eat the fee.
    #[test]
    fn a_fee_reserve_that_leaves_too_little_or_nothing_is_refused_and_journaled() {
        // Gross 3, reserve 2 ⇒ net 1, below a 2-sat minimum.
        let (store, root) = store_with_fees("reserve-below-min", &[3]);
        let mut fake = Fake::new(|_| 2);
        fake.min_msat = 2000;
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::ReserveDoesNotFit {
                gross: 3,
                reserve: 2,
            }),
            "{out}"
        );
        assert!(
            out.contains("REFUSED — after the mint's melt fee reserve (2 sats) the 3 sats unremitted leaves 1 sats, below the destination's minimum of 2 sats (1 sats short)."),
            "{out}"
        );
        assert_eq!(fake.invoices, vec![3], "only the probe was requested");
        assert!(fake.melts.is_empty());
        assert!(store.remittances().expect("rows").is_empty());
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Refused);
        assert_eq!(attempts[0].unremitted_sats, 3);
        assert_eq!(
            attempts[0].detail,
            "the mint's melt fee reserve (2 sats) does not fit inside the 3 sats accrued"
        );
        assert_eq!(attempts[0].remittance_id, None);

        // Gross 2, reserve 2 ⇒ nothing left.
        let (store2, root2) = store_with_fees("reserve-eats-all", &[2]);
        let mut fake = Fake::new(|_| 2);
        let (outcome, out) = run_remit(&store2, &mut fake, RemitTrigger::Command, 100);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::ReserveDoesNotFit {
                gross: 2,
                reserve: 2,
            }),
            "{out}"
        );
        assert!(
            out.contains("needs a melt fee reserve of 2 sats to pay 2 sats, which leaves nothing for the destination"),
            "{out}"
        );
        assert!(fake.melts.is_empty());

        // A reserve that GROWS on the smaller invoice (non-monotone mint) so net + reserve > gross
        // is refused: the seller would pay more than it accrued.
        let (store3, root3) = store_with_fees("reserve-non-monotone", &[15]);
        let mut fake = Fake::new(|amount| if amount == 15 { 2 } else { 3 });
        let (outcome, out) = run_remit(&store3, &mut fake, RemitTrigger::Command, 100);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::ReserveDoesNotFit {
                gross: 15,
                reserve: 3,
            }),
            "{out}"
        );
        assert!(
            out.contains("quotes a 3 sats fee reserve on 13 sats, so up to 16 sats would leave the wallet against 15 sats accrued"),
            "{out}"
        );
        assert_eq!(fake.invoices, vec![15, 13]);
        assert!(fake.melts.is_empty());
        assert!(store3.remittances().expect("rows").is_empty());
        for root in [root, root2, root3] {
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    // Above the resolved maxSendable: refuse (whole balance or nothing), name the bound.
    #[test]
    fn above_the_resolved_maximum_is_refused() {
        let (store, root) = store_with_fees("above-max", &[50]);
        let mut fake = Fake::new(|_| 0);
        fake.max_msat = 20_000;
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::AboveMaximum {
                unremitted: 50,
                max_sats: 20,
            }),
            "{out}"
        );
        assert!(
            out.contains("exceeds the destination's maximum of 20 sats"),
            "{out}"
        );
        assert!(fake.invoices.is_empty() && fake.melts.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    // §3.4: an interrupted remittance is RECOVERABLE, not repeatable. The melt errors after the plan
    // was journaled and the fence admitted it: the row stays SPENDING (addendum 4 §1) and the
    // attempt is journaled FAILED naming it. The next run asks the mint — PAID ⇒ settled with no
    // second melt; the receipts stay discharged.
    #[test]
    fn interrupted_after_the_plan_is_reconciled_as_paid_without_a_second_melt() {
        let (store, root) = store_with_fees("interrupted-paid", &[10]);
        let mut fake = Fake::new(|_| 1);
        fake.melt_results = vec![Err("connection reset during confirm".to_owned())];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Collect, 100);
        assert_eq!(
            outcome,
            RemitOutcome::MeltFailed {
                remittance_id: "hash-9-2".to_owned(),
                error: "connection reset during confirm".to_owned(),
            },
            "{out}"
        );
        assert!(
            out.contains("melt failed: connection reset during confirm"),
            "{out}"
        );
        assert!(
            out.contains("Admitted to spend at unix 100: remittance hash-9-2 is now spending, bound to melt quote paid-quote-lnbc-fake-9-2"),
            "{out}"
        );
        assert!(
            out.contains(
                "remittance hash-9-2 stays journaled as spending, bound to melt quote paid-quote-lnbc-fake-9-2: proofs may have reached the mint"
            ),
            "{out}"
        );
        assert!(
            !out.contains("Recent attempts"),
            "the collect path does not print the journal into the node log:\n{out}"
        );
        let in_flight = store
            .in_flight_remittance()
            .expect("query")
            .expect("a spending row");
        assert_eq!(in_flight.state, RemittanceState::Spending);
        assert_eq!(in_flight.spending_since_unix, Some(100));
        assert_eq!(in_flight.bolt11, "lnbc-fake-9-2");
        assert_eq!(fake.admitted_seen.len(), 1);
        assert_eq!(fake.admitted_seen[0].state, RemittanceState::Spending);
        assert_eq!(store.accrued_fees().expect("read").in_flight_fee_sats, 10);
        assert_eq!(fake.melts.len(), 1);
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            (
                attempts[0].trigger,
                attempts[0].outcome,
                attempts[0].unremitted_sats,
                attempts[0].remittance_id.as_deref()
            ),
            (
                RemitAttemptTrigger::Collect,
                RemitAttemptOutcome::Failed,
                10,
                Some("hash-9-2")
            )
        );
        assert_eq!(
            attempts[0].detail,
            "melt failed: connection reset during confirm"
        );

        // Next run, the mint says PAID: settle, keep the receipts discharged, then find nothing left.
        fake.status = Ok(Some(MeltQuoteStatus {
            mint_url: "https://mint.example".to_owned(),
            quote_id: "paid-quote-lnbc-fake-9-2".to_owned(),
            state: MeltQuoteState::Paid,
            amount_sats: 9,
            fee_reserve_sats: 1,
            expiry_unix: u64::MAX,
        }));
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 101);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::NothingUnremitted),
            "{out}"
        );
        assert!(
            out.contains("unix 100: automatic (after collect) attempt saw 10 sats unremitted — FAILED: melt failed: connection reset during confirm [remittance hash-9-2]"),
            "{out}"
        );
        assert!(
            out.contains("Reconciling in-flight remittance hash-9-2 (planned at unix 100 by fake-owner, lease until unix 400: 9 sats to maxplayer@strike.me, gross 10 sats)"),
            "{out}"
        );
        // Addendum 10 §3: the reserve bounds the LIGHTNING fee, not the inclusive melt fee (which
        // includes the actual proof input fee and was not observed on a quote paid by another run).
        assert!(
            out.contains("reports melt quote paid-quote-lnbc-fake-9-2 PAID — recorded as settled by reconciliation: 9 sats reached maxplayer@strike.me; Lightning fee at most 1 sats (the quote's reserve); the inclusive melt fee (Lightning + actual proof input fee) is recorded as not observed — the mint reports PAID, not what it kept"),
            "{out}"
        );
        assert!(
            !out.contains("melt fee at most"),
            "the reserve is not presented as a bound on the inclusive melt fee:\n{out}"
        );
        assert!(out.contains("Nothing to remit."), "{out}");
        // A spending row is reconciled by its BOUND quote, by id — never by the invoice.
        assert_eq!(
            fake.quote_status_calls,
            vec!["paid-quote-lnbc-fake-9-2".to_owned()]
        );
        assert!(fake.status_calls.is_empty());
        assert_eq!(fake.melts.len(), 1, "reconciliation never melts");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!(rows[0].melt_fee_sats, None, "unobserved, not invented");
        assert_eq!(
            rows[0].settled_by,
            Some(RemittanceState::Settled)
                .map(|_| crate::seller_node::store::SettledBy::Reconciliation),
            "the row says it was settled by reconciliation"
        );
        assert_eq!(
            rows[0].melt_fee_reserve_sats,
            Some(1),
            "the paying quote's reserve — the fee's ceiling — is recorded"
        );
        assert_eq!(rows[0].net_sats, 9);
        assert_eq!(
            rows[0].melt_quote_id,
            Some("paid-quote-lnbc-fake-9-2".to_owned())
        );
        assert_eq!(rows[0].settled_at_unix, Some(101));
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.unremitted_fee_sats,
                accrued.in_flight_fee_sats
            ),
            (10, 0, 0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The other reconciliation outcomes for a SPENDING row (the melt was admitted, then errored):
    // PENDING ⇒ refuse this run, keep the row, melt nothing; UNPAID with a LIVE quote ⇒ HOLD too —
    // even our own row, even long after the lease: the melt that errored may have reached the mint
    // (addendum 4 §1.2), so only the mint's verdict resolves a spending row; UNPAID with the quote
    // EXPIRED, however long ago, or FAILED, or no quote known ⇒ HOLD too (addendum 6 §1.2: the
    // mint pays an expired UNPAID or a FAILED quote, so neither is cancellation); the ONE exit is
    // the mint reporting the bound quote PAID, which settles the row by reconciliation.
    #[test]
    fn an_unpaid_or_pending_interrupted_attempt_is_reconciled_without_paying_twice() {
        let (store, root) = store_with_fees("interrupted-unpaid", &[10]);
        let mut fake = Fake::new(|_| 1);
        fake.melt_results = vec![Err("insufficient funds for melt".to_owned())];
        let (outcome, _) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        assert!(matches!(outcome, RemitOutcome::MeltFailed { .. }));

        // PENDING: refuse, keep the planned row, no melt, no new invoice; journaled as a refusal
        // naming the row.
        fake.status = Ok(Some(MeltQuoteStatus {
            mint_url: "https://mint.example".to_owned(),
            quote_id: "q-pending".to_owned(),
            state: MeltQuoteState::Pending,
            amount_sats: 9,
            fee_reserve_sats: 1,
            expiry_unix: u64::MAX,
        }));
        let invoices_before = fake.invoices.len();
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Collect, 101);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::SpendingHeld {
                remittance_id: "hash-9-2".to_owned(),
                owner: "fake-owner".to_owned(),
                spending_since_unix: 100,
                quote_id: Some("paid-quote-lnbc-fake-9-2".to_owned()),
                observed: format!(
                    "mint https://mint.example reports melt quote q-pending PENDING (expiry unix {})",
                    u64::MAX
                ),
                held_sats: 10,
            }),
            "PENDING on a bound spending row is the same HELD refusal as every other non-PAID answer (addendum 7 §2): {out}"
        );
        assert!(
            out.contains(
                "HELD: remittance hash-9-2 is SPENDING (admitted by fake-owner at unix 100), bound to melt quote paid-quote-lnbc-fake-9-2; mint https://mint.example reports melt quote q-pending PENDING"
            ) && out.contains("10 sats of receipts stay pinned to it")
                && out.contains("REFUSED — nothing moved by this run"),
            "{out}"
        );
        assert_eq!(
            out.lines()
                .filter(|line| line.starts_with("  HELD: remittance hash-9-2"))
                .count(),
            1,
            "exactly one HELD line: {out}"
        );
        assert_eq!(
            store
                .in_flight_remittance()
                .expect("query")
                .expect("the row stays in flight")
                .state,
            RemittanceState::Spending
        );
        assert_eq!(fake.melts.len(), 1);
        assert_eq!(
            fake.invoices.len(),
            invoices_before,
            "no new invoice while pending"
        );
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 2, "the failed melt and the pending refusal");
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Refused);
        assert_eq!(attempts[0].remittance_id.as_deref(), Some("hash-9-2"));
        assert!(
            attempts[0].detail.starts_with(
                "HELD: remittance hash-9-2 is SPENDING (admitted by fake-owner at unix 100), bound to melt quote paid-quote-lnbc-fake-9-2; mint https://mint.example reports melt quote q-pending PENDING"
            ),
            "the journal carries the same HELD line the operator saw: {}",
            attempts[0].detail
        );

        // UNPAID with a LIVE quote (expires at unix 2000): HOLD — our own row, and the lease (until
        // 400) is long gone at 1000, and neither matters: the row is SPENDING. Nothing released, no
        // new invoice, journaled as a refusal naming the row. The status is the BOUND quote's.
        fake.status = Ok(Some(MeltQuoteStatus {
            mint_url: "https://mint.example".to_owned(),
            quote_id: "paid-quote-lnbc-fake-9-2".to_owned(),
            state: MeltQuoteState::Unpaid,
            amount_sats: 9,
            fee_reserve_sats: 1,
            expiry_unix: 2000,
        }));
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 1000);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::SpendingHeld {
                remittance_id: "hash-9-2".to_owned(),
                owner: "fake-owner".to_owned(),
                spending_since_unix: 100,
                quote_id: Some("paid-quote-lnbc-fake-9-2".to_owned()),
                observed: "mint https://mint.example reports melt quote paid-quote-lnbc-fake-9-2 UNPAID (expiry unix 2000)".to_owned(),
                held_sats: 10,
            }),
            "{out}"
        );
        assert!(
            out.contains("HELD: remittance hash-9-2 is SPENDING (admitted by fake-owner at unix 100), bound to melt quote paid-quote-lnbc-fake-9-2; mint https://mint.example reports melt quote paid-quote-lnbc-fake-9-2 UNPAID (expiry unix 2000); 10 sats of receipts stay pinned to it — a spending row is released by nobody and on no clock; it settles only when the mint reports that quote PAID; an operator decision, not a timeout, resolves it. REFUSED — nothing moved by this run"),
            "{out}"
        );
        assert_eq!(fake.melts.len(), 1);
        assert_eq!(
            fake.invoices.len(),
            invoices_before,
            "no new invoice on a held row"
        );
        assert_eq!(
            store
                .in_flight_remittance()
                .expect("query")
                .expect("held")
                .state,
            RemittanceState::Spending
        );
        assert_eq!(store.accrued_fees().expect("read").in_flight_fee_sats, 10);

        // UNPAID and the bound quote has EXPIRED at the mint (2000): still HOLD, at 2001, past the
        // margin at 2061, and ten thousand seconds on (addendum 6 §1.2) — the mint pays an expired
        // UNPAID quote, so no clock makes the receipts payable again. No fresh plan, no dry run of
        // a new invoice, the binding kept, the melt count unchanged.
        for (trigger, now) in [
            (RemitTrigger::DryRun, 2001),
            (RemitTrigger::DryRun, 2061),
            (RemitTrigger::Command, 12_000),
        ] {
            let (outcome, out) = run_remit(&store, &mut fake, trigger, now);
            assert!(
                matches!(outcome, RemitOutcome::Refused(Refusal::SpendingHeld { .. })),
                "at {now}: {out}"
            );
            assert!(
                out.contains("HELD: remittance hash-9-2 is SPENDING (admitted by fake-owner at unix 100), bound to melt quote paid-quote-lnbc-fake-9-2; mint https://mint.example reports melt quote paid-quote-lnbc-fake-9-2 UNPAID (expiry unix 2000); 10 sats of receipts stay pinned to it"),
                "at {now}: {out}"
            );
            assert!(
                !out.contains("DRY RUN"),
                "no fresh plan on a held row: {out}"
            );
            assert_eq!(
                out.lines()
                    .filter(|line| line.starts_with("  HELD: remittance"))
                    .count(),
                1,
                "exactly one HELD line per run (addendum 6 §1.3): {out}"
            );
            let rows = store.remittances().expect("rows");
            assert_eq!(rows.len(), 1, "at {now}");
            assert_eq!(rows[0].state, RemittanceState::Spending, "at {now}");
            assert_eq!(
                rows[0].spending_quote_id.as_deref(),
                Some("paid-quote-lnbc-fake-9-2")
            );
            assert_eq!(fake.melts.len(), 1, "at {now}");
            assert_eq!(fake.invoices.len(), invoices_before, "at {now}");
            assert_eq!(store.accrued_fees().expect("read").in_flight_fee_sats, 10);
        }
        // The one exit: the mint reports the bound quote PAID — settled by reconciliation, the
        // receipts discharged, no second melt. (The paid figures come from the quote; the actual
        // fee is recorded as not observed.)
        fake.status = Ok(Some(MeltQuoteStatus {
            mint_url: "https://mint.example".to_owned(),
            quote_id: "paid-quote-lnbc-fake-9-2".to_owned(),
            state: MeltQuoteState::Paid,
            amount_sats: 9,
            fee_reserve_sats: 1,
            expiry_unix: 2000,
        }));
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 12_001);
        assert!(
            out.contains("reports melt quote paid-quote-lnbc-fake-9-2 PAID — recorded as settled by reconciliation"),
            "{out}"
        );
        assert!(
            matches!(outcome, RemitOutcome::Refused(Refusal::NothingUnremitted)),
            "settled, then nothing left to remit: {outcome:?}\n{out}"
        );
        assert_eq!(fake.melts.len(), 1, "no second melt, ever");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!(rows[0].settled_by, Some(SettledBy::Reconciliation));
        assert_eq!(rows[0].melt_fee_sats, None, "not observed, not invented");
        assert_eq!(store.accrued_fees().expect("read").remitted_fee_sats, 10);

        // No quote at all for a SPENDING row's invoice (the wallet has no record — the live wallet
        // always holds at least the estimate quote, so this is an anomaly, not a normal failure):
        // HOLD, fail-closed — "no quote" is not the mint saying terminal. FAILED holds too: the
        // mint pays a FAILED quote (CDK 0.17.2), so FAILED is not cancellation either.
        let (store2, root2) = store_with_fees("interrupted-noquote", &[10]);
        let mut fake2 = Fake::new(|_| 1);
        fake2.melt_results = vec![Err("mint unreachable".to_owned())];
        assert!(matches!(
            run_remit(&store2, &mut fake2, RemitTrigger::Command, 100).0,
            RemitOutcome::MeltFailed { .. }
        ));
        fake2.status = Ok(None);
        let (outcome, out) = run_remit(&store2, &mut fake2, RemitTrigger::DryRun, 101);
        assert!(
            matches!(outcome, RemitOutcome::Refused(Refusal::SpendingHeld { .. })),
            "{out}"
        );
        assert!(
            out.contains(
                "this wallet holds no such melt quote; 10 sats of receipts stay pinned to it"
            ),
            "{out}"
        );
        assert_eq!(
            store2.remittances().expect("rows")[0].state,
            RemittanceState::Spending
        );
        fake2.status = Ok(Some(status(
            MeltQuoteState::Failed,
            "paid-quote-lnbc-fake-9-2",
        )));
        let (outcome, out) = run_remit(&store2, &mut fake2, RemitTrigger::DryRun, 102);
        assert!(
            matches!(outcome, RemitOutcome::Refused(Refusal::SpendingHeld { .. })),
            "{out}"
        );
        assert!(
            out.contains("reports melt quote paid-quote-lnbc-fake-9-2 FAILED (expiry unix"),
            "{out}"
        );
        assert!(!out.contains("released 10 sats"), "{out}");
        assert_eq!(
            store2.remittances().expect("rows")[0].state,
            RemittanceState::Spending
        );
        assert_eq!(store2.accrued_fees().expect("read").in_flight_fee_sats, 10);
        assert_eq!(fake2.melts.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    // A status query that itself fails is an error that changes nothing: the row stays planned, no
    // melt is attempted — and the attempt is journaled FAILED against the in-flight row.
    #[test]
    fn a_reconciliation_that_cannot_reach_the_mint_leaves_the_row_planned() {
        let (store, root) = store_with_fees("reconcile-error", &[10]);
        let mut fake = Fake::new(|_| 1);
        fake.melt_results = vec![Err("boom".to_owned())];
        assert!(matches!(
            run_remit(&store, &mut fake, RemitTrigger::Command, 100).0,
            RemitOutcome::MeltFailed { .. }
        ));
        fake.status = Err("mint unreachable".to_owned());
        let mut out = Vec::new();
        let error = remit(&store, &mut fake, RemitTrigger::Command, 101, &mut out)
            .expect_err("status failure surfaces");
        assert_eq!(error, "mint unreachable");
        assert!(store.in_flight_remittance().expect("query").is_some());
        assert_eq!(fake.melts.len(), 1);
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Failed);
        assert_eq!(attempts[0].detail, "mint unreachable");
        assert_eq!(attempts[0].remittance_id.as_deref(), Some("hash-9-2"));
        assert_eq!(
            attempts[0].unremitted_sats, 0,
            "the balance is in flight, not unremitted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Addendum gate 2b, the effects half: the LNURL host is unreachable. `remit_best_effort` returns
    // a report — never an error the collect path has to handle — the balance is intact, and the
    // failure is journaled so the read-out shows it. (The collect half — receipt journaled, job
    // marked paid — is `seller_node::run`'s test on the real collect write.)
    #[test]
    fn best_effort_catches_a_failed_attempt_and_leaves_the_balance_intact() {
        let (store, root) = store_with_fees("best-effort-fails", &[10]);
        let mut fake = Fake::new(|_| 1);
        fake.pay_request_error = Some("agi.cash: connection refused".to_owned());
        let report = remit_best_effort(&store, &mut fake, RemitTrigger::Collect, 100);
        assert_eq!(
            report.outcome,
            Err("agi.cash: connection refused".to_owned())
        );
        assert!(!report.is_quiet());
        assert_eq!(
            report.summary(),
            "attempt FAILED (agi.cash: connection refused); the balance stays unremitted and the node retries with backoff while it runs"
        );
        assert!(
            report.is_failure(),
            "an unreachable host is a pacing failure"
        );
        assert!(
            report.lines.iter().any(|line| line
                == "Accrued platform fee: 10 sats all-time — 0 sats remitted, 10 sats unremitted"),
            "{:?}",
            report.lines
        );
        assert!(fake.invoices.is_empty() && fake.melts.is_empty());
        assert_eq!(store.accrued_fees().expect("read").unremitted_fee_sats, 10);
        assert!(store.remittances().expect("rows").is_empty());
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].trigger, RemitAttemptTrigger::Collect);
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Failed);
        assert_eq!(attempts[0].unremitted_sats, 10);
        assert_eq!(attempts[0].detail, "agi.cash: connection refused");

        // The next attempt (a collect here; the retry tick is the other trigger) succeeds: the whole
        // balance, old and new, is paid once.
        store
            .collect_receipt(
                "receipt-next",
                "job-next",
                50,
                ReceiptFees {
                    mint_fee_sats: 1,
                    fee_bps: 1000,
                    fee_sats: 5,
                },
                101,
            )
            .expect("collect");
        let mut fake = Fake::new(|_| 1);
        fake.melt_results = vec![Ok((14, 1))];
        let report = remit_best_effort(&store, &mut fake, RemitTrigger::Collect, 102);
        assert_eq!(
            report.outcome,
            Ok(RemitOutcome::Paid {
                remittance_id: "hash-14-2".to_owned(),
                net_sats: 14,
                melt_fee_sats: 1,
            })
        );
        assert_eq!(store.accrued_fees().expect("read").unremitted_fee_sats, 0);
        assert_eq!(store.accrued_fees().expect("read").remitted_fee_sats, 15);

        // And a collect that lands below the threshold is quiet: no attempt journaled.
        let attempts_before = store.recent_remit_attempts(10).expect("attempts").len();
        store
            .collect_receipt(
                "receipt-tiny",
                "job-tiny",
                5,
                ReceiptFees {
                    mint_fee_sats: 1,
                    fee_bps: 1000,
                    fee_sats: 0,
                },
                103,
            )
            .expect("collect");
        let mut fake = Fake::new(|_| 1);
        let report = remit_best_effort(&store, &mut fake, RemitTrigger::Collect, 104);
        assert_eq!(
            report.outcome,
            Ok(RemitOutcome::Refused(Refusal::NothingUnremitted))
        );
        assert!(report.is_quiet());
        assert_eq!(fake.pay_requests, 0, "no network below the threshold");
        assert_eq!(
            store.recent_remit_attempts(10).expect("attempts").len(),
            attempts_before
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Addendum gate 2c: two attempts race against the same balance — two threads, two connections
    // to the same store, released together — and EXACTLY ONE remittance is recorded and exactly one
    // melt happens. The loser is refused, never paid a second time, by whichever boundary it
    // reaches first (addendum 11 §2, verdict 1ee5cb2 §5.2): the store's plan (both saw no row —
    // `PlanRefused` in flight / total moved), reconciliation of the winner's PLANNED row (another
    // live owner's lease stands — `HeldByOwner`), reconciliation of the winner's SPENDING row bound
    // to its quote (`SpendingHeld` — the product's REQUIRED result after the fence, which round 9's
    // oracle panicked on), or the winner already settled (`NothingUnremitted`). Every arm asserts
    // the row, owner, quote and pinned-receipt figures it names; the two ordered tests below pin
    // the after-fence and after-settled orderings deterministically.
    #[test]
    fn two_racing_attempts_against_the_same_balance_record_exactly_one_remittance() {
        for round in 0..8 {
            let (store, root) = store_with_fees(&format!("race-{round}"), &[10, 5]);
            let db = root.join(STATE_DB_FILE);
            let melts = Arc::new(AtomicUsize::new(0));
            let barrier = Arc::new(Barrier::new(2));
            let mut handles = Vec::new();
            for thread in 0..2 {
                let db = db.clone();
                let melts = Arc::clone(&melts);
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    // A connection of its own, as two collect handlers on two threads would have.
                    let store = SellerStore::open(&db).expect("open");
                    let mut fake = Fake::new(|_| 2);
                    // Two processes are two owners (`process_owner` is per process). With ONE shared
                    // owner string a loser that met the winner's PLANNED row would release it as
                    // "its own earlier attempt" (`ReleaseOn::OwnPlanned`) — a shape no deployment
                    // has (addendum 11 §2).
                    fake.owner = format!("proc-t{thread}");
                    // Two real LNURL calls never hand out the same invoice: distinct hashes, so
                    // the loser cannot be masked by a DuplicateInvoice on the winner's hash.
                    fake.invoice_tag = format!("-t{thread}");
                    fake.melt_results = vec![Ok((13, 1))];
                    fake.melt_counter = Some(melts);
                    fake.set_clock(100 + thread);
                    barrier.wait();
                    let mut out = Vec::new();
                    let outcome = remit(
                        &store,
                        &mut fake,
                        RemitTrigger::Collect,
                        100 + thread,
                        &mut out,
                    );
                    (
                        outcome,
                        String::from_utf8_lossy(&out).into_owned(),
                        fake.invoices.clone(),
                        fake.melts.len(),
                    )
                }));
            }
            let results: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect();

            assert_eq!(
                melts.load(Ordering::SeqCst),
                1,
                "exactly one melt: {results:?}"
            );
            let winners: Vec<usize> = results
                .iter()
                .enumerate()
                .filter(|(_, (outcome, ..))| matches!(outcome, Ok(RemitOutcome::Paid { .. })))
                .map(|(thread, _)| thread)
                .collect();
            assert_eq!(winners.len(), 1, "exactly one attempt paid: {results:?}");
            let winner_thread = winners[0];
            let winner_owner = format!("proc-t{winner_thread}");
            let winner_row_id = format!("hash-13-2-t{winner_thread}");
            let winner_quote = format!("paid-quote-lnbc-fake-13-2-t{winner_thread}");
            let rows = store.remittances().expect("rows");
            assert_eq!(rows.len(), 1, "exactly one remittance row: {rows:?}");
            let winner = &rows[0];
            assert_eq!(winner.state, RemittanceState::Settled);
            assert_eq!(winner.remittance_id, winner_row_id);
            assert_eq!(winner.owner.as_deref(), Some(winner_owner.as_str()));
            assert_eq!(
                winner.spending_quote_id.as_deref(),
                Some(winner_quote.as_str())
            );
            assert_eq!((winner.gross_sats, winner.net_sats), (15, 13));
            assert_eq!(winner.receipts, 2);
            for (thread, (outcome, out, invoices, own_melts)) in results.iter().enumerate() {
                match outcome {
                    Ok(RemitOutcome::Paid {
                        remittance_id,
                        net_sats,
                        melt_fee_sats,
                    }) => {
                        assert_eq!(remittance_id, &winner_row_id);
                        assert_eq!((*net_sats, *melt_fee_sats), (13, 1));
                        assert_eq!(*own_melts, 1);
                    }
                    Ok(RemitOutcome::Refused(Refusal::PlanRefused(reason))) => {
                        assert!(
                            reason.contains("still in flight")
                                || reason.contains("nothing to remit")
                                || reason.contains("unremitted total moved"),
                            "the loser is refused by the store's plan: {reason}\n{out}"
                        );
                        assert_eq!(*own_melts, 0, "{out}");
                    }
                    Ok(RemitOutcome::Refused(Refusal::NothingUnremitted)) => {
                        assert!(invoices.is_empty(), "nothing to plan on: {out}");
                        assert_eq!(*own_melts, 0, "{out}");
                    }
                    Ok(RemitOutcome::Refused(Refusal::HeldByOwner {
                        remittance_id,
                        owner,
                        lease_until_unix,
                    })) => {
                        // The loser met the winner's row PLANNED (journaled, not yet fenced): the
                        // winner is another live owner whose lease stands, so the loser holds.
                        assert_ne!(thread, winner_thread);
                        assert_eq!(remittance_id, &winner_row_id, "{out}");
                        assert_eq!(owner, &winner_owner, "{out}");
                        assert_eq!(*lease_until_unix, 400 + winner_thread as i64, "{out}");
                        assert!(
                            out.contains(&format!(
                                "remittance {winner_row_id} is planned by another live process ({winner_owner}, lease until unix {lease_until_unix}) and its quote is not terminal; not releasing a live payer's intent"
                            )),
                            "{out}"
                        );
                        assert!(invoices.is_empty(), "held before planning: {out}");
                        assert_eq!(*own_melts, 0, "{out}");
                    }
                    Ok(RemitOutcome::Refused(Refusal::SpendingHeld {
                        remittance_id,
                        owner,
                        spending_since_unix,
                        quote_id,
                        observed,
                        held_sats,
                    })) => {
                        // The loser started reconciliation after the winner's fence: the winner's
                        // row is SPENDING, bound to the winner's payment quote — which the LOSER's
                        // wallet never raised — and the whole gross stays pinned to it.
                        assert_ne!(thread, winner_thread);
                        assert_eq!(remittance_id, &winner_row_id, "{out}");
                        assert_eq!(owner, &winner_owner, "{out}");
                        assert_eq!(
                            Some(*spending_since_unix),
                            winner.spending_since_unix,
                            "{out}"
                        );
                        assert_eq!(*spending_since_unix, 100 + winner_thread as i64);
                        assert_eq!(quote_id.as_deref(), Some(winner_quote.as_str()), "{out}");
                        assert_eq!(observed, "this wallet holds no such melt quote", "{out}");
                        assert_eq!(*held_sats, 15, "the winner's gross, pinned: {out}");
                        assert!(
                            out.contains(&format!(
                                "HELD: remittance {winner_row_id} is SPENDING (admitted by {winner_owner} at unix {spending_since_unix}), bound to melt quote {winner_quote}; this wallet holds no such melt quote; 15 sats of receipts stay pinned to it — a spending row is released by nobody and on no clock; it settles only when the mint reports that quote PAID; an operator decision, not a timeout, resolves it. REFUSED — nothing moved by this run"
                            )),
                            "{out}"
                        );
                        assert!(invoices.is_empty(), "held before planning: {out}");
                        assert_eq!(*own_melts, 0, "{out}");
                    }
                    other => panic!("unexpected outcome {other:?}\n{out}"),
                }
            }
            let accrued = store.accrued_fees().expect("read");
            assert_eq!(
                (
                    accrued.remitted_fee_sats,
                    accrued.unremitted_fee_sats,
                    accrued.in_flight_fee_sats
                ),
                (15, 0, 0)
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    // Addendum 11 §2, ordered (i): the racing test's after-fence ordering, deterministic. A (proc-a)
    // pauses right after its fence admitted the melt and bound its quote — its row is SPENDING —
    // and B (proc-b, its own wallet, which never raised A's quote) runs `--confirm`: B reconciles
    // A's bound spending row, its wallet holds no such quote ⇒ `SpendingHeld` naming A's row, A's
    // owner, A's bound quote and the 15 pinned sats, one HELD line; B plans nothing (no invoice),
    // pays nothing, changes nothing. A resumes and pays once. One melt, one Paid, one Settled row,
    // accounting (15, 0, 0); B's refused confirm journaled against A's row.
    #[test]
    fn a_racer_that_starts_after_the_winners_fence_is_held_on_the_winners_bound_quote_and_the_winner_pays_once()
     {
        let (store, root) = store_with_fees("race-after-fence", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let mut a = Fake::new(|_| 2);
        a.owner = "proc-a".to_owned();
        a.invoice_tag = "-a".to_owned();
        a.melt_results = vec![Ok((13, 1))];
        let (a_result, b_results) = run_paused(
            &db,
            a,
            100,
            PauseAt::Admit,
            super::test_support::Gate::new(),
            Arc::clone(&melts),
            |store_b| {
                let x = store_b
                    .in_flight_remittance()
                    .expect("query")
                    .expect("A's row");
                assert_eq!(x.state, RemittanceState::Spending);
                assert_eq!(x.remittance_id, "hash-13-2-a");
                assert_eq!(x.spending_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
                let mut b = Fake::new(|_| 2);
                b.owner = "proc-b".to_owned();
                b.invoice_tag = "-b".to_owned();
                b.melt_results = vec![Ok((13, 1))];
                b.melt_counter = Some(Arc::clone(&melts));
                let (outcome, out) = run_remit(store_b, &mut b, RemitTrigger::Command, 101);
                assert_eq!(
                    outcome,
                    RemitOutcome::Refused(Refusal::SpendingHeld {
                        remittance_id: "hash-13-2-a".to_owned(),
                        owner: "proc-a".to_owned(),
                        spending_since_unix: 100,
                        quote_id: Some(X_PAYMENT_QUOTE.to_owned()),
                        observed: "this wallet holds no such melt quote".to_owned(),
                        held_sats: 15,
                    }),
                    "{out}"
                );
                assert!(
                    out.contains("HELD: remittance hash-13-2-a is SPENDING (admitted by proc-a at unix 100), bound to melt quote paid-quote-lnbc-fake-13-2-a; this wallet holds no such melt quote; 15 sats of receipts stay pinned to it — a spending row is released by nobody and on no clock; it settles only when the mint reports that quote PAID; an operator decision, not a timeout, resolves it. REFUSED — nothing moved by this run"),
                    "{out}"
                );
                assert_eq!(
                    b.quote_status_calls,
                    vec![X_PAYMENT_QUOTE.to_owned()],
                    "B asked its wallet about A's bound quote, once"
                );
                assert!(b.invoices.is_empty(), "B planned nothing: {out}");
                assert!(
                    b.quotes.is_empty() && b.melts.is_empty(),
                    "B paid nothing: {out}"
                );
                assert_eq!(
                    store_b
                        .in_flight_remittance()
                        .expect("query")
                        .expect("still A's row")
                        .state,
                    RemittanceState::Spending,
                    "B changed nothing"
                );
                vec![(outcome, out)]
            },
        );
        let (a_outcome, a_out) = a_result;
        assert_eq!(
            a_outcome,
            Ok(RemitOutcome::Paid {
                remittance_id: "hash-13-2-a".to_owned(),
                net_sats: 13,
                melt_fee_sats: 1,
            }),
            "A pays X once it resumes:\n{a_out}"
        );
        assert_eq!(melts.load(Ordering::SeqCst), 1, "exactly one actual debit");
        assert_eq!(b_results.len(), 1);
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1, "one row, A's: {rows:?}");
        assert_eq!(rows[0].remittance_id, "hash-13-2-a");
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!((rows[0].gross_sats, rows[0].net_sats), (15, 13));
        assert_eq!(rows[0].receipts, 2);
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (15, 0, 0)
        );
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(
            attempts.len(),
            2,
            "A's payment and B's refused --confirm: {attempts:?}"
        );
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Paid);
        assert_eq!(attempts[1].trigger, RemitAttemptTrigger::Command);
        assert_eq!(attempts[1].outcome, RemitAttemptOutcome::Refused);
        assert_eq!(attempts[1].remittance_id.as_deref(), Some("hash-13-2-a"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Addendum 11 §2, ordered (ii): the racing test's after-settled ordering. A pays and settles
    // in full; then B (another owner, its own wallet) runs `--confirm`: no row is in flight, the
    // unremitted balance is 0 ⇒ `NothingUnremitted` — B raises no invoice, no quote, no melt;
    // still one melt, one Settled row, accounting (15, 0, 0).
    #[test]
    fn a_racer_that_starts_after_the_winner_settled_finds_nothing_unremitted_and_pays_nothing() {
        let (store, root) = store_with_fees("race-after-settled", &[10, 5]);
        let mut a = Fake::new(|_| 2);
        a.owner = "proc-a".to_owned();
        a.invoice_tag = "-a".to_owned();
        a.melt_results = vec![Ok((13, 1))];
        let (a_outcome, a_out) = run_remit(&store, &mut a, RemitTrigger::Collect, 100);
        assert_eq!(
            a_outcome,
            RemitOutcome::Paid {
                remittance_id: "hash-13-2-a".to_owned(),
                net_sats: 13,
                melt_fee_sats: 1,
            },
            "{a_out}"
        );
        assert_eq!(a.melts.len(), 1);
        let mut b = Fake::new(|_| 2);
        b.owner = "proc-b".to_owned();
        b.invoice_tag = "-b".to_owned();
        b.melt_results = vec![Ok((13, 1))];
        let (b_outcome, b_out) = run_remit(&store, &mut b, RemitTrigger::Command, 101);
        assert_eq!(
            b_outcome,
            RemitOutcome::Refused(Refusal::NothingUnremitted),
            "{b_out}"
        );
        assert!(
            b_out.contains("Nothing to remit. REFUSED — nothing moved."),
            "{b_out}"
        );
        assert!(
            b.quote_status_calls.is_empty() && b.status_calls.is_empty(),
            "no row in flight, nothing to reconcile: {b_out}"
        );
        assert!(
            b.invoices.is_empty() && b.quotes.is_empty() && b.melts.is_empty(),
            "{b_out}"
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1, "one row, A's: {rows:?}");
        assert_eq!(rows[0].remittance_id, "hash-13-2-a");
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!((rows[0].gross_sats, rows[0].net_sats), (15, 13));
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (15, 0, 0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- addendum 8 §1: the ceiling bounds the ENTIRE wallet debit (verdict B4) ----------------

    // The fake's fee model, checked against the verdict's own arithmetic (§4 B4): a 1000-ppk keyset,
    // one 32-sat proof, a quote of 13 with a 2-sat reserve against 15 accrued. need = 15 = 8+4+2+1
    // ⇒ four output proofs ⇒ input fee 4; selection 19 ⇒ the 32 is swapped ⇒ swap fee 1; total
    // 13 + 2 + 4 + 1 = 20 > 15. The two-figure check admits it; the four-figure one refuses it, and
    // the refusal leaves the pool untouched.
    #[test]
    fn the_fake_wallet_models_the_sdks_proof_input_and_swap_fees_and_the_total_bound_refuses_them()
    {
        assert_eq!(fee_for(1000, 4), 4);
        assert_eq!(fee_for(500, 3), 2, "ceil(1500 / 1000)");
        assert_eq!(fee_for(0, 7), 0);
        assert_eq!(
            layout(1000, Some(&[32]), 15).expect("layout"),
            FakeLayout {
                picked: vec![32],
                input_fee_sats: 4,
                swap_fee_sats: 1,
                requires_swap: true,
            }
        );
        assert_eq!(
            layout(0, Some(&[8, 4, 2, 1]), 15).expect("layout"),
            FakeLayout {
                picked: vec![8, 4, 2, 1],
                input_fee_sats: 0,
                swap_fee_sats: 0,
                requires_swap: false,
            },
            "a fee-free mint keeps the exact-fit branch every earlier test relies on"
        );
        assert!(layout(1000, Some(&[16]), 15).is_err(), "16 < 15 + 4 + 1");

        let mut fake = Fake::new(|_| 2);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let ceiling = MeltCeiling {
            max_debit_sats: 15,
            invoice_sats: 13,
            planned_quote_id: Some("quote-lnbc-fake-13-1".to_owned()),
        };
        assert!(ceiling.admits(13, 2), "the quote's own figures fit exactly");
        let refused = fake
            .prepare_melt("paid-quote-lnbc-fake-13-1", &ceiling)
            .expect_err("the total does not fit");
        match refused {
            MeltFailure::RefusedBeforeSpending(reason) => assert!(
                reason.contains("would debit 19 sats in total (13 sats invoice + 2 sats fee reserve + 3 sats proof input fee + 1 sats swap fee) against a ceiling of 15 sats; the prepared melt was cancelled and its proofs released; nothing was posted to the mint"),
                "{reason}"
            ),
            other => panic!("expected a typed refusal, got {other:?}"),
        }
        assert_eq!(fake.pool_value(), Some(32), "the pool is exactly as it was");
        assert_eq!(fake.ceiling_refusals.len(), 1);
        assert!(fake.melts.is_empty());
        assert!(fake.prepared.is_empty(), "nothing is left prepared");

        // The estimate carries the same fees, reserving nothing.
        let estimate = fake.melt_estimate("lnbc-fake-13-1").expect("estimate");
        assert_eq!(estimate.expected_fees_sats, 5, "4 input + 1 swap");
        assert_eq!(estimate.expected_fees_note, None);
        assert_eq!(fake.pool_value(), Some(32));

        // CONFIRM model (addendum 9 §1.3), against CDK's arithmetic, not the fake's preparation.
        // (a) Prepared figures fit, post-swap arithmetic does not: invoice 12, reserve 0 ⇒ need 12
        // = 8+4 ⇒ prepared input 2, swap 1, prepared total 15 ≤ 20. confirm WOULD swap the 32 to
        // target 14 = [8, 4, 2] (three proofs ⇒ ACTUAL input 3) and need 12 + 0 + 3 = 15 > 14 ⇒
        // CDK refuses AFTER the swap, swap fee gone. Since addendum 10 §1.1 the wallet's prepare
        // gate (`confirm_bound`, the same arithmetic) refuses this BEFORE anything is posted: no
        // swap, no melt, the pool exactly as it was, proofs released.
        assert_eq!(binary_split(14), vec![8, 4, 2]);
        assert_eq!(binary_split(19), vec![16, 2, 1]);
        assert_eq!(binary_split(0), Vec::<u64>::new());
        // §1.2 planning: the advisor's example and its neighbours.
        assert_eq!(
            plan_confirmable_invoice(20, 2, 1, 1000),
            Some(13),
            "gross 20, reserve 2: 13 pays at 19; 14 would swap to 17 < 16 + 2"
        );
        assert_eq!(plan_confirmable_invoice(20, 3, 1, 1000), Some(12));
        assert_eq!(plan_confirmable_invoice(3, 1, 1, 1000), None, "never fits");
        assert_eq!(
            plan_confirmable_invoice(20, 2, 0, 0),
            Some(18),
            "fee-free: gross − reserve"
        );
        assert_eq!(
            plan_confirmable_invoice(2, 2, 0, 0),
            None,
            "reserve eats the gross"
        );
        assert_eq!(post_swap_figures(15, Some(4), 1000), (19, 3));
        assert_eq!(post_swap_figures(16, None, 1000), (17, 2));
        let mut fake = Fake::new(|_| 0);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let ceiling = MeltCeiling {
            max_debit_sats: 20,
            invoice_sats: 12,
            planned_quote_id: Some("quote-lnbc-fake-12-1".to_owned()),
        };
        let refused = fake
            .prepare_melt("paid-quote-lnbc-fake-12-1", &ceiling)
            .expect_err(
                "the prepared total 12 + 0 + 2 + 1 = 15 ≤ 20 is NOT the gate; confirmability is",
            );
        match refused {
            MeltFailure::RefusedBeforeSpending(reason) => assert!(
                reason.contains("would swap to 14 sats ([8, 4, 2])")
                    && reason.contains("actual proof input fee on those proofs is 3 sats (prepared estimate 2 sats at 1000 ppk)")
                    && reason.contains("would need 15 sats")
                    && reason.contains("the SDK would refuse AFTER paying the 1 sats swap fee"),
                "{reason}"
            ),
            other => panic!("expected the prepare gate's typed refusal, got {other:?}"),
        }
        assert!(fake.swaps.is_empty(), "no swap was posted");
        assert!(fake.melts.is_empty(), "no melt was posted");
        assert!(fake.pay_refusals.is_empty(), "the SDK never got to refuse");
        assert_eq!(fake.ceiling_refusals.len(), 1, "the one gate refused it");
        assert!(fake.prepared.is_empty(), "nothing is left prepared");
        assert_eq!(fake.pool_value(), Some(32), "the pool is exactly as it was");
        assert!(fake.proofs_spent.is_empty(), "nothing was spent");
        // And §1.1 says so BEFORE the fence, from the same figures.
        let refused =
            confirm_would_succeed(&fake_preparation(12, 0, 2, 1, 1000), 20).expect_err("predicted");
        assert!(
            refused.contains("swap to 14 sats ([8, 4, 2])")
                && refused.contains("actual proof input fee on those proofs is 3 sats")
                && refused.contains("would need 15 sats"),
            "{refused}"
        );

        // (b) A genuinely confirmable payment: invoice 13, reserve 2 ⇒ need 15 = 8+4+2+1 ⇒ prepared
        // input 4, swap 1, total 20 ≤ 20. confirm swaps to target 19 = [16, 2, 1] ⇒ ACTUAL input 3,
        // needs 13 + 2 + 3 = 18 ≤ 19 ⇒ melt posted; the mint takes a 1-sat Lightning fee and returns
        // 19 − 13 − 1 − 3 = 2 as change. `fee_paid` = 19 − 13 − 2 = 4 = Lightning 1 + actual input 3
        // (inclusive, swap fee excluded). Wallet: 32 → change [8, 4] from the swap + [2] from the
        // melt = 14; delta 18 = 13 + 1 + 3 + 1 ≤ gross 20.
        let mut fake = Fake::new(|_| 2);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        fake.melt_results = vec![Ok((13, 1))];
        let ceiling = MeltCeiling {
            max_debit_sats: 20,
            invoice_sats: 13,
            planned_quote_id: Some("quote-lnbc-fake-13-1".to_owned()),
        };
        let prepared = fake
            .prepare_melt("paid-quote-lnbc-fake-13-1", &ceiling)
            .expect("13 + 2 + 4 + 1 = 20 ≤ 20");
        assert_eq!(
            confirm_would_succeed(&prepared.preparation, 20),
            Ok(3),
            "actual input fee on [16, 2, 1]; 13 + 2 + 3 + 1 = 19 ≤ 20"
        );
        let outcome = fake.confirm_melt(prepared).expect("confirmable");
        assert_eq!(outcome.paid_sats, 13);
        assert_eq!(
            outcome.fee_sats, 4,
            "fee_paid = Lightning 1 + actual input 3"
        );
        assert_eq!(
            outcome.input_fee_sats, 4,
            "the PREPARED estimate, as the SDK reports it"
        );
        assert_eq!(outcome.swap_fee_sats, 1);
        assert_eq!(fake.swaps.len(), 1);
        assert_eq!(fake.swaps[0].received, vec![16, 2, 1]);
        assert_eq!(fake.swaps[0].change, vec![8, 4]);
        assert_eq!(fake.melts.len(), 1, "one melt");
        let after = fake.pool_value().expect("pool");
        assert_eq!(after, 14, "32 − 13 − 1 − 3 − 1");
        let delta = 32 - after;
        assert_eq!(
            delta,
            13 + 1 + 3 + 1,
            "the whole-wallet delta is the four terms"
        );
        assert!(delta <= 20, "≤ gross");
    }

    /// A `MeltPreparation` with the four fee figures and the keyset ppk, for the arithmetic tests.
    fn fake_preparation(
        invoice_sats: u64,
        fee_reserve_sats: u64,
        input_fee_sats: u64,
        swap_fee_sats: u64,
        input_fee_ppk: u64,
    ) -> MeltPreparation {
        MeltPreparation {
            mint_url: "https://mint.example".to_owned(),
            quote_id: "paid-quote-fixture".to_owned(),
            invoice_sats,
            fee_reserve_sats,
            input_fee_sats,
            swap_fee_sats,
            requires_swap: input_fee_ppk > 0,
            input_fee_ppk,
            total_debit_sats: invoice_sats + fee_reserve_sats + input_fee_sats + swap_fee_sats,
            expiry_unix: u64::MAX,
        }
    }

    // Regression (i), addendum 9 §1.4: fee-bearing mint (1000 ppk), surplus funds in a layout that
    // does not fit (one 32-sat proof), reserve 3 at estimate AND payment; the PREPARED input fee
    // differs from the ACTUAL one and the payment still fits ⇒ pays ONCE, swap counted once, melt
    // counted once, and the WALLET — measured, not counted — loses at most the gross. Gross 20 (two
    // 10-sat fees). Planning (§1.2) searches down from 17: 12 is the first invoice whose post-swap
    // arithmetic holds (need 15 = 8+4+2+1 ⇒ prepared input 4 ⇒ target 19 = [16, 2, 1] ⇒ actual 3;
    // 19 ≥ 18; 15 + 3 + 1 = 19 ≤ 20). Payment: prepared 12 + 3 + 4 + 1 = 20 ≤ 20 admitted; confirm
    // swaps the 32 to 19 (swap fee 1, change [8, 4]), recomputes 3, needs 18 ≤ 19, melts; the mint
    // takes a 1-sat Lightning fee and returns 19 − 12 − 1 − 3 = 3 as change. `fee_paid` = 19 − 12 −
    // 3 = 4 = Lightning 1 + actual input 3 (inclusive, CDK `melt/saga/mod.rs:139–148`). Wallet 32 →
    // 12 + 3 = 15: delta 17 = 12 + 1 + 3 + 1 ≤ 20.
    #[test]
    fn a_fee_bearing_payment_whose_prepared_input_fee_differs_from_the_actual_pays_once_and_the_wallet_loses_at_most_the_gross()
     {
        let (store, root) = store_with_fees("fee-bearing-fits", &[10, 10]);
        let mut fake = Fake::new(|_| 3);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        fake.registry = Some(quote_registry());
        fake.melt_results = vec![Ok((12, 1))];
        let before = fake.pool_value().expect("pool");
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        assert!(is_paid(&outcome), "{out}");
        assert!(
            out.contains("invoice amount (maxplayer@strike.me receives): 12 sats"),
            "{out}"
        );
        assert!(
            out.contains("expected proof fees (SDK estimate, bounded exactly at payment): 5 sats = estimated proof input fee 4 sats + swap fee 1 sats; actual proof input fee the SDK recomputes on the swapped proofs: 3 sats ⇒ worst case 19 sats leaves the wallet (≤ 20) = invoice 12 + reserve 3 (bounds the Lightning fee) + actual proof input fee 3 + swap fee 1; the inclusive melt fee (Lightning + actual proof input fee) is known only at payment"),
            "{out}"
        );
        assert_eq!(
            fake.melts,
            vec!["lnbc-fake-12-2".to_owned()],
            "exactly one melt"
        );
        assert_eq!(fake.swaps.len(), 1, "exactly one swap");
        assert_eq!(fake.swaps[0].sent, vec![32]);
        assert_eq!(fake.swaps[0].target_sats, 19);
        assert_eq!(fake.swaps[0].swap_fee_sats, 1);
        assert_eq!(fake.swaps[0].received, vec![16, 2, 1]);
        assert_eq!(fake.swaps[0].change, vec![8, 4]);
        assert!(
            out.contains("Prepared melt of quote paid-quote-lnbc-fake-12-2: proof input fee 4 sats (estimate; actual on the swapped proofs 3 sats), swap fee 1 sats (the wallet's proofs do not fit: a pre-melt swap will be performed); total debit 19 sats (12 invoice + 3 reserve + fees) fits the ceiling of 20 sats; proofs reserved in this wallet only, nothing posted yet"),
            "{out}"
        );
        // §2 G/F2: the SDK's inclusive `fee_paid` (4 = Lightning 1 + actual input 3) is counted
        // ONCE; the prepared estimate (4) is printed, not added — debit 12 + 4 + 1 = 17, not 21.
        for needle in [
            "melt fee taken by the mint: 4 sats (quote paid-quote-lnbc-fake-12-2 reserved 3 sats; ceiling 20 sats held at the moment of spending; this is the SDK's fee_paid = Lightning fee + actual proof input fee)",
            "estimated proof input fee (prepared): 4 sats — replaced by the actual fee inside the melt fee above, not added again; swap fee (charged at swap): 1 sats",
            "actual debit: 17 sats = net + melt fee + swap fee",
            "stays in your wallet (unused reserve): 3 sats",
            "wallet balance now: 1000 sats at https://mint.example",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
        assert!(
            !out.contains("WARNING"),
            "no false overspend warning from double-counting the input fee:\n{out}"
        );
        let after = fake.pool_value().expect("pool");
        assert_eq!(
            (before, after),
            (32, 15),
            "measured: change [8, 4] from the swap + [2, 1] from the melt"
        );
        let delta = before - after;
        assert_eq!(
            delta,
            12 + 1 + 3 + 1,
            "invoice + Lightning fee + ACTUAL input fee + swap fee (not the prepared 4)"
        );
        assert!(
            delta <= 20,
            "the wallet lost {delta} sats against 20 accrued"
        );
        assert_eq!(fake.proofs_spent, vec![vec![32]], "the 32 went to the swap");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!((rows[0].gross_sats, rows[0].net_sats), (20, 12));
        assert_eq!(
            rows[0].melt_fee_sats,
            Some(4),
            "`fee_paid`, inclusive: Lightning 1 + actual input 3"
        );
        assert_eq!(rows[0].melt_fee_reserve_sats, Some(3));
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (20, 0, 0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Addendum 9 §2.4: the same fee-bearing success (prepared input 4 ≠ actual 3) when the
    // observational balance read after the payment FAILS: no false overspend warning, the residual
    // balance printed as "unknown" — never a computed figure — and the settlement unaffected.
    #[test]
    fn a_failed_balance_read_after_a_fee_bearing_payment_prints_unknown_and_no_false_warning() {
        let (store, root) = store_with_fees("fee-bearing-balance-unknown", &[10, 10]);
        let mut fake = Fake::new(|_| 3);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        fake.registry = Some(quote_registry());
        fake.melt_results = vec![Ok((12, 1))];
        fake.balance_read_fails = true;
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        assert!(is_paid(&outcome), "{out}");
        assert_eq!(fake.melts.len(), 1);
        assert_eq!(fake.swaps.len(), 1);
        assert!(
            out.contains("wallet balance now: unknown (the balance read after the payment failed; the payment stands) at https://mint.example"),
            "{out}"
        );
        assert!(
            out.contains("actual debit: 17 sats = net + melt fee + swap fee")
                && out.contains("stays in your wallet (unused reserve): 3 sats"),
            "{out}"
        );
        assert!(!out.contains("WARNING"), "{out}");
        assert!(
            !out.contains("wallet balance now: 9 sats")
                && !out.contains("wallet balance now: 15 sats"),
            "no computed residual is printed:\n{out}"
        );
        assert_eq!(
            fake.pool_value(),
            Some(15),
            "the payment itself is unaffected"
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!(rows[0].melt_fee_sats, Some(4));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Addendum 10 §1.4 (a): the reserve SHRINKS between estimate and payment (3 → 0) and the planned
    // invoice would no longer confirm — planned on reserve 3: invoice 12 (need 15 ⇒ target 19,
    // actual 3); at reserve 0: need 12 = 8+4 ⇒ target 14 = [8, 4, 2] ⇒ actual 3 ⇒ needs 15 > 14.
    // The SAME attempt re-plans ONCE, before anything is prepared: the planner on the live reserve
    // gives 15 (need 15 ⇒ prepared 4 ⇒ target 19 ⇒ actual 3; 19 ≥ 18; 15 + 0 + 3 + 1 = 19 ≤ 20), a
    // 15-sat invoice and its live quote are raised, the Planned row is re-pointed at them (same
    // id, same gross, same two receipts), one "Re-planned:" line prints, and the unchanged sequence
    // pays it once: swap 32 → 19 (fee 1, change [8, 4]), melt 19, Lightning fee 0, change 19 − 15 −
    // 0 − 3 = 1 ⇒ `fee_paid` = 19 − 15 − 1 = 3; pool 32 → 12 + 1 = 13, delta 19 ≤ 20. A
    // next-attempt re-quote could not have done this: it would plan from the probe estimate again.
    #[test]
    fn a_reserve_that_shrinks_between_estimate_and_payment_is_re_planned_once_and_pays_invoice_15()
    {
        let (store, root) = store_with_fees("fee-bearing-replan", &[10, 10]);
        let mut fake = Fake::new(|_| 3);
        fake.live_reserve_for = Some(Box::new(|_| 0));
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let registry = quote_registry();
        fake.registry = Some(Arc::clone(&registry));
        fake.melt_results = vec![Ok((15, 0))];
        let before = fake.pool_value().expect("pool");
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        match &outcome {
            RemitOutcome::Paid {
                remittance_id,
                net_sats,
                melt_fee_sats,
            } => {
                assert_eq!(remittance_id, "hash-12-2", "the row keeps its id");
                assert_eq!((*net_sats, *melt_fee_sats), (15, 3));
            }
            other => panic!("expected Paid, got {other:?}\n{out}"),
        }
        // Planned on the estimate (12), re-planned on the live reserve (15): ONE re-plan line, no
        // refusal, no warning.
        assert!(
            out.contains("invoice amount (maxplayer@strike.me receives): 12 sats"),
            "{out}"
        );
        assert_eq!(
            out.lines()
                .filter(|line| line.starts_with("Re-planned: "))
                .count(),
            1,
            "exactly one re-plan line:\n{out}"
        );
        assert!(
            out.contains("Re-planned: the payment quote's fee reserve is 0 sats (planned on 3 sats); invoice 12 sats would not confirm (the wallet would swap to 14 sats and the SDK's actual proof input fee on those proofs is 3 sats, so the payment would need 15 sats and the SDK would refuse after its swap), invoice 15 sats will (at most 19 sats leaves the wallet, ≤ 20); payment quote paid-quote-lnbc-fake-15-3 raised at mint https://mint.example for 15 sats (fee reserve 0 sats); invoice payment hash: hash-15-3"),
            "{out}"
        );
        assert!(!out.contains("REFUSED"), "{out}");
        assert!(!out.contains("WARNING"), "{out}");
        assert!(
            out.contains("Prepared melt of quote paid-quote-lnbc-fake-15-3: proof input fee 4 sats (estimate; actual on the swapped proofs 3 sats), swap fee 1 sats (the wallet's proofs do not fit: a pre-melt swap will be performed); total debit 19 sats (15 invoice + 0 reserve + fees) fits the ceiling of 20 sats; proofs reserved in this wallet only, nothing posted yet"),
            "{out}"
        );
        for needle in [
            "bound to melt quote paid-quote-lnbc-fake-15-3",
            "melt fee taken by the mint: 3 sats (quote paid-quote-lnbc-fake-15-3 reserved 0 sats;",
            "actual debit: 19 sats = net + melt fee + swap fee",
            "net paid to maxplayer@strike.me: 15 sats",
            "receipts discharged: 2",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
        // Paid ONCE: one swap, one melt — of the RE-PLANNED invoice; the 12-sat quote was never
        // prepared or paid.
        assert_eq!(fake.melts, vec!["lnbc-fake-15-3".to_owned()]);
        assert_eq!(fake.swaps.len(), 1);
        assert_eq!(fake.swaps[0].sent, vec![32]);
        assert_eq!(fake.swaps[0].target_sats, 19);
        assert_eq!(fake.swaps[0].swap_fee_sats, 1);
        assert_eq!(fake.swaps[0].received, vec![16, 2, 1]);
        assert_eq!(fake.swaps[0].change, vec![8, 4]);
        assert!(
            fake.cancels.is_empty(),
            "nothing was prepared before the re-plan"
        );
        assert!(fake.ceiling_refusals.is_empty());
        assert!(fake.pay_refusals.is_empty());
        let after = fake.pool_value().expect("pool");
        assert_eq!((before, after), (32, 13), "change [8, 4] + [1]");
        assert_eq!(
            before - after,
            15 + 3 + 1,
            "delta 19 ≤ 20: invoice + actual input fee + swap fee, Lightning fee 0"
        );
        let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            quotes
                .get("paid-quote-lnbc-fake-15-3")
                .map(|quote| quote.state),
            Some(MeltQuoteState::Paid)
        );
        assert_eq!(
            quotes
                .get("paid-quote-lnbc-fake-12-2")
                .map(|quote| quote.state),
            Some(MeltQuoteState::Unpaid),
            "the first payment quote was raised and never paid"
        );
        drop(quotes);
        // One row, re-pointed then settled: same id and gross, the re-planned invoice's figures,
        // the second quote bound and settled, both receipts discharged.
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].remittance_id, "hash-12-2");
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!((rows[0].gross_sats, rows[0].net_sats), (20, 15));
        assert_eq!(rows[0].payment_hash, "hash-15-3");
        assert_eq!(rows[0].bolt11, "lnbc-fake-15-3");
        assert_eq!(rows[0].melt_fee_sats, Some(3));
        assert_eq!(rows[0].melt_fee_reserve_sats, Some(0));
        assert_eq!(
            rows[0].melt_quote_id.as_deref(),
            Some("paid-quote-lnbc-fake-15-3")
        );
        assert_eq!(
            rows[0].spending_quote_id.as_deref(),
            Some("paid-quote-lnbc-fake-15-3")
        );
        assert_eq!(rows[0].receipts, 2);
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (20, 0, 0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Addendum 10 §1.3 (verdict 4714623 §3.2's witness): gross 19 (fees 10 + 9), reserve 2 at
    // estimate and payment, 1000 ppk, one 32-sat proof. The planner gives 13: need 15 = 8+4+2+1 ⇒
    // prepared 4 ⇒ target 19 = [16, 2, 1] ⇒ actual 3; 19 ≥ 18; 15 + 3 + 1 = 19 ≤ 19. The PREPARED
    // total is 13 + 2 + 4 + 1 = 20 > 19 — round 8's early return on it refused this remittance;
    // since addendum 10 §1.1 the one gate is the actual-confirmability bound and it PAYS, once:
    // swap 32 → 19 (fee 1, change [8, 4]), melt 19, the mint keeps the whole 2-sat reserve as its
    // Lightning fee (worst case) and the actual input 3, change 19 − 13 − 2 − 3 = 1 ⇒ `fee_paid` =
    // 19 − 13 − 1 = 5 = Lightning 2 + actual input 3; pool 32 → 12 + 1 = 13, delta 19 = the gross
    // exactly, never over. (The round-9 plan wrote "fee_paid 4, delta 19" — those two figures are
    // inconsistent; with Lightning 1 the pair is 4 / 18, with Lightning 2 it is 5 / 19. The tight
    // case is asserted here.)
    #[test]
    fn a_fee_bearing_payment_at_a_19_sat_gross_the_prepared_estimate_would_refuse_pays_invoice_13_once()
     {
        let (store, root) = store_with_fees("fee-bearing-19", &[10, 9]);
        let mut fake = Fake::new(|_| 2);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let registry = quote_registry();
        fake.registry = Some(Arc::clone(&registry));
        fake.melt_results = vec![Ok((13, 2))];
        let before = fake.pool_value().expect("pool");
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        match &outcome {
            RemitOutcome::Paid {
                remittance_id,
                net_sats,
                melt_fee_sats,
            } => {
                assert_eq!(remittance_id, "hash-13-2");
                assert_eq!((*net_sats, *melt_fee_sats), (13, 5));
            }
            other => panic!("expected Paid, got {other:?}\n{out}"),
        }
        assert!(
            out.contains("invoice amount (maxplayer@strike.me receives): 13 sats"),
            "{out}"
        );
        assert!(
            out.contains("expected proof fees (SDK estimate, bounded exactly at payment): 5 sats = estimated proof input fee 4 sats + swap fee 1 sats; actual proof input fee the SDK recomputes on the swapped proofs: 3 sats ⇒ worst case 19 sats leaves the wallet (≤ 19) = invoice 13 + reserve 2 (bounds the Lightning fee) + actual proof input fee 3 + swap fee 1; the inclusive melt fee (Lightning + actual proof input fee) is known only at payment"),
            "{out}"
        );
        assert!(
            out.contains("Prepared melt of quote paid-quote-lnbc-fake-13-2: proof input fee 4 sats (estimate; actual on the swapped proofs 3 sats), swap fee 1 sats (the wallet's proofs do not fit: a pre-melt swap will be performed); total debit 19 sats (13 invoice + 2 reserve + fees) fits the ceiling of 19 sats; proofs reserved in this wallet only, nothing posted yet"),
            "the prepared total (20) is printed nowhere as a gate; the bound's 19 is:\n{out}"
        );
        for needle in [
            "melt fee taken by the mint: 5 sats (quote paid-quote-lnbc-fake-13-2 reserved 2 sats; ceiling 19 sats held at the moment of spending; this is the SDK's fee_paid = Lightning fee + actual proof input fee)",
            "estimated proof input fee (prepared): 4 sats — replaced by the actual fee inside the melt fee above, not added again; swap fee (charged at swap): 1 sats",
            "actual debit: 19 sats = net + melt fee + swap fee",
            "net paid to maxplayer@strike.me: 13 sats",
            "receipts discharged: 2",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
        assert!(!out.contains("REFUSED"), "{out}");
        assert!(
            !out.contains("Re-planned"),
            "reserve unchanged: no re-plan\n{out}"
        );
        assert!(
            !out.contains("WARNING"),
            "delta equals the gross exactly; that is not an overspend:\n{out}"
        );
        assert_eq!(
            fake.melts,
            vec!["lnbc-fake-13-2".to_owned()],
            "exactly one melt"
        );
        assert_eq!(fake.swaps.len(), 1, "exactly one swap");
        assert_eq!(fake.swaps[0].sent, vec![32]);
        assert_eq!(fake.swaps[0].target_sats, 19);
        assert_eq!(fake.swaps[0].swap_fee_sats, 1);
        assert_eq!(fake.swaps[0].received, vec![16, 2, 1]);
        assert_eq!(fake.swaps[0].change, vec![8, 4]);
        assert!(fake.cancels.is_empty());
        assert!(fake.ceiling_refusals.is_empty(), "the bound admitted it");
        assert!(fake.pay_refusals.is_empty());
        assert_eq!(fake.proofs_spent, vec![vec![32]], "the 32 went to the swap");
        let after = fake.pool_value().expect("pool");
        assert_eq!((before, after), (32, 13), "change [8, 4] + [1]");
        let delta = before - after;
        assert_eq!(
            delta,
            13 + 2 + 3 + 1,
            "invoice + Lightning + ACTUAL input + swap"
        );
        assert!(
            delta <= 19,
            "the wallet lost {delta} sats against 19 accrued"
        );
        assert_eq!(
            registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get("paid-quote-lnbc-fake-13-2")
                .map(|quote| quote.state),
            Some(MeltQuoteState::Paid)
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!((rows[0].gross_sats, rows[0].net_sats), (19, 13));
        assert_eq!(
            rows[0].melt_fee_sats,
            Some(5),
            "inclusive: Lightning 2 + actual input 3"
        );
        assert_eq!(rows[0].melt_fee_reserve_sats, Some(2));
        assert_eq!(
            rows[0].spending_quote_id.as_deref(),
            Some("paid-quote-lnbc-fake-13-2")
        );
        assert_eq!(rows[0].receipts, 2);
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (19, 0, 0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Record 45 — addendum 11 §1 (verdict 1ee5cb2 §4.2 B/N1): gross 3 (fees 2 + 1), reserve 0,
    // 1000 ppk, one 32-sat proof. The probe on 3 expects popcount(4) = 1 + swap 1 = 2 sats; round
    // 9's gross-probe veto `reserve + expected ≥ gross` (0 + 2 ≥ 3 is false — but with reserve 1
    // it fired, and on 2/0 it fires) sat BEFORE the exact search and refused what a smaller invoice
    // pays. Now the search decides: invoice 1 ⇒ need 1 = [1], prepared 1, target 2 = [2], actual 1,
    // 2 ≥ 1 + 0 + 1; worst 1 + 0 + 1 + 1 = 3 ≤ 3 ⇒ PAYS, once: swap 32 → 2 (fee 1, change
    // [16, 8, 4, 1] = 29), melt [2] for invoice 1, Lightning 0, actual input 1 ⇒ `fee_paid` = 1,
    // no melt change; pool 32 → 29, delta 3 = the gross exactly, never over; row Settled (3, 1).
    #[test]
    fn a_3_sat_gross_with_no_reserve_at_1000_ppk_the_gross_probe_would_veto_pays_invoice_1_once() {
        let (store, root) = store_with_fees("gross-3-witness", &[2, 1]);
        let mut fake = Fake::new(|_| 0);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let registry = quote_registry();
        fake.registry = Some(Arc::clone(&registry));
        fake.melt_results = vec![Ok((1, 0))];
        let before = fake.pool_value().expect("pool");
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        match &outcome {
            RemitOutcome::Paid {
                remittance_id,
                net_sats,
                melt_fee_sats,
            } => {
                assert_eq!(remittance_id, "hash-1-2");
                assert_eq!((*net_sats, *melt_fee_sats), (1, 1));
            }
            other => panic!("expected Paid, got {other:?}\n{out}"),
        }
        assert!(!out.contains("REFUSED"), "{out}");
        assert!(
            out.contains("invoice amount (maxplayer@strike.me receives): 1 sats"),
            "{out}"
        );
        for needle in [
            "actual debit: 3 sats = net + melt fee + swap fee",
            "net paid to maxplayer@strike.me: 1 sats",
            "receipts discharged: 2",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
        assert!(
            !out.contains("WARNING"),
            "delta equals the gross exactly; that is not an overspend:\n{out}"
        );
        assert_eq!(
            fake.invoices,
            vec![3, 1],
            "the gross probe, then the planned net"
        );
        assert_eq!(
            fake.melts,
            vec!["lnbc-fake-1-2".to_owned()],
            "exactly one melt"
        );
        assert_eq!(fake.swaps.len(), 1, "exactly one swap");
        assert_eq!(fake.swaps[0].sent, vec![32]);
        assert_eq!(fake.swaps[0].target_sats, 2);
        assert_eq!(fake.swaps[0].swap_fee_sats, 1);
        assert_eq!(fake.swaps[0].received, vec![2]);
        assert_eq!(fake.swaps[0].change, vec![16, 8, 4, 1]);
        assert!(fake.cancels.is_empty());
        assert!(fake.ceiling_refusals.is_empty(), "the bound admitted it");
        assert!(fake.pay_refusals.is_empty());
        let after = fake.pool_value().expect("pool");
        assert_eq!(
            (before, after),
            (32, 29),
            "change [16, 8, 4, 1], no melt change"
        );
        let delta = before - after;
        assert_eq!(
            delta, 3,
            "invoice 1 + Lightning 0 + ACTUAL input 1 + swap 1 = 3"
        );
        assert!(delta <= 3, "the wallet lost {delta} sats against 3 accrued");
        assert_eq!(
            registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get("paid-quote-lnbc-fake-1-2")
                .map(|quote| quote.state),
            Some(MeltQuoteState::Paid)
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!((rows[0].gross_sats, rows[0].net_sats), (3, 1));
        assert_eq!(
            rows[0].melt_fee_sats,
            Some(1),
            "inclusive: Lightning 0 + actual input 1"
        );
        assert_eq!(rows[0].melt_fee_reserve_sats, Some(0));
        assert_eq!(rows[0].receipts, 2);
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (3, 0, 0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Record 46 — addendum 11 §1, second witness with a non-zero reserve: gross 8 (fees 5 + 3),
    // reserve 5 at estimate and payment, 1000 ppk, one 32-sat proof. Ceiling 8 − 5 = 3; the search:
    // invoice 3 ⇒ need 8 = [8], prepared 1, target 9 = [8, 1], actual 2, worst 3 + 5 + 2 + 1 = 11;
    // invoice 2 ⇒ need 7 = [4, 2, 1], prepared 3, target 10 = [8, 2], actual 2, worst 10; invoice 1
    // ⇒ need 6 = [4, 2], prepared 2, target 8 = [8], actual 1, 8 ≥ 6 + 1, worst 1 + 5 + 1 + 1 = 8 ≤
    // 8 ⇒ PAYS invoice 1, once: swap 32 → 8 (fee 1, change [16, 4, 2, 1] = 23), melt [8], the mint
    // keeps the whole 5-sat reserve as its Lightning fee (worst case) plus actual input 1 ⇒
    // `fee_paid` = 6, melt change 8 − 1 − 5 − 1 = 1; pool 32 → 24, delta 8 = the gross exactly.
    #[test]
    fn an_8_sat_gross_with_a_5_sat_reserve_at_1000_ppk_pays_invoice_1_once_within_the_gross() {
        let (store, root) = store_with_fees("gross-8-witness", &[5, 3]);
        let mut fake = Fake::new(|_| 5);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let registry = quote_registry();
        fake.registry = Some(Arc::clone(&registry));
        fake.melt_results = vec![Ok((1, 5))];
        let before = fake.pool_value().expect("pool");
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        match &outcome {
            RemitOutcome::Paid {
                remittance_id,
                net_sats,
                melt_fee_sats,
            } => {
                assert_eq!(remittance_id, "hash-1-2");
                assert_eq!((*net_sats, *melt_fee_sats), (1, 6));
            }
            other => panic!("expected Paid, got {other:?}\n{out}"),
        }
        assert!(!out.contains("REFUSED"), "{out}");
        assert!(
            out.contains("invoice amount (maxplayer@strike.me receives): 1 sats"),
            "{out}"
        );
        for needle in [
            "actual debit: 8 sats = net + melt fee + swap fee",
            "net paid to maxplayer@strike.me: 1 sats",
            "receipts discharged: 2",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
        assert!(!out.contains("WARNING"), "{out}");
        assert_eq!(
            fake.invoices,
            vec![8, 1],
            "the gross probe, then the planned net"
        );
        assert_eq!(
            fake.melts,
            vec!["lnbc-fake-1-2".to_owned()],
            "exactly one melt"
        );
        assert_eq!(fake.swaps.len(), 1, "exactly one swap");
        assert_eq!(fake.swaps[0].sent, vec![32]);
        assert_eq!(fake.swaps[0].target_sats, 8);
        assert_eq!(fake.swaps[0].swap_fee_sats, 1);
        assert_eq!(fake.swaps[0].received, vec![8]);
        assert_eq!(fake.swaps[0].change, vec![16, 4, 2, 1]);
        assert!(fake.cancels.is_empty());
        assert!(fake.ceiling_refusals.is_empty(), "the bound admitted it");
        assert!(fake.pay_refusals.is_empty());
        let after = fake.pool_value().expect("pool");
        assert_eq!(
            (before, after),
            (32, 24),
            "change [16, 4, 2, 1] + melt change [1]"
        );
        let delta = before - after;
        assert_eq!(
            delta,
            1 + 5 + 1 + 1,
            "invoice + Lightning + ACTUAL input + swap"
        );
        assert!(delta <= 8, "the wallet lost {delta} sats against 8 accrued");
        assert_eq!(
            registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get("paid-quote-lnbc-fake-1-2")
                .map(|quote| quote.state),
            Some(MeltQuoteState::Paid)
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!((rows[0].gross_sats, rows[0].net_sats), (8, 1));
        assert_eq!(
            rows[0].melt_fee_sats,
            Some(6),
            "inclusive: Lightning 5 + actual input 1"
        );
        assert_eq!(rows[0].melt_fee_reserve_sats, Some(5));
        assert_eq!(rows[0].receipts, 2);
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (8, 0, 0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Regression (ii), addendum 9 §1.4 / addendum 10 §1.1: the prepared figures FIT but the
    // post-swap arithmetic does not. Same seller, mint and layout, reserve 3 at estimate AND at
    // payment (so addendum 10 §1.4's re-plan has nothing to do — its schedule, the reserve
    // shrinking 3 → 0, now legitimately re-plans and pays: see `a_reserve_that_shrinks_…` below),
    // but the SDK's PREPARED input fee on the selection is 0 (fee-metadata drift: the selected
    // proofs' keyset reports no fee; the swap lands on the active 1000-ppk keyset), so need = 15 =
    // 8+4+2+1 with a prepared total of 15 + 0 + 1 = 16 ≤ 20 — the prepared total ADMITS it. But
    // confirm would swap to 15 = [8, 4, 2, 1] ⇒ actual input 4 ⇒ needs 19 > 15: pinned CDK refuses
    // AFTER paying the swap (`melt/saga/mod.rs:704–712`) — and on this path that would land after
    // the fence, leaving a bound Spending row held. The §1.1 bound runs that arithmetic BEFORE the
    // fence: refused, the prepared melt cancelled (local), no swap, no melt, wallet delta exactly 0,
    // no request posted (the quote is still UNPAID), the row NOT written (stays Planned, unbound,
    // receipts pinned; the next attempt's reconciliation releases it — addendum 10 §2), one REFUSED
    // line naming the target, the actual fee and the estimate. Until round 8 the
    // fake let the old schedule "succeed" (verdict da0ee92 §4.4).
    #[test]
    fn a_fee_bearing_schedule_whose_prepared_figures_fit_but_post_swap_arithmetic_does_not_is_refused_before_the_fence()
     {
        let (store, root) = store_with_fees("fee-bearing-post-swap", &[10, 10]);
        let mut fake = Fake::new(|_| 3);
        fake.prepared_input_override = Some(0);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let registry = quote_registry();
        fake.registry = Some(Arc::clone(&registry));
        fake.melt_results = vec![Ok((12, 3))];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        match &outcome {
            RemitOutcome::MeltRefused {
                remittance_id,
                reason,
            } => {
                assert_eq!(remittance_id, "hash-12-2");
                assert_eq!(
                    reason,
                    "melt refused before spending: the wallet would swap to 15 sats ([8, 4, 2, 1]) for mint https://mint.example quote paid-quote-lnbc-fake-12-2 and the mint's actual proof input fee on those proofs is 4 sats (prepared estimate 0 sats at 1000 ppk), so 12 sats invoice + 3 sats fee reserve + 4 sats would need 19 sats and the SDK would refuse AFTER paying the 1 sats swap fee; the prepared melt was cancelled before any fee-bearing request"
                );
            }
            other => panic!("expected MeltRefused, got {other:?}\n{out}"),
        }
        assert!(fake.swaps.is_empty(), "no swap was posted");
        assert!(fake.melts.is_empty(), "no melt was posted");
        assert!(
            fake.cancels.is_empty(),
            "since addendum 10 §1.1 the wallet's own gate refuses this preparation (cancelling it inside prepare); nothing prepared reached the caller to cancel"
        );
        assert!(fake.prepared.is_empty());
        assert_eq!(
            fake.ceiling_refusals.len(),
            1,
            "the one gate — the actual-confirmability bound — refused it at prepare"
        );
        assert!(
            fake.pay_refusals.is_empty(),
            "the wallet never got to refuse"
        );
        assert_eq!(
            fake.pool_value(),
            Some(32),
            "wallet delta exactly 0: no swap, no melt"
        );
        assert!(fake.proofs_spent.is_empty());
        assert_eq!(
            fake.melt_results.len(),
            1,
            "the scripted payment was never consumed"
        );
        assert!(fake.admitted_seen.is_empty(), "refused before the fence");
        assert_eq!(
            out.matches("REFUSED before spending").count(),
            1,
            "exactly one refusal line:\n{out}"
        );
        assert!(
            !out.contains("Prepared melt of quote"),
            "refused before the prepared line:\n{out}"
        );
        assert!(!out.contains("WARNING"), "{out}");
        let quote = registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get("paid-quote-lnbc-fake-12-2")
            .cloned()
            .expect("the payment quote was raised");
        assert_eq!(
            quote.state,
            MeltQuoteState::Unpaid,
            "no request reached the mint"
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_refused_before_fence_row_stays_planned(&out, &rows[0], 20, "quote-lnbc-fake-12-2");
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Failed);
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (0, 20, 0),
            "nothing paid; the gross is in flight on the planned row until the next attempt reconciles it (addendum 10 §2)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Fee-exceeds refusal (addendum 8 §1.5 (ii), kept by addendum 9 §1.4 (iii)): the same seller, mint and layout (plan: invoice 12, ceiling
    // 20), but the payment quote's reserve grew to 7: 12 + 7 = 19 passes the two-figure check
    // (≤ 20), yet need = 19 = 16+2+1 ⇒ three output proofs ⇒ input fee 3; selection 22 ⇒ swap fee 1;
    // total 19 + 3 + 1 = 23 > 20 — over, by the fees. The actual-arithmetic bound refuses BEFORE any
    // swap or melt: wallet delta exactly 0, no request posted (the registry's quote is still
    // UNPAID), the row NOT written (stays Planned, unbound, receipts pinned; released by the next
    // attempt's reconciliation — addendum 10 §2), one REFUSED line naming the total, its parts and
    // the ceiling.
    #[test]
    fn a_fee_bearing_total_that_exceeds_the_gross_by_the_fee_is_refused_before_any_swap_or_melt() {
        let (store, root) = store_with_fees("fee-bearing-over", &[10, 10]);
        let mut fake = Fake::new(|_| 3);
        fake.live_reserve_for = Some(Box::new(|_| 7));
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let registry = quote_registry();
        fake.registry = Some(Arc::clone(&registry));
        fake.melt_results = vec![Ok((12, 1))];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Collect, 100);
        match &outcome {
            RemitOutcome::MeltRefused {
                remittance_id,
                reason,
            } => {
                assert_eq!(remittance_id, "hash-12-2");
                assert!(
                    reason.contains("would debit 23 sats in total (12 sats invoice + 7 sats fee reserve + 3 sats proof input fee + 1 sats swap fee) against a ceiling of 20 sats; the prepared melt was cancelled and its proofs released; nothing was posted to the mint"),
                    "{reason}"
                );
            }
            other => panic!("expected MeltRefused, got {other:?}\n{out}"),
        }
        assert!(fake.melts.is_empty(), "no melt");
        assert_eq!(
            fake.ceiling_refusals.len(),
            1,
            "refused by the total bound, once"
        );
        assert!(
            fake.cancels.is_empty(),
            "cancelled inside prepare, not by the fence"
        );
        assert!(fake.prepared.is_empty());
        assert_eq!(
            fake.pool_value(),
            Some(32),
            "wallet delta exactly 0: no swap, no melt"
        );
        assert_eq!(
            fake.melt_results.len(),
            1,
            "the scripted payment was never consumed"
        );
        assert!(fake.admitted_seen.is_empty(), "refused before the fence");
        assert_eq!(
            out.matches("REFUSED before spending").count(),
            1,
            "exactly one refusal line:\n{out}"
        );
        assert!(
            out.contains("REFUSED before spending — melt refused before spending: mint https://mint.example quote paid-quote-lnbc-fake-12-2 would debit 23 sats in total (12 sats invoice + 7 sats fee reserve + 3 sats proof input fee + 1 sats swap fee) against a ceiling of 20 sats"),
            "{out}"
        );
        assert!(!out.contains("Prepared melt of quote"), "{out}");
        let quote = registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get("paid-quote-lnbc-fake-12-2")
            .cloned()
            .expect("the payment quote was raised");
        assert_eq!(
            quote.state,
            MeltQuoteState::Unpaid,
            "no request reached the mint"
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_refused_before_fence_row_stays_planned(&out, &rows[0], 20, "quote-lnbc-fake-12-2");
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Failed);
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (0, 20, 0),
            "nothing paid; the gross is in flight on the planned row until the next attempt reconciles it (addendum 10 §2)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Addendum 9 §1.2, fee-aware planning: the same fee-bearing mint and layout with NO reserve
    // slack (estimate reserve 2, payment reserve 2). Without the fees in the plan the invoice would
    // be 18 and the payment total 18 + 2 + 2 + 1 = 23 > 20 — refused every time, forever. Round 7
    // planned 14 (20 − 2 − probe's 4): its prepared figures 14 + 2 + 1 + 1 = 18 fit, but confirm would
    // swap to 17 = [16, 1] ⇒ actual input 2 ⇒ 18 > 17 — CDK refuses after the swap (verdict da0ee92
    // §4.4). Planning now searches down from 18 for the first invoice whose post-swap arithmetic
    // holds: 13 (need 15 = 8+4+2+1 ⇒ prepared 4 ⇒ target 19 = [16, 2, 1] ⇒ actual 3; 19 ≥ 18; worst
    // 15 + 3 + 1 = 19 ≤ 20). Re-estimate on 15: input 4 + swap 1 = 5 ⇒ the old ceiling line says 20.
    // Payment: prepared 13 + 2 + 4 + 1 = 20 ≤ 20 admitted; swap to 19 (fee 1, change [8, 4]),
    // actual 3, needs 18 ≤ 19, melts; the mint takes its full 2-sat reserve and returns 19 − 13 − 2 −
    // 3 = 1 as change; `fee_paid` = 19 − 13 − 1 = 5 = Lightning 2 + actual input 3. Wallet 32 → 12 +
    // 1 = 13: delta 19 = 13 + 2 + 3 + 1 ≤ 20, measured.
    #[test]
    fn fee_aware_planning_sizes_the_invoice_so_a_fee_bearing_payment_fits_without_reserve_slack() {
        let (store, root) = store_with_fees("fee-aware-plan", &[10, 10]);
        let mut fake = Fake::new(|_| 2);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        fake.registry = Some(quote_registry());
        fake.melt_results = vec![Ok((13, 2))];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::DryRun, 100);
        assert_eq!(outcome, RemitOutcome::DryRun, "{out}");
        for needle in [
            "unremitted platform fee (gross): 20 sats",
            "mint melt fee reserve (bounds the Lightning fee): 2 sats — taken out of the gross, never on top",
            "invoice amount (maxplayer@strike.me receives): 13 sats",
            "leaves your wallet: at most 19 sats (≤ 20); unused reserve returns as change",
            "expected proof fees (SDK estimate, bounded exactly at payment): 5 sats = estimated proof input fee 4 sats + swap fee 1 sats; actual proof input fee the SDK recomputes on the swapped proofs: 3 sats ⇒ worst case 19 sats leaves the wallet (≤ 20) = invoice 13 + reserve 2 (bounds the Lightning fee) + actual proof input fee 3 + swap fee 1; the inclusive melt fee (Lightning + actual proof input fee) is known only at payment",
            "DRY RUN — nothing moved. Re-run with --confirm to pay 13 sats to maxplayer@strike.me.",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
        assert_eq!(
            fake.invoices,
            vec![20, 13],
            "probe on the gross, then the invoice planning chose — not 14"
        );
        assert_eq!(fake.pool_value(), Some(32), "estimating reserves nothing");
        assert!(fake.swaps.is_empty() && fake.melts.is_empty());

        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 101);
        assert!(is_paid(&outcome), "{out}");
        assert_eq!(fake.melts, vec!["lnbc-fake-13-4".to_owned()], "one melt");
        assert_eq!(fake.swaps.len(), 1, "one swap");
        assert_eq!(fake.swaps[0].received, vec![16, 2, 1]);
        assert_eq!(fake.swaps[0].change, vec![8, 4]);
        assert!(
            out.contains("Prepared melt of quote paid-quote-lnbc-fake-13-4: proof input fee 4 sats (estimate; actual on the swapped proofs 3 sats), swap fee 1 sats (the wallet's proofs do not fit: a pre-melt swap will be performed); total debit 19 sats (13 invoice + 2 reserve + fees) fits the ceiling of 20 sats"),
            "{out}"
        );
        assert_eq!(
            fake.pool_value(),
            Some(13),
            "32 − 1 swap fee − 13 − 2 Lightning fee − 3 ACTUAL input fee"
        );
        assert!(
            out.contains("actual debit: 19 sats = net + melt fee + swap fee")
                && out.contains("stays in your wallet (unused reserve): 1 sats"),
            "{out}"
        );
        assert!(!out.contains("WARNING"), "{out}");
        let lost = 32 - fake.pool_value().expect("pool");
        assert_eq!(lost, 13 + 2 + 3 + 1);
        assert!(lost <= 20, "the wallet lost {lost} sats against 20 accrued");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!((rows[0].gross_sats, rows[0].net_sats), (20, 13));
        assert_eq!(
            rows[0].melt_fee_sats,
            Some(5),
            "`fee_paid`, inclusive: Lightning 2 + actual input 3"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The planning refusal for fees that can never fit (addendum 8 §1.3, re-sited by addendum 11
    // §1): a 1000-ppk mint against a 3-sat gross with a 1-sat reserve. No probe-side fee veto any
    // more — the exact search runs every candidate: invoice 2 ⇒ need 3, prepared 2, target 5 =
    // [4, 1], actual 2, worst 2 + 1 + 2 + 1 = 6 > 3; invoice 1 ⇒ need 2, prepared 1, target 3 =
    // [2, 1], actual 2, worst 1 + 1 + 2 + 1 = 5 > 3. NO candidate fits ⇒ FeesDoNotFit with the
    // probe's expected 2 (popcount(4) = 1 + swap 1) on the "no invoice fits" line, no invoice for
    // any net, nothing journaled, the balance intact.
    #[test]
    fn fees_that_can_never_fit_are_refused_at_planning_not_at_payment() {
        let (store, root) = store_with_fees("fee-never-fits", &[3]);
        let mut fake = Fake::new(|_| 1);
        fake.input_fee_ppk = 1000;
        fake.proofs = Some(fake_proofs(&[32]));
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Command, 100);
        assert_eq!(
            outcome,
            RemitOutcome::Refused(Refusal::FeesDoNotFit {
                gross: 3,
                reserve: 1,
                expected_fees: 2,
            }),
            "{out}"
        );
        assert!(
            out.contains("REFUSED — no invoice fits: at mint https://mint.example's 1000 ppk proof fee, no amount up to 2 sats (3 sats less the 1 sats melt fee reserve) can be paid for at most 3 sats once the SDK recomputes its proof input fee on the swapped proofs plus 2 sats of expected proof fees. The balance accumulates. Nothing moved."),
            "{out}"
        );
        assert!(
            !out.contains("which leaves nothing for the destination"),
            "the reserve-only veto did not fire (1 < 3):\n{out}"
        );
        assert_eq!(fake.invoices, vec![3], "only the probe");
        assert!(fake.quotes.is_empty() && fake.melts.is_empty());
        assert_eq!(fake.pool_value(), Some(32));
        assert!(store.remittances().expect("rows").is_empty());
        assert_eq!(
            Refusal::FeesDoNotFit {
                gross: 3,
                reserve: 1,
                expected_fees: 2
            }
            .to_string(),
            "the mint's melt fee reserve (1 sats) plus the expected proof fees (2 sats) do not fit inside the 3 sats accrued"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- addendum 3 §1: the money hold, at the moment of spending (gate 2f) --------------------

    // Gate 2f: gross 15, the ESTIMATE quotes a 2-sat reserve (invoice 13, ceiling 15 holds), but the
    // quote the mint raises FOR THE PAYMENT carries a 4-sat reserve: 13 + 4 = 17 > 15. The melt is
    // REFUSED before any proof is spent — zero debits — the attempt is journaled FAILED naming the
    // row, the row stays planned with its receipts pinned (the next attempt's reconciliation
    // releases it — addendum 10 §2), and nothing left the wallet. Then the same seller
    // with a payment-time reserve that FITS pays exactly once.
    #[test]
    fn a_reserve_that_grows_between_estimate_and_payment_is_refused_before_spending() {
        let (store, root) = store_with_fees("ceiling-refused", &[10, 5]);
        let mut fake = Fake::new(|_| 2);
        fake.live_reserve_for = Some(Box::new(|_| 4));
        fake.melt_results = vec![Ok((13, 1))];
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Collect, 100);
        match &outcome {
            RemitOutcome::MeltRefused {
                remittance_id,
                reason,
            } => {
                assert_eq!(remittance_id, "hash-13-2");
                assert!(
                    reason.contains("would debit 17 sats (13 sats invoice + 4 sats fee reserve; planned invoice 13 sats) against a ceiling of 15 sats; nothing left the wallet"),
                    "{reason}"
                );
            }
            other => panic!("expected MeltRefused, got {other:?}\n{out}"),
        }
        assert!(
            fake.melts.is_empty(),
            "ZERO debits: the refusal happens before the proofs are touched"
        );
        // Addendum 5 §1 rule 1: the payment quote was raised and checked BEFORE the fence — the row
        // was never admitted; it is left planned, unbound and pinned (addendum 10 §2), not released
        // here.
        assert_eq!(fake.quotes, vec!["lnbc-fake-13-2".to_owned()]);
        assert!(fake.admitted_seen.is_empty(), "refused before the fence");
        assert!(
            fake.ceiling_refusals.is_empty(),
            "the wallet's own re-check never ran: nothing reached the payment"
        );
        assert_eq!(
            fake.melt_results.len(),
            1,
            "the scripted payment was never consumed"
        );
        assert!(
            out.contains("REFUSED before spending — melt refused before spending"),
            "{out}"
        );
        // Journaled as a FAILED attempt naming the row; the row itself is NOT written (addendum 10
        // §2): it stays planned with its receipts pinned, for the next attempt to reconcile.
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Failed);
        assert_eq!(attempts[0].remittance_id.as_deref(), Some("hash-13-2"));
        assert!(
            attempts[0]
                .detail
                .starts_with("refused before spending: melt refused before spending"),
            "{}",
            attempts[0].detail
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_refused_before_fence_row_stays_planned(&out, &rows[0], 15, "quote-lnbc-fake-13-2");
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (0, 15, 0),
            "nothing paid; the gross is in flight on the planned row until the next attempt reconciles it"
        );
        // It is a failure for the pacing: the backoff escalates.
        let report = RemitReport {
            outcome: Ok(outcome),
            lines: Vec::new(),
        };
        assert!(report.is_failure());
        assert!(
            report
                .summary()
                .starts_with("melt REFUSED before spending (")
        );

        // The payment-time reserve fits (2: 13 + 2 = 15 ≤ 15): pays exactly once, and the
        // settlement records the PAYING quote's reserve beside the actual fee.
        fake.live_reserve_for = Some(Box::new(|_| 2));
        let (outcome, out) = run_remit(&store, &mut fake, RemitTrigger::Retry, 101);
        assert!(is_paid(&outcome), "{out}");
        assert_eq!(fake.melts.len(), 1, "exactly one debit, ever");
        // Addendum 10 §2: the next attempt FIRST reconciles the refused row — released as this
        // process's own earlier attempt (planned → failed, receipts back) — THEN plans a new row
        // and pays it: [Failed, Settled], the receipts discharged by the second.
        let release_at = out
            .find("the row is this process's own earlier attempt, which is over")
            .unwrap_or_else(|| panic!("the refused row is reconciled first:\n{out}"));
        let plan_at = out
            .find("Journaled remittance hash-13-4")
            .unwrap_or_else(|| panic!("a new row is planned:\n{out}"));
        assert!(release_at < plan_at, "release before the new plan:\n{out}");
        assert!(
            out.contains("melt fee taken by the mint: 1 sats (quote paid-quote-lnbc-fake-13-4 reserved 2 sats; ceiling 15 sats held at the moment of spending; this is the SDK's fee_paid = Lightning fee + actual proof input fee)"),
            "{out}"
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].remittance_id, "hash-13-2");
        assert_eq!(
            rows[0].state,
            RemittanceState::Failed,
            "released by reconciliation"
        );
        assert_eq!(rows[0].receipts, 0);
        assert_eq!(rows[1].remittance_id, "hash-13-4");
        assert_eq!(rows[1].receipts, 2);
        assert_eq!(rows[1].state, RemittanceState::Settled);
        assert_eq!(rows[1].melt_fee_sats, Some(1));
        assert_eq!(rows[1].melt_fee_reserve_sats, Some(2));
        assert_eq!(
            rows[1].settled_by,
            Some(crate::seller_node::store::SettledBy::Melt)
        );
        assert_eq!(
            rows[1].melt_quote_id,
            Some("paid-quote-lnbc-fake-13-4".to_owned()),
            "the quote that actually paid is the one recorded"
        );
        assert_eq!(store.accrued_fees().expect("read").remitted_fee_sats, 15);

        // Exactly at the ceiling is admitted; one sat over is not (the pure rule the melt applies).
        let ceiling = MeltCeiling {
            max_debit_sats: 15,
            invoice_sats: 13,
            planned_quote_id: None,
        };
        assert!(ceiling.admits(13, 2));
        assert!(!ceiling.admits(13, 3));
        assert!(
            !ceiling.admits(12, 0),
            "a quote for a different amount than planned is refused too"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- addendum 3 §2: ownership — never release a live payer's intent (gate 2g) ---------------

    /// A quote status whose expiry is far in the future: live until a test says otherwise.
    fn status(state: MeltQuoteState, quote_id: &str) -> MeltQuoteStatus {
        MeltQuoteStatus {
            mint_url: "https://mint.example".to_owned(),
            quote_id: quote_id.to_owned(),
            state,
            amount_sats: 13,
            fee_reserve_sats: 2,
            expiry_unix: u64::MAX,
        }
    }

    fn planned_row(owner: Option<&str>, lease_until: Option<i64>) -> FeeRemittance {
        FeeRemittance {
            remittance_id: "x".to_owned(),
            gross_sats: 15,
            melt_fee_sats: None,
            melt_fee_reserve_sats: Some(2),
            net_sats: 13,
            destination: PLATFORM_FEE_ADDRESS.to_owned(),
            melt_quote_id: Some("q".to_owned()),
            payment_hash: "x".to_owned(),
            bolt11: "ln-x".to_owned(),
            state: RemittanceState::Planned,
            created_at_unix: 100,
            settled_at_unix: None,
            settled_by: None,
            owner: owner.map(str::to_owned),
            lease_until_unix: lease_until,
            spending_since_unix: None,
            spending_quote_id: None,
            receipts: 2,
        }
    }

    /// A row whose owner's compare-and-set admitted the melt at `since` and BOUND the payment
    /// quote `q-bound` to it (addendum 4 §1.2, addendum 5 §1).
    fn spending_row(owner: &str, lease_until: i64, since: i64) -> FeeRemittance {
        FeeRemittance {
            state: RemittanceState::Spending,
            spending_since_unix: Some(since),
            spending_quote_id: Some("q-bound".to_owned()),
            ..planned_row(Some(owner), Some(lease_until))
        }
    }

    /// The conditional transition a decision carries, or `None` when it is not a release.
    fn release_on(decision: &Reconcile) -> Option<ReleaseOn> {
        match decision {
            Reconcile::Release { on, .. } => Some(on.clone()),
            _ => None,
        }
    }

    // The release rule as a table: PAID settles; PENDING/UNKNOWN hold; FAILED releases whoever owns
    // the row; UNPAID / no quote release only for the row's own process or after the lease — and
    // HOLD while another process's lease stands. A pre-v11 row (no owner, no lease) reads as expired.
    #[test]
    fn reconciliation_releases_only_terminal_quotes_own_rows_or_expired_leases() {
        let theirs = planned_row(Some("proc-b"), Some(400));
        let mine = planned_row(Some("proc-a"), Some(400));
        let legacy = planned_row(None, None);
        let paid = status(MeltQuoteState::Paid, "q-paid");
        let pending = status(MeltQuoteState::Pending, "q-pending");
        let unknown = status(MeltQuoteState::Unknown, "q-unknown");
        let failed = status(MeltQuoteState::Failed, "q-failed");
        let unpaid = status(MeltQuoteState::Unpaid, "q-unpaid");

        assert_eq!(
            reconcile_decision(&theirs, Some(&paid), "proc-a", 200),
            Reconcile::Settle
        );
        assert_eq!(
            reconcile_decision(&theirs, Some(&pending), "proc-a", 200),
            Reconcile::Hold(Refusal::Settling {
                remittance_id: "x".to_owned()
            })
        );
        assert_eq!(
            reconcile_decision(&mine, Some(&unknown), "proc-a", 200),
            Reconcile::Hold(Refusal::Settling {
                remittance_id: "x".to_owned()
            }),
            "unknown is not terminal, even on our own row"
        );
        assert!(matches!(
            reconcile_decision(&theirs, Some(&failed), "proc-a", 200),
            Reconcile::Release { reason, .. }
                if reason.contains("FAILED at the mint, and the row was never admitted to spend")
        ));
        // UNPAID / no quote, another process's live lease: HOLD — "not yet" is not "abandoned".
        assert_eq!(
            reconcile_decision(&theirs, Some(&unpaid), "proc-a", 200),
            Reconcile::Hold(Refusal::HeldByOwner {
                remittance_id: "x".to_owned(),
                owner: "proc-b".to_owned(),
                lease_until_unix: 400,
            })
        );
        assert_eq!(
            reconcile_decision(&theirs, None, "proc-a", 399),
            Reconcile::Hold(Refusal::HeldByOwner {
                remittance_id: "x".to_owned(),
                owner: "proc-b".to_owned(),
                lease_until_unix: 400,
            }),
            "one second before the lease ends it still holds"
        );
        // …and RELEASE once the lease has run out (the owner is provably gone or not spending) —
        // by the LEASE transition, which carries the clock it was decided on and applies to a
        // planned row only (addendum 5 §1, rule 2).
        let on_lease = reconcile_decision(&theirs, Some(&unpaid), "proc-a", 400);
        assert!(matches!(
            &on_lease,
            Reconcile::Release { reason, .. } if reason.contains("lease ran out at unix 400")
        ));
        assert_eq!(
            release_on(&on_lease),
            Some(ReleaseOn::LeaseExpired { now_unix: 400 })
        );
        assert_eq!(
            release_on(&reconcile_decision(&theirs, None, "proc-a", 401)),
            Some(ReleaseOn::LeaseExpired { now_unix: 401 }),
            "never raised a melt quote — released on the lease"
        );
        // Our own row: release on UNPAID / none at any time — our earlier attempt is over — by the
        // OWN-PLANNED transition, naming us.
        let own = reconcile_decision(&mine, Some(&unpaid), "proc-a", 101);
        assert!(matches!(
            &own,
            Reconcile::Release { reason, .. } if reason.contains("this process's own earlier attempt")
        ));
        assert_eq!(
            release_on(&own),
            Some(ReleaseOn::OwnPlanned {
                owner: "proc-a".to_owned()
            })
        );
        assert_eq!(
            release_on(&reconcile_decision(&mine, None, "proc-a", 101)),
            Some(ReleaseOn::OwnPlanned {
                owner: "proc-a".to_owned()
            })
        );
        // Terminal quotes on a PLANNED row release by the planned-terminal transition.
        assert_eq!(
            release_on(&reconcile_decision(&theirs, Some(&failed), "proc-a", 200)),
            Some(ReleaseOn::TerminalQuotePlanned)
        );
        // A pre-v11 row: nobody's, lease expired ⇒ releasable on UNPAID / none, settled on PAID.
        assert!(matches!(
            reconcile_decision(&legacy, Some(&unpaid), "proc-a", 101),
            Reconcile::Release { reason, .. } if reason.contains("owner none recorded")
        ));
        assert_eq!(
            reconcile_decision(&legacy, Some(&paid), "proc-a", 101),
            Reconcile::Settle
        );
        assert!(
            !Refusal::HeldByOwner {
                remittance_id: "x".to_owned(),
                owner: "proc-b".to_owned(),
                lease_until_unix: 400,
            }
            .is_threshold()
        );
        // An UNPAID quote whose expiry is behind the clock is terminal for a PLANNED row too.
        let mut unpaid_expired = status(MeltQuoteState::Unpaid, "q-expired");
        unpaid_expired.expiry_unix = 150;
        assert!(matches!(
            reconcile_decision(&theirs, Some(&unpaid_expired), "proc-a", 151),
            Reconcile::Release { reason, .. } if reason.contains("the quote expired at unix 150")
        ));
        assert_eq!(
            reconcile_decision(&theirs, Some(&unpaid_expired), "proc-a", 150),
            Reconcile::Hold(Refusal::HeldByOwner {
                remittance_id: "x".to_owned(),
                owner: "proc-b".to_owned(),
                lease_until_unix: 400,
            }),
            "at the expiry second the quote is still live"
        );
    }

    // The SPENDING row's rule as a pure table (kept as an extra beside the full-path gate 2g (d)
    // below — addendum 5 §2, addendum 6 §1.2): the status is the BOUND quote's. PAID settles.
    // Everything else HOLDS — FAILED, UNPAID live, UNPAID expired by a second or by ten thousand,
    // no quote at all — whoever owns the row and however long ago its lease ran out; PENDING /
    // UNKNOWN hold as for any row. No clock appears in the bound-spending rule: the mint pays an
    // UNPAID or FAILED quote regardless of its expiry, so no observation proves the bound quote
    // cannot still debit. Only a spending row admitted before quotes were bound keeps the v12
    // release (unbound-spending transition), out of this round's scope.
    #[test]
    fn a_spending_row_is_never_released_by_reconciliation_only_settled() {
        let theirs = spending_row("proc-b", 400, 150);
        let mine = spending_row("proc-a", 400, 150);
        let paid = status(MeltQuoteState::Paid, "q-bound");
        let pending = status(MeltQuoteState::Pending, "q-bound");
        let unknown = status(MeltQuoteState::Unknown, "q-bound");
        let failed = status(MeltQuoteState::Failed, "q-bound");
        let unpaid_live = status(MeltQuoteState::Unpaid, "q-bound");
        let mut unpaid_expired = status(MeltQuoteState::Unpaid, "q-bound");
        unpaid_expired.expiry_unix = 900;

        assert_eq!(
            reconcile_decision(&theirs, Some(&paid), "proc-a", 10_000),
            Reconcile::Settle
        );
        // HOLD on everything but PAID — theirs AND ours, live or expired, FAILED or absent.
        let held = |row: &FeeRemittance, owner: &str, seen: Option<&MeltQuoteStatus>| {
            Reconcile::Hold(Refusal::SpendingHeld {
                remittance_id: row.remittance_id.clone(),
                owner: owner.to_owned(),
                spending_since_unix: 150,
                quote_id: row.spending_quote_id.clone(),
                observed: match seen {
                    None => "this wallet holds no such melt quote".to_owned(),
                    Some(seen) => format!(
                        "mint https://mint.example reports melt quote {} {} (expiry unix {})",
                        seen.quote_id, seen.state, seen.expiry_unix
                    ),
                },
                held_sats: 15,
            })
        };
        assert_eq!(
            reconcile_decision(&theirs, Some(&unpaid_live), "proc-a", 10_000),
            held(&theirs, "proc-b", Some(&unpaid_live))
        );
        assert_eq!(
            reconcile_decision(&mine, Some(&unpaid_live), "proc-a", 10_000),
            held(&mine, "proc-a", Some(&unpaid_live)),
            "our own spending row: the melt that errored may have reached the mint"
        );
        for now in [900, 960, 961, 10_000, i64::MAX] {
            assert_eq!(
                reconcile_decision(&mine, Some(&unpaid_expired), "proc-a", now),
                held(&mine, "proc-a", Some(&unpaid_expired)),
                "UNPAID past expiry (900) is held at {now}: no clock releases a bound spending row"
            );
            assert_eq!(
                reconcile_decision(&theirs, Some(&failed), "proc-a", now),
                held(&theirs, "proc-b", Some(&failed)),
                "FAILED is held at {now}: the mint pays a FAILED quote, so it is not cancellation"
            );
        }
        assert_eq!(
            reconcile_decision(&theirs, None, "proc-a", 10_000),
            held(&theirs, "proc-b", None),
            "no quote is not the mint saying terminal"
        );
        assert!(
            release_on(&reconcile_decision(&mine, Some(&failed), "proc-a", 10_000)).is_none()
                && release_on(&reconcile_decision(
                    &theirs,
                    Some(&unpaid_expired),
                    "proc-a",
                    i64::MAX
                ))
                .is_none(),
            "no decision on a bound spending row carries a ReleaseOn"
        );
        // PENDING / UNKNOWN on a bound spending row: the same HELD refusal as every other non-PAID
        // answer, never the planned row's "settling" (addendum 7 §2).
        assert_eq!(
            reconcile_decision(&theirs, Some(&pending), "proc-a", 10_000),
            held(&theirs, "proc-b", Some(&pending)),
            "PENDING holds a bound spending row as HELD"
        );
        assert_eq!(
            reconcile_decision(&mine, Some(&unknown), "proc-a", 10_000),
            held(&mine, "proc-a", Some(&unknown)),
            "UNKNOWN holds a bound spending row as HELD, ours included"
        );
        // A spending row admitted before v13 (no bound quote): the status is the invoice's; FAILED
        // or plain expiry releases it by the unbound-spending transition, as v12 did.
        let unbound = FeeRemittance {
            spending_quote_id: None,
            ..spending_row("proc-b", 400, 150)
        };
        assert_eq!(
            release_on(&reconcile_decision(&unbound, Some(&failed), "proc-a", 200)),
            Some(ReleaseOn::TerminalUnboundSpending)
        );
        assert_eq!(
            release_on(&reconcile_decision(
                &unbound,
                Some(&unpaid_expired),
                "proc-a",
                901
            )),
            Some(ReleaseOn::TerminalUnboundSpending)
        );
        assert_eq!(
            reconcile_decision(&unbound, Some(&unpaid_live), "proc-a", 10_000),
            held(&unbound, "proc-b", Some(&unpaid_live))
        );
        assert!(
            !Refusal::SpendingHeld {
                remittance_id: "x".to_owned(),
                owner: "proc-b".to_owned(),
                spending_since_unix: 150,
                quote_id: Some("q-bound".to_owned()),
                observed: String::new(),
                held_sats: 15,
            }
            .is_threshold(),
            "a held spending row is a refusal an operator should see, and a failure for pacing"
        );
    }

    /// The paused side's (outcome, output) and every `meanwhile` run's (outcome, output).
    type PausedRun = (
        (Result<RemitOutcome, String>, String),
        Vec<(RemitOutcome, String)>,
    );

    /// Where the paused side stops (addendum 4 §1, addendum 5 §2, addendum 6 §2.1 tests): after
    /// its plan is journaled and BEFORE its payment quote; after its payment quote passed the
    /// ceiling and BEFORE the fence (the row is still `planned`, the quote exists at the mint);
    /// after the fence admitted it (the row is `spending`, bound to that quote) and BEFORE the
    /// melt; or INSIDE the melt, after the wallet's last local check and BEFORE the request reaches
    /// the mint.
    #[derive(Clone, Copy)]
    enum PauseAt {
        /// After the plan is journaled (`Fake::plan_gate`).
        Plan,
        /// After the payment quote passed the ceiling, before the fence (`Fake::quote_gate`).
        Quote,
        /// After the fence admitted the melt and bound the quote, before paying
        /// (`Fake::admit_gate`).
        Admit,
        /// Inside the payment: after the ceiling, the proof selection and `prepare_melt`'s expiry
        /// check, before the mint sees the request (`Fake::melt_gate`) — the verdict's B3 pause.
        Melt,
    }

    /// Two processes against one store: `first` is paused at `gate` (at `pause`), `second` runs
    /// whatever the test scripts meanwhile — including moving the clock `first` will read FRESH at
    /// its fence when it resumes (`first.clock`, cloned by the test before `first` is moved in).
    /// Returns each side's outcome and output.
    fn run_paused(
        db: &PathBuf,
        mut first: Fake,
        first_now: i64,
        pause: PauseAt,
        gate: Arc<super::test_support::Gate>,
        melts: Arc<AtomicUsize>,
        meanwhile: impl FnOnce(&SellerStore) -> Vec<(RemitOutcome, String)>,
    ) -> PausedRun {
        match pause {
            PauseAt::Plan => first.plan_gate = Some(Arc::clone(&gate)),
            PauseAt::Quote => first.quote_gate = Some(Arc::clone(&gate)),
            PauseAt::Admit => first.admit_gate = Some(Arc::clone(&gate)),
            PauseAt::Melt => first.melt_gate = Some(Arc::clone(&gate)),
        }
        first.melt_counter = Some(Arc::clone(&melts));
        first.set_clock(first_now);
        let db_a = db.clone();
        let a = std::thread::spawn(move || {
            let store = SellerStore::open(&db_a).expect("open A");
            let mut out = Vec::new();
            let outcome = remit(
                &store,
                &mut first,
                RemitTrigger::Collect,
                first_now,
                &mut out,
            );
            (outcome, String::from_utf8_lossy(&out).into_owned())
        });
        gate.wait_arrived(Duration::from_secs(10));
        let store_b = SellerStore::open(db).expect("open B");
        let b = meanwhile(&store_b);
        gate.release();
        let a = a.join().expect("A's thread");
        (a, b)
    }

    // Gate 2g (a), the live owner: process A journals its plan (invoice X) and PAUSES before
    // spending. Process B — another owner, another connection, distinct invoices, reading the SAME
    // fake mint through the shared quote registry (addendum 5 §2, addendum 6 §2.2: no scripted
    // status) — runs reconciliation and then `--confirm`: it asks the mint about X's invoice, finds
    // the estimate quote A raised UNPAID, sees X planned by a LIVE owner, and HOLDS. It plans
    // nothing, pays nothing. A resumes, passes the pre-spend gate, pays X once. Exactly one debit
    // (melts counted, not settlement reports); one settled row; B's refusals journaled.
    #[test]
    fn a_second_process_cannot_release_a_live_owners_planned_row_and_exactly_one_debit_happens() {
        let (store, root) = store_with_fees("live-owner", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let registry = quote_registry();
        let (a_result, b_results) = run_paused(
            &db,
            first_process(&registry),
            100,
            PauseAt::Plan,
            super::test_support::Gate::new(),
            Arc::clone(&melts),
            |store_b| {
                {
                    // What the mint holds while A is paused after its plan: A's two estimate
                    // quotes (probe on the gross, then the net invoice), both UNPAID; no payment
                    // quote yet.
                    let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(quotes[X_ESTIMATE_QUOTE].state, MeltQuoteState::Unpaid);
                    assert!(!quotes.contains_key(X_PAYMENT_QUOTE));
                }
                let mut results = Vec::new();
                for (trigger, now) in [(RemitTrigger::DryRun, 110), (RemitTrigger::Command, 111)] {
                    let mut b = second_process(&registry, &melts);
                    let (outcome, out) = run_remit(store_b, &mut b, trigger, now);
                    assert_eq!(
                        b.status_calls,
                        vec![X_BOLT11.to_owned()],
                        "B asks the shared mint about X's invoice: {out}"
                    );
                    assert!(
                        b.quote_status_calls.is_empty(),
                        "a planned row has no bound quote to ask about: {out}"
                    );
                    assert!(b.melts.is_empty(), "B must not pay: {out}");
                    assert!(
                        b.invoices.is_empty() && b.quotes.is_empty(),
                        "B must not even plan on top of a held row: {out}"
                    );
                    assert_eq!(ledger(store_b), (0, 15, 0), "receipts pinned to X");
                    results.push((outcome, out));
                }
                results
            },
        );
        for (outcome, out) in &b_results {
            assert_eq!(
                outcome,
                &RemitOutcome::Refused(Refusal::HeldByOwner {
                    remittance_id: "hash-13-2-a".to_owned(),
                    owner: "proc-a".to_owned(),
                    lease_until_unix: 400,
                }),
                "{out}"
            );
            assert!(
                out.contains("is planned by another live process (proc-a, lease until unix 400) and its quote is not terminal; not releasing a live payer's intent. REFUSED"),
                "{out}"
            );
        }
        let (a_outcome, a_out) = a_result;
        assert!(
            matches!(a_outcome, Ok(RemitOutcome::Paid { .. })),
            "A pays once it resumes: {a_outcome:?}\n{a_out}"
        );
        assert_eq!(melts.load(Ordering::SeqCst), 1, "exactly one actual debit");
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1, "one row, A's: {rows:?}");
        assert_eq!(rows[0].remittance_id, "hash-13-2-a");
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!(rows[0].owner.as_deref(), Some("proc-a"));
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (15, 0, 0)
        );
        // B's --confirm refusal is journaled (the dry run is not an attempt).
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Paid);
        assert_eq!(attempts[1].trigger, RemitAttemptTrigger::Command);
        assert_eq!(attempts[1].outcome, RemitAttemptOutcome::Refused);
        assert_eq!(attempts[1].remittance_id.as_deref(), Some("hash-13-2-a"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Gate 2g (b), addendum 4 §1 — the dangerous ordering the verdict traced: A journals X, its
    // fence ADMITS the melt (X is spending) and A pauses BEFORE the spend. The clock then moves past
    // A's lease (100 + 300 = 400 → 400 and beyond) while A is paused. B — another owner, another
    // connection, distinct invoices — runs `--dry-run` reconciliation and then `--confirm`, with the
    // mint saying UNPAID: B must HOLD on X (a spending row is never released on time) and therefore
    // NOT plan or pay Y. A resumes and pays X exactly once. One melt, one settled row.
    #[test]
    fn a_spending_row_is_not_released_when_its_lease_expires_and_its_owner_pays_exactly_once() {
        let (store, root) = store_with_fees("spending-held", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let mut a = Fake::new(|_| 2);
        a.owner = "proc-a".to_owned();
        a.invoice_tag = "-a".to_owned();
        a.melt_results = vec![Ok((13, 1))];
        let clock = Arc::clone(&a.clock);
        let (a_result, b_results) = run_paused(
            &db,
            a,
            100,
            PauseAt::Admit,
            super::test_support::Gate::new(),
            Arc::clone(&melts),
            |store_b| {
                // A is paused after admission: X is SPENDING in the store, stamped 100.
                let x = store_b
                    .in_flight_remittance()
                    .expect("query")
                    .expect("A's row");
                assert_eq!(x.state, RemittanceState::Spending);
                assert_eq!(x.spending_since_unix, Some(100));
                assert_eq!(x.lease_until_unix, Some(400));
                // The clock A will read when it resumes moves PAST its lease.
                clock.store(450, Ordering::SeqCst);
                let mut results = Vec::new();
                for (trigger, now) in [(RemitTrigger::DryRun, 450), (RemitTrigger::Command, 451)] {
                    let mut b = Fake::new(|_| 2);
                    b.owner = "proc-b".to_owned();
                    b.invoice_tag = "-b".to_owned();
                    b.melt_results = vec![Ok((13, 1))];
                    b.melt_counter = Some(Arc::clone(&melts));
                    b.status = Ok(Some(status(
                        MeltQuoteState::Unpaid,
                        "quote-lnbc-fake-13-2-a",
                    )));
                    let (outcome, out) = run_remit(store_b, &mut b, trigger, now);
                    assert!(b.melts.is_empty(), "B must not pay: {out}");
                    assert!(
                        b.invoices.is_empty(),
                        "B must not even plan on top of a spending row: {out}"
                    );
                    assert_eq!(
                        outcome,
                        RemitOutcome::Refused(Refusal::SpendingHeld {
                            remittance_id: "hash-13-2-a".to_owned(),
                            owner: "proc-a".to_owned(),
                            spending_since_unix: 100,
                            quote_id: Some(X_PAYMENT_QUOTE.to_owned()),
                            observed: format!(
                                "mint https://mint.example reports melt quote quote-lnbc-fake-13-2-a UNPAID (expiry unix {})",
                                u64::MAX
                            ),
                            held_sats: 15,
                        }),
                        "{out}"
                    );
                    assert!(
                        out.contains("HELD: remittance hash-13-2-a is SPENDING (admitted by proc-a at unix 100), bound to melt quote paid-quote-lnbc-fake-13-2-a; mint https://mint.example reports melt quote quote-lnbc-fake-13-2-a UNPAID"),
                        "{out}"
                    );
                    assert!(
                        out.contains("15 sats of receipts stay pinned to it — a spending row is released by nobody and on no clock"),
                        "{out}"
                    );
                    results.push((outcome, out));
                }
                assert_eq!(
                    store_b
                        .in_flight_remittance()
                        .expect("query")
                        .expect("still A's row")
                        .state,
                    RemittanceState::Spending,
                    "B changed nothing"
                );
                results
            },
        );
        let (a_outcome, a_out) = a_result;
        assert!(
            matches!(a_outcome, Ok(RemitOutcome::Paid { .. })),
            "A pays X once it resumes — it was admitted before the clock moved: {a_outcome:?}\n{a_out}"
        );
        assert_eq!(melts.load(Ordering::SeqCst), 1, "exactly one actual debit");
        assert_eq!(b_results.len(), 2);
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1, "one row, A's: {rows:?}");
        assert_eq!(rows[0].remittance_id, "hash-13-2-a");
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!(rows[0].spending_since_unix, Some(100));
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (15, 0, 0)
        );
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(
            attempts.len(),
            2,
            "A's payment and B's refused --confirm: {attempts:?}"
        );
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Paid);
        assert_eq!(attempts[1].trigger, RemitAttemptTrigger::Command);
        assert_eq!(attempts[1].outcome, RemitAttemptOutcome::Refused);
        assert_eq!(attempts[1].remittance_id.as_deref(), Some("hash-13-2-a"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Gate 2g (c), addendum 4 §1 — the gone owner: A journals X and pauses BEFORE its fence; the
    // clock moves past A's lease while it is paused. B runs `--dry-run` reconciliation (which
    // releases X: planned, UNPAID, lease run out) and then `--confirm`, planning a DISTINCT invoice
    // Y and paying it. A resumes, reads the clock FRESH at its fence, and its compare-and-set
    // changes zero rows (X is no longer planned): A REFUSES without touching the wallet. Exactly
    // one debit — B's.
    #[test]
    fn an_owner_that_outlives_its_lease_is_released_and_its_fence_then_changes_zero_rows() {
        let (store, root) = store_with_fees("expired-owner", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let mut a = Fake::new(|_| 2);
        a.owner = "proc-a".to_owned();
        a.invoice_tag = "-a".to_owned();
        a.melt_results = vec![Ok((13, 1))];
        let clock = Arc::clone(&a.clock);
        let (a_result, b_results) = run_paused(
            &db,
            a,
            100,
            PauseAt::Plan,
            super::test_support::Gate::new(),
            Arc::clone(&melts),
            |store_b| {
                assert_eq!(
                    store_b
                        .in_flight_remittance()
                        .expect("query")
                        .expect("A's row")
                        .state,
                    RemittanceState::Planned,
                    "A is paused BEFORE its fence"
                );
                // 100 + REMIT_LEASE (300) = 400: the lease has run out — for B's reconciliation and
                // for the clock A reads fresh when it resumes.
                clock.store(400, Ordering::SeqCst);
                let mut results = Vec::new();
                // B's dry run reconciles: X released. B's confirm then plans and pays Y.
                let mut b = Fake::new(|_| 2);
                b.owner = "proc-b".to_owned();
                b.invoice_tag = "-b".to_owned();
                b.status = Ok(Some(status(
                    MeltQuoteState::Unpaid,
                    "quote-lnbc-fake-13-2-a",
                )));
                let (outcome, out) = run_remit(store_b, &mut b, RemitTrigger::DryRun, 400);
                assert_eq!(outcome, RemitOutcome::DryRun, "{out}");
                assert!(
                    out.contains("its owner's lease ran out at unix 400 (owner proc-a); released 15 sats back to unremitted"),
                    "{out}"
                );
                assert!(b.melts.is_empty());
                results.push((outcome, out));
                let mut b = Fake::new(|_| 2);
                b.owner = "proc-b".to_owned();
                b.invoice_tag = "-b".to_owned();
                b.melt_results = vec![Ok((13, 1))];
                b.melt_counter = Some(Arc::clone(&melts));
                b.status = Ok(None);
                let (outcome, out) = run_remit(store_b, &mut b, RemitTrigger::Command, 401);
                assert!(is_paid(&outcome), "B pays Y once X is released: {out}");
                assert_eq!(b.melts, vec!["lnbc-fake-13-2-b".to_owned()]);
                results.push((outcome, out));
                results
            },
        );
        let (a_outcome, a_out) = a_result;
        match a_outcome {
            Ok(RemitOutcome::Refused(Refusal::OwnershipLost {
                remittance_id,
                reason,
            })) => {
                assert_eq!(remittance_id, "hash-13-2-a");
                assert!(
                    reason.contains(
                        "the row is no longer planned (now failed): another process reconciled it"
                    ),
                    "{reason}"
                );
            }
            other => panic!("A must refuse at the fence, got {other:?}\n{a_out}"),
        }
        assert!(
            a_out.contains("REFUSED before spending — the row is no longer planned (now failed): another process reconciled it (checked at unix 400)"),
            "{a_out}"
        );
        assert_eq!(
            melts.load(Ordering::SeqCst),
            1,
            "exactly one actual debit — B's"
        );
        assert_eq!(b_results.len(), 2);
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(
            rows.iter()
                .map(|r| (r.remittance_id.as_str(), r.state, r.spending_since_unix))
                .collect::<Vec<_>>(),
            vec![
                ("hash-13-2-a", RemittanceState::Failed, None),
                ("hash-13-2-b", RemittanceState::Settled, Some(401))
            ],
            "X was never admitted; Y was admitted at B's clock"
        );
        let accrued = store.accrued_fees().expect("read");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (15, 0, 0)
        );
        // A's refusal at the fence is journaled as a refused attempt naming X.
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        let a_attempt = attempts
            .iter()
            .find(|a| a.trigger == RemitAttemptTrigger::Collect)
            .expect("A's attempt");
        assert_eq!(a_attempt.outcome, RemitAttemptOutcome::Refused);
        assert_eq!(a_attempt.remittance_id.as_deref(), Some("hash-13-2-a"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Addendum 4 §1.1, the time condition alone: A journals X at 100 (lease until 400) and pauses
    // before its fence; NOBODY else runs, but the clock moves to 340 — exactly SPEND_MARGIN left.
    // A resumes, reads the clock fresh, and its compare-and-set changes zero rows (`400 > 340 + 60`
    // is false): A refuses, spends nothing, and — the row being still planned and its own — releases
    // it for the next attempt. The stale entry time (100) is not what the fence compares.
    #[test]
    fn an_owner_whose_lease_ran_down_while_it_paused_is_refused_by_its_own_fence() {
        let (store, root) = store_with_fees("lease-margin", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let mut a = Fake::new(|_| 2);
        a.owner = "proc-a".to_owned();
        a.melt_results = vec![Ok((13, 1))];
        let clock = Arc::clone(&a.clock);
        let (a_result, _) = run_paused(
            &db,
            a,
            100,
            PauseAt::Plan,
            super::test_support::Gate::new(),
            Arc::clone(&melts),
            |_| {
                clock.store(340, Ordering::SeqCst);
                Vec::new()
            },
        );
        let (a_outcome, a_out) = a_result;
        match a_outcome {
            Ok(RemitOutcome::Refused(Refusal::OwnershipLost {
                remittance_id,
                reason,
            })) => {
                assert_eq!(remittance_id, "hash-13-2");
                assert_eq!(
                    reason,
                    OwnershipLost::LeaseTooShort {
                        lease_until_unix: Some(400),
                        now_unix: 340,
                        margin_secs: 60,
                    }
                    .to_string()
                );
            }
            other => panic!("A must refuse at the fence, got {other:?}\n{a_out}"),
        }
        assert!(
            a_out.contains("(checked at unix 340). Nothing moved by this run; released 15 sats back to unremitted"),
            "{a_out}"
        );
        assert_eq!(melts.load(Ordering::SeqCst), 0, "no debit");
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemittanceState::Failed);
        assert_eq!(rows[0].spending_since_unix, None, "never admitted");
        assert_eq!(store.accrued_fees().expect("read").unremitted_fee_sats, 15);
        assert_eq!(lease_secs(REMIT_LEASE), 300);
        assert_eq!(lease_secs(SPEND_MARGIN), 60);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- addendum 5 §2: the bound quote and the conditional release, full path -------------------

    /// A's invoice X, its estimate quote and its PAYMENT quote Q, as the Fake names them: A probes
    /// the gross (invoice 1, 15 sats), invoices the net (invoice 2, 13 sats) and quotes it twice —
    /// the estimate at plan time and the payment quote before the fence.
    const X_BOLT11: &str = "lnbc-fake-13-2-a";
    const X_ID: &str = "hash-13-2-a";
    const X_ESTIMATE_QUOTE: &str = "quote-lnbc-fake-13-2-a";
    const X_PAYMENT_QUOTE: &str = "paid-quote-lnbc-fake-13-2-a";

    /// Process A for the addendum 5 §2 tests: owner `proc-a`, invoices tagged `-a`, one payment
    /// scripted, speaking to the shared fake mint.
    fn first_process(registry: &QuoteRegistry) -> Fake {
        let mut a = Fake::new(|_| 2);
        a.owner = "proc-a".to_owned();
        a.invoice_tag = "-a".to_owned();
        a.melt_results = vec![Ok((13, 1))];
        a.registry = Some(Arc::clone(registry));
        a
    }

    /// Process B: another owner, another connection's effects, DISTINCT invoices (`-b`), reading the
    /// SAME fake mint as A and counting its debits on the shared counter.
    fn second_process(registry: &QuoteRegistry, melts: &Arc<AtomicUsize>) -> Fake {
        let mut b = Fake::new(|_| 2);
        b.owner = "proc-b".to_owned();
        b.invoice_tag = "-b".to_owned();
        b.melt_results = vec![Ok((13, 1))];
        b.melt_counter = Some(Arc::clone(melts));
        b.registry = Some(Arc::clone(registry));
        b
    }

    fn ledger(store: &SellerStore) -> (u64, u64, u64) {
        let accrued = store.accrued_fees().expect("read");
        (
            accrued.remitted_fee_sats,
            accrued.in_flight_fee_sats,
            accrued.unremitted_fee_sats,
        )
    }

    // Gate 2g (b1), addendum 5 §2 — the ordering the verdict traced at 534247b (B1): A's fence
    // admitted X and BOUND the payment quote Q; A pauses before paying. Q is live, but the ESTIMATE
    // quote A raised earlier for the same invoice hits its mint expiry while A is paused (a mint
    // quote's clock is not the invoice's), and A's lease runs out too. B — another owner, another
    // connection, the SAME fake mint — runs `--dry-run` then `--confirm`: it asks the mint about Q
    // BY ID (never "the most alive quote for the invoice"), finds it UNPAID and live, and HOLDS. It
    // plans no Y and pays nothing. A resumes and pays Q — exactly one debit, and the mint saw
    // exactly one quote paid.
    #[test]
    fn a_spending_row_is_reconciled_by_its_bound_quote_not_by_an_expired_estimate() {
        let (store, root) = store_with_fees("bound-quote-live", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let registry = quote_registry();
        let (a_result, b_results) = run_paused(
            &db,
            first_process(&registry),
            100,
            PauseAt::Admit,
            Gate::new(),
            Arc::clone(&melts),
            |store_b| {
                let x = store_b
                    .in_flight_remittance()
                    .expect("query")
                    .expect("A's row");
                assert_eq!(x.state, RemittanceState::Spending);
                assert_eq!(x.spending_since_unix, Some(100));
                assert_eq!(x.spending_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
                // The mint: A's ESTIMATE quote for X expires at 130; Q stays live.
                {
                    let mut quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
                    quotes
                        .get_mut(X_ESTIMATE_QUOTE)
                        .expect("A's estimate quote")
                        .expiry_unix = 130;
                    assert_eq!(quotes[X_PAYMENT_QUOTE].state, MeltQuoteState::Unpaid);
                    assert_eq!(quotes[X_PAYMENT_QUOTE].expiry_unix, u64::MAX);
                }
                let mut results = Vec::new();
                // Past the estimate's expiry AND past A's lease (400): neither releases a spending
                // row whose bound quote is live.
                for (trigger, now) in [(RemitTrigger::DryRun, 450), (RemitTrigger::Command, 451)] {
                    let mut b = second_process(&registry, &melts);
                    let (outcome, out) = run_remit(store_b, &mut b, trigger, now);
                    assert_eq!(
                        b.quote_status_calls,
                        vec![X_PAYMENT_QUOTE.to_owned()],
                        "B asks the mint about the BOUND quote, by id: {out}"
                    );
                    assert!(
                        b.status_calls.is_empty(),
                        "B never ranks the invoice's quotes for a bound row: {out}"
                    );
                    assert!(b.melts.is_empty(), "B must not pay: {out}");
                    assert!(
                        b.invoices.is_empty(),
                        "B must not plan on top of a held row: {out}"
                    );
                    assert_eq!(
                        outcome,
                        RemitOutcome::Refused(Refusal::SpendingHeld {
                            remittance_id: X_ID.to_owned(),
                            owner: "proc-a".to_owned(),
                            spending_since_unix: 100,
                            quote_id: Some(X_PAYMENT_QUOTE.to_owned()),
                            observed: format!(
                                "mint https://mint.example reports melt quote {X_PAYMENT_QUOTE} UNPAID (expiry unix {})",
                                u64::MAX
                            ),
                            held_sats: 15,
                        }),
                        "{out}"
                    );
                    assert!(
                        out.contains("bound to melt quote paid-quote-lnbc-fake-13-2-a: asking the mint about that quote by id"),
                        "{out}"
                    );
                    results.push((outcome, out));
                }
                assert_eq!(ledger(store_b), (0, 15, 0), "receipts still pinned to X");
                results
            },
        );
        let (a_outcome, a_out) = a_result;
        assert!(
            matches!(a_outcome, Ok(RemitOutcome::Paid { .. })),
            "A pays Q once it resumes: {a_outcome:?}\n{a_out}"
        );
        assert_eq!(
            melts.load(Ordering::SeqCst),
            1,
            "exactly one actual debit — A's, on Q"
        );
        assert_eq!(b_results.len(), 2);
        {
            let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                quotes[X_PAYMENT_QUOTE].state,
                MeltQuoteState::Paid,
                "the mint saw exactly Q paid"
            );
            assert_eq!(
                quotes[X_ESTIMATE_QUOTE].state,
                MeltQuoteState::Unpaid,
                "the expired estimate was never paid"
            );
            assert_eq!(
                quotes
                    .values()
                    .filter(|q| q.state == MeltQuoteState::Paid)
                    .count(),
                1
            );
        }
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1, "one row, A's: {rows:?}");
        assert_eq!(rows[0].remittance_id, X_ID);
        assert_eq!(rows[0].state, RemittanceState::Settled);
        assert_eq!(rows[0].melt_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
        assert_eq!(ledger(&store), (15, 0, 0));
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert_eq!(attempts[0].outcome, RemitAttemptOutcome::Paid);
        assert_eq!(attempts[1].trigger, RemitAttemptTrigger::Command);
        assert_eq!(attempts[1].outcome, RemitAttemptOutcome::Refused);
        assert_eq!(attempts[1].remittance_id.as_deref(), Some(X_ID));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Gate 2g (b2), addendum 5 §2 as addendum 6 §1.2 re-rules it — the same pause, the quote
    // expiring: the mint stamps every quote it raises with expiry 130. A runs at 60 (lease until
    // 360): Q is raised with 70 s of life — more than the margin — so A's fence admits X and binds
    // Q; A pauses before paying. While it is paused the clock, SHARED by A and B, moves to 200: Q
    // has expired and the old spending margin has passed. B `--dry-run` then `--confirm`: asks
    // about Q by id, finds it UNPAID past expiry — and HOLDS: expiry is not cancellation (the mint
    // pays an expired UNPAID quote), so X stays spending, its receipts stay pinned by real SQL, and
    // B plans no Y and pays nothing. A resumes: it reads the clock fresh, refuses to pay a bound
    // quote inside (here: past) its margin — attempt avoidance, not the safety — journals the
    // attempt failed, raises no other quote and does not touch the mint. ZERO melts. X is held for
    // the mint's PAID or an operator; nobody's clock releases it.
    #[test]
    fn a_bound_quote_expired_past_the_margin_is_held_and_its_owner_refuses_to_pay_it() {
        let (store, root) = store_with_fees("bound-quote-expired", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let registry = quote_registry();
        let mut a = first_process(&registry);
        a.quote_expiry_unix = 130;
        let clock = Arc::clone(&a.clock);
        let (a_result, b_results) = run_paused(
            &db,
            a,
            60,
            PauseAt::Admit,
            Gate::new(),
            Arc::clone(&melts),
            |store_b| {
                let x = store_b
                    .in_flight_remittance()
                    .expect("query")
                    .expect("A's row");
                assert_eq!(x.state, RemittanceState::Spending);
                assert_eq!(x.spending_since_unix, Some(60));
                assert_eq!(x.spending_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
                {
                    let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(quotes[X_PAYMENT_QUOTE].expiry_unix, 130, "Q expires at 130");
                    assert_eq!(quotes[X_PAYMENT_QUOTE].state, MeltQuoteState::Unpaid);
                }
                // The clock both processes read moves to 200: past 130, and past the old margin.
                let mut results = Vec::new();
                for (trigger, now) in [(RemitTrigger::DryRun, 200), (RemitTrigger::Command, 201)] {
                    let mut b = second_process(&registry, &melts);
                    b.clock = Arc::clone(&clock);
                    let (outcome, out) = run_remit(store_b, &mut b, trigger, now);
                    assert_eq!(
                        outcome,
                        RemitOutcome::Refused(Refusal::SpendingHeld {
                            remittance_id: X_ID.to_owned(),
                            owner: "proc-a".to_owned(),
                            spending_since_unix: 60,
                            quote_id: Some(X_PAYMENT_QUOTE.to_owned()),
                            observed: format!(
                                "mint https://mint.example reports melt quote {X_PAYMENT_QUOTE} UNPAID (expiry unix 130)"
                            ),
                            held_sats: 15,
                        }),
                        "at {now}: {out}"
                    );
                    assert_eq!(b.quote_status_calls, vec![X_PAYMENT_QUOTE.to_owned()]);
                    assert!(b.status_calls.is_empty());
                    assert!(
                        out.contains("HELD: remittance hash-13-2-a is SPENDING (admitted by proc-a at unix 60), bound to melt quote paid-quote-lnbc-fake-13-2-a; mint https://mint.example reports melt quote paid-quote-lnbc-fake-13-2-a UNPAID (expiry unix 130); 15 sats of receipts stay pinned to it — a spending row is released by nobody and on no clock"),
                        "at {now}: {out}"
                    );
                    assert!(!out.contains("released 15 sats"), "at {now}: {out}");
                    assert!(
                        b.invoices.is_empty() && b.melts.is_empty(),
                        "B neither plans nor pays on a held row: {out}"
                    );
                    assert_eq!(
                        store_b
                            .in_flight_remittance()
                            .expect("query")
                            .expect("still X")
                            .state,
                        RemittanceState::Spending
                    );
                    assert_eq!(
                        ledger(store_b),
                        (0, 15, 0),
                        "receipts still pinned to X at {now}: the expired quote may yet be paid"
                    );
                    results.push((outcome, out));
                }
                results
            },
        );
        let (a_outcome, a_out) = a_result;
        match a_outcome {
            Ok(RemitOutcome::MeltFailed {
                remittance_id,
                error,
            }) => {
                assert_eq!(remittance_id, X_ID);
                assert_eq!(
                    error,
                    "bound melt quote paid-quote-lnbc-fake-13-2-a expires at unix 130, within 60 s of now (unix 201); not paid"
                );
            }
            other => {
                panic!("A must refuse its bound quote and pay nothing, got {other:?}\n{a_out}")
            }
        }
        assert!(
            a_out.contains("this process raises no other quote for it"),
            "{a_out}"
        );
        assert_eq!(
            melts.load(Ordering::SeqCst),
            0,
            "no debit at all: A refused its bound quote and B was held"
        );
        assert_eq!(b_results.len(), 2);
        {
            let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                quotes[X_PAYMENT_QUOTE].state,
                MeltQuoteState::Unpaid,
                "Q was never paid"
            );
            assert!(
                quotes.keys().all(|id| id.ends_with("-a")),
                "B raised no quote at all: {:?}",
                quotes.keys().collect::<Vec<_>>()
            );
            assert_eq!(
                quotes.len(),
                3,
                "A raised its two estimates and Q, and nothing after: {:?}",
                quotes.keys().collect::<Vec<_>>()
            );
        }
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(
            rows.iter()
                .map(|r| (r.remittance_id.as_str(), r.state))
                .collect::<Vec<_>>(),
            vec![(X_ID, RemittanceState::Spending)],
            "X is held, bound, for the mint's PAID or an operator"
        );
        assert_eq!(rows[0].spending_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
        assert_eq!(ledger(&store), (0, 15, 0), "nothing paid, nothing released");
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert_eq!(
            (
                attempts[0].trigger,
                attempts[0].outcome,
                attempts[0].remittance_id.as_deref()
            ),
            (
                RemitAttemptTrigger::Collect,
                RemitAttemptOutcome::Failed,
                Some(X_ID)
            ),
            "A's refusal of its own bound quote is journaled as a failed attempt"
        );
        assert_eq!(
            (
                attempts[1].trigger,
                attempts[1].outcome,
                attempts[1].remittance_id.as_deref()
            ),
            (
                RemitAttemptTrigger::Command,
                RemitAttemptOutcome::Refused,
                Some(X_ID)
            ),
            "B's hold is journaled naming X"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Gate 2g (c), addendum 5 §2 — the gone owner, paused one step later than addendum 4's (c): A
    // has journaled X AND raised its payment quote Q (Q exists at the mint, live) and pauses BEFORE
    // the fence; the shared clock moves past A's lease. B `--dry-run` reconciles: X is planned
    // (never admitted — `spending_since_unix IS NULL`), the invoice's quotes are live UNPAID, the
    // lease has run out — released by the lease transition; B `--confirm` plans and pays a DISTINCT
    // Y. A resumes: its fence reads the clock fresh and changes zero rows (X is failed): it refuses
    // and never pays Q. One melt — Y's. Q is left UNPAID at the mint, bound to nothing.
    #[test]
    fn an_owner_paused_after_its_quote_and_past_its_lease_is_released_and_never_pays_that_quote() {
        let (store, root) = store_with_fees("quote-then-lease", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let registry = quote_registry();
        let a = first_process(&registry);
        let clock = Arc::clone(&a.clock);
        let (a_result, b_results) = run_paused(
            &db,
            a,
            100,
            PauseAt::Quote,
            Gate::new(),
            Arc::clone(&melts),
            |store_b| {
                let x = store_b
                    .in_flight_remittance()
                    .expect("query")
                    .expect("A's row");
                assert_eq!(
                    x.state,
                    RemittanceState::Planned,
                    "A is paused BEFORE its fence"
                );
                assert_eq!(x.spending_since_unix, None);
                assert_eq!(x.spending_quote_id, None);
                {
                    let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(
                        quotes[X_PAYMENT_QUOTE].state,
                        MeltQuoteState::Unpaid,
                        "Q exists at the mint before the fence"
                    );
                }
                // 100 + REMIT_LEASE (300) = 400: the lease has run out — for B's reconciliation and
                // for the clock A reads fresh inside its fence when it resumes.
                clock.store(400, Ordering::SeqCst);
                let mut results = Vec::new();
                let mut b = second_process(&registry, &melts);
                b.clock = Arc::clone(&clock);
                let (outcome, out) = run_remit(store_b, &mut b, RemitTrigger::DryRun, 400);
                assert_eq!(outcome, RemitOutcome::DryRun, "{out}");
                assert_eq!(
                    b.status_calls,
                    vec![X_BOLT11.to_owned()],
                    "a planned row has no bound quote: the invoice's quotes are asked: {out}"
                );
                assert!(b.quote_status_calls.is_empty());
                assert!(
                    out.contains("its owner's lease ran out at unix 400 (owner proc-a); released 15 sats back to unremitted (release condition: planned, never admitted, and its owner's lease had run out at unix 400)"),
                    "{out}"
                );
                assert!(b.melts.is_empty());
                assert_eq!(ledger(store_b), (0, 0, 15));
                results.push((outcome, out));
                let mut b = second_process(&registry, &melts);
                b.clock = Arc::clone(&clock);
                let (outcome, out) = run_remit(store_b, &mut b, RemitTrigger::Command, 401);
                assert!(is_paid(&outcome), "B pays Y once X is released: {out}");
                assert_eq!(b.melts, vec!["lnbc-fake-13-2-b".to_owned()]);
                results.push((outcome, out));
                results
            },
        );
        let (a_outcome, a_out) = a_result;
        match a_outcome {
            Ok(RemitOutcome::Refused(Refusal::OwnershipLost {
                remittance_id,
                reason,
            })) => {
                assert_eq!(remittance_id, X_ID);
                assert!(
                    reason.contains(
                        "the row is no longer planned (now failed): another process reconciled it"
                    ),
                    "{reason}"
                );
            }
            other => panic!("A must refuse at the fence, got {other:?}\n{a_out}"),
        }
        assert!(
            a_out.contains("REFUSED before spending — the row is no longer planned (now failed): another process reconciled it (checked at unix 401)"),
            "{a_out}"
        );
        assert_eq!(
            melts.load(Ordering::SeqCst),
            1,
            "exactly one actual debit — B's"
        );
        assert_eq!(b_results.len(), 2);
        {
            let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                quotes[X_PAYMENT_QUOTE].state,
                MeltQuoteState::Unpaid,
                "Q was raised and never paid"
            );
            assert_eq!(
                quotes
                    .values()
                    .filter(|q| q.state == MeltQuoteState::Paid)
                    .count(),
                1
            );
        }
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(
            rows.iter()
                .map(|r| (
                    r.remittance_id.as_str(),
                    r.state,
                    r.spending_since_unix,
                    r.spending_quote_id.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![
                (X_ID, RemittanceState::Failed, None, None),
                (
                    "hash-13-2-b",
                    RemittanceState::Settled,
                    Some(401),
                    Some("paid-quote-lnbc-fake-13-2-b")
                )
            ],
            "X was never admitted and binds no quote; Y was admitted at B's clock, bound to its quote"
        );
        assert_eq!(ledger(&store), (15, 0, 0));
        let a_attempt = store
            .recent_remit_attempts(10)
            .expect("attempts")
            .into_iter()
            .find(|a| a.trigger == RemitAttemptTrigger::Collect)
            .expect("A's attempt");
        assert_eq!(a_attempt.outcome, RemitAttemptOutcome::Refused);
        assert_eq!(a_attempt.remittance_id.as_deref(), Some(X_ID));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Gate 2g (B2), addendum 5 §2 — the verdict's second ordering, deterministic: B reads X planned
    // by A (lease until 400) on an entry clock of 401 and DECIDES to release it on lease expiry;
    // before B writes, A's fence lands — the clock A reads INSIDE the store's lock is the shared
    // one, still 100 — admitting X and binding Q. B resumes: its release is the conditional lease
    // transition (`state = 'planned' AND spending_since_unix IS NULL AND lease_until_unix <= 401`)
    // and changes ZERO rows: B HOLDS, says the row changed under it, plans nothing, pays nothing.
    // A pays X. One melt. Both Fakes share one clock `Arc` and one fake mint; three gates order the
    // two threads (A after its quote, B after its decision, A after its admission).
    #[test]
    fn a_release_decided_on_a_stale_planned_snapshot_cannot_revoke_a_later_admission() {
        let (store, root) = store_with_fees("stale-release", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let registry = quote_registry();
        let mut a = first_process(&registry);
        a.melt_counter = Some(Arc::clone(&melts));
        let clock = Arc::clone(&a.clock);
        a.set_clock(100);
        let a_quote_gate = Gate::new();
        let a_admit_gate = Gate::new();
        a.quote_gate = Some(Arc::clone(&a_quote_gate));
        a.admit_gate = Some(Arc::clone(&a_admit_gate));
        let db_a = db.clone();
        let a_thread = std::thread::spawn(move || {
            let store = SellerStore::open(&db_a).expect("open A");
            let mut out = Vec::new();
            let outcome = remit(&store, &mut a, RemitTrigger::Collect, 100, &mut out);
            (outcome, String::from_utf8_lossy(&out).into_owned(), a)
        });
        // 1. A: X planned (lease until 400), Q raised, paused before its fence.
        a_quote_gate.wait_arrived(Duration::from_secs(10));
        // 2. B, entry clock 401: reads X planned with its lease run out, decides to release it, and
        //    pauses before writing. (Its Fake shares A's clock, which still reads 100.)
        let mut b = second_process(&registry, &melts);
        b.clock = Arc::clone(&clock);
        let b_decision_gate = Gate::new();
        b.decision_gate = Some(Arc::clone(&b_decision_gate));
        let db_b = db.clone();
        let b_thread = std::thread::spawn(move || {
            let store = SellerStore::open(&db_b).expect("open B");
            let mut out = Vec::new();
            let outcome = remit(&store, &mut b, RemitTrigger::Command, 401, &mut out);
            (outcome, String::from_utf8_lossy(&out).into_owned(), b)
        });
        b_decision_gate.wait_arrived(Duration::from_secs(10));
        {
            let store = SellerStore::open(&db).expect("open");
            let x = store.in_flight_remittance().expect("query").expect("X");
            assert_eq!(
                x.state,
                RemittanceState::Planned,
                "B has decided; nothing is written yet"
            );
        }
        // 3. A's fence lands: the clock read inside the lock is 100 — 400 > 100 + 60 — admitted,
        //    Q bound.
        a_quote_gate.release();
        a_admit_gate.wait_arrived(Duration::from_secs(10));
        {
            let store = SellerStore::open(&db).expect("open");
            let x = store.in_flight_remittance().expect("query").expect("X");
            assert_eq!(x.state, RemittanceState::Spending);
            assert_eq!(x.spending_since_unix, Some(100));
            assert_eq!(x.spending_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
        }
        // 4. B resumes and writes its release: zero rows changed. HOLD.
        b_decision_gate.release();
        let (b_outcome, b_out, b) = b_thread.join().expect("B's thread");
        assert_eq!(
            b_outcome,
            Ok(RemitOutcome::Refused(Refusal::RowChangedUnderMe {
                remittance_id: X_ID.to_owned(),
            })),
            "{b_out}"
        );
        assert!(
            b_out.contains("its owner's lease ran out at unix 400 (owner proc-a) — but the row changed under me between that decision and the release (condition: planned, never admitted, and its owner's lease had run out at unix 401): nothing written. REFUSED — nothing moved by this run"),
            "{b_out}"
        );
        assert_eq!(b.decisions_seen.len(), 1);
        assert!(
            matches!(
                &b.decisions_seen[0],
                Reconcile::Release {
                    on: ReleaseOn::LeaseExpired { now_unix: 401 },
                    ..
                }
            ),
            "B's decision was the lease release: {:?}",
            b.decisions_seen
        );
        assert!(
            b.melts.is_empty() && b.invoices.is_empty(),
            "B planned and paid nothing: {b_out}"
        );
        {
            let store = SellerStore::open(&db).expect("open");
            let x = store
                .in_flight_remittance()
                .expect("query")
                .expect("X, still A's");
            assert_eq!(
                x.state,
                RemittanceState::Spending,
                "B's release touched nothing"
            );
            assert_eq!(ledger(&store), (0, 15, 0), "receipts still pinned to X");
        }
        // 5. A pays X.
        a_admit_gate.release();
        let (a_outcome, a_out, a) = a_thread.join().expect("A's thread");
        assert!(
            matches!(a_outcome, Ok(RemitOutcome::Paid { .. })),
            "{a_outcome:?}\n{a_out}"
        );
        assert_eq!(a.melts, vec![X_BOLT11.to_owned()]);
        assert_eq!(melts.load(Ordering::SeqCst), 1, "exactly one actual debit");
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (
                rows[0].remittance_id.as_str(),
                rows[0].state,
                rows[0].spending_since_unix
            ),
            (X_ID, RemittanceState::Settled, Some(100))
        );
        assert_eq!(ledger(&store), (15, 0, 0));
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert_eq!(
            (attempts[0].trigger, attempts[0].outcome),
            (RemitAttemptTrigger::Collect, RemitAttemptOutcome::Paid)
        );
        assert_eq!(
            (
                attempts[1].trigger,
                attempts[1].outcome,
                attempts[1].remittance_id.as_deref()
            ),
            (
                RemitAttemptTrigger::Command,
                RemitAttemptOutcome::Refused,
                Some(X_ID)
            ),
            "B's hold is journaled naming X"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Gate 2g (d), addendum 5 §2 — the FULL PATH for a spending row bound to Q, four arms, each on
    // its own store with a real second process (the pure decision table above is kept as an extra):
    // A's fence admitted X and bound Q; A pauses before paying; the fake mint's registry moves Q to
    // the arm's state; B — another owner, another connection, the same mint — runs `--dry-run`
    // then `--confirm` at 450/451, past A's lease. Melts are counted per arm. Under addendum 6
    // §1.2 B HOLDS in EVERY arm — X stays spending, its receipts stay pinned (real SQL), B plans no
    // Y and pays nothing — and what differs is what the mint then does with A's prepared payment:
    //   FAILED  → the mint ACCEPTS Q (CDK 0.17.2 admits UNPAID or FAILED): A pays. ONE melt (A's).
    //             Had B released on FAILED and paid Y, that would have been two.
    //   UNPAID, live, lease long run out → A pays Q. ONE melt (A's).
    //   PENDING → the mint refuses Q (PENDING); no debit. ZERO melts; X spending, receipts pinned.
    //   UNKNOWN → the same way. ZERO melts; X spending, receipts pinned.
    // Every arm asserts the attempts journal: A's outcome, then B's --confirm refusal naming X.
    #[test]
    fn a_spending_rows_bound_quote_decides_its_release_on_the_full_path() {
        struct Arm {
            label: &'static str,
            state: MeltQuoteState,
            a_pays: bool,
            hold_text: &'static str,
        }
        let arms = [
            Arm {
                label: "failed",
                state: MeltQuoteState::Failed,
                a_pays: true,
                hold_text: "HELD: remittance hash-13-2-a is SPENDING (admitted by proc-a at unix 100), bound to melt quote paid-quote-lnbc-fake-13-2-a; mint https://mint.example reports melt quote paid-quote-lnbc-fake-13-2-a FAILED",
            },
            Arm {
                label: "unpaid-live",
                state: MeltQuoteState::Unpaid,
                a_pays: true,
                hold_text: "HELD: remittance hash-13-2-a is SPENDING (admitted by proc-a at unix 100), bound to melt quote paid-quote-lnbc-fake-13-2-a; mint https://mint.example reports melt quote paid-quote-lnbc-fake-13-2-a UNPAID",
            },
            Arm {
                label: "pending",
                state: MeltQuoteState::Pending,
                a_pays: false,
                hold_text: "HELD: remittance hash-13-2-a is SPENDING (admitted by proc-a at unix 100), bound to melt quote paid-quote-lnbc-fake-13-2-a; mint https://mint.example reports melt quote paid-quote-lnbc-fake-13-2-a PENDING",
            },
            Arm {
                label: "unknown",
                state: MeltQuoteState::Unknown,
                a_pays: false,
                hold_text: "HELD: remittance hash-13-2-a is SPENDING (admitted by proc-a at unix 100), bound to melt quote paid-quote-lnbc-fake-13-2-a; mint https://mint.example reports melt quote paid-quote-lnbc-fake-13-2-a UNKNOWN",
            },
        ];
        for arm in &arms {
            let (store, root) = store_with_fees(&format!("full-path-{}", arm.label), &[10, 5]);
            drop(store);
            let db = root.join(STATE_DB_FILE);
            let melts = Arc::new(AtomicUsize::new(0));
            let registry = quote_registry();
            let (a_result, b_results) = run_paused(
                &db,
                first_process(&registry),
                100,
                PauseAt::Admit,
                Gate::new(),
                Arc::clone(&melts),
                |store_b| {
                    let x = store_b
                        .in_flight_remittance()
                        .expect("query")
                        .expect("A's row");
                    assert_eq!(x.state, RemittanceState::Spending, "[{}]", arm.label);
                    assert_eq!(x.spending_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
                    // The mint moves Q to this arm's state.
                    registry
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_mut(X_PAYMENT_QUOTE)
                        .expect("Q")
                        .state = arm.state;
                    let mut results = Vec::new();
                    for (trigger, now) in
                        [(RemitTrigger::DryRun, 450), (RemitTrigger::Command, 451)]
                    {
                        let mut b = second_process(&registry, &melts);
                        let (outcome, out) = run_remit(store_b, &mut b, trigger, now);
                        assert_eq!(
                            b.quote_status_calls,
                            vec![X_PAYMENT_QUOTE.to_owned()],
                            "[{}] B asks the mint about Q by id, and about nothing else: {out}",
                            arm.label
                        );
                        assert!(b.status_calls.is_empty(), "[{}] {out}", arm.label);
                        assert!(
                            b.melts.is_empty() && b.invoices.is_empty() && b.quotes.is_empty(),
                            "[{}] B must neither plan, quote nor pay on a held row: {out}",
                            arm.label
                        );
                        // Addendum 7 §2: every bound non-PAID observation — FAILED, UNPAID, PENDING,
                        // UNKNOWN — is the SAME refusal and renders the SAME single `HELD:` line.
                        let expected_hold = RemitOutcome::Refused(Refusal::SpendingHeld {
                            remittance_id: X_ID.to_owned(),
                            owner: "proc-a".to_owned(),
                            spending_since_unix: 100,
                            quote_id: Some(X_PAYMENT_QUOTE.to_owned()),
                            observed: format!(
                                "mint https://mint.example reports melt quote {X_PAYMENT_QUOTE} {} (expiry unix {})",
                                arm.state,
                                u64::MAX
                            ),
                            held_sats: 15,
                        });
                        assert_eq!(outcome, expected_hold, "[{}] {out}", arm.label);
                        assert!(
                            out.contains(arm.hold_text)
                                && out.contains("15 sats of receipts stay pinned to it")
                                && out.contains("REFUSED — nothing moved by this run"),
                            "[{}] {out}",
                            arm.label
                        );
                        assert_eq!(
                            out.lines()
                                .filter(|line| line.starts_with("  HELD: remittance hash-13-2-a"))
                                .count(),
                            1,
                            "[{}] exactly one HELD line, naming the row, on {trigger:?}: {out}",
                            arm.label
                        );
                        assert!(
                            !out.contains("still settling"),
                            "[{}] a bound spending row is HELD, never merely 'settling': {out}",
                            arm.label
                        );
                        assert!(!out.contains("released 15 sats"), "[{}] {out}", arm.label);
                        let x = store_b
                            .in_flight_remittance()
                            .expect("query")
                            .expect("still X");
                        assert_eq!(
                            (x.state, x.spending_quote_id.as_deref()),
                            (RemittanceState::Spending, Some(X_PAYMENT_QUOTE)),
                            "[{}] held: nothing written",
                            arm.label
                        );
                        assert_eq!(
                            ledger(store_b),
                            (0, 15, 0),
                            "[{}] receipts pinned to X while Q can still be paid",
                            arm.label
                        );
                        results.push((outcome, out));
                    }
                    results
                },
            );
            let (a_outcome, a_out) = a_result;
            if arm.a_pays {
                assert!(
                    matches!(a_outcome, Ok(RemitOutcome::Paid { .. })),
                    "[{}] {a_outcome:?}\n{a_out}",
                    arm.label
                );
            } else {
                match a_outcome {
                    Ok(RemitOutcome::MeltFailed {
                        remittance_id,
                        error,
                    }) => {
                        assert_eq!(remittance_id, X_ID);
                        assert!(
                            error.contains(
                                "refuses to pay melt quote paid-quote-lnbc-fake-13-2-a: it is"
                            ),
                            "[{}] {error}",
                            arm.label
                        );
                    }
                    other => panic!(
                        "[{}] A must be refused by the mint, got {other:?}\n{a_out}",
                        arm.label
                    ),
                }
                assert!(
                    a_out.contains("This process raises no other quote for the row"),
                    "[{}] {a_out}",
                    arm.label
                );
            }
            assert_eq!(b_results.len(), 2);
            let expected_melts = usize::from(arm.a_pays);
            assert_eq!(
                melts.load(Ordering::SeqCst),
                expected_melts,
                "[{}] actual debits — never two",
                arm.label
            );
            let store = SellerStore::open(&db).expect("open");
            let rows = store
                .remittances()
                .expect("rows")
                .into_iter()
                .map(|r| (r.remittance_id, r.state))
                .collect::<Vec<_>>();
            if arm.a_pays {
                assert_eq!(
                    rows,
                    vec![(X_ID.to_owned(), RemittanceState::Settled)],
                    "[{}] one row, X, paid by its owner",
                    arm.label
                );
                assert_eq!(ledger(&store), (15, 0, 0), "[{}]", arm.label);
            } else {
                assert_eq!(
                    rows,
                    vec![(X_ID.to_owned(), RemittanceState::Spending)],
                    "[{}] held for the mint to resolve",
                    arm.label
                );
                assert_eq!(ledger(&store), (0, 15, 0), "[{}]", arm.label);
            }
            let attempts = store.recent_remit_attempts(10).expect("attempts");
            assert_eq!(attempts.len(), 2, "[{}] {attempts:?}", arm.label);
            assert_eq!(
                (
                    attempts[0].trigger,
                    attempts[0].outcome,
                    attempts[0].remittance_id.as_deref()
                ),
                (
                    RemitAttemptTrigger::Collect,
                    if arm.a_pays {
                        RemitAttemptOutcome::Paid
                    } else {
                        RemitAttemptOutcome::Failed
                    },
                    Some(X_ID)
                ),
                "[{}] A's attempt, journaled last",
                arm.label
            );
            assert_eq!(
                (
                    attempts[1].trigger,
                    attempts[1].outcome,
                    attempts[1].remittance_id.as_deref()
                ),
                (
                    RemitAttemptTrigger::Command,
                    RemitAttemptOutcome::Refused,
                    Some(X_ID)
                ),
                "[{}] B's --confirm hold, journaled naming X (the dry run is not an attempt)",
                arm.label
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    // Addendum 6 §2.1 / verdict at 6fc77e1 §4 B3 — the DELAYED CONFIRM, on the full path with a real
    // store, two owners, two connections, one shared fake mint and one shared fake wallet:
    //   A plans X at 100, its fence admits X and binds Q (Q expires at 200; X's invoice is still
    //   valid), A passes its own pre-await margin check (200 > 100 + 60), the wallet's ceiling and
    //   proof selection (exact denominations 8+4+2+1 out of two disjoint such sets) and
    //   `prepare_melt`'s expiry check on the wallet's clock — and PAUSES there, its request built
    //   but not yet at the mint.
    //   The shared clock moves to 261: Q is past expiry and past the old margin.
    //   B — another owner, disjoint proofs left in the same wallet — is refused at TWO named
    //   boundaries, exercised separately (addendum 7 §1; verdict at 6fd13df §5 D1):
    //     (i) RECONCILIATION: B runs `--dry-run` then `--confirm` through the real `remit`. Step 1
    //         of `remit_inner` finds X in flight, asks the mint about Q BY ID, gets UNPAID (expired),
    //         and `reconcile_decision` HOLDS — `Refusal::SpendingHeld`, returned before any plan.
    //         While X is bound and spending, EVERY `remit` on this store stops here by design; it
    //         never reaches `plan_remittance`, so these two runs do not exercise the store's own
    //         predicate — the next boundary does.
    //     (ii) STORE: between those two runs B makes a concrete planning attempt for a distinct
    //         invoice Y, on B's own connection, through the store's admission entry
    //         `plan_remittance` — the call `remit_inner` step 6 makes. Its in-flight predicate
    //         (`in_flight_remittance_in`, inside the `IMMEDIATE` transaction) refuses with
    //         `PlanRefused::InFlight` naming X: no Y row, receipts still pinned to X, no quote
    //         raised, no debit. This is the race-closing layer the ordinary held-row path never
    //         reaches while X is spending; it is reached here directly, not by hand-written SQL.
    //   At both boundaries 15 sats of exact proofs are still in the wallet, so neither refusal is
    //   for want of funds.
    //   A resumes: the mint (as the inspected CDK 0.17.2 implementation does) accepts the expired
    //   UNPAID Q and pays X.
    // Exactly ONE debit, never two — and it is one because B was never admitted while X was bound
    // and spending, NOT because Q expired: at 6fc77e1 the same ordering released X on the expired
    // Q, B paid Y, and A's late request paid Q — two payments against one accrued balance. The
    // receipt invariant is asserted through the ordering (pinned to X at each of B's three
    // observations, discharged once by A's debit), not only in the final row count.
    #[test]
    fn a_payment_prepared_before_expiry_cannot_be_doubled_by_a_release_after_it() {
        let (store, root) = store_with_fees("delayed-confirm", &[10, 5]);
        drop(store);
        let db = root.join(STATE_DB_FILE);
        let melts = Arc::new(AtomicUsize::new(0));
        let registry = quote_registry();
        // Two disjoint exact sets for a 15-sat payment (13 + reserve 2): whichever A takes, B's
        // selection cannot collide with it.
        let proofs = fake_proofs(&[8, 4, 2, 1, 8, 4, 2, 1]);
        let remaining = |proofs: &super::test_support::FakeProofs| {
            let mut left = proofs.lock().unwrap_or_else(|e| e.into_inner()).clone();
            left.sort_unstable();
            left
        };
        let mut a = first_process(&registry);
        a.quote_expiry_unix = 200;
        a.proofs = Some(Arc::clone(&proofs));
        let clock = Arc::clone(&a.clock);
        let (a_result, b_results) = run_paused(
            &db,
            a,
            100,
            PauseAt::Melt,
            Gate::new(),
            Arc::clone(&melts),
            |store_b| {
                // A is inside its payment: X spending, bound to Q; Q UNPAID at the mint, expiring
                // at 200; A's proofs reserved — one exact set left for anyone else.
                let x = store_b
                    .in_flight_remittance()
                    .expect("query")
                    .expect("A's row");
                assert_eq!(x.state, RemittanceState::Spending);
                assert_eq!(x.spending_since_unix, Some(100));
                assert_eq!(x.spending_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
                {
                    let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(quotes[X_PAYMENT_QUOTE].state, MeltQuoteState::Unpaid);
                    assert_eq!(quotes[X_PAYMENT_QUOTE].expiry_unix, 200);
                }
                assert_eq!(
                    remaining(&proofs),
                    vec![1, 2, 4, 8],
                    "A reserved one exact 15-sat set; the other is still in the wallet"
                );
                // The clock both processes read moves past Q's expiry and past the old margin.
                clock.store(261, Ordering::SeqCst);
                let mut results = Vec::new();
                for (trigger, now) in [(RemitTrigger::DryRun, 261), (RemitTrigger::Command, 262)] {
                    let mut b = second_process(&registry, &melts);
                    b.clock = Arc::clone(&clock);
                    b.proofs = Some(Arc::clone(&proofs));
                    let (outcome, out) = run_remit(store_b, &mut b, trigger, now);
                    assert_eq!(
                        outcome,
                        RemitOutcome::Refused(Refusal::SpendingHeld {
                            remittance_id: X_ID.to_owned(),
                            owner: "proc-a".to_owned(),
                            spending_since_unix: 100,
                            quote_id: Some(X_PAYMENT_QUOTE.to_owned()),
                            observed: format!(
                                "mint https://mint.example reports melt quote {X_PAYMENT_QUOTE} UNPAID (expiry unix 200)"
                            ),
                            held_sats: 15,
                        }),
                        "at {now}: {out}"
                    );
                    assert_eq!(
                        b.quote_status_calls,
                        vec![X_PAYMENT_QUOTE.to_owned()],
                        "B asks about Q by id and nothing else: {out}"
                    );
                    assert!(b.status_calls.is_empty(), "{out}");
                    assert!(
                        b.invoices.is_empty() && b.quotes.is_empty() && b.melts.is_empty(),
                        "B was held at RECONCILIATION, before any plan: no invoice, no quote, no payment: {out}"
                    );
                    assert!(!out.contains("released 15 sats"), "at {now}: {out}");
                    assert_eq!(
                        out.lines()
                            .filter(|line| line.starts_with("  HELD: remittance hash-13-2-a"))
                            .count(),
                        1,
                        "at {now}: exactly one HELD line, naming the row: {out}"
                    );
                    assert_eq!(
                        store_b
                            .in_flight_remittance()
                            .expect("query")
                            .expect("still X")
                            .state,
                        RemittanceState::Spending,
                        "at {now}: held, nothing written"
                    );
                    assert_eq!(
                        ledger(store_b),
                        (0, 15, 0),
                        "at {now}: the receipts stay pinned to X while A's prepared Q can still pay"
                    );
                    assert_eq!(
                        remaining(&proofs),
                        vec![1, 2, 4, 8],
                        "at {now}: B had exact proofs for a 15-sat payment and did not use them — reconciliation held it before the wallet was asked; the store's own refusal is exercised below, not inferred from this"
                    );
                    results.push((outcome, out));

                    if trigger == RemitTrigger::DryRun {
                        // Boundary (ii), the STORE — between B's dry-run and its confirm, with A
                        // still parked inside its payment and X spending/bound to Q. B builds a
                        // concrete plan for a DISTINCT invoice Y exactly as `remit_inner` would
                        // (its LNURL pay request, its own `-b`-tagged invoice for the 13-sat net,
                        // the same 15-sat gross and 2-sat reserve its dry-run would print) and
                        // takes it to the store's admission entry — the call at step 6 — on B's
                        // own connection. Y's melt quote is None on purpose: raising an estimate
                        // for Y at the mint IS a quote raised, which this observation asserts did
                        // not happen, and the store's in-flight predicate runs before any use of
                        // the plan's quote id.
                        let mut b_plan = second_process(&registry, &melts);
                        b_plan.clock = Arc::clone(&clock);
                        b_plan.proofs = Some(Arc::clone(&proofs));
                        let address = LightningAddress::parse(PLATFORM_FEE_ADDRESS)
                            .expect("platform address");
                        let pay = b_plan.pay_request(&address).expect("LNURL pay request");
                        let y = b_plan.invoice(&pay, 13).expect("Y invoice");
                        assert!(
                            y.payment_hash.ends_with("-b") && y.payment_hash != X_ID,
                            "Y is B's own invoice, distinct from X: {}",
                            y.payment_hash
                        );
                        let plan_y = RemittancePlan {
                            payment_hash: y.payment_hash.clone(),
                            gross_sats: 15,
                            net_sats: 13,
                            melt_fee_reserve_sats: 2,
                            destination: address.to_string(),
                            bolt11: y.bolt11.clone(),
                            melt_quote_id: None,
                        };
                        let refused = store_b
                            .plan_remittance(
                                &plan_y,
                                b_plan.owner(),
                                now.saturating_add(lease_secs(REMIT_LEASE)),
                                now,
                            )
                            .expect_err("the STORE refuses B's plan while X is in flight");
                        match &refused {
                            PlanRefused::InFlight(active) => {
                                assert_eq!(
                                    (
                                        active.remittance_id.as_str(),
                                        active.state,
                                        active.spending_quote_id.as_deref(),
                                        active.owner.as_deref(),
                                        active.spending_since_unix,
                                    ),
                                    (
                                        X_ID,
                                        RemittanceState::Spending,
                                        Some(X_PAYMENT_QUOTE),
                                        Some("proc-a"),
                                        Some(100),
                                    ),
                                    "the STORE refused B's plan naming X, spending and bound to Q: {active:?}"
                                );
                            }
                            other => panic!(
                                "expected the STORE's PlanRefused::InFlight naming X, got {other:?}"
                            ),
                        }
                        let printed = refused.to_string();
                        assert!(
                            printed.contains("remittance hash-13-2-a")
                                && printed.contains("still in flight"),
                            "what `remit_inner` would print as REFUSED — ...: {printed}"
                        );
                        // Nothing moved by that attempt: no Y row, receipts still pinned to X, no
                        // quote raised at the mint, no proof selected, no debit.
                        assert_eq!(
                            store_b
                                .remittances()
                                .expect("rows")
                                .iter()
                                .map(|row| (row.remittance_id.as_str(), row.state))
                                .collect::<Vec<_>>(),
                            vec![(X_ID, RemittanceState::Spending)],
                            "the STORE wrote no Y row"
                        );
                        assert_eq!(
                            ledger(store_b),
                            (0, 15, 0),
                            "the STORE's refusal left every receipt pinned to X; none became payable"
                        );
                        {
                            let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
                            assert!(
                                quotes.keys().all(|id| id.ends_with("-a")),
                                "B's planning attempt raised no quote at the mint: {:?}",
                                quotes.keys().collect::<Vec<_>>()
                            );
                            assert_eq!(quotes[X_PAYMENT_QUOTE].state, MeltQuoteState::Unpaid);
                        }
                        assert_eq!(
                            b_plan.invoices,
                            vec![13],
                            "Y's LNURL invoice is the only effect"
                        );
                        assert!(
                            b_plan.estimates.is_empty()
                                && b_plan.quotes.is_empty()
                                && b_plan.melts.is_empty(),
                            "no estimate, no payment quote, no melt for Y"
                        );
                        assert_eq!(
                            melts.load(Ordering::SeqCst),
                            0,
                            "no debit while A is still parked and B is refused by the STORE"
                        );
                        assert_eq!(
                            remaining(&proofs),
                            vec![1, 2, 4, 8],
                            "B had exact proofs for Y and the STORE, not the wallet, refused it"
                        );
                    }
                }
                results
            },
        );
        // A resumes: the mint accepts the expired UNPAID quote (no expiry check on that path) and X
        // is paid — once.
        let (a_outcome, a_out) = a_result;
        assert!(
            matches!(a_outcome, Ok(RemitOutcome::Paid { .. })),
            "A's prepared payment lands: {a_outcome:?}\n{a_out}"
        );
        assert!(
            a_out.contains("PAID — remittance hash-13-2-a settled"),
            "{a_out}"
        );
        assert_eq!(b_results.len(), 2);
        assert_eq!(
            melts.load(Ordering::SeqCst),
            1,
            "exactly one actual debit — A's, on X; never two"
        );
        assert_eq!(
            remaining(&proofs),
            vec![1, 2, 4, 8],
            "A spent exactly its reserved 15 sats; B's set is untouched"
        );
        {
            let quotes = registry.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                quotes[X_PAYMENT_QUOTE].state,
                MeltQuoteState::Paid,
                "the mint paid the expired quote"
            );
            assert!(
                quotes.keys().all(|id| id.ends_with("-a")),
                "B raised no quote: {:?}",
                quotes.keys().collect::<Vec<_>>()
            );
            assert_eq!(
                quotes
                    .values()
                    .filter(|quote| quote.state == MeltQuoteState::Paid)
                    .count(),
                1,
                "one quote paid at the mint, ever"
            );
        }
        let store = SellerStore::open(&db).expect("open");
        let rows = store.remittances().expect("rows");
        assert_eq!(
            rows.iter()
                .map(|row| (row.remittance_id.as_str(), row.state))
                .collect::<Vec<_>>(),
            vec![(X_ID, RemittanceState::Settled)],
            "one row, X, settled by its owner's melt; no Y ever existed"
        );
        assert_eq!(rows[0].melt_quote_id.as_deref(), Some(X_PAYMENT_QUOTE));
        assert_eq!(rows[0].settled_by, Some(SettledBy::Melt));
        assert_eq!(
            ledger(&store),
            (15, 0, 0),
            "the 15 sats accrued were discharged exactly once"
        );
        // Two attempts journaled: A's paid collect and B's refused confirm (dry-runs journal
        // nothing). B's direct planning attempt at the store journals nothing either — attempts are
        // written by `remit`'s wrapper, and `plan_remittance` refused inside its own transaction.
        let attempts = store.recent_remit_attempts(10).expect("attempts");
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert_eq!(
            (
                attempts[0].trigger,
                attempts[0].outcome,
                attempts[0].remittance_id.as_deref()
            ),
            (
                RemitAttemptTrigger::Collect,
                RemitAttemptOutcome::Paid,
                Some(X_ID)
            )
        );
        assert_eq!(
            (
                attempts[1].trigger,
                attempts[1].outcome,
                attempts[1].remittance_id.as_deref()
            ),
            (
                RemitAttemptTrigger::Command,
                RemitAttemptOutcome::Refused,
                Some(X_ID)
            ),
            "B's --confirm hold at RECONCILIATION is journaled naming X"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The attempts journal is bounded and newest-first, and a limit of zero returns nothing.
    #[test]
    fn recent_attempts_are_newest_first_and_bounded() {
        let (store, root) = store_with_fees("attempts-order", &[]);
        for at in 1..=7 {
            store
                .record_remit_attempt(&RemitAttempt {
                    attempt_id: 0,
                    started_at_unix: at,
                    trigger: RemitAttemptTrigger::Collect,
                    unremitted_sats: 3,
                    outcome: RemitAttemptOutcome::Failed,
                    detail: format!("failure {at}"),
                    remittance_id: None,
                })
                .expect("record");
        }
        let recent = store
            .recent_remit_attempts(RECENT_ATTEMPTS_SHOWN)
            .expect("read");
        assert_eq!(
            recent.iter().map(|a| a.started_at_unix).collect::<Vec<_>>(),
            vec![7, 6, 5, 4, 3]
        );
        assert!(store.recent_remit_attempts(0).expect("read").is_empty());
        let mut out = Vec::new();
        print_recent_attempts(&store, &mut out).expect("print");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.starts_with("Recent attempts (newest first, last 5):\n  unix 7: automatic (after collect) attempt saw 3 sats unremitted — FAILED: failure 7\n"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- retry pacing (addendum 2, gate 2d) ----------------------------------------------------

    fn failed(error: &str) -> RemitReport {
        RemitReport {
            outcome: Err(error.to_owned()),
            lines: Vec::new(),
        }
    }

    fn paid(net_sats: u64) -> RemitReport {
        RemitReport {
            outcome: Ok(RemitOutcome::Paid {
                remittance_id: "r".to_owned(),
                net_sats,
                melt_fee_sats: 1,
            }),
            lines: Vec::new(),
        }
    }

    fn refused(refusal: Refusal) -> RemitReport {
        RemitReport {
            outcome: Ok(RemitOutcome::Refused(refusal)),
            lines: Vec::new(),
        }
    }

    // Gate 2d: consecutive failures GROW the delay from the base, doubling, and it is CAPPED; the
    // jittered delay never exceeds the computed delay for that streak and never falls below zero; a
    // success RESETS it to base. Driven on the pure computation — nothing here sleeps.
    #[test]
    fn retry_backoff_doubles_from_base_caps_at_thirty_minutes_and_resets_on_success() {
        let mut pacing = RemitBackoff::new();
        assert_eq!(pacing.computed_delay(), Duration::from_secs(30));
        assert_eq!(RETRY_BASE, Duration::from_secs(30));
        assert_eq!(RETRY_CAP, Duration::from_secs(30 * 60));

        assert_eq!(
            pacing.observe(&failed("host down"), 1_000),
            Pacing::FirstFailure
        );
        let mut expected = vec![Duration::from_secs(30)];
        let mut seen_cap_transition = 0;
        for attempt in 2..=16u32 {
            let delay = pacing.computed_delay();
            expected.push(delay);
            // Never above the computed delay, never below zero, for the extremes and the middle.
            for entropy in [0, 1, u64::MAX / 3, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
                let slept = pacing_jitter_probe(&pacing, entropy);
                assert!(
                    slept <= delay,
                    "streak {}: {slept:?} > {delay:?}",
                    pacing.streak()
                );
                assert!(slept >= Duration::ZERO);
            }
            assert_eq!(pacing_jitter_probe(&pacing, u64::MAX), delay);
            assert_eq!(pacing_jitter_probe(&pacing, 0), Duration::ZERO);
            let drawn = pacing.next_delay();
            assert!(
                drawn <= delay,
                "the RNG draw stays under the computed delay"
            );
            match pacing.observe(&failed("host down"), 1_000 + i64::from(attempt) * 60) {
                Pacing::RepeatFailure {
                    streak,
                    entered_cap,
                } => {
                    assert_eq!(streak, attempt);
                    if entered_cap {
                        seen_cap_transition += 1;
                    }
                }
                other => panic!("attempt {attempt}: expected a repeat failure, got {other:?}"),
            }
        }
        // 30, 60, 120, 240, 480, 960, 1800 (cap), 1800, 1800, ...
        assert_eq!(
            expected
                .iter()
                .take(8)
                .map(Duration::as_secs)
                .collect::<Vec<_>>(),
            vec![30, 60, 120, 240, 480, 960, 1800, 1800]
        );
        assert!(
            expected.iter().all(|delay| *delay <= RETRY_CAP),
            "the doubling is capped: {expected:?}"
        );
        assert!(
            expected.iter().skip(6).all(|delay| *delay == RETRY_CAP),
            "once capped it stays capped: {expected:?}"
        );
        assert_eq!(
            seen_cap_transition, 1,
            "the transition into the cap is reported exactly once"
        );
        assert!(pacing.at_cap());

        // A success resets to base and reports the streak it ended.
        assert_eq!(
            pacing.observe(&paid(10), 1_000 + 17 * 60),
            Pacing::Recovered {
                failed_attempts: 16,
                owed_for_secs: 17 * 60,
            }
        );
        assert_eq!(pacing.streak(), 0);
        assert_eq!(pacing.computed_delay(), RETRY_BASE);
        assert!(!pacing.at_cap());
        // A success with no streak behind it is just a payment.
        assert_eq!(pacing.observe(&paid(10), 2_000), Pacing::Paid);
        assert_eq!(pacing.computed_delay(), RETRY_BASE);
    }

    fn pacing_jitter_probe(pacing: &RemitBackoff, entropy: u64) -> Duration {
        jittered(pacing.computed_delay(), entropy)
    }

    // Rules 4 and 5: below the threshold is NOT a failure, and a zero balance is not one either —
    // neither escalates the streak, neither resets it, and neither is logged as a failure.
    #[test]
    fn a_balance_under_the_threshold_or_at_zero_neither_escalates_nor_resets_the_backoff() {
        let mut pacing = RemitBackoff::new();
        let below = refused(Refusal::BelowMinimum {
            unremitted: 0,
            min_sats: 1,
        });
        let zero = refused(Refusal::NothingUnremitted);
        assert!(!below.is_failure() && !zero.is_failure());
        assert!(below.is_quiet() && zero.is_quiet());

        // Healthy node: idle at the base interval.
        assert_eq!(pacing.observe(&below, 1), Pacing::Idle);
        assert_eq!(pacing.observe(&zero, 2), Pacing::Idle);
        assert_eq!(pacing.computed_delay(), RETRY_BASE);
        assert_eq!(pacing.streak(), 0);

        // Mid-streak: the threshold outcomes leave the streak exactly where it was — they are not a
        // success, so they do not reset it (rule 3: success and nothing else), and they are not a
        // failure, so they do not lengthen it.
        pacing.observe(&failed("a"), 10);
        pacing.observe(&failed("b"), 11);
        let before = pacing.clone();
        assert_eq!(pacing.observe(&below, 12), Pacing::Idle);
        assert_eq!(pacing.observe(&zero, 13), Pacing::Idle);
        assert_eq!(pacing, before);
        assert_eq!(pacing.computed_delay(), Duration::from_secs(120));

        // A refusal that is NOT at the threshold left the fee owed for a reason retrying every 30 s
        // cannot fix: it paces like a failure.
        let reserve = refused(Refusal::ReserveDoesNotFit {
            gross: 2,
            reserve: 2,
        });
        assert!(reserve.is_failure());
        assert_eq!(
            pacing.observe(&reserve, 14),
            Pacing::RepeatFailure {
                streak: 3,
                entered_cap: false
            }
        );
        // A melt that failed after the plan is a failure too.
        let melt_failed = RemitReport {
            outcome: Ok(RemitOutcome::MeltFailed {
                remittance_id: "r".to_owned(),
                error: "mint timeout".to_owned(),
            }),
            lines: Vec::new(),
        };
        assert!(melt_failed.is_failure());
        // A dry run is nothing to the pacing.
        let dry = RemitReport {
            outcome: Ok(RemitOutcome::DryRun),
            lines: Vec::new(),
        };
        assert!(!dry.is_failure());
        assert_eq!(pacing.observe(&dry, 15), Pacing::Idle);
    }

    // Explicit bounds (for the loop test that cannot sleep 30 minutes): the cap clamps to the base,
    // and the arithmetic is the same.
    #[test]
    fn retry_backoff_honours_explicit_bounds() {
        let mut pacing =
            RemitBackoff::with_bounds(Duration::from_millis(20), Duration::from_millis(50));
        assert_eq!(pacing.computed_delay(), Duration::from_millis(20));
        pacing.observe(&failed("x"), 0);
        assert_eq!(pacing.computed_delay(), Duration::from_millis(40));
        pacing.observe(&failed("x"), 0);
        assert_eq!(pacing.computed_delay(), Duration::from_millis(50));
        assert!(pacing.at_cap());
        let clamped = RemitBackoff::with_bounds(Duration::from_secs(5), Duration::from_secs(1));
        assert_eq!(clamped.computed_delay(), Duration::from_secs(5));
        assert!(clamped.at_cap());
    }

    // Addendum 3 RULING 1: the first attempt after boot fires no earlier than one base delay, with
    // an additive jitter in [0, base] — [30 s, 60 s]. Zero is never a legal first delay.
    #[test]
    fn the_boot_delay_is_never_less_than_the_base_and_at_most_twice_it() {
        assert_eq!(boot_delay_for(RETRY_BASE, 0), Duration::from_secs(30));
        assert_eq!(
            boot_delay_for(RETRY_BASE, u64::MAX),
            Duration::from_secs(60)
        );
        let mid = boot_delay_for(RETRY_BASE, u64::MAX / 2);
        assert!(mid > Duration::from_secs(44) && mid < Duration::from_secs(46));
        for entropy in [0, 1, u64::MAX / 3, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
            let delay = boot_delay_for(RETRY_BASE, entropy);
            assert!(delay >= RETRY_BASE, "{delay:?} is below the base");
            assert!(delay <= RETRY_BASE * 2, "{delay:?} is above twice the base");
        }
        assert_eq!(boot_delay_for(Duration::MAX, u64::MAX), Duration::MAX);
        let pacing = RemitBackoff::new();
        for _ in 0..64 {
            let drawn = pacing.boot_delay();
            assert!(drawn >= RETRY_BASE && drawn <= RETRY_BASE * 2, "{drawn:?}");
        }
        // Explicit bounds: the boot delay follows the base the test set.
        let short =
            RemitBackoff::with_bounds(Duration::from_millis(25), Duration::from_millis(100));
        let drawn = short.boot_delay();
        assert!(drawn >= Duration::from_millis(25) && drawn <= Duration::from_millis(50));
    }

    // Full jitter is a uniform point in [0, computed]: the two extremes are exact, and a huge
    // computed delay does not overflow.
    #[test]
    fn full_jitter_stays_inside_zero_to_computed() {
        let computed = Duration::from_secs(1800);
        assert_eq!(jittered(computed, 0), Duration::ZERO);
        assert_eq!(jittered(computed, u64::MAX), computed);
        let half = jittered(computed, u64::MAX / 2);
        assert!(half > Duration::from_secs(899) && half < Duration::from_secs(901));
        assert!(jittered(Duration::MAX, u64::MAX) <= Duration::MAX);
        assert_eq!(jittered(Duration::ZERO, u64::MAX), Duration::ZERO);
    }

    // Addendum 2 §3: one attempt in flight, ever; a second taker skips (gets `None`) and does not
    // block; the slot frees when the permit drops.
    #[test]
    fn single_flight_admits_one_attempt_and_frees_the_slot_on_drop() {
        let flight = RemitFlight::new();
        assert!(!flight.in_flight());
        let permit = flight.try_acquire().expect("the slot starts free");
        assert!(flight.in_flight());
        assert!(
            flight.try_acquire().is_none(),
            "a second taker skips while the first holds the slot"
        );
        let sibling = flight.clone();
        assert!(
            sibling.try_acquire().is_none(),
            "clones share the one slot — the loop and the collect thread see the same guard"
        );
        drop(permit);
        assert!(!flight.in_flight());
        let again = sibling.try_acquire();
        assert!(
            again.is_some(),
            "the slot is free again once the permit dropped"
        );
        // A permit dropped by a panicking thread frees the slot too: Drop runs on unwind.
        let flight_for_thread = flight.clone();
        drop(again);
        let outcome = std::thread::spawn(move || {
            let _permit = flight_for_thread.try_acquire().expect("free");
            panic!("attempt died");
        })
        .join();
        assert!(outcome.is_err());
        assert!(!flight.in_flight(), "the slot is not leaked by a panic");
    }
}
