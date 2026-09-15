//! The seller node's durable lifecycle state: `$MAXPLAYER_HOME/seller.sqlite`.
//!
//! Opened only by the node (single-owner, guaranteed by the home lock). This SQLite database — in
//! WAL mode, `synchronous=FULL`, foreign keys on — is the **source of truth** for the seller's
//! trade lifecycle: the offers it has seen, the claims it has parked, the awards it has been
//! selected for, the jobs it is running, its deliveries and its collected receipts. Alongside them
//! sits the **nostr event outbox**: every event the node publishes is written to the DB and
//! enqueued in the SAME transaction as the state change that produced it, then handed to an async
//! publisher that retries until the relay confirms it or it expires. A crash between "state
//! changed" and "event sent" therefore never loses the obligation to publish, and never publishes
//! twice — the outbox `dedup_key` makes re-enqueue a no-op and the stored `created_at` makes the
//! signed event's id deterministic, so a re-publish is relay-idempotent.
//!
//! Every transition here is idempotent: replaying an award, a delivery, or a receipt lands the same
//! state and never double-credits. `rusqlite`'s [`Connection`] is `Send` but not `Sync`, so the
//! store keeps it behind a mutex and callers reach it from the async runtime via `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::checks::EnvKind;
use crate::gateway::EventDraft;

/// Current on-disk schema version. v8 added `receipts.fee_bps` / `receipts.fee_sats` (the platform
/// fee, stage 1 — journaled, not remitted). v9 added `receipts.mint_fee_sats` (the mint's own swap
/// fee, so a receipt shows every figure between what the buyer paid and what the seller keeps).
/// v10 (stage 2a) added the `fee_remittances` table and `receipts.remittance_id` — which
/// remittance, if any, discharged each receipt's platform fee — so the unremitted balance is a
/// query and a paid fee can never be paid twice; and `fee_remit_attempts`, the journal of every
/// attempt to pay (automatic or by command) with its outcome, so a failing payout is visible.
/// v11 (stage 2a, addendum 3) added four nullable columns to `fee_remittances`: `owner` and
/// `lease_until_unix` — the durable ownership of a `planned` row, so reconciliation in another
/// process can never release a live payer's intent — and `melt_fee_reserve_sats` / `settled_by`,
/// so a settlement records the reserve of the quote that paid and how the row was settled (by the
/// melt itself, or by reconciliation against the mint, which can report the quote PAID but not the
/// fee it kept).
/// v12 (stage 2a, addendum 4) added one nullable column, `spending_since_unix`: set by the payer's
/// compare-and-set immediately before the melt, it marks the planned row SPENDING — admitted to the
/// irreversible spend — and a spending row is never released on lease expiry, only on a quote the
/// mint reports terminal.
/// v13 (stage 2a, addendum 5) added one nullable column, `spending_quote_id`: the melt quote the
/// compare-and-set BOUND to the row at admission. The payer pays exactly that quote, by id, and never
/// raises another for the row; reconciliation of a spending row asks the mint about that quote by id
/// and releases the row only on a transition naming it ([`SellerStore::release_remittance`]).
pub const SCHEMA_VERSION: i64 = 13;

/// The platform fee as journaled so far: what is owed on paper, what has been remitted, and the
/// figures around them. Returned by [`SellerStore::accrued_fees`]. A query and nothing more — the
/// only things that move the unremitted balance are the remittance writes
/// [`SellerStore::plan_remittance`] / [`SellerStore::settle_remittance`] /
/// [`SellerStore::release_remittance`], driven by `crate::fee_remit` (automatically after a collect,
/// or by `maxplayer seller fees remit --confirm`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccruedFees {
    /// Sum of `amount_sats` (the offer face — what buyers paid) over every receipt ever collected.
    pub total_amount_sats: u64,
    /// Sum of `mint_fee_sats` over the receipts that RECORDED one. Rows from before v9 carry no mint
    /// fee and contribute nothing here; `rows_without_mint_fee` counts them so a read-out can say
    /// "plus N collections whose mint fee was not recorded" instead of presenting this as complete.
    pub total_mint_fee_sats: u64,
    /// Receipts collected before the mint fee was journaled (schema < v9). Their mint fee is
    /// unknown — not zero — and so is what the seller kept of them.
    pub rows_without_mint_fee: usize,
    /// Sum of `fee_sats` (the platform fee) over every receipt ever collected — accrued all-time,
    /// remitted or not.
    pub total_fee_sats: u64,
    /// Sum of `fee_sats` over receipts NOT yet discharged by any remittance (`remittance_id IS
    /// NULL`). This is the figure the remit command pays.
    pub unremitted_fee_sats: u64,
    /// Sum of `fee_sats` over receipts discharged by a SETTLED remittance.
    pub remitted_fee_sats: u64,
    /// Sum of `fee_sats` over receipts pinned to a remittance that is still `planned` — money that
    /// may be in flight at the mint. Non-zero only between a `--confirm` and its settle/fail.
    pub in_flight_fee_sats: u64,
    /// One entry per receipt row, oldest collection first — in practice one per paid job.
    pub by_job: Vec<JobFeeAccrual>,
}

impl AccruedFees {
    /// What the seller kept across the receipts whose mint fee is known:
    /// `Σ(face − mint_fee − platform_fee)` over those rows only. `None` when there are rows but
    /// none of them recorded a mint fee, so a read-out never prints a kept total it could not have
    /// measured.
    pub fn total_kept_sats(&self) -> Option<u64> {
        let known: Vec<u64> = self
            .by_job
            .iter()
            .filter_map(JobFeeAccrual::kept_sats)
            .collect();
        if known.is_empty() && !self.by_job.is_empty() {
            return None;
        }
        Some(known.into_iter().fold(0u64, u64::saturating_add))
    }
}

/// The fee figures journaled beside a receipt in the same insert as the receipt itself — the input
/// half of [`JobFeeAccrual`]. All three are known at the collect seam only after the redeem has
/// classified `Finalize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptFees {
    /// The mint's own swap fee on this payment, in sats.
    pub mint_fee_sats: u64,
    /// The platform rate in force at collection, in basis points (10% = 1000).
    pub fee_bps: u32,
    /// `floor(face × fee_bps / 10_000)` — the platform fee, in sats.
    pub fee_sats: u64,
}

/// One receipt's figures: what the buyer paid, what the mint kept, what the platform fee came to.
/// What the seller keeps is derived by [`Self::kept_sats`], never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFeeAccrual {
    pub job_id: String,
    /// The offer's FACE amount — the price the buyer paid. This is the base the platform fee was
    /// taken on. It is NOT the wallet net; the mint's swap fee is `mint_fee_sats`.
    pub amount_sats: u64,
    /// The mint's own swap fee, deducted by the mint before the sats reached the wallet. `None` on a
    /// row collected before schema v9: the fee was not recorded then, which is a different fact
    /// from a recorded fee of zero, and a read-out must say so rather than print `0`.
    pub mint_fee_sats: Option<u64>,
    /// The platform rate in force at collection, in basis points (10% = 1000).
    pub fee_bps: u32,
    /// `floor(amount_sats × fee_bps / 10_000)`, as written at collection.
    pub fee_sats: u64,
    pub received_at_unix: i64,
    /// The remittance that discharged this receipt's platform fee (`fee_remittances.remittance_id`),
    /// or `None` while it is unremitted. Set when a remittance is planned, cleared if that
    /// remittance fails, kept once it settles.
    pub remittance_id: Option<String>,
}

impl JobFeeAccrual {
    /// `face − mint_fee − platform_fee`, saturating — what the seller keeps of this payment. `None`
    /// when the mint fee was not recorded, because the answer is then unknown, not zero.
    pub fn kept_sats(&self) -> Option<u64> {
        self.mint_fee_sats.map(|mint_fee| {
            crate::platform_fee::kept_sats(self.amount_sats, mint_fee, self.fee_sats)
        })
    }
}

/// One seller-store job the market owes money for — qualified by RECORDED CONFIRMED RESULT
/// PUBLICATION, not mere enqueue. A `deliveries` row means the result was ENQUEUED for
/// publication, not that it reached the world; only where the store holds a CONFIRMED
/// `result:{job_id}` outbox record (`state = 'confirmed'`) do we treat the work as actually
/// published. The row is therefore **local unpaid-publication evidence**, not authoritative wallet
/// debt: the store knows what it recorded, not what a wallet owes.
///
/// It must also be UNPAID (no `receipts` row resolves it) and not a free job
/// (`deliveries.payment != 'none'`; legacy NULL resolves PRICED). This is the supported read for
/// "what have I published and not been paid for?" — the query operator arrears tooling should call
/// rather than hand-rolling `delivered AND no receipt`, which forgets `deliveries.payment` and
/// counts every ENQUEUED (not confirmed-published) job's terminal `delivered` state as a debt.
///
/// Returned by [`SellerStore::arrears`]. `deliveries.job_id` is the PRIMARY KEY, `receipts` is
/// probed with an anti-join, and the publication check is a correlated EXISTS over the UNIQUE
/// `nostr_event_outbox.dedup_key`, so the result is 1:1 with delivery rows — the query cannot fan
/// out and invent debt that was never owed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrearsJob {
    pub job_id: String,
    /// The delivery record's own reference (`deliveries.result_ref`), the seller-authored snapshot
    /// that was published.
    pub result_ref: String,
    pub delivered_at_unix: i64,
}

/// Lifecycle of one remittance attempt. `Planned` and `Spending` are the two states under which
/// money may be moving — together the one in-flight row: at most ONE row may be in flight at a time
/// (enforced by a partial unique index AND by [`SellerStore::plan_remittance`]), which is what makes
/// a second `--confirm` a no-op rather than a second payment.
///
/// On disk (addendum 4 §1, ledger): `Spending` is the in-flight row (`state = 'planned'`) with
/// `spending_since_unix` set — a nullable column added in v12, never a new value in the `state`
/// column, because SQLite cannot widen an existing table's CHECK additively and a store written by
/// an earlier binary of this branch already carries the three-value CHECK. Every reader derives
/// the state from both columns ([`Self::from_columns`]); every writer of the `state` column writes
/// only the three CHECK values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemittanceState {
    /// Journaled before the melt: the receipts it covers are pinned to it and the invoice is known.
    /// The melt has NOT been admitted: nothing has been spent against this row.
    Planned,
    /// The owner's compare-and-set admitted the melt ([`SellerStore::admit_remittance_spend`]) and
    /// BOUND the one quote the payer may pay (`spending_quote_id`): the payer may be mid-melt on that
    /// quote, so the row is released only when the mint reports THAT quote terminal — never on lease
    /// expiry, never on the state of some other quote for the same invoice (addendum 4 §1.2,
    /// addendum 5 §1).
    Spending,
    /// The melt settled; the receipts stay discharged.
    Settled,
    /// The melt did not happen (mint reports the quote failed or expired, or no quote was ever
    /// raised, or the owner refused before spending); the receipts are released back to unremitted.
    Failed,
}

impl RemittanceState {
    /// The state's name, for messages. `Spending` is never written to the `state` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Spending => "spending",
            Self::Settled => "settled",
            Self::Failed => "failed",
        }
    }

    /// The `state` column's value. Only the three CHECK values exist on disk (see the type's doc).
    fn column_value(self) -> &'static str {
        match self {
            Self::Planned | Self::Spending => "planned",
            Self::Settled => "settled",
            Self::Failed => "failed",
        }
    }

    /// The state as read from the `state` column ALONE — `Spending` is indistinguishable from
    /// `Planned` here; use [`Self::from_columns`] where the row is at hand.
    fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "planned" => Ok(Self::Planned),
            "settled" => Ok(Self::Settled),
            "failed" => Ok(Self::Failed),
            other => Err(StoreError(format!("unknown remittance state {other:?}"))),
        }
    }

    /// The state as the two columns encode it: a `planned` row with `spending_since_unix` set is
    /// `Spending`.
    fn from_columns(raw: &str, spending_since_unix: Option<i64>) -> Result<Self, StoreError> {
        match (Self::parse(raw)?, spending_since_unix) {
            (Self::Planned, Some(_)) => Ok(Self::Spending),
            (state, _) => Ok(state),
        }
    }

    /// Whether the row is the one in flight — planned or spending — i.e. money may be moving.
    pub fn is_in_flight(self) -> bool {
        matches!(self, Self::Planned | Self::Spending)
    }
}

/// What the remit command knows BEFORE it pays, journaled by [`SellerStore::plan_remittance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemittancePlan {
    /// The idempotency key: the bolt11 payment hash (hex). One invoice, one row, ever.
    pub payment_hash: String,
    /// The unremitted platform fee this attempt discharges, in sats. Must equal the store's own
    /// unremitted sum at plan time or the plan is refused.
    pub gross_sats: u64,
    /// The invoice amount — what the platform receives: gross minus the melt fee reserve.
    pub net_sats: u64,
    /// The melt fee reserve the estimate quoted on the net invoice — the plan's ceiling figure.
    /// The spend re-checks the reserve of the quote it actually pays under (addendum 3 §1).
    pub melt_fee_reserve_sats: u64,
    /// The Lightning address literal being paid, journaled so a later change leaves history.
    pub destination: String,
    /// The invoice being paid, kept so an interrupted attempt can be reconciled against the mint.
    pub bolt11: String,
    /// The melt quote id from the estimate, if one was raised.
    pub melt_quote_id: Option<String>,
}

/// The invoice a still-PLANNED row is re-pointed at when the live quote's fee reserve differs from
/// the estimate the row was planned on and the planned invoice would not confirm (addendum 10
/// §1.4). Same gross, same receipts, same row: only the invoice-side figures move, by
/// [`SellerStore::replan_remittance`], BEFORE any spend is prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemittanceReplan {
    /// The re-planned invoice amount; must still fit under the row's gross.
    pub net_sats: u64,
    /// The new invoice's payment hash (hex). The row's `remittance_id` — its receipts' pin — stays
    /// the ORIGINAL hash; `payment_hash` is what the ledger and the mint are reconciled on.
    pub payment_hash: String,
    pub bolt11: String,
    /// The live quote's fee reserve, the figure the re-plan was bounded by.
    pub melt_fee_reserve_sats: u64,
    /// The melt quote raised on the new invoice — the one the fence will bind.
    pub melt_quote_id: Option<String>,
}

/// How a `settled` remittance row came to be settled — the row says so itself, because the two
/// paths can observe different things (addendum 3 §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettledBy {
    /// The melt confirmed in this process: net paid, actual melt fee and paying quote all observed.
    Melt,
    /// Reconciliation: the mint reports the quote PAID. The quote carries amount, fee reserve and
    /// id; the fee the mint actually kept is not reported for a quote paid by another run, so the
    /// row records the reserve (the fee's ceiling) and leaves the actual fee unobserved — said so,
    /// never invented.
    Reconciliation,
}

impl SettledBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Melt => "melt",
            Self::Reconciliation => "reconciliation",
        }
    }

    fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "melt" => Ok(Self::Melt),
            "reconciliation" => Ok(Self::Reconciliation),
            other => Err(StoreError(format!("unknown settled_by {other:?}"))),
        }
    }
}

/// What a settlement observed, for [`SellerStore::settle_remittance`]. `None` fields are
/// "not observed", and the row keeps its planned figure (net) or NULL (fee); never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemitSettlement {
    pub net_paid_sats: Option<u64>,
    pub melt_fee_sats: Option<u64>,
    /// The fee reserve of the quote that paid — the actual fee's ceiling. Observable on both paths.
    pub melt_fee_reserve_sats: Option<u64>,
    pub melt_quote_id: Option<String>,
    pub settled_by: SettledBy,
}

/// One row of `fee_remittances`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeRemittance {
    pub remittance_id: String,
    pub gross_sats: u64,
    /// The melt fee the mint actually took. `None` while planned, and on a row settled by
    /// reconciliation (the mint confirms PAID but the fee it kept was not observed).
    pub melt_fee_sats: Option<u64>,
    /// The fee reserve of the quote this row was planned against, replaced at settlement by the
    /// reserve of the quote that actually paid. `None` only on rows written before v11.
    pub melt_fee_reserve_sats: Option<u64>,
    pub net_sats: u64,
    pub destination: String,
    pub melt_quote_id: Option<String>,
    pub payment_hash: String,
    pub bolt11: String,
    pub state: RemittanceState,
    pub created_at_unix: i64,
    pub settled_at_unix: Option<i64>,
    /// How the row was settled; `None` while planned or failed, and on settled rows written before
    /// v11.
    pub settled_by: Option<SettledBy>,
    /// The process that planned this row and is the only one entitled to pay it (addendum 3 §2):
    /// an opaque per-process token. `None` on rows planned before v11.
    pub owner: Option<String>,
    /// Until when the owner's claim stands. Another process may release a `planned` row on
    /// UNPAID / no-quote only once this has passed — or on a quote the mint reports FAILED, which
    /// is terminal whoever owns it. `None` on rows planned before v11 (read as expired).
    pub lease_until_unix: Option<i64>,
    /// When the owner's compare-and-set admitted the melt (addendum 4 §1): `Some` exactly on a
    /// [`RemittanceState::Spending`] row. `None` on every row written before v12.
    pub spending_since_unix: Option<i64>,
    /// The melt quote the compare-and-set bound to this row at admission (addendum 5 §1, rule 1) —
    /// the ONLY quote its owner pays, by id, and the quote reconciliation asks the mint about to
    /// resolve a spending row. `Some` exactly on a row admitted by a v13 binary; `None` on every
    /// planned row, and on a spending row admitted before v13 (which reconciliation resolves by the
    /// invoice's quotes, as before).
    pub spending_quote_id: Option<String>,
    /// How many receipt rows are pinned to this remittance.
    pub receipts: usize,
}

impl FeeRemittance {
    /// Whether `owner`'s claim on this row stands at `now_unix` with MORE than `margin_secs` to
    /// spare — the same predicate [`SellerStore::admit_remittance_spend`] evaluates in SQL
    /// (`lease_until > now + margin`, addendum 4 §1.1). A row with no lease (pre-v11) is read as
    /// expired: fail-closed toward "not yours".
    pub fn lease_holds(&self, owner: &str, now_unix: i64, margin_secs: i64) -> bool {
        self.owner.as_deref() == Some(owner)
            && self
                .lease_until_unix
                .is_some_and(|until| until > now_unix.saturating_add(margin_secs))
    }

    /// Whether the owner's lease has run out at `now_unix` (a missing lease counts as run out).
    pub fn lease_expired(&self, now_unix: i64) -> bool {
        self.lease_until_unix.is_none_or(|until| now_unix >= until)
    }
}

/// The REASON a release is being written, which is also its SQL predicate
/// ([`SellerStore::release_remittance`], addendum 5 §1 rule 2): every release is a conditional
/// state transition that changes zero rows if the row is no longer as the reason found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOn {
    /// A SPENDING row whose BOUND quote (`spending_quote_id`) is named. Names the quote, so the
    /// release lands only if that is still the row's bound quote. **Not emitted by reconciliation
    /// since addendum 6** (`fee_remit::reconcile_decision` holds a bound spending row on
    /// everything but PAID: the mint pays an UNPAID or FAILED quote regardless of expiry, so no
    /// observation proves the bound quote cannot still debit); kept as the store's conditional
    /// transition with its tests, with no automatic caller.
    TerminalBoundQuote { quote_id: String },
    /// A SPENDING row admitted by a v12 binary — before admissions bound a quote — whose invoice's
    /// quote(s) the mint reports terminal: the release v12 had, kept only for rows v12 wrote
    /// (`spending_quote_id IS NULL`). A v13 admission always binds, so this never applies to a row
    /// this binary admitted.
    TerminalUnboundSpending,
    /// A PLANNED row (never admitted: nothing spent against it) whose invoice's quote the mint
    /// reports terminal.
    TerminalQuotePlanned,
    /// A PLANNED row whose owner's lease has run out at `now_unix` — the owner is provably not
    /// spending: its fence refuses inside the margin and, past the lease, changes zero rows. Never
    /// applies to a spending row.
    LeaseExpired { now_unix: i64 },
    /// This process's own PLANNED row: its earlier attempt is over (a process runs one attempt at
    /// a time), or this attempt refused before spending.
    OwnPlanned { owner: String },
}

impl ReleaseOn {
    /// The reason in a phrase, for messages.
    pub fn describe(&self) -> String {
        match self {
            Self::TerminalBoundQuote { quote_id } => {
                format!("its bound melt quote {quote_id} is terminal at the mint")
            }
            Self::TerminalUnboundSpending => {
                "spending without a bound quote (admitted before v13) and its invoice's quote is terminal at the mint"
                    .to_owned()
            }
            Self::TerminalQuotePlanned => {
                "planned, never admitted, and its quote is terminal at the mint".to_owned()
            }
            Self::LeaseExpired { now_unix } => {
                format!("planned, never admitted, and its owner's lease had run out at unix {now_unix}")
            }
            Self::OwnPlanned { owner } => {
                format!("planned, never admitted, and this process's own ({owner})")
            }
        }
    }
}

/// Why the pre-spend compare-and-set changed zero rows ([`SellerStore::admit_remittance_spend`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipLost {
    /// The row no longer exists.
    Missing,
    /// The row is no longer `planned`: another process reconciled it while this one paused (or it
    /// is already `spending` — admitted once; a second admission is refused).
    NotPlanned { state: RemittanceState },
    /// The row is planned but owned by someone else (or by nobody: a pre-v11 row).
    OtherOwner { owner: Option<String> },
    /// The row is this caller's and still planned, but too little of its lease remains to start a
    /// payment safely: another process is entitled to release a PLANNED row once its lease ends
    /// ([`ReleaseOn::LeaseExpired`]), and an admission that landed this close to that instant would
    /// race the release. (Once admitted, the row is spending and no lease releases it.)
    LeaseTooShort {
        lease_until_unix: Option<i64>,
        now_unix: i64,
        margin_secs: i64,
    },
}

impl std::fmt::Display for OwnershipLost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(formatter, "the planned row no longer exists"),
            Self::NotPlanned { state } => write!(
                formatter,
                "the row is no longer planned (now {}): {}",
                state.as_str(),
                if *state == RemittanceState::Spending {
                    "its melt was already admitted once"
                } else {
                    "another process reconciled it"
                }
            ),
            Self::OtherOwner { owner } => write!(
                formatter,
                "the planned row is owned by {}",
                owner
                    .as_deref()
                    .unwrap_or("nobody (planned before ownership was recorded)")
            ),
            Self::LeaseTooShort {
                lease_until_unix,
                now_unix,
                margin_secs,
            } => write!(
                formatter,
                "the row is ours but its lease {} leaves less than the {margin_secs}s spending margin at unix {now_unix}",
                match lease_until_unix {
                    Some(until) => format!("(until unix {until})"),
                    None => "(none recorded)".to_owned(),
                }
            ),
        }
    }
}

/// Who attempted a remittance — the three callers of `crate::fee_remit::remit` that pay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemitAttemptTrigger {
    /// The seller node, after a receipt was journaled `Collected::New`.
    Collect,
    /// `maxplayer seller fees remit --confirm`, run by an operator.
    Command,
    /// The seller node's retry tick (stage 2a, addendum 2): the loop's own backoff clock.
    Retry,
}

impl RemitAttemptTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collect => "collect",
            Self::Command => "command",
            Self::Retry => "retry",
        }
    }

    fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "collect" => Ok(Self::Collect),
            "command" => Ok(Self::Command),
            "retry" => Ok(Self::Retry),
            other => Err(StoreError(format!(
                "unknown remit attempt trigger {other:?}"
            ))),
        }
    }
}

/// How a remittance attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemitAttemptOutcome {
    /// The melt settled; `remittance_id` names the settled row.
    Paid,
    /// Declined and moved nothing, for a reason other than the threshold (an attempt still in
    /// flight, a fee reserve that does not fit, a balance above the destination's maximum).
    Refused,
    /// An error: the LNURL host, the mint quote, the reconciliation query or the melt itself failed.
    /// If a `remittance_id` is named, that row stays `planned` for the next attempt to reconcile.
    Failed,
}

impl RemitAttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paid => "paid",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }

    fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "paid" => Ok(Self::Paid),
            "refused" => Ok(Self::Refused),
            "failed" => Ok(Self::Failed),
            other => Err(StoreError(format!(
                "unknown remit attempt outcome {other:?}"
            ))),
        }
    }
}

/// One row of `fee_remit_attempts` — one attempt to pay the accrued platform fee and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemitAttempt {
    /// Assigned by the store; `0` on a record that has not been written yet.
    pub attempt_id: i64,
    pub started_at_unix: i64,
    pub trigger: RemitAttemptTrigger,
    /// The unremitted balance the attempt saw when it read the ledger.
    pub unremitted_sats: u64,
    pub outcome: RemitAttemptOutcome,
    /// The sentence the attempt printed for its outcome (the error text on `Failed`).
    pub detail: String,
    /// The `fee_remittances` row this attempt planned, if it got as far as journaling one.
    pub remittance_id: Option<String>,
}

/// Why a plan was refused. Typed so the command can print the right sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRefused {
    /// A `planned` row already exists — a payment may be in flight. Following
    /// `crossmint_hop`'s `DuplicatePlanned`: refuse, never stack a second attempt.
    InFlight(Box<FeeRemittance>),
    /// The store's unremitted sum is not what the caller computed — receipts landed (or a
    /// remittance settled) between the read and the plan. Re-read and re-plan; never pay a stale
    /// figure.
    GrossMismatch {
        planned: u64,
        unremitted: u64,
    },
    /// The payment hash was already used by an earlier attempt (any state).
    DuplicateInvoice {
        payment_hash: String,
    },
    /// Nothing is unremitted.
    NothingToRemit,
    Store(StoreError),
}

impl std::fmt::Display for PlanRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InFlight(row) => write!(
                formatter,
                "remittance {} (planned at {}, {} sats to {}) is still in flight; refusing a second \
                 attempt while the first is unresolved",
                row.remittance_id, row.created_at_unix, row.net_sats, row.destination
            ),
            Self::GrossMismatch {
                planned,
                unremitted,
            } => write!(
                formatter,
                "unremitted total moved: planned {planned} sats but the store now holds {unremitted}; \
                 re-run to re-plan"
            ),
            Self::DuplicateInvoice { payment_hash } => write!(
                formatter,
                "invoice {payment_hash} was already used by an earlier remittance attempt"
            ),
            Self::NothingToRemit => write!(formatter, "nothing to remit"),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for PlanRefused {}

impl From<StoreError> for PlanRefused {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<rusqlite::Error> for PlanRefused {
    fn from(value: rusqlite::Error) -> Self {
        Self::Store(value.into())
    }
}

/// Resolve a nullable `payment` column into a [`crate::gateway::PaymentMode`].
///
/// NULL ⇒ [`crate::gateway::PaymentMode::Sat`] — a row written before the column existed, and every
/// such row was a priced job. An unrecognized value resolves the same way, fail-closed: a store this
/// binary cannot read a mode out of is read as PAID, never as free.
fn payment_mode_from_column(stored: Option<String>) -> crate::gateway::PaymentMode {
    match stored.as_deref().map(str::trim) {
        Some(crate::gateway::PAYMENT_NONE) => crate::gateway::PaymentMode::None,
        _ => crate::gateway::PaymentMode::Sat,
    }
}

/// A cloneable handle to the node-owned SQLite state.
#[derive(Clone)]
pub struct SellerStore {
    conn: Arc<Mutex<Connection>>,
}

/// Store open / query failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "seller store error: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self(value.to_string())
    }
}

/// An offer the relay ingester has seen and the node may claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub offer_id: String,
    pub buyer_pubkey: String,
    pub amount_sats: u64,
    pub unit: String,
    pub task: String,
    pub deadline_unix: i64,
    pub targeted: bool,
    /// The harness the offer asked for (`["param", "agent", …]`), canonicalised; `None` ⇒ no
    /// preference. Journaled with the other offer facts because execution can be a RESTART away
    /// from the claim: a resumed job reads its requested harness from here, so it dispatches to
    /// the harness the buyer asked for and not to whichever one happens to be preferred now.
    pub requested_agent: Option<String>,
    /// #686: the output type the buyer declared on the offer's `["output", …]` tag — a MIME / output
    /// type (`text/plain`, `application/json`). Mandatory on ingest, so a row this binary wrote always
    /// carries it; `None` ⇒ a row recorded before this column existed (absence, never a default —
    /// there is no output type to state that a buyer did not state).
    ///
    /// Journaled for the SAME reason as `requested_agent` above: execution can be a RESTART away from
    /// the claim, and the resumed job composes its agent prompt from this row. Unpersisted, the buyer's
    /// declared type would be gone for that job permanently.
    pub output: Option<String>,
    /// How this offer settles (§1.1), read off its `["param","payment", …]` tag at ingest.
    ///
    /// Journaled for the SAME reason as `requested_agent` and `output` above: execution can be a
    /// RESTART away from the claim, and the delivery row records the mode the job settled under. A
    /// row written before this column existed reads NULL ⇒ [`crate::gateway::PaymentMode::Sat`],
    /// which is correct by construction — every job recorded then was priced.
    pub payment_mode: crate::gateway::PaymentMode,
}

/// #591: the target + base a SERVED contribution job clones into its delivery workdir. The buyer's
/// pin is owner-scoped, so `owner_pubkey` records the target's identity; `clone_url` + `base_branch`
/// + `base_oid` are what the clone fetches and checks out. Absent ⇒ a from-scratch job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributionPin {
    pub owner_pubkey: String,
    pub clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobChecks {
    pub job_id: String,
    pub declaration_bytes: Vec<u8>,
    pub env_kind: EnvKind,
    pub env_lock_ref: String,
    pub captured_at_unix: i64,
}

/// The lifecycle state of a job (execution side of a claim that was awarded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Awarded,
    Executing,
    Delivered,
    Paid,
    Failed,
}

impl JobState {
    /// Every variant, so a predicate over states can be checked against all of them rather than
    /// against the one that motivated it.
    pub const ALL: [Self; 5] = [
        Self::Awarded,
        Self::Executing,
        Self::Delivered,
        Self::Paid,
        Self::Failed,
    ];

    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "awarded" => Self::Awarded,
            "executing" => Self::Executing,
            "delivered" => Self::Delivered,
            "paid" => Self::Paid,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    /// Execution is over for this job — nothing will run for it again, so a re-served offer naming
    /// it is not re-claimable. `Delivered` counts: the work is finished and only payment is
    /// outstanding, which is why it holds no execution slot either.
    pub(super) fn is_finished(self) -> bool {
        matches!(self, Self::Delivered | Self::Paid | Self::Failed)
    }

    /// The stored spelling — the same literal the write statements use.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Awarded => "awarded",
            Self::Executing => "executing",
            Self::Delivered => "delivered",
            Self::Paid => "paid",
            Self::Failed => "failed",
        }
    }

    /// Whether a job in this state is occupying execution capacity **right now**.
    ///
    /// This is the single definition of "in flight". [`SellerStore::jobs_in_flight`] builds its SQL
    /// from it and `should_resume_execution` answers with it, so the `queue_depth` on the wire and
    /// the set a restart re-drives cannot drift apart.
    ///
    /// `Delivered` is deliberately excluded: execution has finished and the job is awaiting payment,
    /// so it holds no slot — which is also why `resumable_jobs` selects it but resume does not
    /// execute it.
    pub fn occupies_execution_slot(self) -> bool {
        matches!(self, Self::Awarded | Self::Executing)
    }
}

/// Outcome of parking a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claimed {
    /// A fresh claim row + a fresh outbox enqueue landed.
    New,
    /// The claim already existed — an idempotent replay, nothing re-enqueued.
    Idempotent,
}

/// Outcome of recording an award.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Awarded {
    /// First time this award id was seen: the claim moved to `awarded` and a job row was created.
    New,
    /// This award id was already recorded — a duplicate, ignored (no second job).
    Duplicate,
    /// The award names a claim this node never parked — recorded, but no job created.
    NoClaim,
}

/// Outcome of recording a collected receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collected {
    /// First time this receipt id was seen: the job moved to `paid`.
    New,
    /// This receipt id was already recorded — deduped, not credited a second time.
    Duplicate,
}

/// A pending outbox row the publisher must send. `draft` is the FULL event to sign — kind, content,
/// and every protocol/routing tag (`["v","1"]`, `["t","maxplayer"]`, the `e`/`p` tags) — so what the
/// publisher signs is wire-valid by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxItem {
    pub id: i64,
    pub dedup_key: String,
    pub draft: EventDraft,
    /// The fixed authored-at second: signing with this makes the event id deterministic, so a
    /// re-publish after a crash is idempotent at the relay.
    pub created_at_unix: i64,
    pub attempts: i64,
    pub expires_at_unix: i64,
}

/// A point-in-time view of the store for `status` / reconcile reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub schema_version: i64,
    pub started_at_unix: i64,
    pub offers: i64,
    pub open_claims: i64,
    pub jobs: i64,
    pub pending_outbox: i64,
}

impl SellerStore {
    /// Open (creating if absent) the state DB at `path` with WAL + crash-safe pragmas and ensure
    /// the schema is present.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path.as_ref())?;
        // WAL for concurrent reads alongside the single writer; FULL sync + FK enforcement because
        // this DB holds money-adjacent lifecycle state. A bounded busy timeout avoids an immediate
        // SQLITE_BUSY under contention.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS seller_meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- Offers the ingester has seen. One row per offer event id.
             CREATE TABLE IF NOT EXISTS offers (
                 offer_id        TEXT PRIMARY KEY,
                 buyer_pubkey    TEXT NOT NULL,
                 amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                 unit            TEXT NOT NULL,
                 task            TEXT NOT NULL,
                 deadline_unix   INTEGER NOT NULL,
                 targeted        INTEGER NOT NULL,
                 created_at_unix INTEGER NOT NULL,
                 -- The harness the offer requested. NULL ⇒ no preference, which is also what an
                 -- offer recorded before this column existed reads as.
                 requested_agent TEXT,
                 -- #686: the buyer's declared output type (the offer's `output` tag — a MIME / output
                 -- type). Mandatory on ingest, so this binary always writes it; NULL ⇒ an offer
                 -- recorded before this column existed, which states no output type to the agent.
                 output          TEXT,
                 -- The offer's PAYMENT MODE (spec 1.1): 'none' for a free job, 'sat' otherwise.
                 -- NULL => 'sat', which is what an offer recorded before this column existed reads
                 -- as: the same fail-closed direction the wire default takes.
                 --
                 -- Journaled for the SAME reason requested_agent and output are: execution can be a
                 -- RESTART away from the claim, and the delivery row below records the mode this
                 -- offer settled under. Unpersisted, a resumed free job would write its delivery as
                 -- paid.
                 payment         TEXT
             );
             -- Claims the node parked. `state` is the claim's own lifecycle; `awarded` marks the
             -- one the buyer selected, `released` the ones it stepped back from.
             CREATE TABLE IF NOT EXISTS claims (
                 job_id          TEXT PRIMARY KEY,
                 offer_id        TEXT NOT NULL,
                 state           TEXT NOT NULL CHECK (state IN ('claimed','awarded','released')),
                 -- The seller creq (NUT-18 payment request) authored from the offer terms at CLAIM
                 -- time (audit N-4). It is the single source of truth for the trade's payment terms:
                 -- the delivery cosignature signs ITS hash (never a rebuild from live config, so a
                 -- config change between claim and delivery cannot break the buyer/seller cosig), and
                 -- the restart redeem-guard settles against the mints IT lists (Fix Q — original terms,
                 -- not current config).
                 --
                 -- EMPTY STRING => a FREE claim (spec 2.2): this seat claimed the job and authored
                 -- NO creq, because a free trade has no payment terms. It is NOT null, and that is
                 -- load-bearing: the presence of this ROW is how job_creq answers whether we claimed
                 -- the job at all (#814/#626), so a free claim must be present-and-empty rather than
                 -- absent. Widening the column to NULL would need a table rebuild, which migrate's
                 -- additive-only contract forbids on a live money store.
                 creq            TEXT NOT NULL,
                 created_at_unix INTEGER NOT NULL,
                 updated_at_unix INTEGER NOT NULL
             );
             -- Awards received. `award_id` (the award event id) is UNIQUE so a re-seen award is
             -- deduped and never creates a second job.
             CREATE TABLE IF NOT EXISTS awards (
                 award_id        TEXT PRIMARY KEY,
                 job_id          TEXT NOT NULL,
                 buyer_pubkey    TEXT NOT NULL,
                 created_at_unix INTEGER NOT NULL
             );
             -- Jobs the node is executing (one per awarded claim). `agent_name` is the harness that
             -- actually ran it — the journal row naming which agent did the job, and the evidence
             -- that a harness-requesting job was served by the harness it asked for.
             CREATE TABLE IF NOT EXISTS jobs (
                 job_id          TEXT PRIMARY KEY,
                 offer_id        TEXT NOT NULL,
                 agent_name      TEXT,
                 state           TEXT NOT NULL
                     CHECK (state IN ('awarded','executing','delivered','paid','failed')),
                 created_at_unix INTEGER NOT NULL,
                 updated_at_unix INTEGER NOT NULL,
                 -- The delivery commit oid, journaled immediately AFTER a successful push and BEFORE
                 -- the receipt sign+enqueue (#552). On a still-`awarded`/`executing` row it means the
                 -- delivery was pushed but the enqueue was interrupted: resume FINALIZES from this
                 -- commit (re-sign + enqueue) instead of re-running the agent. NULL ⇒ never pushed.
                 pushed_commit   TEXT,
                 -- #563: a RELAY-DERIVED settled-elsewhere marker. Set only when a resume refine
                 -- fetched POSITIVE settlement evidence for this offer from the relay: our own
                 -- already-published result, or a buyer receipt (settled with us or another seat),
                 -- for the live-deadline residual resume_action would otherwise re-drive. Written
                 -- AFTER that evidence is in hand (arm-after-the-event), never speculatively, so a
                 -- crash mid-derive leaves the row re-checkable. Provenance-honest: relay-derived,
                 -- DISTINCT from a local deliveries row. NULL means not derived-settled; a unix ts
                 -- means when we derived it.
                 settled_elsewhere_at_unix INTEGER
             );
             -- One delivery per job (the seller-authored snapshot the daemon published).
             CREATE TABLE IF NOT EXISTS deliveries (
                 job_id          TEXT PRIMARY KEY,
                 result_ref      TEXT NOT NULL,
                 delivered_at_unix INTEGER NOT NULL,
                 -- Spec 3.2: how this job settled. 'none' => a FREE job: a free job still writes
                 -- the delivery record, with the payment recorded as none. 'sat' => a priced job.
                 -- NULL => a legacy row, which every reader resolves to 'sat'.
                 --
                 -- A free job's terminal jobs.state stays 'delivered' and never advances to 'paid':
                 -- widening the CHECK above needs a table rebuild, which migrate's additive-only
                 -- contract forbids on a live money store. THIS COLUMN is the fact that says the job
                 -- will never advance further. Operator tooling that reads delivered-but-not-paid as
                 -- ARREARS must read this column before it reports.
                 payment         TEXT
             );
             -- Collected receipts. `receipt_id` is UNIQUE — the dedup that stops a replayed
             -- payment from crediting the same job twice.
             CREATE TABLE IF NOT EXISTS receipts (
                 receipt_id      TEXT PRIMARY KEY,
                 job_id          TEXT NOT NULL,
                 amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                 received_at_unix INTEGER NOT NULL,
                 -- `amount_sats` above is the offer FACE — what the buyer paid — not the wallet net.
                 -- Platform fee (stage 1), written in the SAME insert as the receipt so no row can
                 -- exist without its fee. `fee_bps` is the rate in force at collection, in basis
                 -- points (10% = 1000); `fee_sats` is floor(amount_sats × fee_bps / 10_000), i.e.
                 -- charged on the FACE. ACCRUED, NOT REMITTED: these columns record what the fee
                 -- came to; nothing reads them to move money. A row from before v8 reads 0/0 — no
                 -- fee was configured when it was collected, so 0 is the fact, not a guess.
                 fee_bps         INTEGER NOT NULL DEFAULT 0 CHECK (fee_bps >= 0 AND fee_bps <= 10000),
                 fee_sats        INTEGER NOT NULL DEFAULT 0 CHECK (fee_sats >= 0),
                 -- The mint's own swap fee (v9), taken by the mint before the sats reached the
                 -- wallet: wallet net = amount_sats − mint_fee_sats. NULLABLE ON PURPOSE with no
                 -- default: a row from before v9 reads NULL, meaning NOT RECORDED — never a
                 -- measured 0. What the seller keeps (face − mint fee − platform fee) is derived at
                 -- read time from these three columns and is deliberately not a fourth column.
                 mint_fee_sats   INTEGER CHECK (mint_fee_sats IS NULL OR mint_fee_sats >= 0),
                 -- v10 (stage 2a): the fee_remittances row that discharged this receipt's platform
                 -- fee. NULL ⇒ UNREMITTED — the balance `maxplayer seller fees remit` pays. Set in
                 -- the same transaction that journals a planned remittance, cleared if that
                 -- remittance fails, kept once it settles. Not a foreign key on purpose: the
                 -- additive-only migration cannot add one, and the fresh schema must match it.
                 remittance_id   TEXT
             );
             -- Intent-to-receive breadcrumbs, written BEFORE the mint swap (payment ordering,
             -- invariant 3). A breadcrumb records ONLY that a swap was attempted for a token — it is
             -- NEVER proof the swap landed (the mint reporting already-spent + a COMPLETED receipt is
             -- the only proof of our own prior collection). `token_hash` is SHA-256 of the token
             -- string; no proof/secret material is stored.
             CREATE TABLE IF NOT EXISTS pending_receive (
                 job_id          TEXT NOT NULL,
                 token_hash      TEXT NOT NULL,
                 buyer_pubkey    TEXT NOT NULL,
                 mint            TEXT NOT NULL,
                 amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                 created_at_unix INTEGER NOT NULL,
                 PRIMARY KEY (job_id, token_hash)
             );
             -- The nostr event outbox. `dedup_key` (UNIQUE) makes an enqueue idempotent; `draft_json`
             -- is the full serialized EventDraft (kind + content + all protocol/routing tags) so the
             -- publisher signs a wire-valid event. The publisher drains `pending` rows, signs with
             -- the fixed `created_at_unix` (so the event id is deterministic and re-publish is
             -- relay-idempotent), and marks each `confirmed` or `expired`.
             CREATE TABLE IF NOT EXISTS nostr_event_outbox (
                 id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                 dedup_key          TEXT NOT NULL UNIQUE,
                 draft_json         TEXT NOT NULL,
                 created_at_unix    INTEGER NOT NULL,
                 state              TEXT NOT NULL CHECK (state IN ('pending','confirmed','expired')),
                 attempts           INTEGER NOT NULL DEFAULT 0,
                 expires_at_unix    INTEGER NOT NULL,
                 published_event_id TEXT,
                 updated_at_unix    INTEGER NOT NULL
             );
             -- #591: the pinned target + base a SERVED contribution job clones into its delivery
             -- workdir. One row per contribution job, written at claim time (the only place the offer
             -- tags are in scope). ABSENT ⇒ a from-scratch job — the empty-workdir default. A store
             -- from a pre-#591 binary simply has no rows here, so the fallback is unchanged.
             CREATE TABLE IF NOT EXISTS contribution_pins (
                 job_id          TEXT PRIMARY KEY,
                 owner_pubkey    TEXT NOT NULL,
                 clone_url       TEXT NOT NULL,
                 base_branch     TEXT NOT NULL,
                 base_oid        TEXT NOT NULL,
                 created_at_unix INTEGER NOT NULL
             );
             -- #599: the exact checks declaration captured from the pinned base plus its resolved,
             -- immutable environment reference. Additive: older stores simply have no rows.
             CREATE TABLE IF NOT EXISTS job_checks (
                 job_id             TEXT PRIMARY KEY,
                 declaration_bytes  BLOB NOT NULL,
                 env_kind           TEXT NOT NULL,
                 env_lock_ref       TEXT NOT NULL,
                 captured_at_unix   INTEGER NOT NULL
             );
             -- v10 (stage 2a): every attempt to remit the accrued platform fee, one row per invoice.
             -- `remittance_id` IS the bolt11 payment hash (hex) — the idempotency key: one invoice
             -- can be journaled once, ever. `gross_sats` is the unremitted fee the attempt
             -- discharges; `net_sats` the invoice amount (gross minus the melt fee reserve — the fee
             -- comes OUT of the gross, never on top); `melt_fee_sats` what the mint actually took,
             -- NULL until settled. `destination` is the address LITERAL paid, so a later change of
             -- the constant leaves history. `state` moves planned → settled | failed; the receipts
             -- pinned to a planned row (receipts.remittance_id) are released on failed and kept on
             -- settled. The partial unique index below lets at most ONE row be planned at a time.
             -- v11 (addendum 3): `owner` / `lease_until_unix` are the planned row's durable
             -- ownership — only the owner pays it, and another process may release it on
             -- UNPAID/no-quote only after the lease, or on a quote the mint reports FAILED;
             -- `melt_fee_reserve_sats` is the reserve of the quote planned against, replaced at
             -- settlement by the reserve of the quote that paid; `settled_by` says whether the melt
             -- itself or reconciliation settled the row. All four nullable, reaching existing stores
             -- through `migrate` as ALTER TABLE ADD COLUMN — never a rebuild.
             -- v12 (addendum 4): `spending_since_unix` marks a planned row SPENDING — the owner's
             -- compare-and-set set it immediately before the melt, with a fresh clock. A spending
             -- row is released only on a quote the mint reports terminal, never on lease expiry.
             -- A column rather than a fourth `state` value because SQLite cannot widen this
             -- table's CHECK on an existing store; the one-in-flight index is unchanged, since a
             -- spending row is still the one `planned` row. Nullable, additive, ALTER-added below.
             -- v13 (addendum 5): `spending_quote_id` is the melt quote the compare-and-set BOUND to
             -- the row at admission — the only quote the owner pays (by id, never re-quoting), and
             -- the quote reconciliation checks by id to resolve a spending row. Every release is a
             -- conditional UPDATE carrying its reason's predicate (`release_remittance`); a release
             -- of a spending row must name this quote. Nullable, additive, ALTER-added below.
             CREATE TABLE IF NOT EXISTS fee_remittances (
                 remittance_id   TEXT PRIMARY KEY,
                 gross_sats      INTEGER NOT NULL CHECK (gross_sats >= 0),
                 melt_fee_sats   INTEGER CHECK (melt_fee_sats IS NULL OR melt_fee_sats >= 0),
                 net_sats        INTEGER NOT NULL CHECK (net_sats >= 0 AND net_sats <= gross_sats),
                 destination     TEXT NOT NULL,
                 melt_quote_id   TEXT,
                 payment_hash    TEXT NOT NULL UNIQUE,
                 bolt11          TEXT NOT NULL,
                 state           TEXT NOT NULL CHECK (state IN ('planned','settled','failed')),
                 created_at_unix INTEGER NOT NULL,
                 settled_at_unix INTEGER,
                 owner           TEXT,
                 lease_until_unix INTEGER,
                 melt_fee_reserve_sats INTEGER CHECK (melt_fee_reserve_sats IS NULL OR melt_fee_reserve_sats >= 0),
                 settled_by      TEXT CHECK (settled_by IS NULL OR settled_by IN ('melt','reconciliation')),
                 spending_since_unix INTEGER,
                 spending_quote_id TEXT
             );
             CREATE UNIQUE INDEX IF NOT EXISTS fee_remittances_one_planned
                 ON fee_remittances (state) WHERE state = 'planned';
             -- v10 (stage 2a, addendum 1): every ATTEMPT to pay the accrued platform fee, whether it
             -- paid, was refused, or failed — the record an operator reads when the automatic payout
             -- is not landing. `trigger` says who attempted ('collect' = the seller node after a
             -- receipt was journaled New; 'retry' = the seller node's backoff tick, addendum 2;
             -- 'command' = `maxplayer seller fees remit --confirm`). The table is new in v10, which
             -- has not shipped, so widening the CHECK here is the table's first definition on every
             -- store that will ever have it — no existing table is rebuilt;
             -- `unremitted_sats` is the balance the attempt saw; `remittance_id` names the
             -- fee_remittances row it planned, if it got that far. Attempts that stop at the threshold
             -- (nothing unremitted, or below the destination's minimum) are the expected steady state
             -- for small sellers and are NOT journaled here. Additive: older stores simply have no rows.
             CREATE TABLE IF NOT EXISTS fee_remit_attempts (
                 attempt_id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 started_at_unix  INTEGER NOT NULL,
                 trigger          TEXT NOT NULL CHECK (trigger IN ('collect','command','retry')),
                 unremitted_sats  INTEGER NOT NULL CHECK (unremitted_sats >= 0),
                 outcome          TEXT NOT NULL CHECK (outcome IN ('paid','refused','failed')),
                 detail           TEXT NOT NULL,
                 remittance_id    TEXT
             );",
        )?;
        Self::migrate(conn)?;
        conn.execute(
            "INSERT INTO seller_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE CAST(seller_meta.value AS INTEGER) < CAST(excluded.value AS INTEGER)",
            [SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Bring a store created by an older binary up to [`SCHEMA_VERSION`]. `CREATE TABLE IF NOT
    /// EXISTS` never alters a table that already exists, so a column added to the schema above
    /// reaches existing stores only through here.
    ///
    /// Every step is ADDITIVE and idempotent — a nullable or DEFAULT-valued column whose absence
    /// reads the same as its default. Nothing here rewrites or drops a row: this store holds live
    /// trade state.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        if !Self::column_exists(conn, "offers", "requested_agent")? {
            conn.execute_batch("ALTER TABLE offers ADD COLUMN requested_agent TEXT;")?;
        }
        // #552: the pushed-delivery marker. A store from a pre-#552 binary reads NULL for its
        // awarded/executing rows and is armed going forward at push time. Pre-existing stale rows are
        // caught at resume by the deadline-lapse check (a passed deadline ⇒ fail, never re-drive); the
        // narrow live-deadline residual (pushed pre-marker, or settled elsewhere) is a tracked follow-up.
        if !Self::column_exists(conn, "jobs", "pushed_commit")? {
            conn.execute_batch("ALTER TABLE jobs ADD COLUMN pushed_commit TEXT;")?;
        }
        // #563: the relay-derived "settled elsewhere" marker. A store from a pre-#563 binary reads
        // NULL (not derived-settled) for its rows and is armed going forward at resume time. Additive
        // + idempotent, exactly like the columns above.
        if !Self::column_exists(conn, "jobs", "settled_elsewhere_at_unix")? {
            conn.execute_batch("ALTER TABLE jobs ADD COLUMN settled_elsewhere_at_unix INTEGER;")?;
        }
        // #686: the buyer's declared output type. A store from a pre-#686 binary reads NULL for its
        // existing offers — those jobs simply state no output type in their agent prompt — and is
        // armed going forward at the next ingest. Additive + idempotent, exactly like the columns above.
        if !Self::column_exists(conn, "offers", "output")? {
            conn.execute_batch("ALTER TABLE offers ADD COLUMN output TEXT;")?;
        }
        // §3.2 — the payment mode, on both the offer it was stated on and the delivery it settled
        // under. A store from a pre-free-lane binary reads NULL for its existing rows, which every
        // reader resolves to 'sat': those jobs were all priced, so the default is not a guess.
        // Additive + idempotent, exactly like the columns above; nothing is rewritten or dropped.
        if !Self::column_exists(conn, "offers", "payment")? {
            conn.execute_batch("ALTER TABLE offers ADD COLUMN payment TEXT;")?;
        }
        if !Self::column_exists(conn, "deliveries", "payment")? {
            conn.execute_batch("ALTER TABLE deliveries ADD COLUMN payment TEXT;")?;
        }
        // v8 — the platform fee (stage 1) journaled beside each receipt. A store from an earlier
        // binary reads 0 for both on every existing row, which is the truth of those rows: no fee was
        // configured when they were collected. `NOT NULL DEFAULT 0` is still additive — SQLite serves
        // the default for pre-existing rows without rewriting them. Additive + idempotent, exactly
        // like the columns above.
        if !Self::column_exists(conn, "receipts", "fee_bps")? {
            conn.execute_batch(
                "ALTER TABLE receipts ADD COLUMN fee_bps INTEGER NOT NULL DEFAULT 0
                     CHECK (fee_bps >= 0 AND fee_bps <= 10000);",
            )?;
        }
        if !Self::column_exists(conn, "receipts", "fee_sats")? {
            conn.execute_batch(
                "ALTER TABLE receipts ADD COLUMN fee_sats INTEGER NOT NULL DEFAULT 0
                     CHECK (fee_sats >= 0);",
            )?;
        }
        // v9 — the mint's swap fee beside each receipt. Nullable with NO default: every pre-existing
        // row reads NULL, which the read path reports as "mint fee not recorded". A default of 0
        // would invent a measurement for a collection nobody measured. Additive + idempotent.
        if !Self::column_exists(conn, "receipts", "mint_fee_sats")? {
            conn.execute_batch(
                "ALTER TABLE receipts ADD COLUMN mint_fee_sats INTEGER
                     CHECK (mint_fee_sats IS NULL OR mint_fee_sats >= 0);",
            )?;
        }
        // v10 — which remittance discharged each receipt's platform fee. Nullable, no default: every
        // pre-existing row reads NULL, i.e. UNREMITTED, which is the truth of a store that has never
        // remitted. The `fee_remittances` table and its index are created by `CREATE ... IF NOT
        // EXISTS` in the schema above, which runs on every open. Additive + idempotent.
        if !Self::column_exists(conn, "receipts", "remittance_id")? {
            conn.execute_batch("ALTER TABLE receipts ADD COLUMN remittance_id TEXT;")?;
        }
        // v11 — ownership and settlement provenance on the remittance row (addendum 3). Nullable, no
        // default: a v10 row reads `owner = NULL, lease_until_unix = NULL`, which every reader treats
        // as an EXPIRED claim by nobody — fail-closed toward "not yours to pay", releasable by
        // reconciliation once its quote is known terminal or unpaid. `melt_fee_reserve_sats` and
        // `settled_by` read NULL: not recorded, never a guess. Additive + idempotent.
        if !Self::column_exists(conn, "fee_remittances", "owner")? {
            conn.execute_batch("ALTER TABLE fee_remittances ADD COLUMN owner TEXT;")?;
        }
        if !Self::column_exists(conn, "fee_remittances", "lease_until_unix")? {
            conn.execute_batch("ALTER TABLE fee_remittances ADD COLUMN lease_until_unix INTEGER;")?;
        }
        if !Self::column_exists(conn, "fee_remittances", "melt_fee_reserve_sats")? {
            conn.execute_batch(
                "ALTER TABLE fee_remittances ADD COLUMN melt_fee_reserve_sats INTEGER
                     CHECK (melt_fee_reserve_sats IS NULL OR melt_fee_reserve_sats >= 0);",
            )?;
        }
        if !Self::column_exists(conn, "fee_remittances", "settled_by")? {
            conn.execute_batch(
                "ALTER TABLE fee_remittances ADD COLUMN settled_by TEXT
                     CHECK (settled_by IS NULL OR settled_by IN ('melt','reconciliation'));",
            )?;
        }
        // v12 — the SPENDING mark on the in-flight remittance row (addendum 4 §1). Nullable, no
        // default: every pre-existing planned row reads NULL, i.e. PLANNED — its melt was never
        // admitted by a compare-and-set, so the pre-v12 release rules (owner gone or quote terminal)
        // still apply to it, which is the truth of a row written before the fence existed. Old rows
        // are otherwise untouched; the `state` CHECK and the one-in-flight index are unchanged.
        // Additive + idempotent.
        if !Self::column_exists(conn, "fee_remittances", "spending_since_unix")? {
            conn.execute_batch(
                "ALTER TABLE fee_remittances ADD COLUMN spending_since_unix INTEGER;",
            )?;
        }
        // v13 — the quote BOUND to the in-flight row at admission (addendum 5 §1). Nullable, no
        // default: every pre-existing row reads NULL — a planned row has no bound quote yet (the
        // fence sets it), and a spending row admitted by a v12 binary was admitted without one, so
        // reconciliation resolves it by the invoice's quotes as v12 did. Nothing rewritten, the
        // `state` CHECK and the one-in-flight index unchanged. Additive + idempotent.
        if !Self::column_exists(conn, "fee_remittances", "spending_quote_id")? {
            conn.execute_batch("ALTER TABLE fee_remittances ADD COLUMN spending_quote_id TEXT;")?;
        }
        Ok(())
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
        let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Record (idempotently overwrite) the node's most recent start time.
    pub fn record_start(&self, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO seller_meta (key, value) VALUES ('started_at_unix', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [now_unix.to_string()],
        )?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError("state DB mutex poisoned".into()))
    }

    // ---- Offer ingest ---------------------------------------------------------------------------

    /// Record a seen offer. Idempotent: a re-seen offer id is a no-op. Returns whether a new row
    /// landed.
    pub fn record_offer(&self, offer: &Offer, now_unix: i64) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO offers
                 (offer_id, buyer_pubkey, amount_sats, unit, task, deadline_unix, targeted, created_at_unix,
                  requested_agent, output, payment)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                offer.offer_id,
                offer.buyer_pubkey,
                offer.amount_sats as i64,
                offer.unit,
                offer.task,
                offer.deadline_unix,
                offer.targeted as i64,
                now_unix,
                offer.requested_agent,
                offer.output,
                offer.payment_mode.as_wire(),
            ],
        )?;
        Ok(changed == 1)
    }

    /// The `(buyer_pubkey, amount_sats, unit)` of a recorded offer, if any. The award arm reads the
    /// buyer to authorize an award (the award author MUST be the offer's buyer), and the pay path
    /// reads amount/unit as the redeem terms. `None` when the node never recorded this offer.
    pub fn offer_facts(&self, offer_id: &str) -> Result<Option<(String, u64, String)>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT buyer_pubkey, amount_sats, unit FROM offers WHERE offer_id = ?1",
                [offer_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? as u64,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The full recorded [`Offer`], if any. The execute arm needs the task (agent prompt + delivery
    /// message) and the absolute deadline (the unified job timeout) on top of the buyer/amount/unit
    /// that [`Self::offer_facts`] returns. `None` when the node never recorded this offer.
    pub fn offer_row(&self, offer_id: &str) -> Result<Option<Offer>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT offer_id, buyer_pubkey, amount_sats, unit, task, deadline_unix, targeted,
                        requested_agent, output, payment
                 FROM offers WHERE offer_id = ?1",
                [offer_id],
                |row| {
                    Ok(Offer {
                        offer_id: row.get(0)?,
                        buyer_pubkey: row.get(1)?,
                        amount_sats: row.get::<_, i64>(2)? as u64,
                        unit: row.get(3)?,
                        task: row.get(4)?,
                        deadline_unix: row.get(5)?,
                        targeted: row.get::<_, i64>(6)? != 0,
                        requested_agent: row.get(7)?,
                        output: row.get(8)?,
                        // NULL ⇒ `Sat`. Resolved HERE rather than left to the caller so no reader
                        // of this row can accidentally treat "column absent" as a third state.
                        payment_mode: payment_mode_from_column(row.get::<_, Option<String>>(9)?),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// #591: persist the pin a served contribution job clones at execute time, keyed by job_id (the
    /// offer event id). INSERT OR IGNORE — idempotent with the offer/claim re-ingest, so a re-driven
    /// offer never double-writes. `claim_offer` writes this BEFORE `record_offer` so a crash can never
    /// leave an offer recorded (hence claimable/awardable/executable) without its pin — the only crash
    /// window strands a harmless orphan pin (no offer ⇒ no claim ⇒ no execute).
    pub fn record_contribution_pin(
        &self,
        job_id: &str,
        pin: &ContributionPin,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO contribution_pins
                 (job_id, owner_pubkey, clone_url, base_branch, base_oid, created_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                job_id,
                pin.owner_pubkey,
                pin.clone_url,
                pin.base_branch,
                pin.base_oid,
                now_unix,
            ],
        )?;
        Ok(changed == 1)
    }

    /// The pin for a job if it was recorded as a contribution; `None` ⇒ a from-scratch job (execute
    /// provisions an empty workdir). Read at execute time on BOTH the fresh-award and restart paths —
    /// the store is the only source of the served contribution's base there.
    pub fn contribution_pin(&self, job_id: &str) -> Result<Option<ContributionPin>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT owner_pubkey, clone_url, base_branch, base_oid
                 FROM contribution_pins WHERE job_id = ?1",
                [job_id],
                |row| {
                    Ok(ContributionPin {
                        owner_pubkey: row.get(0)?,
                        clone_url: row.get(1)?,
                        base_branch: row.get(2)?,
                        base_oid: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn record_job_checks(
        &self,
        job_id: &str,
        declaration_bytes: &[u8],
        env_kind: EnvKind,
        env_lock_ref: &str,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO job_checks
                 (job_id, declaration_bytes, env_kind, env_lock_ref, captured_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(job_id) DO UPDATE SET
                 declaration_bytes = excluded.declaration_bytes,
                 env_kind = excluded.env_kind,
                 env_lock_ref = excluded.env_lock_ref,
                 captured_at_unix = excluded.captured_at_unix",
            params![
                job_id,
                declaration_bytes,
                env_kind.as_str(),
                env_lock_ref,
                now_unix,
            ],
        )?;
        Ok(())
    }

    pub fn job_checks(&self, job_id: &str) -> Result<Option<JobChecks>, StoreError> {
        let conn = self.lock()?;
        let raw = conn
            .query_row(
                "SELECT job_id, declaration_bytes, env_kind, env_lock_ref, captured_at_unix
                 FROM job_checks WHERE job_id = ?1",
                [job_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        raw.map(|(job_id, declaration_bytes, env_kind, env_lock_ref, captured_at_unix)| {
            let env_kind = EnvKind::from_wire(&env_kind)
                .ok_or_else(|| StoreError(format!("unknown persisted env_kind {env_kind:?}")))?;
            Ok(JobChecks {
                job_id,
                declaration_bytes,
                env_kind,
                env_lock_ref,
                captured_at_unix,
            })
        })
        .transpose()
    }

    // ---- Claim (state change + outbox enqueue in one transaction) -------------------------------

    /// Park a claim and enqueue its claim event in ONE transaction: either both the claim row and
    /// the outbox row land, or neither does. Idempotent — a replay for a `job_id` that already has
    /// a claim row changes nothing and re-enqueues nothing.
    ///
    /// `draft` is the full claim nostr event to publish (kind + content + protocol/routing tags);
    /// `created_at_unix` is its fixed authored-at second; `expires_at_unix` bounds how long the
    /// publisher retries before giving up. `creq` is the seller creq (NUT-18 payment request)
    /// authored from the offer terms at claim time (audit N-4) — journaled here so the delivery
    /// cosignature signs its stored hash and the restart redeem-guard settles against its stored
    /// mints, never a rebuild from live config.
    #[allow(clippy::too_many_arguments)]
    pub fn claim_and_enqueue(
        &self,
        job_id: &str,
        offer_id: &str,
        // `None` ⇒ a FREE claim (§2.2), journaled as the empty string so the row still exists and
        // `job_creq` still answers "yes, we claimed this".
        creq: Option<&str>,
        draft: &EventDraft,
        created_at_unix: i64,
        expires_at_unix: i64,
        now_unix: i64,
    ) -> Result<Claimed, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if claim_state(&tx, job_id)?.is_some() {
            tx.commit()?;
            return Ok(Claimed::Idempotent);
        }
        tx.execute(
            "INSERT INTO claims (job_id, offer_id, state, creq, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, 'claimed', ?3, ?4, ?4)",
            params![job_id, offer_id, creq.unwrap_or(""), now_unix],
        )?;
        enqueue_event(
            &tx,
            &format!("claim:{job_id}"),
            draft,
            created_at_unix,
            expires_at_unix,
            now_unix,
        )?;
        tx.commit()?;
        Ok(Claimed::New)
    }

    /// Release a parked claim (offer expired, another seller won, capacity reached). Idempotent:
    /// only a still-`claimed` row is released; `awarded`/`released`/absent are no-ops.
    ///
    /// Returns the number of rows released — 0 or 1. A caller that ANNOUNCES a release must read
    /// this and log the disposition it actually got. The `state = 'claimed'` guard is deliberately
    /// narrow (it is what stops a release from regressing an awarded or terminal row), so a 0 is a
    /// normal outcome, not an error — and a caller that reports success on a 0 is reporting an
    /// action the UPDATE never performed. Use [`Self::claim_row_state`] to name the state instead.
    pub fn release_claim(&self, job_id: &str, now_unix: i64) -> Result<usize, StoreError> {
        let conn = self.lock()?;
        let released = conn.execute(
            "UPDATE claims SET state = 'released', updated_at_unix = ?2
             WHERE job_id = ?1 AND state = 'claimed'",
            params![job_id, now_unix],
        )?;
        Ok(released)
    }

    /// Offers recorded but never claimed and still fresh (`deadline_unix > now`): the capacity-skip
    /// set. `on_offer` records an offer BEFORE it reserves a slot, so an offer skipped for `SlotsBusy`
    /// leaves a row here with no claim. `reconsider_capacity_skips` re-drives these once a slot frees
    /// (#450) — a relay re-subscribe cannot, because the pool suppresses a re-delivery of an
    /// already-seen event. An offer that WAS claimed (even one whose claim later lapsed to `released`)
    /// has a claim row and is excluded: a lapsed-unawarded offer is not re-claimed.
    pub fn offers_awaiting_claim(&self, now_unix: i64) -> Result<Vec<Offer>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT offer_id, buyer_pubkey, amount_sats, unit, task, deadline_unix, targeted,
                    requested_agent, output, payment
             FROM offers
             WHERE deadline_unix > ?1
               AND offer_id NOT IN (SELECT job_id FROM claims)",
        )?;
        let rows = stmt.query_map([now_unix], |row| {
            Ok(Offer {
                offer_id: row.get(0)?,
                buyer_pubkey: row.get(1)?,
                amount_sats: row.get::<_, i64>(2)? as u64,
                unit: row.get(3)?,
                task: row.get(4)?,
                deadline_unix: row.get(5)?,
                targeted: row.get::<_, i64>(6)? != 0,
                requested_agent: row.get(7)?,
                output: row.get(8)?,
                payment_mode: payment_mode_from_column(row.get::<_, Option<String>>(9)?),
            })
        })?;
        let mut offers = Vec::new();
        for row in rows {
            offers.push(row?);
        }
        Ok(offers)
    }

    /// The claim row's state for `job_id` (`claimed` / `awarded` / `released`), or `None` if this
    /// node never parked a claim for it.
    ///
    /// Read by [`Self::release_claim`]'s callers to NAME the state when a release moved no row —
    /// the `state = 'claimed'` guard is narrow by design, and a log that cannot say which state
    /// blocked it reports a release it never made. Also the #450 capacity-skip regression's
    /// assertion that a lapsed claim's row survives as `released` (so a re-delivered offer dedups
    /// on it rather than being re-claimed) while the freed slot lets the capacity-skipped offer
    /// claim.
    pub fn claim_row_state(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let state = conn
            .query_row(
                "SELECT state FROM claims WHERE job_id = ?1",
                [job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(state)
    }

    // ---- Award ----------------------------------------------------------------------------------

    /// Record an award for `job_id`. The `award_id` (award event id) is deduped: the first sighting
    /// moves the claim to `awarded` and creates the job row; a re-seen award id is a
    /// [`Awarded::Duplicate`] no-op (never a second job). An award naming a claim this node never
    /// parked is recorded but creates no job ([`Awarded::NoClaim`]).
    ///
    /// ⛔ AUTHORIZATION IS THE CALLER'S. This writes the `awards` row on the strength of its
    /// arguments alone — it does NOT check that the award's author is the offer's buyer. Every caller
    /// must gate on that first (`on_award` via `match_award`, `on_accept` and the #814 suppression
    /// path inline), or a forged award writes a row and suppresses real work. Pass the buyer read
    /// from OUR OWN recorded offer, never the event's author — that is what keeps the check
    /// non-circular.
    ///
    /// #814 WIDENED WHAT A ROW MEANS, and a reader must know it: an `awards` row used to imply "we
    /// won this job", because the only caller held a claim. The suppression path now records an
    /// authentic buyer award for an offer we recorded but never claimed — someone ELSE's win — so the
    /// row means "an award for this job exists", nothing more. The discriminator for "we won" is a
    /// CLAIM row (what this function's own `claim_state` read uses), never the presence of an award.
    /// [`Self::offers_awarded_elsewhere`] is the complement, and encodes that in SQL.
    pub fn record_award(
        &self,
        award_id: &str,
        job_id: &str,
        buyer_pubkey: &str,
        now_unix: i64,
    ) -> Result<Awarded, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let inserted = tx.execute(
            "INSERT OR IGNORE INTO awards (award_id, job_id, buyer_pubkey, created_at_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![award_id, job_id, buyer_pubkey, now_unix],
        )?;
        if inserted == 0 {
            tx.commit()?;
            return Ok(Awarded::Duplicate);
        }

        let claim = claim_state(&tx, job_id)?;
        let offer_id = match &claim {
            Some((_, offer_id)) => offer_id.clone(),
            None => {
                // Award for a claim we do not hold — record the award, create no job.
                tx.commit()?;
                return Ok(Awarded::NoClaim);
            }
        };
        tx.execute(
            "UPDATE claims SET state = 'awarded', updated_at_unix = ?2 WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO jobs (job_id, offer_id, agent_name, state, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, NULL, 'awarded', ?3, ?3)",
            params![job_id, offer_id, now_unix],
        )?;
        tx.commit()?;
        Ok(Awarded::New)
    }

    /// Buyer pubkey carried by the recorded AWARD for this job.
    pub fn job_award_buyer(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        Ok(conn.query_row(
            "SELECT buyer_pubkey FROM awards WHERE job_id = ?1 ORDER BY created_at_unix, award_id LIMIT 1",
            [job_id], |row| row.get(0)).optional()?)
    }

    /// #814 — offers this node recorded that were AWARDED TO SOMEONE ELSE and are still live:
    /// `(offer_id, buyer_pubkey, deadline_unix)` for every offer holding an award row, holding NO
    /// claim row of ours, whose `deadline_unix` has not passed. The boot re-hydration source for the
    /// in-memory suppression cache, so the claim gate is correct on the FIRST event after a restart
    /// rather than only after a relay backfill succeeds — "relay deafness manufactures absence"
    /// (#560/#563), so a redelivery that never arrives must not be what stands between us and
    /// re-publishing a losing claim.
    ///
    /// Three clauses, each load-bearing:
    /// - `NOT IN (SELECT job_id FROM claims)` IS the hard invariant of #814 in SQL: a job we hold a
    ///   claim for can never be re-hydrated as suppressed, so a resumed award is never stranded (the
    ///   #563 FOIL). It also carries the widened `awards` meaning — an award row alone no longer says
    ///   whose win it was, and only the absent claim says it was not ours. `claims` is keyed by
    ///   `job_id` and matched against `offers.offer_id` because a claim's job id IS its offer id
    ///   (see [`Self::offers_awaiting_claim`], which excludes on the same identity).
    /// - `deadline_unix > ?1` keeps this FAIL-OPEN and bounded: a suppression outlives neither the
    ///   offer it belongs to nor the gate's own `Lapsed` check, so the set cannot grow without limit.
    /// - `buyer_pubkey` comes from the OFFER, never from the award — the same non-circularity the
    ///   live path relies on. A forged award that somehow reached the table still re-hydrates under
    ///   the REAL buyer's key, so it can never satisfy the buyer-bound gate at claim time.
    ///
    /// `EXISTS` rather than a JOIN so two award rows for one job (they are possible — see
    /// [`Self::job_award_time`]) yield ONE row here, not a duplicate.
    pub fn offers_awarded_elsewhere(
        &self,
        now_unix: i64,
    ) -> Result<Vec<(String, String, i64)>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT offer_id, buyer_pubkey, deadline_unix
             FROM offers
             WHERE deadline_unix > ?1
               AND offer_id NOT IN (SELECT job_id FROM claims)
               AND EXISTS (SELECT 1 FROM awards WHERE awards.job_id = offers.offer_id)",
        )?;
        let rows = stmt.query_map([now_unix], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ---- Job execution --------------------------------------------------------------------------

    /// Record which harness ran a job. Idempotent (last write wins).
    pub fn assign_agent(&self, job_id: &str, agent_name: &str) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET agent_name = ?2 WHERE job_id = ?1",
            params![job_id, agent_name],
        )?;
        Ok(())
    }

    /// Move a job to `executing`. Idempotent: only an `awarded` job advances; a job already
    /// executing/delivered/paid is left as-is.
    pub fn mark_executing(&self, job_id: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET state = 'executing', updated_at_unix = ?2
             WHERE job_id = ?1 AND state = 'awarded'",
            params![job_id, now_unix],
        )?;
        Ok(())
    }

    /// Journal the pushed delivery commit for a job (#552). Called immediately AFTER a successful
    /// push and BEFORE the receipt sign+enqueue, so a crash in that window leaves a durable marker:
    /// on resume the job FINALIZES from this commit (re-sign + enqueue) rather than re-running the
    /// agent. Idempotent — last write wins; does NOT change `state` (the atomic advance to
    /// `delivered` stays with `deliver_and_enqueue`).
    pub fn mark_pushed(&self, job_id: &str, commit: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET pushed_commit = ?2, updated_at_unix = ?3 WHERE job_id = ?1",
            params![job_id, commit, now_unix],
        )?;
        Ok(())
    }

    /// Record a delivery and enqueue its result event in ONE transaction. Idempotent — a replay for
    /// a job that already has a delivery row changes nothing and re-enqueues nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn deliver_and_enqueue(
        &self,
        job_id: &str,
        result_ref: &str,
        payment_mode: crate::gateway::PaymentMode,
        draft: &EventDraft,
        created_at_unix: i64,
        expires_at_unix: i64,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM deliveries WHERE job_id = ?1",
                [job_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            tx.commit()?;
            return Ok(false);
        }
        // §3.2 — ruling 3's record, with the payment stated explicitly rather than inferred from
        // the absence of a receipt. A free job's row is written here and never advances past
        // `state = 'delivered'` below; `collect_receipt` is the only writer of `'paid'` and no
        // kind-1059 wrap ever arrives for a free job.
        tx.execute(
            "INSERT INTO deliveries (job_id, result_ref, delivered_at_unix, payment)
             VALUES (?1, ?2, ?3, ?4)",
            params![job_id, result_ref, now_unix, payment_mode.as_wire()],
        )?;
        tx.execute(
            "UPDATE jobs SET state = 'delivered', updated_at_unix = ?2 WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        enqueue_event(
            &tx,
            &format!("result:{job_id}"),
            draft,
            created_at_unix,
            expires_at_unix,
            now_unix,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Mark a job failed. Idempotent (last write wins) but never overwrites a terminal `paid`.
    ///
    /// Returns the number of rows failed — 0 or 1. This is the write `ResumeAction::SkipLapsed`
    /// uses to heal a stale `awarded` row, so a caller that treats it as unconditional can report a
    /// heal that never happened: the `state != 'paid'` guard (and an absent row) both yield 0.
    pub fn fail_job(&self, job_id: &str, now_unix: i64) -> Result<usize, StoreError> {
        let conn = self.lock()?;
        let failed = conn.execute(
            "UPDATE jobs SET state = 'failed', updated_at_unix = ?2
             WHERE job_id = ?1 AND state != 'paid'",
            params![job_id, now_unix],
        )?;
        Ok(failed)
    }

    /// Record a collected receipt and mark the job paid. The `receipt_id` is deduped: the first
    /// sighting credits the job (`New`); a replay is a [`Collected::Duplicate`] no-op that never
    /// marks paid a second time. This is the money-safe boundary — a job is only ever `paid` once,
    /// keyed on the unique receipt id.
    ///
    /// `amount_sats` is the offer FACE — what the buyer paid — as the daemon invariants have always
    /// had it. The fees ([`ReceiptFees`]) ride in the SAME insert: `fee_bps` is the platform rate in
    /// force and `fee_sats` what it came to on that face; `mint_fee_sats` is the mint's own swap fee,
    /// so the row carries every figure between the face and what the seller keeps (derived on read,
    /// never stored). One row, one write — a receipt can never exist without its fees, and nothing is
    /// recorded for a payment that did not land. The caller has all three only after the redeem
    /// classified `Finalize`. Journaled, not remitted: nothing reads these columns to move money.
    pub fn collect_receipt(
        &self,
        receipt_id: &str,
        job_id: &str,
        amount_sats: u64,
        fees: ReceiptFees,
        now_unix: i64,
    ) -> Result<Collected, StoreError> {
        let ReceiptFees {
            mint_fee_sats,
            fee_bps,
            fee_sats,
        } = fees;
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO receipts
                 (receipt_id, job_id, amount_sats, received_at_unix, fee_bps, fee_sats, mint_fee_sats)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                receipt_id,
                job_id,
                amount_sats as i64,
                now_unix,
                i64::from(fee_bps),
                fee_sats as i64,
                mint_fee_sats as i64
            ],
        )?;
        if inserted == 0 {
            tx.commit()?;
            return Ok(Collected::Duplicate);
        }
        tx.execute(
            "UPDATE jobs SET state = 'paid', updated_at_unix = ?2 WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        tx.commit()?;
        Ok(Collected::New)
    }

    /// Write the durable intent-to-receive breadcrumb BEFORE a mint swap (payment ordering, invariant
    /// 3). Idempotent on `(job_id, token_hash)` — a replay is a no-op. A breadcrumb NEVER proves the
    /// swap landed; it exists so a crash between swap and receipt is diagnosable and the re-see is
    /// classified by the COMPLETED-receipt read, not by the breadcrumb.
    pub fn append_pending_receive(
        &self,
        job_id: &str,
        token_hash: &str,
        buyer_pubkey: &str,
        mint: &str,
        amount_sats: u64,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO pending_receive
                 (job_id, token_hash, buyer_pubkey, mint, amount_sats, created_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![job_id, token_hash, buyer_pubkey, mint, amount_sats as i64, now_unix],
        )?;
        Ok(())
    }

    /// Whether a COMPLETED receipt exists for `job_id`. This is the ONLY positive proof of our own
    /// prior collection (finding S): on an already-spent re-see, `true` ⇒ idempotent no-op, `false` ⇒
    /// refuse (never forge a receipt from a breadcrumb), and a read error fails CLOSED at the caller.
    /// The most recent collected receipt's timestamp, or `None` when nothing has ever been
    /// collected. One half of the wrap-backfill cursor.
    pub fn last_receipt_unix(&self) -> Result<Option<i64>, StoreError> {
        let conn = self.lock()?;
        let latest = conn.query_row(
            "SELECT MAX(received_at_unix) FROM receipts",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok(latest)
    }

    /// Delivery timestamp of the OLDEST job that has been delivered but never paid, or `None` when
    /// every delivery has settled. The clamp that stops the wrap-backfill cursor from stepping over
    /// an older job's still-uncollected payment.
    pub fn oldest_unsettled_delivery_unix(&self) -> Result<Option<i64>, StoreError> {
        let conn = self.lock()?;
        let oldest = conn.query_row(
            "SELECT MIN(d.delivered_at_unix) FROM deliveries d
             WHERE NOT EXISTS (SELECT 1 FROM receipts r WHERE r.job_id = d.job_id)",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok(oldest)
    }

    pub fn has_receipt(&self, job_id: &str) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let found = conn
            .query_row(
                "SELECT 1 FROM receipts WHERE job_id = ?1 LIMIT 1",
                [job_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    /// Jobs the market owes money for — actually PUBLISHED (recorded confirmed result
    /// publication) AND unpaid AND not free — the supported seller-store arrears read. The result
    /// is **local unpaid-publication evidence**, not authoritative wallet debt. This is what
    /// operator arrears tooling should call instead of hand-rolling "delivered but no receipt",
    /// which cannot see `deliveries.payment` and would count every enqueued job's terminal
    /// `delivered` state as a debt.
    ///
    /// Rules (issue #975, revised R2):
    /// - The job must have a RECORDED CONFIRMED result publication: a `nostr_event_outbox` row with
    ///   `dedup_key = 'result:<job_id>'` and `state = 'confirmed'`. A `deliveries` row alone means
    ///   ENQUEUED, not published — so a job whose result was only enqueued (still `pending`, a
    ///   failed attempt, or EXPIRED-unpublished) is NOT reported, because the work may never have
    ///   reached the world and the debt may never have been incurred.
    /// - `deliveries.payment = 'none'` is EXCLUDED — a free job is not owed money.
    /// - `payment = 'sat'` AND the legacy `NULL` both resolve PRICED (`payment_mode_from_column`'s
    ///   fail-closed default). A legacy row is reported ONLY when it also carries confirmed
    ///   publication proof; a legacy delivery WITHOUT publication proof is NOT asserted owed.
    /// - Only rows with a `deliveries` row, a confirmed result publication, and NO `receipts` row
    ///   (unpaid) count. Confirmed-and-paid, non-delivered, and enqueued-but-unconfirmed jobs are
    ///   excluded.
    ///
    /// Disclosure of the omitted population: the store deliberately does not report jobs whose
    /// result publication was never confirmed (pending, failed attempt, or expired). These may or
    /// may not have reached the buyer; the store holds no record proving publication, so it does
    /// not assert a debt for them. A job that is enqueued but unconfirmed is therefore ABSENT from
    /// this read — that absence is intentional, not an omission.
    ///
    /// Cardinality: the join is `deliveries` (whose `job_id` is the PRIMARY KEY, so one row per job)
    /// anti-joined against `receipts` via `NOT EXISTS` on the same `job_id` key, and the publication
    /// check is a correlated EXISTS over the UNIQUE `nostr_event_outbox.dedup_key`. `receipts` and
    /// the outbox are only tested for EXISTENCE and never joined through to the output, so a single
    /// published unpaid job appears at most once regardless of how many receipt or outbox rows it
    /// has. The anti-join therefore cannot fan out: one published unpaid job yields exactly one
    /// [`ArrearsJob`].
    ///
    /// A query and nothing more — nothing here moves money, changes state, or writes a row.
    pub fn arrears(&self) -> Result<Vec<ArrearsJob>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT d.job_id, d.result_ref, d.delivered_at_unix
             FROM deliveries d
             WHERE (d.payment IS NULL OR d.payment != 'none')
               AND NOT EXISTS (SELECT 1 FROM receipts r WHERE r.job_id = d.job_id)
               AND EXISTS (SELECT 1 FROM nostr_event_outbox o
                           WHERE o.dedup_key = 'result:' || d.job_id AND o.state = 'confirmed')
             ORDER BY d.delivered_at_unix ASC, d.job_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ArrearsJob {
                job_id: row.get(0)?,
                result_ref: row.get(1)?,
                delivered_at_unix: row.get(2)?,
            })
        })?;
        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row?);
        }
        Ok(jobs)
    }

    /// What the platform fee has come to: the all-time total, how much of it is remitted /
    /// unremitted / in flight, and the receipt rows behind it, oldest collection first. This is the
    /// read-out the remit command settles against; it is a query and nothing more — nothing here
    /// moves the balance it reports.
    ///
    /// Rows collected before the fee existed report `fee_bps = 0, fee_sats = 0` (the migration
    /// default), which is what they owed. Rows collected before v9 report `mint_fee_sats = None`:
    /// the mint fee was not recorded, and the totals say how many such rows there are rather than
    /// counting them as zero. Rows collected before v10 report `remittance_id = None`: unremitted,
    /// which is the truth of a store that has never remitted. `by_job` carries one entry per receipt
    /// row; the collect path receipts a job at most once (`has_receipt` guards the redeem), so that
    /// is one per job.
    pub fn accrued_fees(&self) -> Result<AccruedFees, StoreError> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT r.job_id, r.amount_sats, r.mint_fee_sats, r.fee_bps, r.fee_sats,
                    r.received_at_unix, r.remittance_id, f.state
             FROM receipts r
             LEFT JOIN fee_remittances f ON f.remittance_id = r.remittance_id
             ORDER BY r.received_at_unix ASC, r.receipt_id ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    JobFeeAccrual {
                        job_id: row.get(0)?,
                        amount_sats: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                        mint_fee_sats: row
                            .get::<_, Option<i64>>(2)?
                            .map(|fee| u64::try_from(fee).unwrap_or(0)),
                        fee_bps: u32::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                        fee_sats: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                        received_at_unix: row.get(5)?,
                        remittance_id: row.get(6)?,
                    },
                    row.get::<_, Option<String>>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut totals = AccruedFees::default();
        let mut by_job = Vec::with_capacity(rows.len());
        for (row, remittance_state) in rows {
            totals.total_amount_sats = totals.total_amount_sats.saturating_add(row.amount_sats);
            totals.total_fee_sats = totals.total_fee_sats.saturating_add(row.fee_sats);
            match row.mint_fee_sats {
                Some(mint_fee) => {
                    totals.total_mint_fee_sats =
                        totals.total_mint_fee_sats.saturating_add(mint_fee);
                }
                None => totals.rows_without_mint_fee += 1,
            }
            // A receipt pinned to a row that no longer exists, or to one in an unknown state, is
            // read as unremitted: fail-closed toward "still owed", never toward "already paid".
            match remittance_state.as_deref().map(RemittanceState::parse) {
                Some(Ok(RemittanceState::Settled)) => {
                    totals.remitted_fee_sats =
                        totals.remitted_fee_sats.saturating_add(row.fee_sats);
                }
                Some(Ok(RemittanceState::Planned)) => {
                    totals.in_flight_fee_sats =
                        totals.in_flight_fee_sats.saturating_add(row.fee_sats);
                }
                _ => {
                    totals.unremitted_fee_sats =
                        totals.unremitted_fee_sats.saturating_add(row.fee_sats);
                }
            }
            by_job.push(row);
        }
        totals.by_job = by_job;
        Ok(totals)
    }

    // ---- Platform fee remittance (stage 2a) ------------------------------------------------------

    /// Sum of `fee_sats` over receipts pinned to no remittance — computed inside the caller's
    /// transaction so the plan checks the figure it is about to pin.
    fn unremitted_fee_sats_in(conn: &Connection) -> Result<u64, StoreError> {
        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(fee_sats), 0) FROM receipts WHERE remittance_id IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(sum).unwrap_or(0))
    }

    fn read_remittance(row: &rusqlite::Row<'_>) -> rusqlite::Result<FeeRemittance> {
        let state_raw: String = row.get(8)?;
        let spending_since_unix: Option<i64> = row.get(16)?;
        let state =
            RemittanceState::from_columns(&state_raw, spending_since_unix).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
        let settled_by = row
            .get::<_, Option<String>>(15)?
            .map(|raw| {
                SettledBy::parse(&raw).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        15,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()?;
        Ok(FeeRemittance {
            remittance_id: row.get(0)?,
            gross_sats: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
            melt_fee_sats: row
                .get::<_, Option<i64>>(2)?
                .map(|fee| u64::try_from(fee).unwrap_or(0)),
            net_sats: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
            destination: row.get(4)?,
            melt_quote_id: row.get(5)?,
            payment_hash: row.get(6)?,
            bolt11: row.get(7)?,
            state,
            created_at_unix: row.get(9)?,
            settled_at_unix: row.get(10)?,
            receipts: usize::try_from(row.get::<_, i64>(11)?).unwrap_or(0),
            owner: row.get(12)?,
            lease_until_unix: row.get(13)?,
            melt_fee_reserve_sats: row
                .get::<_, Option<i64>>(14)?
                .map(|reserve| u64::try_from(reserve).unwrap_or(0)),
            settled_by,
            spending_since_unix,
            spending_quote_id: row.get(17)?,
        })
    }

    const REMITTANCE_COLUMNS: &'static str =
        "f.remittance_id, f.gross_sats, f.melt_fee_sats, f.net_sats, f.destination, f.melt_quote_id,
         f.payment_hash, f.bolt11, f.state, f.created_at_unix, f.settled_at_unix,
         (SELECT COUNT(*) FROM receipts r WHERE r.remittance_id = f.remittance_id),
         f.owner, f.lease_until_unix, f.melt_fee_reserve_sats, f.settled_by, f.spending_since_unix,
         f.spending_quote_id";

    fn in_flight_remittance_in(conn: &Connection) -> Result<Option<FeeRemittance>, StoreError> {
        let found = conn
            .query_row(
                &format!(
                    "SELECT {} FROM fee_remittances f WHERE f.state = 'planned' LIMIT 1",
                    Self::REMITTANCE_COLUMNS
                ),
                [],
                Self::read_remittance,
            )
            .optional()?;
        Ok(found)
    }

    /// The single `planned` remittance, if one exists — a payment that may be in flight at the mint
    /// and MUST be reconciled (settled or failed) before another may be planned.
    pub fn in_flight_remittance(&self) -> Result<Option<FeeRemittance>, StoreError> {
        let conn = self.lock()?;
        Self::in_flight_remittance_in(&conn)
    }

    /// Every remittance attempt, oldest first.
    pub fn remittances(&self) -> Result<Vec<FeeRemittance>, StoreError> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(&format!(
            "SELECT {} FROM fee_remittances f ORDER BY f.created_at_unix ASC, f.remittance_id ASC",
            Self::REMITTANCE_COLUMNS
        ))?;
        let rows = statement
            .query_map([], Self::read_remittance)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Journal a remittance BEFORE paying it, and pin every currently-unremitted receipt to it, in
    /// one `IMMEDIATE` transaction. This is the durable intent record the payment is made against:
    /// a crash after it leaves a `planned` row the next run reconciles, never a payment nobody
    /// journaled.
    ///
    /// Refused, with nothing written, when: a `planned` row already exists ([`PlanRefused::InFlight`]
    /// — the precedent is `crossmint_hop`'s refusal of a duplicate Planned record); the store's
    /// unremitted sum differs from `plan.gross_sats` ([`PlanRefused::GrossMismatch`] — the ledger
    /// moved under the caller); the payment hash was already journaled
    /// ([`PlanRefused::DuplicateInvoice`]); or there is nothing unremitted.
    ///
    /// `owner` is the planning process's token and `lease_until_unix` how long its claim stands
    /// (addendum 3 §2): only the owner pays this row — its pre-spend fence
    /// [`Self::admit_remittance_spend`] advances it to spending and binds the quote it pays — and,
    /// while the row is still PLANNED, another process may release it on UNPAID / no-quote only once
    /// the lease has passed. Once admitted (spending, quote bound) the lease no longer matters: the
    /// row is held until the mint reports that quote PAID (addendum 6 §1.2).
    pub fn plan_remittance(
        &self,
        plan: &RemittancePlan,
        owner: &str,
        lease_until_unix: i64,
        now_unix: i64,
    ) -> Result<FeeRemittance, PlanRefused> {
        if plan.net_sats > plan.gross_sats {
            return Err(PlanRefused::Store(StoreError(format!(
                "net {} exceeds gross {}: the melt fee must come out of the gross, never on top",
                plan.net_sats, plan.gross_sats
            ))));
        }
        if owner.trim().is_empty() {
            return Err(PlanRefused::Store(StoreError(
                "a remittance plan needs an owner token".to_owned(),
            )));
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(active) = Self::in_flight_remittance_in(&tx)? {
            return Err(PlanRefused::InFlight(Box::new(active)));
        }
        let unremitted = Self::unremitted_fee_sats_in(&tx)?;
        if unremitted == 0 {
            return Err(PlanRefused::NothingToRemit);
        }
        if unremitted != plan.gross_sats {
            return Err(PlanRefused::GrossMismatch {
                planned: plan.gross_sats,
                unremitted,
            });
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO fee_remittances
                 (remittance_id, gross_sats, melt_fee_sats, net_sats, destination, melt_quote_id,
                  payment_hash, bolt11, state, created_at_unix, settled_at_unix,
                  owner, lease_until_unix, melt_fee_reserve_sats, settled_by)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?1, ?6, ?8, ?7, NULL, ?9, ?10, ?11, NULL)",
            params![
                plan.payment_hash,
                plan.gross_sats as i64,
                plan.net_sats as i64,
                plan.destination,
                plan.melt_quote_id,
                plan.bolt11,
                now_unix,
                RemittanceState::Planned.column_value(),
                owner,
                lease_until_unix,
                plan.melt_fee_reserve_sats as i64,
            ],
        )?;
        if inserted == 0 {
            return Err(PlanRefused::DuplicateInvoice {
                payment_hash: plan.payment_hash.clone(),
            });
        }
        tx.execute(
            "UPDATE receipts SET remittance_id = ?1 WHERE remittance_id IS NULL",
            params![plan.payment_hash],
        )?;
        let row = tx.query_row(
            &format!(
                "SELECT {} FROM fee_remittances f WHERE f.remittance_id = ?1",
                Self::REMITTANCE_COLUMNS
            ),
            params![plan.payment_hash],
            Self::read_remittance,
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// **The pre-spend fence** (addendum 4 §1.1, addendum 5 §1 rule 1): advance the row
    /// `planned → spending` and BIND the quote the payer will pay, by ONE conditional update,
    /// immediately before the irreversible spend —
    ///
    /// ```sql
    /// UPDATE fee_remittances SET spending_since_unix = :now, spending_quote_id = :quote
    ///  WHERE remittance_id = :id AND state = 'planned' AND spending_since_unix IS NULL
    ///    AND owner = :owner AND lease_until_unix > :now + :margin
    /// ```
    ///
    /// `:now` is read from `clock` INSIDE this call, after the store's lock is held and the
    /// `IMMEDIATE` transaction has begun — never a value the caller sampled earlier, however
    /// recently: a payer descheduled between sampling and the lock would otherwise be admitted on a
    /// clock that is no longer now (addendum 5 §1, B2). `clock` is called exactly once; a caller
    /// that wants the instant used reads it from the admitted row's `spending_since_unix` or from
    /// [`OwnershipLost::LeaseTooShort`]. `quote_id` is the melt quote raised for the row's invoice
    /// before this call and checked against the ceiling; from here on the owner pays THAT quote by
    /// id and never raises another for this row.
    ///
    /// **Zero rows changed ⇒ `Err(OwnershipLost)`**, diagnosed from the row as it stands: it is
    /// gone, no longer planned (another process reconciled it, or it is already spending), owned by
    /// someone else, or ours with `margin_secs` or less of lease left — another process is entitled
    /// to release a PLANNED row once its lease ends, and an admission that close would race the
    /// release. `Ok(row)` is the admitted row, now [`RemittanceState::Spending`] with the quote
    /// bound. Once admitted the row is HELD until the mint reports the bound quote PAID
    /// (`fee_remit.rs`, the PAID branch of reconciliation settles it): no terminal-state release,
    /// no clock release (§1.2; addendum 10 §6 item 8) — an UNPAID or FAILED verdict on a bound
    /// quote holds the row too, since the mint can still pay a quote it once reported unpaid.
    ///
    /// One `IMMEDIATE` transaction, so two processes cannot both pass: the second sees the first's
    /// mark and changes zero rows.
    pub fn admit_remittance_spend(
        &self,
        remittance_id: &str,
        owner: &str,
        quote_id: &str,
        margin_secs: i64,
        clock: &mut dyn FnMut() -> i64,
    ) -> Result<Result<FeeRemittance, OwnershipLost>, StoreError> {
        if quote_id.trim().is_empty() {
            return Err(StoreError(
                "a remittance is admitted to spend only against a named melt quote".to_owned(),
            ));
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // The clock, read now: the lock is held and the write transaction has begun, so nothing can
        // change the row between this instant and the UPDATE below.
        let now_unix = clock();
        let changed = tx.execute(
            "UPDATE fee_remittances SET spending_since_unix = ?3, spending_quote_id = ?6
             WHERE remittance_id = ?1 AND state = ?5 AND spending_since_unix IS NULL
               AND owner = ?2 AND lease_until_unix > ?3 + ?4",
            params![
                remittance_id,
                owner,
                now_unix,
                margin_secs,
                RemittanceState::Planned.column_value(),
                quote_id,
            ],
        )?;
        let row = tx
            .query_row(
                &format!(
                    "SELECT {} FROM fee_remittances f WHERE f.remittance_id = ?1",
                    Self::REMITTANCE_COLUMNS
                ),
                params![remittance_id],
                Self::read_remittance,
            )
            .optional()?;
        tx.commit()?;
        let Some(row) = row else {
            return Ok(Err(OwnershipLost::Missing));
        };
        if changed == 1 {
            debug_assert_eq!(row.state, RemittanceState::Spending);
            return Ok(Ok(row));
        }
        // Zero rows changed: say which condition failed, from the row as it stands now.
        if row.state != RemittanceState::Planned {
            return Ok(Err(OwnershipLost::NotPlanned { state: row.state }));
        }
        if row.owner.as_deref() != Some(owner) {
            return Ok(Err(OwnershipLost::OtherOwner {
                owner: row.owner.clone(),
            }));
        }
        Ok(Err(OwnershipLost::LeaseTooShort {
            lease_until_unix: row.lease_until_unix,
            now_unix,
            margin_secs,
        }))
    }

    /// **Re-plan** a still-PLANNED, still-UNBOUND row of ours onto a new invoice (addendum 10 §1.4):
    /// the live quote's fee reserve differs from the estimate the row was planned on and the
    /// planned invoice would not confirm, so the SAME attempt raises a new (re-planned) invoice —
    /// smaller or larger, whatever fits at the live reserve — BEFORE it prepares any spend. ONE
    /// conditional update —
    ///
    /// ```sql
    /// UPDATE fee_remittances SET net_sats, payment_hash, bolt11, melt_fee_reserve_sats, melt_quote_id
    ///  WHERE remittance_id = :id AND state = 'planned' AND spending_since_unix IS NULL AND owner = :owner
    /// ```
    ///
    /// — so a row that was admitted (spending, quote bound), resolved by another process, or never
    /// ours changes ZERO rows ⇒ `Ok(None)`: the caller refuses before the fence and prints that the
    /// row changed under it. `gross_sats` is untouched, the receipts stay pinned to the row's
    /// `remittance_id` (the ORIGINAL payment hash), the owner and lease stand; one row per attempt.
    /// The new `payment_hash` must be unused by any earlier row (the column is UNIQUE) and `net`
    /// must fit under the gross.
    pub fn replan_remittance(
        &self,
        remittance_id: &str,
        owner: &str,
        replan: &RemittanceReplan,
    ) -> Result<Option<FeeRemittance>, StoreError> {
        if replan.payment_hash.trim().is_empty() || replan.bolt11.trim().is_empty() {
            return Err(StoreError(
                "a re-plan names the new invoice: payment hash and bolt11".to_owned(),
            ));
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let gross: Option<i64> = tx
            .query_row(
                "SELECT gross_sats FROM fee_remittances WHERE remittance_id = ?1",
                params![remittance_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(gross) = gross else {
            tx.commit()?;
            return Ok(None);
        };
        if replan.net_sats as i64 > gross {
            return Err(StoreError(format!(
                "re-planned net {} exceeds gross {gross}: the melt fee must come out of the gross, never on top",
                replan.net_sats
            )));
        }
        let changed = tx.execute(
            "UPDATE fee_remittances
             SET net_sats = ?3, payment_hash = ?4, bolt11 = ?5,
                 melt_fee_reserve_sats = ?6, melt_quote_id = ?7
             WHERE remittance_id = ?1 AND state = ?8 AND spending_since_unix IS NULL AND owner = ?2",
            params![
                remittance_id,
                owner,
                replan.net_sats as i64,
                replan.payment_hash,
                replan.bolt11,
                replan.melt_fee_reserve_sats as i64,
                replan.melt_quote_id,
                RemittanceState::Planned.column_value(),
            ],
        )?;
        if changed == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let row = tx.query_row(
            &format!(
                "SELECT {} FROM fee_remittances f WHERE f.remittance_id = ?1",
                Self::REMITTANCE_COLUMNS
            ),
            params![remittance_id],
            Self::read_remittance,
        )?;
        tx.commit()?;
        debug_assert_eq!(row.state, RemittanceState::Planned);
        Ok(Some(row))
    }

    /// Mark a `planned` remittance settled: the melt confirmed (or the mint reports the quote PAID
    /// on reconciliation). The [`RemitSettlement`] carries what was observed — `None` where it could
    /// not be — and says which path settled the row. Pinned receipts stay discharged. Refused if the
    /// row is not `planned` — a settled or failed row never moves again.
    pub fn settle_remittance(
        &self,
        remittance_id: &str,
        settlement: &RemitSettlement,
        now_unix: i64,
    ) -> Result<FeeRemittance, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE fee_remittances
             SET state = ?6,
                 settled_at_unix = ?2,
                 melt_fee_sats = ?3,
                 net_sats = COALESCE(?4, net_sats),
                 melt_quote_id = COALESCE(?5, melt_quote_id),
                 melt_fee_reserve_sats = COALESCE(?8, melt_fee_reserve_sats),
                 settled_by = ?9
             WHERE remittance_id = ?1 AND state = ?7",
            params![
                remittance_id,
                now_unix,
                settlement.melt_fee_sats.map(|fee| fee as i64),
                settlement.net_paid_sats.map(|net| net as i64),
                settlement.melt_quote_id,
                RemittanceState::Settled.column_value(),
                RemittanceState::Planned.column_value(),
                settlement
                    .melt_fee_reserve_sats
                    .map(|reserve| reserve as i64),
                settlement.settled_by.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError(format!(
                "remittance {remittance_id} is not planned; refusing to settle it"
            )));
        }
        let row = tx.query_row(
            &format!(
                "SELECT {} FROM fee_remittances f WHERE f.remittance_id = ?1",
                Self::REMITTANCE_COLUMNS
            ),
            params![remittance_id],
            Self::read_remittance,
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// **Release** the in-flight remittance — mark it failed and return its receipts to unremitted
    /// so the next attempt pays them — by ONE conditional update whose predicate is the REASON for
    /// the release ([`ReleaseOn`]), in one `IMMEDIATE` transaction (addendum 5 §1, rule 2). The
    /// decision to release is taken on a snapshot of the row and the mint's answer; by the time
    /// the UPDATE runs, the row may have moved — its owner may have been admitted (it is now
    /// spending, bound to a quote), or another process may have resolved it. Each predicate
    /// requires the row to still be in the state the reason was decided on, so a stale decision
    /// changes ZERO rows rather than revoking a newer admission. **Zero rows changed ⇒ `Ok(None)`:
    /// HOLD** — nothing written, and the caller prints that the row changed under it; never an
    /// error that aborts the run.
    ///
    /// The predicates, each on top of `remittance_id = :id AND state = 'planned'`:
    /// - [`ReleaseOn::TerminalBoundQuote`] — a SPENDING row, by its bound quote:
    ///   `AND spending_since_unix IS NOT NULL AND spending_quote_id = :quote`. It names the quote,
    ///   so a release decided on some other quote's state changes nothing. **No automatic caller**
    ///   since addendum 6: `fee_remit::reconcile_decision` holds a bound spending row on everything
    ///   but PAID (the mint pays an UNPAID or FAILED quote regardless of expiry, so no observation
    ///   proves the bound quote cannot still debit); the transition and its tests are retained as
    ///   the store's conditional primitive only. A bound spending row is released by nobody in this
    ///   round.
    /// - [`ReleaseOn::TerminalUnboundSpending`] — a spending row a v12 binary admitted without
    ///   binding a quote: `AND spending_since_unix IS NOT NULL AND spending_quote_id IS NULL`.
    /// - [`ReleaseOn::TerminalQuotePlanned`] — a PLANNED row (never admitted) whose invoice's quote
    ///   the mint reports terminal: `AND spending_since_unix IS NULL`.
    /// - [`ReleaseOn::LeaseExpired`] — a PLANNED row whose owner's lease has run out:
    ///   `AND spending_since_unix IS NULL AND lease_until_unix <= :now`. Lease expiry never touches
    ///   a spending row (addendum 4 §1.2), and the clock is compared IN the predicate, so an
    ///   admission that landed first (fresh clock, inside its own lock) is not revoked by a release
    ///   decided on a snapshot taken before it.
    /// - [`ReleaseOn::OwnPlanned`] — this process's own PLANNED row (its earlier attempt is over,
    ///   or it refused before spending): `AND spending_since_unix IS NULL AND owner = :owner`.
    ///
    /// A missing lease (pre-v11 row) is read as expired by [`ReleaseOn::LeaseExpired`]
    /// (`lease_until_unix IS NULL` counts), matching [`FeeRemittance::lease_expired`].
    pub fn release_remittance(
        &self,
        remittance_id: &str,
        on: &ReleaseOn,
        now_unix: i64,
    ) -> Result<Option<FeeRemittance>, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let failed = RemittanceState::Failed.column_value();
        let planned = RemittanceState::Planned.column_value();
        let changed = match on {
            ReleaseOn::TerminalBoundQuote { quote_id } => tx.execute(
                "UPDATE fee_remittances SET state = ?3, settled_at_unix = ?2
                 WHERE remittance_id = ?1 AND state = ?4
                   AND spending_since_unix IS NOT NULL AND spending_quote_id = ?5",
                params![remittance_id, now_unix, failed, planned, quote_id],
            )?,
            ReleaseOn::TerminalUnboundSpending => tx.execute(
                "UPDATE fee_remittances SET state = ?3, settled_at_unix = ?2
                 WHERE remittance_id = ?1 AND state = ?4
                   AND spending_since_unix IS NOT NULL AND spending_quote_id IS NULL",
                params![remittance_id, now_unix, failed, planned],
            )?,
            ReleaseOn::TerminalQuotePlanned => tx.execute(
                "UPDATE fee_remittances SET state = ?3, settled_at_unix = ?2
                 WHERE remittance_id = ?1 AND state = ?4 AND spending_since_unix IS NULL",
                params![remittance_id, now_unix, failed, planned],
            )?,
            ReleaseOn::LeaseExpired { now_unix: at } => tx.execute(
                "UPDATE fee_remittances SET state = ?3, settled_at_unix = ?2
                 WHERE remittance_id = ?1 AND state = ?4 AND spending_since_unix IS NULL
                   AND (lease_until_unix IS NULL OR lease_until_unix <= ?5)",
                params![remittance_id, now_unix, failed, planned, at],
            )?,
            ReleaseOn::OwnPlanned { owner } => tx.execute(
                "UPDATE fee_remittances SET state = ?3, settled_at_unix = ?2
                 WHERE remittance_id = ?1 AND state = ?4 AND spending_since_unix IS NULL
                   AND owner = ?5",
                params![remittance_id, now_unix, failed, planned, owner],
            )?,
        };
        if changed == 0 {
            // The row is not as the reason found it: HOLD, touch nothing (not even the receipts).
            return Ok(None);
        }
        tx.execute(
            "UPDATE receipts SET remittance_id = NULL WHERE remittance_id = ?1",
            params![remittance_id],
        )?;
        let row = tx.query_row(
            &format!(
                "SELECT {} FROM fee_remittances f WHERE f.remittance_id = ?1",
                Self::REMITTANCE_COLUMNS
            ),
            params![remittance_id],
            Self::read_remittance,
        )?;
        tx.commit()?;
        Ok(Some(row))
    }

    /// Journal one remittance attempt and its outcome (see `fee_remit_attempts`). Returns the
    /// assigned `attempt_id`. Append-only: nothing here is ever updated or deleted.
    pub fn record_remit_attempt(&self, attempt: &RemitAttempt) -> Result<i64, StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO fee_remit_attempts
                 (started_at_unix, trigger, unremitted_sats, outcome, detail, remittance_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                attempt.started_at_unix,
                attempt.trigger.as_str(),
                attempt.unremitted_sats as i64,
                attempt.outcome.as_str(),
                attempt.detail,
                attempt.remittance_id,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The most recent `limit` remittance attempts, NEWEST first — what `maxplayer seller fees
    /// remit` prints so an operator can see whether the automatic payout has been landing.
    pub fn recent_remit_attempts(&self, limit: usize) -> Result<Vec<RemitAttempt>, StoreError> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT attempt_id, started_at_unix, trigger, unremitted_sats, outcome, detail,
                    remittance_id
             FROM fee_remit_attempts
             ORDER BY attempt_id DESC
             LIMIT ?1",
        )?;
        let rows = statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                let trigger_raw: String = row.get(2)?;
                let outcome_raw: String = row.get(4)?;
                let trigger = RemitAttemptTrigger::parse(&trigger_raw).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                let outcome = RemitAttemptOutcome::parse(&outcome_raw).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(RemitAttempt {
                    attempt_id: row.get(0)?,
                    started_at_unix: row.get(1)?,
                    trigger,
                    unremitted_sats: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                    outcome,
                    detail: row.get(5)?,
                    remittance_id: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether a delivery has been journaled for `job_id` (#552). A delivery row is written only by
    /// [`Self::deliver_and_enqueue`], atomically with the `delivered` state advance — so this is the
    /// durable proof the result was already produced and enqueued, independent of the `state` column
    /// (belt-and-braces against a lagged state).
    pub fn has_delivery(&self, job_id: &str) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let found = conn
            .query_row(
                "SELECT 1 FROM deliveries WHERE job_id = ?1 LIMIT 1",
                [job_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    /// The delivery commit oid journaled at push time for `job_id`, if any (#552). `Some` on a
    /// still-`awarded`/`executing` row means the delivery was pushed but the enqueue was interrupted
    /// — resume finalizes from it instead of re-running the agent. `None` ⇒ never pushed.
    pub fn pushed_commit(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let commit: Option<String> = conn
            .query_row(
                "SELECT pushed_commit FROM jobs WHERE job_id = ?1",
                [job_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(commit)
    }

    /// #563 — mark a job RELAY-DERIVED as settled elsewhere: a resume refine fetched POSITIVE
    /// settlement evidence for its offer from the relay (our own already-published result, or a buyer
    /// receipt — settled with us or another seat). Written ONLY after that evidence is in hand
    /// (arm-after-the-event), never speculatively on the way into the query, so a crash between issuing
    /// the derive and getting evidence leaves the row re-checkable next restart. Idempotent — last
    /// write wins; does NOT change `state`. Provenance-honest: relay-DERIVED, distinct from a local
    /// `deliveries` row (which only [`Self::deliver_and_enqueue`] writes).
    pub fn mark_settled_elsewhere(&self, job_id: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET settled_elsewhere_at_unix = ?2, updated_at_unix = ?2 WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        Ok(())
    }

    /// Whether `job_id` was relay-derived as settled elsewhere (see [`Self::mark_settled_elsewhere`]).
    /// A resume refine consults this FIRST and short-circuits — a durable marker means it need never
    /// re-query the relay.
    pub fn has_settled_elsewhere(&self, job_id: &str) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let found = conn
            .query_row(
                "SELECT 1 FROM jobs WHERE job_id = ?1 AND settled_elsewhere_at_unix IS NOT NULL",
                [job_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    // ---- Outbox ---------------------------------------------------------------------------------

    /// Every still-`pending` outbox row that has not yet expired (`expires_at_unix > now`),
    /// oldest first — the batch the publisher must send.
    pub fn pending_outbox(&self, now_unix: i64) -> Result<Vec<OutboxItem>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, dedup_key, draft_json, created_at_unix, attempts, expires_at_unix
             FROM nostr_event_outbox
             WHERE state = 'pending' AND expires_at_unix > ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map([now_unix], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (id, dedup_key, draft_json, created_at_unix, attempts, expires_at_unix) = row?;
            let draft: EventDraft = serde_json::from_str(&draft_json)
                .map_err(|error| StoreError(format!("outbox draft decode: {error}")))?;
            items.push(OutboxItem {
                id,
                dedup_key,
                draft,
                created_at_unix,
                attempts,
                expires_at_unix,
            });
        }
        Ok(items)
    }

    /// Mark an outbox row confirmed by the relay, recording the published event id.
    pub fn mark_confirmed(
        &self,
        id: i64,
        published_event_id: &str,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE nostr_event_outbox
             SET state = 'confirmed', published_event_id = ?2, attempts = attempts + 1,
                 updated_at_unix = ?3
             WHERE id = ?1",
            params![id, published_event_id, now_unix],
        )?;
        Ok(())
    }

    /// Bump the attempt counter after a failed publish (the row stays `pending` to retry).
    pub fn record_attempt(&self, id: i64, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE nostr_event_outbox SET attempts = attempts + 1, updated_at_unix = ?2
             WHERE id = ?1",
            params![id, now_unix],
        )?;
        Ok(())
    }

    /// Mark an outbox row expired (retry window elapsed) so the publisher stops sending it.
    pub fn expire_outbox(&self, now_unix: i64) -> Result<usize, StoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE nostr_event_outbox SET state = 'expired', updated_at_unix = ?1
             WHERE state = 'pending' AND expires_at_unix <= ?1",
            [now_unix],
        )?;
        Ok(changed)
    }

    /// The `(state, attempts, published_event_id)` of an outbox row by dedup key. Inspection/tests.
    pub fn outbox_row(
        &self,
        dedup_key: &str,
    ) -> Result<Option<(String, i64, Option<String>)>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT state, attempts, published_event_id FROM nostr_event_outbox
                 WHERE dedup_key = ?1",
                [dedup_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    // ---- Reconcile / inspection -----------------------------------------------------------------

    /// The jobs that must resume after a restart: everything not yet terminal (`awarded`,
    /// `executing`, `delivered`), oldest first. `paid`/`failed` are done and excluded.
    pub fn resumable_jobs(&self) -> Result<Vec<(String, JobState)>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, state FROM jobs
             WHERE state IN ('awarded','executing','delivered')
             ORDER BY created_at_unix, job_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut jobs = Vec::new();
        for row in rows {
            let (job_id, state) = row?;
            let state = JobState::parse(&state)
                .ok_or_else(|| StoreError(format!("unknown job state {state:?}")))?;
            jobs.push((job_id, state));
        }
        Ok(jobs)
    }

    /// The state of a single job, if any. Inspection/tests.
    pub fn job_state(&self, job_id: &str) -> Result<Option<JobState>, StoreError> {
        let conn = self.lock()?;
        let raw: Option<String> = conn
            .query_row("SELECT state FROM jobs WHERE job_id = ?1", [job_id], |row| {
                row.get(0)
            })
            .optional()?;
        match raw {
            None => Ok(None),
            Some(state) => JobState::parse(&state)
                .map(Some)
                .ok_or_else(|| StoreError(format!("unknown job state {state:?}"))),
        }
    }

    /// The unix second the award for `job_id` was recorded, if any. This is a durable, restart-STABLE
    /// value (written once at `record_award`), so the execute path uses it as the delivery commit's
    /// authored-at — a re-created delivery after a restart is then byte-identical (invariant 2). `None`
    /// when the job was never awarded.
    ///
    /// ORDERED, and that is what makes "restart-STABLE" true rather than merely usually-true. The
    /// `awards` PRIMARY KEY is the AWARD id, not the job id, so ONE JOB CAN HOLD MORE THAN ONE ROW —
    /// `SellerNodeRunner::on_accept`'s doc names the hazard in the code's own words, and
    /// `execute_job`'s notes a redundant second award "seen live in the smoke". A bare `SELECT` then
    /// returns whichever row SQLite hands back first, which is free to differ across restarts — and a
    /// differing authored-at is exactly the invariant-2 break this value exists to prevent. Taking the
    /// EARLIEST (`created_at_unix`, `award_id` as the tie-break) is deterministic for any row set, and
    /// matches [`Self::job_award_buyer`] one function below, which has always read this way.
    ///
    /// #814 adds a SECOND route to two rows — an award for an offer we recorded but never claimed is
    /// now persisted too — so this ordering is a precondition of that change, not a nicety.
    pub fn job_award_time(&self, job_id: &str) -> Result<Option<i64>, StoreError> {
        let conn = self.lock()?;
        let ts: Option<i64> = conn
            .query_row(
                "SELECT created_at_unix FROM awards WHERE job_id = ?1
                 ORDER BY created_at_unix, award_id LIMIT 1",
                [job_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(ts)
    }

    /// The creq journaled for a job at claim time (audit N-4). The delivery path signs its hash into
    /// the receipt preimage and the restart redeem-guard reads its mints, so a config change between
    /// claim and delivery can never alter the cosigned terms or the settlement mint set. `None` when
    /// the node never parked a claim for this job.
    /// The claim-time creq journaled for a job.
    ///
    /// ⛔ THREE STATES, NOT TWO. `None` ⇒ this node holds NO claim for the job — the discriminator
    /// #814/#626 rest on. `Some("")` ⇒ a claim exists and it is FREE (§2.2), which carries no
    /// payment terms. `Some(creq)` ⇒ a priced claim. A caller asking "did we claim this" wants
    /// `is_some()`; a caller wanting the payment TERMS must also reject the empty string.
    pub fn job_creq(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let creq: Option<String> = conn
            .query_row("SELECT creq FROM claims WHERE job_id = ?1", [job_id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(creq)
    }

    /// The assigned agent for a job, if any. Inspection/tests.
    pub fn job_agent(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let agent: Option<Option<String>> = conn
            .query_row(
                "SELECT agent_name FROM jobs WHERE job_id = ?1",
                [job_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(agent.flatten())
    }

    /// Read the current health view for `status`.
    /// How many jobs are occupying execution capacity right now.
    ///
    /// ⚠ **Not [`HealthSnapshot::jobs`]**, which is `COUNT(*)` over every job row ever written and is
    /// never pruned. Reading that as "in flight" is what made a seat publish `accepting=n`
    /// permanently from its first job onward (#313): the count's healthy baseline grew with use, so
    /// the seat that had delivered the most looked the busiest and stopped being selectable.
    ///
    /// The state list comes from [`JobState::occupies_execution_slot`] rather than being written out
    /// here, so this count and the resume predicate have one definition between them.
    pub fn jobs_in_flight(&self) -> Result<u32, StoreError> {
        let conn = self.lock()?;
        let occupying: Vec<String> = JobState::ALL
            .iter()
            .filter(|state| state.occupies_execution_slot())
            .map(|state| format!("'{}'", state.as_str()))
            .collect();
        // Every element is a compile-time constant from JobState, so there is no untrusted input in
        // this string; a bound-parameter list cannot be spliced into `IN (...)` without building it.
        let total = count(
            &conn,
            &format!(
                "SELECT COUNT(*) FROM jobs WHERE state IN ({})",
                occupying.join(",")
            ),
        )?;
        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }

    pub fn health(&self) -> Result<HealthSnapshot, StoreError> {
        let conn = self.lock()?;
        let schema_version = read_meta_i64(&conn, "schema_version")?.unwrap_or(0);
        let started_at_unix = read_meta_i64(&conn, "started_at_unix")?.unwrap_or(0);
        let offers = count(&conn, "SELECT COUNT(*) FROM offers")?;
        let open_claims = count(&conn, "SELECT COUNT(*) FROM claims WHERE state = 'claimed'")?;
        let jobs = count(&conn, "SELECT COUNT(*) FROM jobs")?;
        let pending_outbox = count(
            &conn,
            "SELECT COUNT(*) FROM nostr_event_outbox WHERE state = 'pending'",
        )?;
        Ok(HealthSnapshot {
            schema_version,
            started_at_unix,
            offers,
            open_claims,
            jobs,
            pending_outbox,
        })
    }
}

/// Enqueue an event into the outbox within a live transaction. Idempotent on `dedup_key`: a second
/// enqueue with the same key is a no-op (`INSERT OR IGNORE`), which is what makes the transitions
/// that call this safe to replay.
fn enqueue_event(
    tx: &rusqlite::Transaction<'_>,
    dedup_key: &str,
    draft: &EventDraft,
    created_at_unix: i64,
    expires_at_unix: i64,
    now_unix: i64,
) -> Result<(), StoreError> {
    let draft_json = serde_json::to_string(draft)
        .map_err(|error| StoreError(format!("outbox draft encode: {error}")))?;
    tx.execute(
        "INSERT OR IGNORE INTO nostr_event_outbox
             (dedup_key, draft_json, created_at_unix, state, attempts, expires_at_unix, updated_at_unix)
         VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5)",
        params![dedup_key, draft_json, created_at_unix, expires_at_unix, now_unix],
    )?;
    Ok(())
}

/// Read a claim's `(state, offer_id)` from any connection-like handle (a transaction derefs to
/// one). `None` when no claim row exists.
fn claim_state(
    conn: &Connection,
    job_id: &str,
) -> Result<Option<(String, String)>, StoreError> {
    let row = conn
        .query_row(
            "SELECT state, offer_id FROM claims WHERE job_id = ?1",
            [job_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(row)
}

fn count(conn: &Connection, sql: &str) -> Result<i64, StoreError> {
    Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))?)
}

fn read_meta_i64(conn: &Connection, key: &str) -> Result<Option<i64>, StoreError> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM seller_meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    match value {
        Some(text) => text
            .parse::<i64>()
            .map(Some)
            .map_err(|error| StoreError(format!("seller_meta.{key} not an integer: {error}"))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fee triple a test journals beside a receipt: (mint fee, platform bps, platform sats).
    fn fees(mint_fee_sats: u64, fee_bps: u32, fee_sats: u64) -> ReceiptFees {
        ReceiptFees {
            mint_fee_sats,
            fee_bps,
            fee_sats,
        }
    }
    use crate::gateway::TagSpec;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-seller-store-{label}-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    fn fresh_store(label: &str) -> (SellerStore, std::path::PathBuf) {
        let path = temp_db(label);
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        (store, path)
    }

    /// Put a job row in an exact state. Direct SQL on purpose: driving five states through the
    /// public transition path would make the state-coverage test below a test of the transitions
    /// instead of a test of the predicate.
    fn insert_job(store: &SellerStore, job_id: &str, state: JobState) {
        let conn = store.lock().expect("lock");
        conn.execute(
            "INSERT INTO jobs (job_id, offer_id, agent_name, state, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, NULL, ?3, 1, 1)",
            params![job_id, format!("offer-{job_id}"), state.as_str()],
        )
        .expect("insert job");
    }

    /// The two narrowing mutators must REPORT that they moved nothing, not just decline to move it.
    ///
    /// Both guard on state (`release_claim` on `= 'claimed'`, `fail_job` on `!= 'paid'`), so zero
    /// rows is a normal outcome rather than an error — which is exactly why the count has to reach
    /// the caller. A discarded rowcount here is what let a losing seat announce a release it never
    /// performed while its claim sat at `awarded` (#626), and `fail_job` is the write the lapse heal
    /// depends on, so the same silence there would report a repair that did not happen.
    #[test]
    fn the_narrowing_mutators_report_when_they_move_nothing() {
        let (store, path) = fresh_store("rowcount");

        // fail_job: a real transition reports 1; `paid` is terminal and reports 0.
        insert_job(&store, "job-live", JobState::Awarded);
        assert_eq!(store.fail_job("job-live", 5).expect("fail live"), 1, "an awarded row fails");
        insert_job(&store, "job-paid", JobState::Paid);
        assert_eq!(
            store.fail_job("job-paid", 5).expect("fail paid"),
            0,
            "`paid` is terminal — the guard refuses, and the caller must be able to see that"
        );
        assert_eq!(
            store.fail_job("job-absent", 5).expect("fail absent"),
            0,
            "no row at all is also zero"
        );

        // release_claim: only a still-`claimed` row releases; an awarded one reports 0.
        let draft = crate::gateway::claim_draft("job-c", &"b".repeat(64), &"s".repeat(64), crate::gateway::ClaimPayment::Sat("creq"), &[], &Default::default());
        store
            .claim_and_enqueue("job-c", "job-c", Some("creq"), &draft, 1, 9_999_999_999, 1)
            .expect("claim");
        assert_eq!(store.release_claim("job-c", 6).expect("release"), 1, "a parked claim releases");
        assert_eq!(
            store.release_claim("job-c", 7).expect("re-release"),
            0,
            "already released — the second call moves nothing and says so"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The stored spellings, written out by hand.
    ///
    /// Deliberately NOT derived from `as_str` — the point is to disagree with it if it drifts. These
    /// same five literals also live in the `jobs.state` CHECK constraint and in `JobState::parse`, so
    /// a silent rename in one place would otherwise surface as a runtime CHECK violation or an
    /// "unknown job state" error rather than a failing test.
    #[test]
    fn job_state_spellings_are_the_literals_the_schema_stores() {
        assert_eq!(JobState::Awarded.as_str(), "awarded");
        assert_eq!(JobState::Executing.as_str(), "executing");
        assert_eq!(JobState::Delivered.as_str(), "delivered");
        assert_eq!(JobState::Paid.as_str(), "paid");
        assert_eq!(JobState::Failed.as_str(), "failed");
        for state in JobState::ALL {
            assert_eq!(
                JobState::parse(state.as_str()),
                Some(state),
                "{state:?} must round-trip through its stored spelling"
            );
        }
    }

    #[test]
    fn only_awarded_and_executing_occupy_an_execution_slot() {
        assert!(JobState::Awarded.occupies_execution_slot());
        assert!(JobState::Executing.occupies_execution_slot());
        // Delivered has finished executing and is awaiting payment — it holds no slot.
        assert!(!JobState::Delivered.occupies_execution_slot());
        assert!(!JobState::Paid.occupies_execution_slot());
        assert!(!JobState::Failed.occupies_execution_slot());
    }

    /// Enumerated over EVERY variant rather than the two that motivated the change, so adding a state
    /// without deciding whether it occupies a slot fails here instead of on the wire.
    #[test]
    fn jobs_in_flight_counts_exactly_the_occupying_states() {
        for state in JobState::ALL {
            let (store, path) = fresh_store(&format!("inflight-{}", state.as_str()));
            insert_job(&store, "job-1", state);
            let expected = u32::from(state.occupies_execution_slot());
            assert_eq!(
                store.jobs_in_flight().expect("count"),
                expected,
                "a single {state:?} job must count as {expected} in flight"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    /// ★ THE #313 REGRESSION, and it must start from a NON-EMPTY store.
    ///
    /// The old predicate was `health().jobs > 0` — `COUNT(*)` over every row ever written — so a seat
    /// that had finished work advertised `accepting=n` forever. A fixture starting from an empty
    /// store cannot tell the fix from the bug: both report zero. The discriminator is terminal rows
    /// PRESENT, and this test asserts the two counts DISAGREE, which is the whole defect.
    #[test]
    fn a_store_holding_only_terminal_jobs_reports_none_in_flight() {
        let (store, path) = fresh_store("terminal-only");
        insert_job(&store, "job-paid-1", JobState::Paid);
        insert_job(&store, "job-paid-2", JobState::Paid);
        insert_job(&store, "job-failed", JobState::Failed);
        insert_job(&store, "job-delivered", JobState::Delivered);

        assert_eq!(
            store.jobs_in_flight().expect("count"),
            0,
            "a finished job holds no slot and must not raise the published queue_depth"
        );
        assert_eq!(
            store.health().expect("health").jobs,
            4,
            "health().jobs stays the lifetime total — that is its job, which is why it must not be \
             read as in-flight"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn jobs_in_flight_is_a_count_not_a_flag() {
        let (store, path) = fresh_store("inflight-depth");
        insert_job(&store, "job-a", JobState::Awarded);
        insert_job(&store, "job-b", JobState::Executing);
        insert_job(&store, "job-c", JobState::Awarded);
        insert_job(&store, "job-done", JobState::Paid);
        assert_eq!(
            store.jobs_in_flight().expect("count"),
            3,
            "three occupying jobs must report 3 — a 0/1 answer is the #313 shape"
        );
        let _ = std::fs::remove_file(&path);
    }

    fn sample_offer(id: &str) -> Offer {
        Offer {
            payment_mode: crate::gateway::PaymentMode::Sat,
            offer_id: id.to_owned(),
            buyer_pubkey: "b".repeat(64),
            amount_sats: 100,
            unit: "sat".to_owned(),
            task: "do the thing".to_owned(),
            deadline_unix: 10_000,
            targeted: true,
            requested_agent: None,
            output: Some("text/plain".to_owned()),
        }
    }

    /// A wire-valid draft carrying the protocol tags every maxplayer event needs.
    fn wire_draft(kind: u16) -> EventDraft {
        use crate::gateway::{MAXPLAYER_TAG, PROTOCOL_VERSION};
        EventDraft::new(
            kind,
            vec![
                TagSpec::new(["t", MAXPLAYER_TAG]),
                TagSpec::new(["v", PROTOCOL_VERSION]),
            ],
            "content",
        )
    }

    fn claim() -> EventDraft {
        wire_draft(crate::gateway::JOB_CLAIM_KIND)
    }

    fn result() -> EventDraft {
        wire_draft(crate::gateway::JOB_RESULT_KIND)
    }

    #[test]
    fn open_is_wal_and_carries_schema_and_start() {
        let (store, path) = fresh_store("wal");
        store.record_start(1234).expect("record start");
        let health = store.health().expect("health");
        assert_eq!(health.schema_version, SCHEMA_VERSION);
        assert_eq!(health.started_at_unix, 1234);
        assert_eq!(health.jobs, 0);

        let conn = Connection::open(&path).expect("reopen");
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH — the harness an offer requested is journaled with its other facts and READS BACK
    // across a reopen. Execution can be a restart away from the claim, so a request that lived only
    // in memory would let a resumed job run on whatever harness the node prefers now.
    #[test]
    fn requested_agent_survives_a_reopen() {
        let path = temp_db("requested-agent");
        let _ = std::fs::remove_file(&path);
        {
            let store = SellerStore::open(&path).expect("open");
            let mut offer = sample_offer("o1");
            offer.requested_agent = Some("codex".to_owned());
            store.record_offer(&offer, 1).expect("record");
            // An offer with no preference stays None — absence is a value here, not a default.
            store.record_offer(&sample_offer("o2"), 1).expect("record");
        }
        let store = SellerStore::open(&path).expect("reopen");
        assert_eq!(
            store.offer_row("o1").expect("row").expect("o1").requested_agent.as_deref(),
            Some("codex")
        );
        assert_eq!(
            store.offer_row("o2").expect("row").expect("o2").requested_agent,
            None
        );
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH (#686) — the output type the buyer DECLARED is journaled with the other offer facts and
    // READS BACK across a reopen. Same reason as the harness request above: execution can be a
    // restart away from the claim, and the resumed job composes its agent prompt from this row, so a
    // type that lived only in memory would be gone for that job permanently.
    //
    // Bite (measured): drop `output` from the INSERT column list in `record_offer` (or from the
    // `offer_row` SELECT) and this test goes red — the reopened row reads None.
    #[test]
    fn the_declared_output_type_survives_a_reopen() {
        let path = temp_db("declared-output");
        let _ = std::fs::remove_file(&path);
        {
            let store = SellerStore::open(&path).expect("open");
            let mut offer = sample_offer("o1");
            offer.output = Some("application/json".to_owned());
            store.record_offer(&offer, 1).expect("record");
            // A second offer with a DIFFERENT type: the read must return each row's own value, which
            // a single hardcoded default would not.
            store.record_offer(&sample_offer("o2"), 1).expect("record");
        }
        let store = SellerStore::open(&path).expect("reopen");
        assert_eq!(
            store.offer_row("o1").expect("row").expect("o1").output.as_deref(),
            Some("application/json"),
            "the declared output type must survive a restart — the prompt is composed from this row"
        );
        assert_eq!(
            store.offer_row("o2").expect("row").expect("o2").output.as_deref(),
            Some("text/plain")
        );
        // The capacity-skip re-drive reads offers through a DIFFERENT statement; it carries the
        // field too, so a re-considered offer is not silently stripped of it.
        let awaiting = store.offers_awaiting_claim(1).expect("awaiting");
        let o1 = awaiting.iter().find(|offer| offer.offer_id == "o1").expect("o1 awaits a claim");
        assert_eq!(o1.output.as_deref(), Some("application/json"));
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH — a store written by a binary from before this column opens, MIGRATES, and reads its
    // existing rows as "no preference". `CREATE TABLE IF NOT EXISTS` silently skips an existing
    // table, so without the ALTER an upgraded node would fail every offer read on a live store.
    #[test]
    fn a_store_from_before_the_column_migrates_and_reads_no_preference() {
        let path = temp_db("pre-agent-schema");
        let _ = std::fs::remove_file(&path);
        // The offers table exactly as the previous schema had it, holding a live row.
        {
            let conn = Connection::open(&path).expect("create old store");
            conn.execute_batch(
                "CREATE TABLE offers (
                     offer_id        TEXT PRIMARY KEY,
                     buyer_pubkey    TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     unit            TEXT NOT NULL,
                     task            TEXT NOT NULL,
                     deadline_unix   INTEGER NOT NULL,
                     targeted        INTEGER NOT NULL,
                     created_at_unix INTEGER NOT NULL
                 );
                 INSERT INTO offers VALUES ('old', 'buyer', 21, 'sat', 'task', 10000, 1, 1);",
            )
            .expect("old schema");
        }

        let store = SellerStore::open(&path).expect("open migrates");
        let row = store.offer_row("old").expect("read").expect("the pre-existing row survives");
        assert_eq!(row.amount_sats, 21, "the row is migrated, not replaced");
        assert_eq!(row.requested_agent, None, "an offer from before the column asked for no harness");
        // #686: the same store predates the `output` column. It migrates and its live row reads as
        // "no declared type" — that job's prompt simply states none, rather than the read failing.
        assert_eq!(row.output, None, "an offer from before the column declared no output type");
        // Forward from here the column is armed: a fresh ingest into the SAME migrated store keeps
        // its type, so the migration adds a working column and not just a silent one.
        store.record_offer(&sample_offer("new"), 2).expect("record into the migrated store");
        assert_eq!(
            store.offer_row("new").expect("read").expect("new row").output.as_deref(),
            Some("text/plain")
        );
        // Migration is idempotent: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
        assert!(store.offer_row("old").expect("read").is_some());
        let _ = std::fs::remove_file(&path);
    }

    // #563 — the relay-derived settled-elsewhere marker round-trips over a real row and DEFAULTS false.
    // A false default is load-bearing: absence of the marker must read as "not derived-settled" so the
    // resume refine still queries the relay (never a silent skip on a missing fact).
    #[test]
    fn settled_elsewhere_marker_round_trips_and_defaults_false() {
        let (store, path) = fresh_store("settled-elsewhere");
        insert_job(&store, "job-se", JobState::Awarded);
        assert!(
            !store.has_settled_elsewhere("job-se").expect("read unmarked"),
            "an un-derived job is not settled-elsewhere (default false ⇒ the refine still checks)"
        );
        store.mark_settled_elsewhere("job-se", 1_234).expect("mark");
        assert!(
            store.has_settled_elsewhere("job-se").expect("read marked"),
            "after the relay-derived marker the job reads settled-elsewhere"
        );
        // Idempotent: a later derive re-writes the ts, stays true, never errors.
        store.mark_settled_elsewhere("job-se", 5_678).expect("re-mark");
        assert!(store.has_settled_elsewhere("job-se").expect("read"), "idempotent — last write wins");
        let _ = std::fs::remove_file(&path);
    }

    // #591 — the contribution pin round-trips and is ABSENT for a from-scratch job (the empty-workdir
    // default execute_job falls back to). INSERT OR IGNORE makes a re-ingest of the same offer a
    // no-op, never a second write — the property the crash-safe pin-before-offer ordering relies on.
    #[test]
    fn contribution_pin_round_trips_and_absent_for_from_scratch() {
        let (store, path) = fresh_store("contribution-pin");
        assert_eq!(
            store.contribution_pin("scratch").expect("read"),
            None,
            "a from-scratch job has no pin"
        );
        let pin = ContributionPin {
            owner_pubkey: "b".repeat(64),
            clone_url: "https://relay.maxplayer.ai/git/owner/repo.git".to_owned(),
            base_branch: "main".to_owned(),
            base_oid: "a".repeat(40),
        };
        assert!(
            store.record_contribution_pin("job-c", &pin, 7).expect("record"),
            "the first write inserts"
        );
        assert_eq!(
            store.contribution_pin("job-c").expect("read"),
            Some(pin.clone()),
            "the pin reads back"
        );
        assert!(
            !store.record_contribution_pin("job-c", &pin, 8).expect("re-record"),
            "INSERT OR IGNORE ⇒ a re-ingest is idempotent"
        );
        assert_eq!(
            store.contribution_pin("job-c").expect("read"),
            Some(pin),
            "unchanged after the re-ingest"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn job_checks_round_trip_and_absent_row_is_none() {
        let (store, path) = fresh_store("job-checks");
        assert_eq!(store.job_checks("absent").expect("read absent"), None);
        let bytes = b"schema = 1\n# retain exact base bytes\n";
        store
            .record_job_checks(
                "checked-job",
                bytes,
                EnvKind::ContainerImage,
                "registry.example/checks@sha256:abcd",
                1234,
            )
            .expect("record checks");
        assert_eq!(
            store.job_checks("checked-job").expect("read checks"),
            Some(JobChecks {
                job_id: "checked-job".to_owned(),
                declaration_bytes: bytes.to_vec(),
                env_kind: EnvKind::ContainerImage,
                env_lock_ref: "registry.example/checks@sha256:abcd".to_owned(),
                captured_at_unix: 1234,
            })
        );
        let _ = std::fs::remove_file(&path);
    }

    // #591 — a v4 store (no contribution_pins) opens CLEAN under v5: the additive CREATE TABLE IF NOT
    // EXISTS adds the new table, the version bumps, and the pre-existing money-path row is UNTOUCHED
    // (no ALTER/DROP crosses claims/wallet tables). The store then persists a pin.
    #[test]
    fn a_v4_store_opens_clean_under_v5_and_gains_contribution_pins() {
        let path = temp_db("pre-contribution-pins");
        let _ = std::fs::remove_file(&path);
        // A v4 store: version 4 + a live money-path claims row, WITHOUT contribution_pins.
        {
            let conn = Connection::open(&path).expect("create v4 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '4');
                 CREATE TABLE claims (
                     job_id TEXT PRIMARY KEY, offer_id TEXT NOT NULL, state TEXT NOT NULL,
                     creq TEXT NOT NULL, created_at_unix INTEGER NOT NULL, updated_at_unix INTEGER NOT NULL
                 );
                 INSERT INTO claims VALUES ('live-job', 'live-job', 'awarded', 'live-creq', 1, 1);",
            )
            .expect("v4 schema");
        }
        let store = SellerStore::open(&path).expect("a v4 store opens clean under v5");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION,
            "the version bumped to v5"
        );
        let pin = ContributionPin {
            owner_pubkey: "b".repeat(64),
            clone_url: "https://x/git/o/r.git".to_owned(),
            base_branch: "main".to_owned(),
            base_oid: "a".repeat(40),
        };
        assert!(store.record_contribution_pin("live-job", &pin, 2).expect("record"));
        assert_eq!(store.contribution_pin("live-job").expect("read"), Some(pin));
        drop(store);
        // The pre-existing money-path row was NOT touched by the v5 migration.
        let conn = Connection::open(&path).expect("reopen raw");
        let creq: String = conn
            .query_row("SELECT creq FROM claims WHERE job_id = 'live-job'", [], |row| row.get(0))
            .expect("the v4 claims row survives the v5 migration");
        assert_eq!(creq, "live-creq");
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH — a store written by a pre-#563 binary (a #552-era jobs table WITH pushed_commit but
    // WITHOUT settled_elsewhere_at_unix) opens, MIGRATES additively, and reads its existing rows as
    // "not settled-elsewhere". Without the ALTER an upgraded node would fail every has_settled_elsewhere
    // read on a live store.
    #[test]
    fn a_store_from_before_the_settled_elsewhere_column_migrates_and_reads_false() {
        let path = temp_db("pre-settled-elsewhere-schema");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create old store");
            conn.execute_batch(
                "CREATE TABLE jobs (
                     job_id          TEXT PRIMARY KEY,
                     offer_id        TEXT NOT NULL,
                     agent_name      TEXT,
                     state           TEXT NOT NULL
                         CHECK (state IN ('awarded','executing','delivered','paid','failed')),
                     created_at_unix INTEGER NOT NULL,
                     updated_at_unix INTEGER NOT NULL,
                     pushed_commit   TEXT
                 );
                 INSERT INTO jobs (job_id, offer_id, state, created_at_unix, updated_at_unix)
                 VALUES ('old-job', 'old-offer', 'awarded', 1, 1);",
            )
            .expect("old schema");
        }

        let store = SellerStore::open(&path).expect("open migrates");
        assert!(
            !store.has_settled_elsewhere("old-job").expect("read"),
            "a row from before the column is not derived-settled (the refine must still check the relay)"
        );
        // The migrated column is writable: the refine can arm it going forward on the live store.
        store.mark_settled_elsewhere("old-job", 2).expect("mark on migrated store");
        assert!(store.has_settled_elsewhere("old-job").expect("read"), "marker persists post-migration");
        // Idempotent: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_offer_is_idempotent() {
        let (store, path) = fresh_store("offer");
        let offer = sample_offer(&"a".repeat(64));
        assert!(store.record_offer(&offer, 1).expect("first"));
        assert!(!store.record_offer(&offer, 2).expect("second"), "re-seen offer is a no-op");
        assert_eq!(store.health().expect("h").offers, 1);
        // offer_facts serves the award-auth (buyer) and pay (amount/unit) reads.
        assert_eq!(
            store.offer_facts(&offer.offer_id).expect("facts"),
            Some((offer.buyer_pubkey.clone(), offer.amount_sats, offer.unit.clone()))
        );
        assert_eq!(store.offer_facts(&"z".repeat(64)).expect("absent"), None);
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH 2 (charter) — RED ON REVERT for the outbox. `claim_and_enqueue` must write the claim
    // row AND the outbox row atomically. This asserts the outbox MUTATION LANDED (a pending row
    // carrying the full wire-valid draft — right kind AND the `["v","1"]` + `["t","maxplayer"]` protocol
    // tags a live buyer requires), not merely that no error was returned. Deleting the
    // `enqueue_event` call in `claim_and_enqueue` leaves the claim row but no outbox row, so the
    // length / kind / tag assertions fail — the revert turns this test red.
    #[test]
    fn tooth_outbox_write_lands_atomically_with_the_claim() {
        use crate::gateway::{JOB_CLAIM_KIND, MAXPLAYER_TAG, PROTOCOL_VERSION};
        let (store, path) = fresh_store("outbox-redonrevert");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        assert_eq!(
            store
                .claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 500, 999, 1)
                .expect("claim"),
            Claimed::New
        );

        // The outbox row LANDED — pending, the claim kind, and the protocol tags, not yet published.
        let pending = store.pending_outbox(2).expect("pending");
        assert_eq!(pending.len(), 1, "exactly one pending outbox row must exist");
        let item = &pending[0];
        assert_eq!(item.dedup_key, format!("claim:{job}"));
        assert_eq!(item.draft.kind, JOB_CLAIM_KIND);
        assert_eq!(item.created_at_unix, 500);
        assert_eq!(item.attempts, 0);
        // The enqueued draft is wire-valid: it carries the version + namespace tags parse_offer/
        // the buyer require, so a signed event from it is not rejected on the wire.
        assert!(has_tag(&item.draft, "v", PROTOCOL_VERSION), "draft must carry [\"v\",\"1\"]");
        assert!(has_tag(&item.draft, "t", MAXPLAYER_TAG), "draft must carry [\"t\",\"maxplayer\"]");

        let row = store.outbox_row(&format!("claim:{job}")).expect("row").expect("exists");
        assert_eq!(row.0, "pending");
        assert!(row.2.is_none(), "not yet published");
        let _ = std::fs::remove_file(&path);
    }

    fn has_tag(draft: &EventDraft, name: &str, value: &str) -> bool {
        draft
            .tags
            .iter()
            .any(|tag| tag.first() == Some(name) && tag.value() == Some(value))
    }

    #[test]
    fn claim_and_enqueue_is_idempotent_no_double_enqueue() {
        let (store, path) = fresh_store("claim-idem");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        assert_eq!(
            store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("first"),
            Claimed::New
        );
        // A replay carrying a DIFFERENT creq is a no-op: neither the outbox nor the journaled
        // claim-time creq is overwritten. The first creq — the one that was on the wire — stands.
        assert_eq!(
            store.claim_and_enqueue(&job, &offer, Some("creqB"), &claim(), 1, 999, 2).expect("replay"),
            Claimed::Idempotent
        );
        assert_eq!(store.pending_outbox(3).expect("pending").len(), 1, "no second enqueue");
        assert_eq!(
            store.job_creq(&job).expect("creq").as_deref(),
            Some("creqA"),
            "the claim-time creq is immutable across replays"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `job_award_time` must be DETERMINISTIC when one job holds more than one award row. The
    /// `awards` PRIMARY KEY is the award id, not the job id, so two rows are possible — a redundant
    /// award was "seen live in the smoke" (see `run.rs` `execute_job`). The value is the delivery
    /// commit's authored-at, so a reading that can vary across restarts breaks invariant 2
    /// (byte-identical re-created delivery), which is the property the buyer verifies.
    ///
    /// ⛔ THE ROW ORDER HERE IS THE TEST. Inserting the LATER-timestamped award FIRST is what makes
    /// the pre-fix query deterministically WRONG: an unordered `SELECT … LIMIT 1` walks the table in
    /// rowid order and hands back that first-inserted later row, while the ordered query returns the
    /// earlier one. Insert them the other way round and the pre-fix code returns the right answer BY
    /// LUCK — the test would pass against the very bug it exists to catch.
    ///
    /// RED ON REVERT: drop `ORDER BY created_at_unix, award_id LIMIT 1` from `job_award_time` and
    /// this fails with `assertion left == right failed … left: Some(900), right: Some(100)`.
    #[test]
    fn job_award_time_is_deterministic_when_a_job_holds_two_awards() {
        let (store, path) = fresh_store("award-time-order");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let buyer = "b".repeat(64);
        store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("claim");

        // The LATER award is inserted FIRST — see the note above.
        store.record_award(&"z".repeat(64), &job, &buyer, 900).expect("later award");
        store.record_award(&"a".repeat(64), &job, &buyer, 100).expect("earlier award");

        assert_eq!(
            store.job_award_time(&job).expect("award time"),
            Some(100),
            "with two award rows the EARLIEST must win, whatever order they were written in"
        );
        // The sibling read has always been ordered; assert they agree rather than trusting it.
        assert_eq!(
            store.job_award_buyer(&job).expect("award buyer"),
            Some(buyer),
            "job_award_buyer resolves the same row"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// #814 — the boot re-hydration source. An offer is returned ONLY when an award exists for it, we
    /// hold NO claim, and its deadline has not passed. Each of the three clauses is asserted by a
    /// counter-example, because a query that simply returned every offer would satisfy the happy path
    /// alone.
    ///
    /// The claim clause is the #563 FOIL in SQL: re-hydrating a suppression for a job we hold would
    /// strand our own award. RED ON REVERT: delete `AND offer_id NOT IN (SELECT job_id FROM claims)`
    /// and the "ours" case appears in the result set.
    #[test]
    fn offers_awarded_elsewhere_selects_only_live_unclaimed_awarded_offers() {
        let (store, path) = fresh_store("awarded-elsewhere");
        let buyer = "b".repeat(64);
        let now = 5_000;

        let offer_at = |id: &str, deadline: i64| {
            let mut offer = sample_offer(id);
            offer.buyer_pubkey = buyer.clone();
            offer.deadline_unix = deadline;
            store.record_offer(&offer, 1).expect("record offer");
        };
        // (1) THE CASE: recorded, awarded, unclaimed, still live.
        offer_at(&"1".repeat(64), 10_000);
        store.record_award(&"w1".repeat(32), &"1".repeat(64), &buyer, 2).expect("award 1");
        // (2) awarded and live, but WE HOLD THE CLAIM — ours, never suppressed.
        offer_at(&"2".repeat(64), 10_000);
        store
            .claim_and_enqueue(&"2".repeat(64), &"2".repeat(64), Some("creq"), &claim(), 1, 999, 1)
            .expect("claim 2");
        store.record_award(&"w2".repeat(32), &"2".repeat(64), &buyer, 2).expect("award 2");
        // (3) recorded and unclaimed, but NO award — nothing decided it.
        offer_at(&"3".repeat(64), 10_000);
        // (4) awarded and unclaimed, but its deadline has PASSED — fail-open, already `Lapsed`.
        offer_at(&"4".repeat(64), 4_000);
        store.record_award(&"w4".repeat(32), &"4".repeat(64), &buyer, 2).expect("award 4");

        let rows = store.offers_awarded_elsewhere(now).expect("read");
        assert_eq!(
            rows,
            vec![("1".repeat(64), buyer.clone(), 10_000)],
            "only the recorded + awarded + unclaimed + live offer re-hydrates"
        );

        // Two award rows for one job must still yield ONE row (EXISTS, not a JOIN).
        store.record_award(&"w5".repeat(32), &"1".repeat(64), &buyer, 3).expect("second award");
        assert_eq!(
            store.offers_awarded_elsewhere(now).expect("read again").len(),
            1,
            "a second award row must not duplicate the offer"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn award_dedup_creates_one_job_and_ignores_replays() {
        let (store, path) = fresh_store("award");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let award = "w".repeat(64);
        let buyer = "b".repeat(64);
        store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("claim");

        assert_eq!(
            store.record_award(&award, &job, &buyer, 2).expect("award"),
            Awarded::New
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Awarded));
        // The award time is the durable, restart-stable delivery author-date (invariant 2 source).
        assert_eq!(store.job_award_time(&job).expect("award time"), Some(2));
        assert_eq!(store.job_award_time(&"z".repeat(64)).expect("absent"), None);

        // A re-seen award id is a dedup no-op — no second job, state unchanged.
        assert_eq!(
            store.record_award(&award, &job, &buyer, 3).expect("replay"),
            Awarded::Duplicate
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Awarded));

        // An award for an unknown claim is recorded but creates no job.
        let orphan_job = "k".repeat(64);
        let orphan_award = "x".repeat(64);
        assert_eq!(
            store.record_award(&orphan_award, &orphan_job, &buyer, 4).expect("orphan"),
            Awarded::NoClaim
        );
        assert_eq!(store.job_state(&orphan_job).expect("state"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deliver_is_idempotent_and_enqueues_result_once() {
        let (store, path) = fresh_store("deliver");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let buyer = "b".repeat(64);
        store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("claim");
        store.record_award(&"w".repeat(64), &job, &buyer, 2).expect("award");
        store.mark_executing(&job, 3).expect("exec");

        assert!(store
            .deliver_and_enqueue(&job, "ref-1", crate::gateway::PaymentMode::Sat, &result(), 4, 999, 5)
            .expect("deliver"));
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Delivered));
        // Replay: no second delivery, no second result enqueue.
        assert!(!store
            .deliver_and_enqueue(&job, "ref-1", crate::gateway::PaymentMode::Sat, &result(), 4, 999, 6)
            .expect("replay"));
        assert_eq!(
            store.outbox_row(&format!("result:{job}")).expect("row").expect("exists").0,
            "pending"
        );
        let _ = std::fs::remove_file(&path);
    }

    // Money-safe dedup: a replayed receipt never marks a job paid twice.
    #[test]
    fn collect_receipt_dedups_and_pays_once() {
        let (store, path) = fresh_store("collect");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let receipt = "r".repeat(64);
        store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("claim");
        store.record_award(&"w".repeat(64), &job, &"b".repeat(64), 2).expect("award");

        assert_eq!(
            store
                .collect_receipt(&receipt, &job, 100, fees(0, 0, 0), 3)
                .expect("collect"),
            Collected::New
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Paid));
        assert_eq!(
            store
                .collect_receipt(&receipt, &job, 100, fees(0, 0, 0), 4)
                .expect("replay"),
            Collected::Duplicate,
            "a replayed receipt must not credit twice"
        );
        let _ = std::fs::remove_file(&path);
    }

    // Platform fee (stage 1) — the fee is journaled in the SAME row as the receipt and the read-out
    // sums exactly what was written: per job and all-time. A replay adds nothing to either.
    #[test]
    fn collect_receipt_journals_the_fee_in_the_same_row_and_accrued_fees_sums_it() {
        let (store, path) = fresh_store("fee-journal");
        assert_eq!(
            store.accrued_fees().expect("empty read-out"),
            AccruedFees::default(),
            "nothing collected ⇒ nothing accrued"
        );

        // Job A: 100-sat offer (mint fee 1) at 2% (200 bp) ⇒ 2 sats. Job B: 1_000-sat offer (mint
        // fee 3) at 2.5% (250 bp) ⇒ 25. The platform fee is on the FACE; the mint fee rides beside it.
        let job_a = "a".repeat(64);
        let job_b = "b".repeat(64);
        store
            .claim_and_enqueue(&job_a, &"o".repeat(64), Some("creqA"), &claim(), 1, 999, 1)
            .expect("claim a");
        store
            .record_award(&"w".repeat(64), &job_a, &"b".repeat(64), 2)
            .expect("award a");
        assert_eq!(
            store
                .collect_receipt(&"r".repeat(64), &job_a, 100, fees(1, 200, 2), 3)
                .expect("collect a"),
            Collected::New
        );
        assert_eq!(
            store.job_state(&job_a).expect("state"),
            Some(JobState::Paid),
            "the receipt still marks paid"
        );
        assert_eq!(
            store
                .collect_receipt(&"s".repeat(64), &job_b, 1_000, fees(3, 250, 25), 4)
                .expect("collect b"),
            Collected::New
        );

        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(
            accrued.total_fee_sats, 27,
            "all-time total is the sum of the rows"
        );
        assert_eq!(
            accrued.by_job,
            vec![
                JobFeeAccrual {
                    job_id: job_a.clone(),
                    amount_sats: 100,
                    mint_fee_sats: Some(1),
                    fee_bps: 200,
                    fee_sats: 2,
                    received_at_unix: 3,
                    remittance_id: None,
                },
                JobFeeAccrual {
                    job_id: job_b.clone(),
                    amount_sats: 1_000,
                    mint_fee_sats: Some(3),
                    fee_bps: 250,
                    fee_sats: 25,
                    received_at_unix: 4,
                    remittance_id: None,
                },
            ],
            "one row per receipt, oldest first, each carrying the rate in force and what it came to"
        );

        // A replayed wrap — even one claiming a different fee — is a dedup no-op and accrues nothing.
        assert_eq!(
            store
                .collect_receipt(&"r".repeat(64), &job_a, 100, fees(99, 10_000, 100), 5)
                .expect("replay"),
            Collected::Duplicate
        );
        assert_eq!(
            store.accrued_fees().expect("read-out").total_fee_sats,
            27,
            "a replay accrues nothing"
        );

        // The row survives a close/reopen: read back off disk, not from memory.
        drop(store);
        let store = SellerStore::open(&path).expect("reopen");
        assert_eq!(store.accrued_fees().expect("read-out").total_fee_sats, 27);
        let _ = std::fs::remove_file(&path);
    }

    // Platform fee (stage 1) — a store written by a pre-v8 binary (a receipts table WITHOUT the fee
    // columns) opens, migrates additively, and reads 0 for every existing receipt: no fee was
    // configured when those payments were collected. The migrated store then journals a fee on its
    // next collection, and a second open is a no-op.
    #[test]
    fn a_store_from_before_the_fee_columns_migrates_and_reads_zero() {
        let path = temp_db("pre-fee-columns");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create old store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '7');
                 CREATE TABLE receipts (
                     receipt_id      TEXT PRIMARY KEY,
                     job_id          TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     received_at_unix INTEGER NOT NULL
                 );
                 INSERT INTO receipts VALUES ('old-receipt', 'old-job', 21, 7);",
            )
            .expect("v7 schema");
        }

        let store =
            SellerStore::open(&path).expect("a v7 store opens clean under the current schema");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        let accrued = store.accrued_fees().expect("read-out on a migrated store");
        assert_eq!(
            accrued.total_fee_sats, 0,
            "nothing accrued before the fee existed"
        );
        assert_eq!(
            accrued.by_job,
            vec![JobFeeAccrual {
                job_id: "old-job".to_owned(),
                amount_sats: 21,
                mint_fee_sats: None,
                fee_bps: 0,
                fee_sats: 0,
                received_at_unix: 7,
                remittance_id: None,
            }],
            "the pre-existing receipt is untouched, reads a fee of 0/0, and has NO mint fee (not a zero one)"
        );
        assert_eq!(
            accrued.by_job[0].kept_sats(),
            None,
            "what the seller kept of a row with no recorded mint fee is unknown, never 21"
        );
        assert_eq!(accrued.rows_without_mint_fee, 1);
        assert_eq!(accrued.total_mint_fee_sats, 0);
        assert_eq!(
            accrued.total_kept_sats(),
            None,
            "no row recorded a mint fee ⇒ no kept total is claimed"
        );
        assert!(
            store.has_receipt("old-job").expect("read"),
            "the legacy receipt still counts as paid"
        );

        // The migrated columns are writable: the next collection journals its fee.
        assert_eq!(
            store
                .collect_receipt("new-receipt", "new-job", 100, fees(1, 200, 2), 8)
                .expect("collect on migrated store"),
            Collected::New
        );
        assert_eq!(store.accrued_fees().expect("read-out").total_fee_sats, 2);

        // RE-ENTRANT: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        assert_eq!(store.accrued_fees().expect("read-out").by_job.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    // Round 2 — a store written by a v8 binary (fee columns present, NO mint_fee_sats column) opens,
    // migrates additively to v9, and reads its existing receipt with `mint_fee_sats = None`: the
    // mint fee was not recorded, and neither the row nor the totals may present that as a measured
    // 0. The next collection on the migrated store records its mint fee; a second open is a no-op.
    #[test]
    fn a_v8_store_migrates_to_v9_and_reads_no_mint_fee_rather_than_zero() {
        let path = temp_db("pre-mint-fee-column");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create v8 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '8');
                 CREATE TABLE receipts (
                     receipt_id      TEXT PRIMARY KEY,
                     job_id          TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     received_at_unix INTEGER NOT NULL,
                     fee_bps         INTEGER NOT NULL DEFAULT 0 CHECK (fee_bps >= 0 AND fee_bps <= 10000),
                     fee_sats        INTEGER NOT NULL DEFAULT 0 CHECK (fee_sats >= 0)
                 );
                 INSERT INTO receipts VALUES ('v8-receipt', 'v8-job', 100, 7, 1000, 10);",
            )
            .expect("v8 schema");
        }

        let store = SellerStore::open(&path).expect("a v8 store opens clean under v9");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        let accrued = store.accrued_fees().expect("read-out on a migrated store");
        assert_eq!(
            accrued.by_job,
            vec![JobFeeAccrual {
                job_id: "v8-job".to_owned(),
                amount_sats: 100,
                mint_fee_sats: None,
                fee_bps: 1000,
                fee_sats: 10,
                received_at_unix: 7,
                remittance_id: None,
            }],
            "the v8 row keeps its fee and carries NO mint fee"
        );
        assert_eq!(
            accrued.by_job[0].kept_sats(),
            None,
            "kept is unknown, not 90"
        );
        assert_eq!(accrued.rows_without_mint_fee, 1);
        assert_eq!(accrued.total_mint_fee_sats, 0);
        assert_eq!(accrued.total_fee_sats, 10);
        assert_eq!(accrued.total_kept_sats(), None);

        // The migrated column is writable: the next collection records its mint fee, and the totals
        // separate what was measured from what was not.
        assert_eq!(
            store
                .collect_receipt("v9-receipt", "v9-job", 100, fees(1, 1000, 10), 8)
                .expect("collect on migrated store"),
            Collected::New
        );
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(accrued.by_job[1].mint_fee_sats, Some(1));
        assert_eq!(accrued.by_job[1].kept_sats(), Some(89));
        assert_eq!(accrued.rows_without_mint_fee, 1);
        assert_eq!(accrued.total_mint_fee_sats, 1);
        assert_eq!(
            accrued.total_kept_sats(),
            Some(89),
            "kept total covers only the row that recorded a mint fee"
        );

        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        assert_eq!(store.accrued_fees().expect("read-out").by_job.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    // ---- Stage 2a: the fee remittance ledger ----

    fn plan(hash: &str, gross: u64, net: u64) -> RemittancePlan {
        RemittancePlan {
            payment_hash: hash.to_owned(),
            gross_sats: gross,
            net_sats: net,
            melt_fee_reserve_sats: gross.saturating_sub(net),
            destination: "maxplayer@agi.cash".to_owned(),
            bolt11: format!("lnbc-test-{hash}"),
            melt_quote_id: Some(format!("quote-{hash}")),
        }
    }

    /// The test process's owner token and a lease far in the future: these tests are about the
    /// ledger, not the lease; `ownership_*` below are about the lease.
    const OWNER: &str = "test-owner";
    const LEASE: i64 = 1_000_000;

    fn by_melt(net: u64, fee: u64, quote: Option<&str>) -> RemitSettlement {
        RemitSettlement {
            net_paid_sats: Some(net),
            melt_fee_sats: Some(fee),
            melt_fee_reserve_sats: Some(fee.saturating_add(1)),
            melt_quote_id: quote.map(str::to_owned),
            settled_by: SettledBy::Melt,
        }
    }

    fn by_reconciliation(quote: Option<&str>, reserve: Option<u64>) -> RemitSettlement {
        RemitSettlement {
            net_paid_sats: None,
            melt_fee_sats: None,
            melt_fee_reserve_sats: reserve,
            melt_quote_id: quote.map(str::to_owned),
            settled_by: SettledBy::Reconciliation,
        }
    }

    // A store written by a v9 binary (mint_fee_sats present, NO remittance_id column, no
    // fee_remittances table) opens under v10: the column and table are added additively, the
    // existing receipt reads as UNREMITTED (remittance_id None), the totals put its fee in
    // `unremitted_fee_sats`, and a second open is a no-op.
    #[test]
    fn a_v9_store_migrates_to_v10_and_reads_its_receipts_as_unremitted() {
        let path = temp_db("pre-remittance");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create v9 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '9');
                 CREATE TABLE receipts (
                     receipt_id      TEXT PRIMARY KEY,
                     job_id          TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     received_at_unix INTEGER NOT NULL,
                     fee_bps         INTEGER NOT NULL DEFAULT 0 CHECK (fee_bps >= 0 AND fee_bps <= 10000),
                     fee_sats        INTEGER NOT NULL DEFAULT 0 CHECK (fee_sats >= 0),
                     mint_fee_sats   INTEGER CHECK (mint_fee_sats IS NULL OR mint_fee_sats >= 0)
                 );
                 INSERT INTO receipts VALUES ('v9-receipt', 'v9-job', 100, 7, 1000, 10, 1);",
            )
            .expect("v9 schema");
            let tables: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
                .expect("prepare")
                .query_map([], |row| row.get(0))
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("names");
            assert!(
                !tables.iter().any(|name| name == "fee_remittances"),
                "fixture must predate the remittance table: {tables:?}"
            );
        }

        let store = SellerStore::open(&path).expect("a v9 store opens clean under v10");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        let accrued = store.accrued_fees().expect("read-out on a migrated store");
        assert_eq!(
            accrued.by_job,
            vec![JobFeeAccrual {
                job_id: "v9-job".to_owned(),
                amount_sats: 100,
                mint_fee_sats: Some(1),
                fee_bps: 1000,
                fee_sats: 10,
                received_at_unix: 7,
                remittance_id: None,
            }],
            "the v9 row keeps every figure and is UNREMITTED"
        );
        assert_eq!(accrued.total_fee_sats, 10);
        assert_eq!(accrued.unremitted_fee_sats, 10);
        assert_eq!(accrued.remitted_fee_sats, 0);
        assert_eq!(accrued.in_flight_fee_sats, 0);
        assert!(store.remittances().expect("table exists").is_empty());
        assert_eq!(store.in_flight_remittance().expect("query"), None);

        // The migrated store can plan against its old row: the column is writable.
        let row = store
            .plan_remittance(&plan("h1", 10, 9), OWNER, LEASE, 100)
            .expect("plan on migrated store");
        assert_eq!(row.receipts, 1);
        assert_eq!(
            store.accrued_fees().expect("read-out").by_job[0].remittance_id,
            Some("h1".to_owned())
        );

        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        assert_eq!(store.remittances().expect("rows").len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    // The load-bearing property (brief §3.4): plan pins exactly the unremitted receipts; settle keeps
    // them discharged so the unremitted balance is ZERO afterwards and a second plan finds nothing to
    // remit; a receipt collected AFTER the plan is not swept into it.
    #[test]
    fn plan_then_settle_discharges_the_receipts_and_a_second_plan_finds_nothing() {
        let (store, path) = fresh_store("remit-settle");
        store
            .collect_receipt("r1", "job-1", 100, fees(1, 1000, 10), 1)
            .expect("collect 1");
        store
            .collect_receipt("r2", "job-2", 50, fees(1, 1000, 5), 2)
            .expect("collect 2");
        // A zero-fee receipt (a 9-sat job at 10% owes 0) is discharged too — it owes nothing.
        store
            .collect_receipt("r0", "job-0", 9, fees(0, 1000, 0), 3)
            .expect("collect 0");
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(accrued.unremitted_fee_sats, 15);

        let planned = store
            .plan_remittance(&plan("h1", 15, 14), OWNER, LEASE, 10)
            .expect("plan");
        assert_eq!(planned.state, RemittanceState::Planned);
        assert_eq!(planned.remittance_id, "h1");
        assert_eq!(planned.payment_hash, "h1");
        assert_eq!((planned.gross_sats, planned.net_sats), (15, 14));
        assert_eq!(planned.melt_fee_sats, None);
        assert_eq!(planned.destination, "maxplayer@agi.cash");
        assert_eq!(planned.melt_quote_id, Some("quote-h1".to_owned()));
        assert_eq!(planned.bolt11, "lnbc-test-h1");
        assert_eq!(planned.receipts, 3);
        assert_eq!(planned.owner.as_deref(), Some(OWNER));
        assert_eq!(planned.lease_until_unix, Some(LEASE));
        assert_eq!(planned.melt_fee_reserve_sats, Some(1));
        assert_eq!(planned.settled_by, None);
        assert_eq!(
            (planned.created_at_unix, planned.settled_at_unix),
            (10, None)
        );
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(
            accrued.unremitted_fee_sats, 0,
            "pinned receipts are no longer unremitted"
        );
        assert_eq!(accrued.in_flight_fee_sats, 15, "…they are in flight");
        assert_eq!(accrued.remitted_fee_sats, 0);
        assert_eq!(
            accrued.total_fee_sats, 15,
            "the all-time total does not move"
        );
        assert!(
            accrued
                .by_job
                .iter()
                .all(|row| row.remittance_id.as_deref() == Some("h1"))
        );
        assert_eq!(
            store.in_flight_remittance().expect("query"),
            Some(planned.clone())
        );

        // A receipt collected while the payment is in flight is NOT swept into it.
        store
            .collect_receipt("r3", "job-3", 200, fees(2, 1000, 20), 11)
            .expect("collect 3");
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(accrued.unremitted_fee_sats, 20);
        assert_eq!(accrued.in_flight_fee_sats, 15);

        // Settle with what the mint reported: paid 14, fee 1, quote id from the payment.
        let settled = store
            .settle_remittance("h1", &by_melt(14, 1, Some("quote-pay-h1")), 12)
            .expect("settle");
        assert_eq!(settled.state, RemittanceState::Settled);
        assert_eq!(settled.melt_fee_sats, Some(1));
        assert_eq!(settled.net_sats, 14);
        assert_eq!(settled.melt_quote_id, Some("quote-pay-h1".to_owned()));
        assert_eq!(settled.settled_by, Some(SettledBy::Melt));
        assert_eq!(
            settled.melt_fee_reserve_sats,
            Some(2),
            "replaced by the reserve of the quote that paid"
        );
        assert_eq!(settled.settled_at_unix, Some(12));
        assert_eq!(settled.receipts, 3);
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(accrued.remitted_fee_sats, 15);
        assert_eq!(accrued.in_flight_fee_sats, 0);
        assert_eq!(
            accrued.unremitted_fee_sats, 20,
            "only the post-plan receipt is owed"
        );
        assert_eq!(store.in_flight_remittance().expect("query"), None);

        // A settled row never moves again.
        assert!(
            store
                .settle_remittance("h1", &by_reconciliation(None, None), 13)
                .is_err()
        );
        assert_eq!(
            store
                .release_remittance(
                    "h1",
                    &ReleaseOn::OwnPlanned {
                        owner: OWNER.to_owned()
                    },
                    13
                )
                .expect("query"),
            None,
            "a settled row is not released: zero rows, hold"
        );

        // The next plan covers exactly the new receipt; planning the OLD figure is a mismatch.
        assert_eq!(
            store.plan_remittance(&plan("h2", 15, 15), OWNER, LEASE, 14),
            Err(PlanRefused::GrossMismatch {
                planned: 15,
                unremitted: 20
            })
        );
        let second = store
            .plan_remittance(&plan("h2", 20, 19), OWNER, LEASE, 14)
            .expect("second plan");
        assert_eq!(second.receipts, 1);
        store
            .settle_remittance("h2", &by_melt(19, 1, None), 15)
            .expect("settle 2");
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(accrued.unremitted_fee_sats, 0);
        assert_eq!(accrued.remitted_fee_sats, 35);
        // Everything is discharged: a third plan has nothing to remit, whatever figure it claims.
        assert_eq!(
            store.plan_remittance(&plan("h3", 0, 0), OWNER, LEASE, 16),
            Err(PlanRefused::NothingToRemit)
        );
        assert_eq!(
            store.plan_remittance(&plan("h3", 35, 35), OWNER, LEASE, 16),
            Err(PlanRefused::NothingToRemit)
        );
        assert_eq!(store.remittances().expect("rows").len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    // The crossmint_hop precedent: while a planned row exists, a second plan is REFUSED — at the
    // API and, belt-and-braces, by the partial unique index — so no second payment can be journaled
    // against receipts that may already be paid for.
    #[test]
    fn a_second_plan_while_one_is_in_flight_is_refused_by_api_and_by_index() {
        let (store, path) = fresh_store("remit-duplicate");
        store
            .collect_receipt("r1", "job-1", 100, fees(1, 1000, 10), 1)
            .expect("collect");
        let first = store
            .plan_remittance(&plan("h1", 10, 9), OWNER, LEASE, 2)
            .expect("first plan");
        match store.plan_remittance(&plan("h2", 10, 9), OWNER, LEASE, 3) {
            Err(PlanRefused::InFlight(active)) => assert_eq!(*active, first),
            other => panic!("expected InFlight, got {other:?}"),
        }
        // Even a plan for a fresh receipt is refused while the first is unresolved.
        store
            .collect_receipt("r2", "job-2", 100, fees(1, 1000, 10), 4)
            .expect("collect 2");
        assert!(matches!(
            store.plan_remittance(&plan("h3", 10, 9), OWNER, LEASE, 5),
            Err(PlanRefused::InFlight(_))
        ));
        // The index refuses a raw second planned row too.
        {
            let conn = store.lock().expect("lock");
            let raw = conn.execute(
                "INSERT INTO fee_remittances
                     (remittance_id, gross_sats, net_sats, destination, payment_hash, bolt11, state, created_at_unix)
                 VALUES ('raw', 1, 1, 'x@y', 'raw', 'ln', 'planned', 6)",
                [],
            );
            assert!(
                raw.is_err(),
                "the partial unique index must refuse a second planned row"
            );
        }
        // Nothing was written by the refusals.
        assert_eq!(store.remittances().expect("rows").len(), 1);
        assert_eq!(
            store.accrued_fees().expect("read-out").unremitted_fee_sats,
            10
        );
        let _ = std::fs::remove_file(&path);
    }

    // Recovery: a planned row whose melt never happened is FAILED, which releases its receipts back
    // to unremitted so the next attempt pays them — and the failed row keeps its history. The same
    // invoice can never be journaled twice, in any state.
    #[test]
    fn fail_releases_the_receipts_and_the_invoice_stays_used() {
        let (store, path) = fresh_store("remit-fail");
        store
            .collect_receipt("r1", "job-1", 100, fees(1, 1000, 10), 1)
            .expect("collect");
        store
            .plan_remittance(&plan("h1", 10, 9), OWNER, LEASE, 2)
            .expect("plan");
        // A planned row is released on the reasons that apply to a PLANNED row — and each release
        // is conditional: the wrong reason (this row is not spending, has no bound quote, its lease
        // stands, and it is OWNER's) changes zero rows and touches nothing.
        assert_eq!(
            store
                .release_remittance(
                    "h1",
                    &ReleaseOn::TerminalBoundQuote {
                        quote_id: "q-any".to_owned()
                    },
                    3
                )
                .expect("query"),
            None,
            "a planned row has no bound quote: the spending release changes zero rows"
        );
        assert_eq!(
            store
                .release_remittance(
                    "h1",
                    &ReleaseOn::LeaseExpired {
                        now_unix: LEASE - 1
                    },
                    3
                )
                .expect("query"),
            None,
            "the lease stands: zero rows"
        );
        assert_eq!(
            store
                .release_remittance(
                    "h1",
                    &ReleaseOn::OwnPlanned {
                        owner: "someone-else".to_owned()
                    },
                    3
                )
                .expect("query"),
            None,
            "not that process's row: zero rows"
        );
        assert_eq!(
            store.accrued_fees().expect("read-out").in_flight_fee_sats,
            10,
            "three held releases touched nothing"
        );
        let failed = store
            .release_remittance(
                "h1",
                &ReleaseOn::OwnPlanned {
                    owner: OWNER.to_owned(),
                },
                3,
            )
            .expect("query")
            .expect("released");
        assert_eq!(failed.state, RemittanceState::Failed);
        assert_eq!(failed.settled_at_unix, Some(3));
        assert_eq!(failed.receipts, 0, "its receipts were released");
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(accrued.unremitted_fee_sats, 10);
        assert_eq!(accrued.in_flight_fee_sats, 0);
        assert_eq!(accrued.by_job[0].remittance_id, None);
        assert_eq!(store.in_flight_remittance().expect("query"), None);
        // A failed row never moves again.
        assert!(
            store
                .settle_remittance("h1", &by_reconciliation(None, None), 4)
                .is_err()
        );
        assert_eq!(
            store
                .release_remittance(
                    "h1",
                    &ReleaseOn::OwnPlanned {
                        owner: OWNER.to_owned()
                    },
                    4
                )
                .expect("query"),
            None
        );
        // The same invoice cannot be re-planned; a fresh one can.
        assert_eq!(
            store.plan_remittance(&plan("h1", 10, 9), OWNER, LEASE, 5),
            Err(PlanRefused::DuplicateInvoice {
                payment_hash: "h1".to_owned()
            })
        );
        assert_eq!(
            store.accrued_fees().expect("read-out").unremitted_fee_sats,
            10,
            "a refused plan pins nothing"
        );
        let second = store
            .plan_remittance(&plan("h2", 10, 9), OWNER, LEASE, 6)
            .expect("re-plan");
        assert_eq!(second.receipts, 1);
        // Settling by reconciliation (mint says PAID, fee unobserved) records None for the fee.
        let settled = store
            .settle_remittance("h2", &by_reconciliation(Some("quote-seen"), Some(2)), 7)
            .expect("settle by reconciliation");
        assert_eq!(settled.melt_fee_sats, None);
        assert_eq!(
            settled.net_sats, 9,
            "the planned net stands when the mint's figure is unobserved"
        );
        assert_eq!(settled.melt_quote_id, Some("quote-seen".to_owned()));
        assert_eq!(
            settled.settled_by,
            Some(SettledBy::Reconciliation),
            "the row says HOW it was settled, which is why its fee is unobserved"
        );
        assert_eq!(
            settled.melt_fee_reserve_sats,
            Some(2),
            "the paying quote's reserve — the fee's ceiling — IS observable and is recorded"
        );
        let history = store.remittances().expect("rows");
        assert_eq!(
            history
                .iter()
                .map(|row| (row.remittance_id.as_str(), row.state))
                .collect::<Vec<_>>(),
            vec![
                ("h1", RemittanceState::Failed),
                ("h2", RemittanceState::Settled)
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    // The amount rule (brief §3.3) at the store boundary: a net above the gross is refused before any
    // transaction, and the schema refuses it too.
    #[test]
    fn a_plan_whose_net_exceeds_its_gross_is_refused() {
        let (store, path) = fresh_store("remit-net-gt-gross");
        store
            .collect_receipt("r1", "job-1", 100, fees(1, 1000, 10), 1)
            .expect("collect");
        assert!(matches!(
            store.plan_remittance(&plan("h1", 10, 11), OWNER, LEASE, 2),
            Err(PlanRefused::Store(_))
        ));
        assert!(store.remittances().expect("rows").is_empty());
        let conn = store.lock().expect("lock");
        assert!(
            conn.execute(
                "INSERT INTO fee_remittances
                     (remittance_id, gross_sats, net_sats, destination, payment_hash, bolt11, state, created_at_unix)
                 VALUES ('raw', 10, 11, 'x@y', 'raw', 'ln', 'planned', 3)",
                [],
            )
            .is_err(),
            "CHECK (net_sats <= gross_sats) must refuse"
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    // Addendum 3 §2.4: a store written by a v10 binary (the remittance table WITHOUT owner / lease /
    // reserve / settled_by) opens under v11 additively — the four columns are added, its planned row
    // survives and reads as owned by NOBODY with an EXPIRED lease (fail-closed: not yours to pay,
    // releasable by reconciliation), its settled row reads `settled_by = None` (not recorded, not
    // invented) — and a second open is a no-op.
    #[test]
    fn a_v10_store_migrates_to_v11_additively_and_its_rows_read_as_unowned() {
        let path = temp_db("v10-to-v11");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create v10 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '10');
                 CREATE TABLE receipts (
                     receipt_id      TEXT PRIMARY KEY,
                     job_id          TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     received_at_unix INTEGER NOT NULL,
                     fee_bps         INTEGER NOT NULL DEFAULT 0,
                     fee_sats        INTEGER NOT NULL DEFAULT 0,
                     mint_fee_sats   INTEGER,
                     remittance_id   TEXT
                 );
                 INSERT INTO receipts VALUES ('r-settled', 'job-s', 100, 1, 1000, 10, 1, 'v10-settled');
                 INSERT INTO receipts VALUES ('r-planned', 'job-p', 50, 2, 1000, 5, 1, 'v10-planned');
                 CREATE TABLE fee_remittances (
                     remittance_id   TEXT PRIMARY KEY,
                     gross_sats      INTEGER NOT NULL CHECK (gross_sats >= 0),
                     melt_fee_sats   INTEGER,
                     net_sats        INTEGER NOT NULL CHECK (net_sats >= 0 AND net_sats <= gross_sats),
                     destination     TEXT NOT NULL,
                     melt_quote_id   TEXT,
                     payment_hash    TEXT NOT NULL UNIQUE,
                     bolt11          TEXT NOT NULL,
                     state           TEXT NOT NULL CHECK (state IN ('planned','settled','failed')),
                     created_at_unix INTEGER NOT NULL,
                     settled_at_unix INTEGER
                 );
                 CREATE UNIQUE INDEX fee_remittances_one_planned ON fee_remittances (state) WHERE state = 'planned';
                 INSERT INTO fee_remittances VALUES ('v10-settled', 10, 1, 9, 'maxplayer@agi.cash', 'q1', 'v10-settled', 'ln1', 'settled', 3, 4);
                 INSERT INTO fee_remittances VALUES ('v10-planned', 5, NULL, 4, 'maxplayer@agi.cash', 'q2', 'v10-planned', 'ln2', 'planned', 5, NULL);",
            )
            .expect("v10 schema");
        }

        let store = SellerStore::open(&path).expect("a v10 store opens clean under v11");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 2, "both v10 rows survive");
        let settled = rows
            .iter()
            .find(|r| r.remittance_id == "v10-settled")
            .expect("settled");
        assert_eq!(settled.state, RemittanceState::Settled);
        assert_eq!(
            (settled.gross_sats, settled.melt_fee_sats, settled.net_sats),
            (10, Some(1), 9)
        );
        assert_eq!(
            settled.settled_by, None,
            "not recorded by v10, not invented by v11"
        );
        assert_eq!(settled.melt_fee_reserve_sats, None);
        let planned = rows
            .iter()
            .find(|r| r.remittance_id == "v10-planned")
            .expect("planned");
        assert_eq!(planned.state, RemittanceState::Planned);
        assert_eq!(planned.owner, None);
        assert_eq!(planned.lease_until_unix, None);
        assert!(
            planned.lease_expired(0),
            "a row planned before ownership was recorded reads as an expired claim by nobody"
        );
        assert!(!planned.lease_holds("anyone", 0, 0));
        assert_eq!(
            planned.spending_since_unix, None,
            "a v10 row's melt was never admitted by a fence: PLANNED, not spending"
        );
        assert_eq!(
            store
                .admit_remittance_spend("v10-planned", "anyone", "q-any", 60, &mut || 6)
                .expect("query"),
            Err(OwnershipLost::OtherOwner { owner: None }),
            "nobody may PAY a pre-v11 planned row; reconciliation releases or settles it"
        );
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(
            (
                accrued.remitted_fee_sats,
                accrued.in_flight_fee_sats,
                accrued.unremitted_fee_sats
            ),
            (10, 5, 0)
        );
        // Reconciliation can still release it, and the release reads back through the new columns.
        // Its missing lease reads as run out, so the lease-expiry release applies to it.
        let released = store
            .release_remittance("v10-planned", &ReleaseOn::LeaseExpired { now_unix: 7 }, 7)
            .expect("query")
            .expect("released");
        assert_eq!(released.state, RemittanceState::Failed);
        assert_eq!(
            store.accrued_fees().expect("read-out").unremitted_fee_sats,
            5
        );

        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        assert_eq!(store.remittances().expect("rows").len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    // Addendum 4 §1.1: the pre-spend fence is ONE compare-and-set in the store — planned → spending
    // only for the owner, only while the row is planned, and only while the lease ends MORE than the
    // margin after the clock the caller reads at that instant. Zero rows changed is diagnosed from
    // the row as it stands; a row admitted once is not admitted twice.
    #[test]
    fn the_spend_fence_admits_only_the_owner_of_a_planned_row_with_lease_to_spare_and_only_once() {
        let (store, path) = fresh_store("remit-fence");
        store
            .collect_receipt("r1", "job-1", 100, fees(1, 1000, 10), 1)
            .expect("collect");
        assert!(
            matches!(
                store.plan_remittance(&plan("h0", 10, 9), "   ", 500, 2),
                Err(PlanRefused::Store(_))
            ),
            "an empty owner token is refused"
        );
        let planned = store
            .plan_remittance(&plan("h1", 10, 9), "proc-a", 500, 2)
            .expect("plan");
        assert_eq!(planned.owner.as_deref(), Some("proc-a"));
        assert_eq!(planned.lease_until_unix, Some(500));
        assert_eq!(planned.state, RemittanceState::Planned);
        assert_eq!(planned.spending_since_unix, None);

        // Another process, however early: zero rows — even a live lease is not ITS lease.
        assert_eq!(
            store
                .admit_remittance_spend("h1", "proc-b", "q-b", 60, &mut || 3)
                .expect("query"),
            Err(OwnershipLost::OtherOwner {
                owner: Some("proc-a".to_owned()),
            })
        );
        // The owner with EXACTLY the margin left: zero rows — `lease_until > now + margin` is
        // strict (500 > 440 + 60 is false). The row is untouched: still planned.
        assert_eq!(
            store
                .admit_remittance_spend("h1", "proc-a", "q-a", 60, &mut || 440)
                .expect("query"),
            Err(OwnershipLost::LeaseTooShort {
                lease_until_unix: Some(500),
                now_unix: 440,
                margin_secs: 60,
            })
        );
        assert_eq!(
            store
                .in_flight_remittance()
                .expect("row")
                .expect("planned")
                .state,
            RemittanceState::Planned
        );
        assert_eq!(
            store
                .in_flight_remittance()
                .expect("row")
                .expect("planned")
                .spending_quote_id,
            None,
            "a refused admission binds no quote"
        );
        // One second more to spare: admitted — the row is now SPENDING, stamped with the clock
        // read INSIDE the call (the closure runs once, after the lock), bound to the quote named,
        // and it is still the one row in flight.
        let mut clock_reads = 0;
        let admitted = store
            .admit_remittance_spend("h1", "proc-a", "q-a", 60, &mut || {
                clock_reads += 1;
                439
            })
            .expect("query")
            .expect("admitted");
        assert_eq!(
            clock_reads, 1,
            "the clock is read exactly once, inside the fence"
        );
        assert_eq!(admitted.state, RemittanceState::Spending);
        assert_eq!(admitted.spending_since_unix, Some(439));
        assert_eq!(admitted.spending_quote_id.as_deref(), Some("q-a"));
        assert!(admitted.state.is_in_flight());
        assert_eq!(
            store
                .in_flight_remittance()
                .expect("row")
                .expect("spending")
                .state,
            RemittanceState::Spending
        );
        assert_eq!(
            store.accrued_fees().expect("read-out").in_flight_fee_sats,
            10,
            "a spending row's fee is in flight, not unremitted and not remitted"
        );
        // Admitted once is admitted once: the same owner, the same instant, zero rows — and the
        // bound quote is not rebound.
        assert_eq!(
            store
                .admit_remittance_spend("h1", "proc-a", "q-a2", 60, &mut || 439)
                .expect("query"),
            Err(OwnershipLost::NotPlanned {
                state: RemittanceState::Spending,
            })
        );
        assert_eq!(
            store
                .in_flight_remittance()
                .expect("row")
                .expect("spending")
                .spending_quote_id
                .as_deref(),
            Some("q-a")
        );
        // A SPENDING row is released by exactly one transition: its BOUND quote terminal. Not on
        // its lease (however far past), not as "planned" (it is not), not on some other quote.
        for (wrong, why) in [
            (
                ReleaseOn::LeaseExpired { now_unix: 10_000 },
                "lease expiry never touches a spending row",
            ),
            (
                ReleaseOn::TerminalQuotePlanned,
                "the planned-row release requires no admission mark",
            ),
            (
                ReleaseOn::OwnPlanned {
                    owner: "proc-a".to_owned(),
                },
                "even the owner's own planned-row release: the row is spending",
            ),
            (
                ReleaseOn::TerminalBoundQuote {
                    quote_id: "q-a2".to_owned(),
                },
                "a terminal verdict on a quote that is not the bound one",
            ),
        ] {
            assert_eq!(
                store.release_remittance("h1", &wrong, 10).expect("query"),
                None,
                "{why}"
            );
        }
        assert_eq!(
            store.accrued_fees().expect("read-out").in_flight_fee_sats,
            10,
            "four held releases touched nothing"
        );
        let released = store
            .release_remittance(
                "h1",
                &ReleaseOn::TerminalBoundQuote {
                    quote_id: "q-a".to_owned(),
                },
                10,
            )
            .expect("query")
            .expect("released on the bound quote");
        assert_eq!(released.state, RemittanceState::Failed);
        assert_eq!(
            released.spending_quote_id.as_deref(),
            Some("q-a"),
            "history kept"
        );
        // A row that is no longer in flight: zero rows, whoever asks; a missing row says so.
        assert_eq!(
            store
                .admit_remittance_spend("h1", "proc-a", "q-a", 60, &mut || 11)
                .expect("query"),
            Err(OwnershipLost::NotPlanned {
                state: RemittanceState::Failed,
            })
        );
        assert_eq!(
            store
                .admit_remittance_spend("nope", "proc-a", "q-a", 60, &mut || 11)
                .expect("query"),
            Err(OwnershipLost::Missing)
        );
        assert!(
            store
                .admit_remittance_spend("h1", "proc-a", "  ", 60, &mut || 11)
                .is_err(),
            "no admission without a named quote"
        );
        // The pure helper states the same strict predicate the SQL evaluates.
        assert!(planned.lease_holds("proc-a", 439, 60));
        assert!(!planned.lease_holds("proc-a", 440, 60));
        assert!(!planned.lease_holds("proc-b", 3, 60));
        assert!(!planned.lease_expired(499));
        assert!(planned.lease_expired(500));
        let _ = std::fs::remove_file(&path);
    }

    // Addendum 10 §1.4 (ledger): a re-plan moves ONLY the invoice-side figures of OUR still-planned,
    // still-unbound row — net, payment hash, bolt11, reserve, quote — and nothing else: gross,
    // remittance_id (the receipts' pin), owner, lease and state are as planned. Another owner, and a
    // row already admitted by the fence, change zero rows (`Ok(None)`) and are left exactly as they
    // were; a net over the gross is refused before any write.
    #[test]
    fn replan_remittance_updates_only_our_own_planned_unbound_row_and_keeps_receipts_pinned() {
        let (store, path) = fresh_store("remit-replan");
        store
            .collect_receipt("r1", "job-1", 100, fees(1, 1000, 10), 1)
            .expect("collect");
        store
            .collect_receipt("r2", "job-2", 100, fees(1, 1000, 10), 1)
            .expect("collect");
        let planned = store
            .plan_remittance(&plan("h1", 20, 17), "proc-a", 500, 2)
            .expect("plan");
        assert_eq!(planned.net_sats, 17);
        assert_eq!(planned.melt_fee_reserve_sats, Some(3));
        let replan = RemittanceReplan {
            net_sats: 15,
            payment_hash: "h1-replan".to_owned(),
            bolt11: "lnbc-test-h1-replan".to_owned(),
            melt_fee_reserve_sats: 0,
            melt_quote_id: Some("quote-h1-replan".to_owned()),
        };

        // Another owner: zero rows, and the row is untouched.
        assert_eq!(
            store
                .replan_remittance("h1", "proc-b", &replan)
                .expect("query"),
            None
        );
        assert_eq!(
            store.in_flight_remittance().expect("row").expect("planned"),
            planned,
            "a refused re-plan writes nothing"
        );
        // A row that does not exist: zero rows.
        assert_eq!(
            store
                .replan_remittance("h-none", "proc-a", &replan)
                .expect("query"),
            None
        );
        // Net over the gross: refused before any write.
        assert!(
            store
                .replan_remittance(
                    "h1",
                    "proc-a",
                    &RemittanceReplan {
                        net_sats: 21,
                        ..replan.clone()
                    },
                )
                .is_err()
        );
        assert!(
            store
                .replan_remittance(
                    "h1",
                    "proc-a",
                    &RemittanceReplan {
                        payment_hash: "  ".to_owned(),
                        ..replan.clone()
                    },
                )
                .is_err()
        );
        assert_eq!(
            store.in_flight_remittance().expect("row").expect("planned"),
            planned
        );

        // The owner, on its planned unbound row: ONE row changed; only the invoice-side figures moved.
        let replanned = store
            .replan_remittance("h1", "proc-a", &replan)
            .expect("query")
            .expect("re-planned");
        assert_eq!(
            replanned.remittance_id, "h1",
            "the receipts' pin does not move"
        );
        assert_eq!(replanned.gross_sats, 20, "gross is untouched");
        assert_eq!(replanned.net_sats, 15);
        assert_eq!(replanned.payment_hash, "h1-replan");
        assert_eq!(replanned.bolt11, "lnbc-test-h1-replan");
        assert_eq!(replanned.melt_fee_reserve_sats, Some(0));
        assert_eq!(replanned.melt_quote_id.as_deref(), Some("quote-h1-replan"));
        assert_eq!(replanned.state, RemittanceState::Planned);
        assert_eq!(replanned.owner.as_deref(), Some("proc-a"));
        assert_eq!(replanned.lease_until_unix, Some(500));
        assert_eq!(replanned.spending_since_unix, None);
        assert_eq!(replanned.spending_quote_id, None);
        assert_eq!(replanned.melt_fee_sats, None);
        assert_eq!(replanned.settled_at_unix, None);
        assert_eq!(replanned.created_at_unix, planned.created_at_unix);
        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(
            (accrued.in_flight_fee_sats, accrued.unremitted_fee_sats),
            (20, 0),
            "both receipts stay pinned to the re-planned row"
        );
        // A second plan is still refused: the re-planned row is the one in flight.
        assert!(matches!(
            store.plan_remittance(&plan("h2", 20, 17), "proc-a", 500, 3),
            Err(PlanRefused::InFlight(_))
        ));
        // The original hash is the row id, so the NEW hash cannot be reused by a later plan.
        // (Exercised once the row is terminal; here the fence binds the re-planned quote.)
        let admitted = store
            .admit_remittance_spend("h1", "proc-a", "quote-h1-replan", 60, &mut || 3)
            .expect("query")
            .expect("admitted");
        assert_eq!(admitted.state, RemittanceState::Spending);
        assert_eq!(admitted.net_sats, 15);
        // Once admitted (spending, quote bound): zero rows — a re-plan never moves a spend in
        // progress.
        assert_eq!(
            store
                .replan_remittance(
                    "h1",
                    "proc-a",
                    &RemittanceReplan {
                        net_sats: 14,
                        payment_hash: "h1-again".to_owned(),
                        ..replan.clone()
                    },
                )
                .expect("query"),
            None
        );
        let held = store
            .in_flight_remittance()
            .expect("row")
            .expect("spending");
        assert_eq!(held.state, RemittanceState::Spending);
        assert_eq!(held.net_sats, 15);
        assert_eq!(held.payment_hash, "h1-replan");
        assert_eq!(held.spending_quote_id.as_deref(), Some("quote-h1-replan"));
        let _ = std::fs::remove_file(&path);
    }

    // Addendum 4 §1 (ledger): a store written by a v11 binary — the remittance table WITH owner /
    // lease / reserve / settled_by but WITHOUT `spending_since_unix` — opens under v12 additively:
    // the one column is added, its planned row survives and reads PLANNED (its melt was never
    // admitted by a fence, which is the truth of a row written before the fence existed), its owner
    // and lease are exactly as written, the fence then works on it, and a second open is a no-op.
    #[test]
    fn a_v11_store_migrates_to_v12_additively_and_its_planned_row_reads_as_not_spending() {
        let path = temp_db("v11-to-v12");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create v11 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '11');
                 CREATE TABLE receipts (
                     receipt_id      TEXT PRIMARY KEY,
                     job_id          TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     received_at_unix INTEGER NOT NULL,
                     fee_bps         INTEGER NOT NULL DEFAULT 0,
                     fee_sats        INTEGER NOT NULL DEFAULT 0,
                     mint_fee_sats   INTEGER,
                     remittance_id   TEXT
                 );
                 INSERT INTO receipts VALUES ('r-planned', 'job-p', 50, 2, 1000, 5, 1, 'v11-planned');
                 CREATE TABLE fee_remittances (
                     remittance_id   TEXT PRIMARY KEY,
                     gross_sats      INTEGER NOT NULL CHECK (gross_sats >= 0),
                     melt_fee_sats   INTEGER,
                     net_sats        INTEGER NOT NULL CHECK (net_sats >= 0 AND net_sats <= gross_sats),
                     destination     TEXT NOT NULL,
                     melt_quote_id   TEXT,
                     payment_hash    TEXT NOT NULL UNIQUE,
                     bolt11          TEXT NOT NULL,
                     state           TEXT NOT NULL CHECK (state IN ('planned','settled','failed')),
                     created_at_unix INTEGER NOT NULL,
                     settled_at_unix INTEGER,
                     owner           TEXT,
                     lease_until_unix INTEGER,
                     melt_fee_reserve_sats INTEGER,
                     settled_by      TEXT
                 );
                 CREATE UNIQUE INDEX fee_remittances_one_planned ON fee_remittances (state) WHERE state = 'planned';
                 INSERT INTO fee_remittances VALUES ('v11-planned', 5, NULL, 4, 'maxplayer@agi.cash', 'q2', 'v11-planned', 'ln2', 'planned', 100, NULL, 'proc-old', 400, 1, NULL);",
            )
            .expect("v11 schema");
        }

        let store = SellerStore::open(&path).expect("a v11 store opens clean under v12");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        let planned = store
            .in_flight_remittance()
            .expect("row")
            .expect("the v11 planned row is the row in flight");
        assert_eq!(planned.state, RemittanceState::Planned);
        assert_eq!(planned.spending_since_unix, None);
        assert_eq!(planned.owner.as_deref(), Some("proc-old"));
        assert_eq!(planned.lease_until_unix, Some(400));
        assert_eq!(planned.melt_fee_reserve_sats, Some(1));
        // The fence works on the migrated row: its owner, inside the lease, is admitted; the state
        // CHECK is untouched because SPENDING lives in the new column, not in `state`.
        let admitted = store
            .admit_remittance_spend("v11-planned", "proc-old", "q-pay", 60, &mut || 300)
            .expect("query")
            .expect("admitted");
        assert_eq!(admitted.state, RemittanceState::Spending);
        assert_eq!(admitted.spending_since_unix, Some(300));
        assert_eq!(admitted.spending_quote_id.as_deref(), Some("q-pay"));
        let raw_state: String = {
            let conn = store.lock().expect("lock");
            conn.query_row(
                "SELECT state FROM fee_remittances WHERE remittance_id = 'v11-planned'",
                [],
                |row| row.get(0),
            )
            .expect("raw state")
        };
        assert_eq!(
            raw_state, "planned",
            "on disk a spending row is a planned row with a mark"
        );

        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        assert_eq!(
            store.remittances().expect("rows")[0].state,
            RemittanceState::Spending,
            "the mark survives a reopen"
        );
        let _ = std::fs::remove_file(&path);
    }

    // Addendum 5 §1 (ledger): a store written by a v12 binary — the remittance table WITH
    // `spending_since_unix` but WITHOUT `spending_quote_id` — opens under v13 additively: the one
    // column is added, its SPENDING row survives and reads SPENDING with NO bound quote (the truth of
    // a row admitted before admissions bound a quote), its owner / lease / mark are exactly as
    // written, and its release goes the way v12's did — on its invoice's quote being terminal,
    // never on time, and never through the bound-quote release (it has none). A second open is a
    // no-op, and a row THIS binary admits on the migrated store is bound.
    #[test]
    fn a_v12_store_migrates_to_v13_additively_and_its_spending_row_reads_as_unbound() {
        let path = temp_db("v12-to-v13");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create v12 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '12');
                 CREATE TABLE receipts (
                     receipt_id      TEXT PRIMARY KEY,
                     job_id          TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     received_at_unix INTEGER NOT NULL,
                     fee_bps         INTEGER NOT NULL DEFAULT 0,
                     fee_sats        INTEGER NOT NULL DEFAULT 0,
                     mint_fee_sats   INTEGER,
                     remittance_id   TEXT
                 );
                 INSERT INTO receipts VALUES ('r-spending', 'job-s', 50, 2, 1000, 5, 1, 'v12-spending');
                 INSERT INTO receipts VALUES ('r-free', 'job-f', 30, 3, 1000, 3, 1, NULL);
                 CREATE TABLE fee_remittances (
                     remittance_id   TEXT PRIMARY KEY,
                     gross_sats      INTEGER NOT NULL CHECK (gross_sats >= 0),
                     melt_fee_sats   INTEGER,
                     net_sats        INTEGER NOT NULL CHECK (net_sats >= 0 AND net_sats <= gross_sats),
                     destination     TEXT NOT NULL,
                     melt_quote_id   TEXT,
                     payment_hash    TEXT NOT NULL UNIQUE,
                     bolt11          TEXT NOT NULL,
                     state           TEXT NOT NULL CHECK (state IN ('planned','settled','failed')),
                     created_at_unix INTEGER NOT NULL,
                     settled_at_unix INTEGER,
                     owner           TEXT,
                     lease_until_unix INTEGER,
                     melt_fee_reserve_sats INTEGER,
                     settled_by      TEXT,
                     spending_since_unix INTEGER
                 );
                 CREATE UNIQUE INDEX fee_remittances_one_planned ON fee_remittances (state) WHERE state = 'planned';
                 INSERT INTO fee_remittances VALUES ('v12-spending', 5, NULL, 4, 'maxplayer@agi.cash', 'q-est', 'v12-spending', 'ln3', 'planned', 100, NULL, 'proc-v12', 400, 1, NULL, 150);",
            )
            .expect("v12 schema");
        }

        let store = SellerStore::open(&path).expect("a v12 store opens clean under v13");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        assert_eq!(SCHEMA_VERSION, 13);
        let spending = store
            .in_flight_remittance()
            .expect("row")
            .expect("the v12 spending row is the row in flight");
        assert_eq!(spending.state, RemittanceState::Spending);
        assert_eq!(spending.spending_since_unix, Some(150));
        assert_eq!(
            spending.spending_quote_id, None,
            "admitted before quotes were bound"
        );
        assert_eq!(spending.owner.as_deref(), Some("proc-v12"));
        assert_eq!(spending.lease_until_unix, Some(400));
        assert_eq!(spending.melt_quote_id.as_deref(), Some("q-est"));
        assert_eq!(
            store.accrued_fees().expect("read-out").in_flight_fee_sats,
            5
        );
        // No release on time, and none through the bound-quote transition (nothing is bound).
        for (wrong, why) in [
            (
                ReleaseOn::LeaseExpired { now_unix: 10_000 },
                "lease expiry never touches a spending row, migrated or not",
            ),
            (
                ReleaseOn::TerminalBoundQuote {
                    quote_id: "q-est".to_owned(),
                },
                "the estimate quote was never BOUND: the bound-quote release changes zero rows",
            ),
            (
                ReleaseOn::OwnPlanned {
                    owner: "proc-v12".to_owned(),
                },
                "not a planned row",
            ),
        ] {
            assert_eq!(
                store
                    .release_remittance("v12-spending", &wrong, 500)
                    .expect("query"),
                None,
                "{why}"
            );
        }
        // The one release an unbound spending row has: its invoice's quote terminal, as v12 did it.
        let released = store
            .release_remittance("v12-spending", &ReleaseOn::TerminalUnboundSpending, 500)
            .expect("query")
            .expect("released");
        assert_eq!(released.state, RemittanceState::Failed);
        assert_eq!(released.receipts, 0);
        assert_eq!(
            store.accrued_fees().expect("read-out").unremitted_fee_sats,
            8
        );
        // A row THIS binary admits on the migrated store is bound, and its unbound release then
        // changes zero rows: the v12 path is for v12 rows only.
        let planned = store
            .plan_remittance(&plan("h-new", 8, 7), "proc-new", 900, 501)
            .expect("plan");
        assert_eq!(planned.spending_quote_id, None);
        let admitted = store
            .admit_remittance_spend("h-new", "proc-new", "q-bound", 60, &mut || 502)
            .expect("query")
            .expect("admitted");
        assert_eq!(admitted.spending_quote_id.as_deref(), Some("q-bound"));
        assert_eq!(
            store
                .release_remittance("h-new", &ReleaseOn::TerminalUnboundSpending, 503)
                .expect("query"),
            None
        );
        let raw: (String, Option<i64>, Option<String>) = {
            let conn = store.lock().expect("lock");
            conn.query_row(
                "SELECT state, spending_since_unix, spending_quote_id FROM fee_remittances WHERE remittance_id = 'h-new'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("raw")
        };
        assert_eq!(
            raw,
            ("planned".to_owned(), Some(502), Some("q-bound".to_owned())),
            "on disk a bound spending row is a planned row with a mark and a quote"
        );

        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        let rows = store.remittances().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1].spending_quote_id.as_deref(),
            Some("q-bound"),
            "the binding survives a reopen"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expire_outbox_stops_the_publisher_from_sending() {
        let (store, path) = fresh_store("expire");
        let job = "j".repeat(64);
        store.claim_and_enqueue(&job, &"o".repeat(64), Some("creqA"), &claim(), 1, 100, 1).expect("claim");
        // now=200 is past expires_at=100.
        assert_eq!(store.expire_outbox(200).expect("expire"), 1);
        assert!(store.pending_outbox(200).expect("pending").is_empty());
        assert_eq!(
            store.outbox_row(&format!("claim:{job}")).expect("row").expect("exists").0,
            "expired"
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod free_lane_tests {
    use super::*;
    use crate::gateway::PaymentMode;
    use rusqlite::Connection;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-free-lane-store-{label}-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    fn wire_draft(kind: u16) -> EventDraft {
        EventDraft::new(kind, vec![crate::gateway::TagSpec::new(["t", "maxplayer"])], "")
    }

    fn offer_row(job: &str, mode: PaymentMode) -> Offer {
        Offer {
            offer_id: job.to_owned(),
            buyer_pubkey: "b".repeat(64),
            amount_sats: if mode.is_free() { 0 } else { 21 },
            unit: "sat".to_owned(),
            task: "t".to_owned(),
            deadline_unix: 2_000_000_000,
            targeted: true,
            requested_agent: None,
            output: Some("text/plain".to_owned()),
            payment_mode: mode,
        }
    }

    fn payment_column(path: &std::path::Path, table: &str, job: &str) -> Option<String> {
        let conn = Connection::open(path).expect("reopen raw");
        conn.query_row(
            &format!("SELECT payment FROM {table} WHERE {} = ?1", if table == "offers" { "offer_id" } else { "job_id" }),
            [job],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("read payment column")
    }

    /// §3.2 — RULING 3's RECORD. A free job still writes a delivery row, and that row says `none`.
    ///
    /// Both modes are asserted from the same store, because "writes 'none'" alone would pass a
    /// writer that wrote `none` for everything — which would mis-report every priced delivery in
    /// the market as unpaid-forever.
    #[test]
    fn a_free_delivery_records_payment_none_and_a_priced_one_records_sat() {
        let path = temp_db("delivery-payment");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");

        for (job, mode, expected) in [("free-job", PaymentMode::None, "none"), ("paid-job", PaymentMode::Sat, "sat")] {
            store.record_offer(&offer_row(job, mode), 1).expect("record offer");
            store
                .claim_and_enqueue(job, job, if mode.is_free() { None } else { Some("creqA") }, &wire_draft(crate::gateway::JOB_CLAIM_KIND), 1, 9_999, 1)
                .expect("claim");
            store
                .record_award(&format!("award-{job}"), job, &"b".repeat(64), 2)
                .expect("award");
            assert!(
                store
                    .deliver_and_enqueue(job, "ref", mode, &wire_draft(crate::gateway::JOB_RESULT_KIND), 3, 9_999, 3)
                    .expect("deliver"),
                "the delivery row must be written for BOTH modes — ruling 3"
            );
            assert_eq!(
                payment_column(&path, "deliveries", job).as_deref(),
                Some(expected),
                "{job} delivery row payment column"
            );
        }

        // A free job's terminal state stays 'delivered' — it never advances to 'paid', and the
        // deliveries.payment column is the fact that says so.
        assert_eq!(store.job_state("free-job").expect("state"), Some(JobState::Delivered));
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// The offer's mode is JOURNALED, so a delivery that happens a RESTART after the claim still
    /// records the mode the job was posted under.
    ///
    /// Without the persisted column a resumed free job would read its offer back as `Sat` — the
    /// fail-closed default — and write its delivery as PAID, which is the one row an operator's
    /// arrears tooling reads.
    #[test]
    fn the_offers_payment_mode_survives_a_restart() {
        let path = temp_db("offer-mode-restart");
        let _ = std::fs::remove_file(&path);
        {
            let store = SellerStore::open(&path).expect("open");
            store.record_offer(&offer_row("free-job", PaymentMode::None), 1).expect("record");
            store.record_offer(&offer_row("paid-job", PaymentMode::Sat), 1).expect("record");
        }
        let store = SellerStore::open(&path).expect("reopen — the process died and came back");
        assert_eq!(
            store.offer_row("free-job").expect("read").expect("row").payment_mode,
            PaymentMode::None
        );
        assert_eq!(
            store.offer_row("paid-job").expect("read").expect("row").payment_mode,
            PaymentMode::Sat
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// The v6→v7 migration is ADDITIVE, IDEMPOTENT and RE-ENTRANT on a live store, and it does not
    /// touch the `jobs.state` CHECK.
    ///
    /// The pre-existing money-path rows are read back after the migration, so a migration that
    /// rewrote or dropped a row fails here rather than in production. The second open proves
    /// re-entrance: `column_exists` must make the ALTERs no-ops, not errors.
    #[test]
    fn a_v6_store_migrates_to_v7_additively_and_re_entrantly() {
        let path = temp_db("v6-to-v7");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create v6 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '6');
                 CREATE TABLE offers (
                     offer_id TEXT PRIMARY KEY, buyer_pubkey TEXT NOT NULL,
                     amount_sats INTEGER NOT NULL CHECK (amount_sats >= 0), unit TEXT NOT NULL,
                     task TEXT NOT NULL, deadline_unix INTEGER NOT NULL, targeted INTEGER NOT NULL,
                     created_at_unix INTEGER NOT NULL, requested_agent TEXT, output TEXT
                 );
                 INSERT INTO offers VALUES ('legacy-job','bb','21','sat','t',2000000000,1,1,NULL,'text/plain');
                 CREATE TABLE deliveries (
                     job_id TEXT PRIMARY KEY, result_ref TEXT NOT NULL, delivered_at_unix INTEGER NOT NULL
                 );
                 INSERT INTO deliveries VALUES ('legacy-job','legacy-ref',7);",
            )
            .expect("v6 schema");
        }

        let store =
            SellerStore::open(&path).expect("a v6 store opens clean under the current schema");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        assert_eq!(
            SCHEMA_VERSION, 13,
            "v7 was the free lane; v8 added the receipt fee columns; v9 the mint fee; v10 the fee remittance ledger; v11 its ownership and settlement provenance; v12 the spending mark; v13 the quote bound at admission"
        );

        // The legacy rows SURVIVE and read as PAID — correct by construction, because every job
        // recorded before this column existed was priced.
        let legacy = store
            .offer_row("legacy-job")
            .expect("read")
            .expect("the v6 offer survives");
        assert_eq!(
            legacy.amount_sats, 21,
            "the pre-existing money-path row is untouched"
        );
        assert_eq!(
            legacy.payment_mode,
            PaymentMode::Sat,
            "a NULL payment column resolves to PAID, never to a third state"
        );
        assert_eq!(
            payment_column(&path, "deliveries", "legacy-job"),
            None,
            "the migration ADDS a nullable column; it does not backfill or rewrite a live row"
        );

        // RE-ENTRANT: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
        let again = store.offer_row("legacy-job").expect("read").expect("row");
        assert_eq!(again.amount_sats, 21);
        drop(store);

        // §3.2 — the `jobs.state` CHECK is UNTOUCHED: 'settled_free' was rejected as a terminal
        // state precisely because widening this constraint needs a table rebuild, which migrate's
        // additive-only contract forbids on a live money store.
        let conn = Connection::open(&path).expect("reopen raw");
        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='jobs'", [], |row| row.get(0))
            .expect("jobs DDL");
        assert!(
            ddl.contains("CHECK (state IN ('awarded','executing','delivered','paid','failed'))"),
            "the jobs.state CHECK must be byte-unchanged by the free lane: {ddl}"
        );
        assert!(
            !ddl.contains("settled_free"),
            "no new terminal state was added — deliveries.payment carries the fact instead: {ddl}"
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    // ---- seller-store arrears query (issue #975) ---------------------------------------------

    /// Drive a job through the real write path to a CONFIRMED-PUBLISHED delivery. `mode` is the
    /// payment mode the offer and delivery are journaled under (`Sat` for paid, `None` for free).
    /// After `deliver_and_enqueue` lands the `result:{job}` outbox row in `pending`, this marks it
    /// CONFIRMED via the same `mark_confirmed` path the publisher uses — so the fixture is proof of
    /// published work, not merely enqueue. (R2 F1 lesson: a fixture that only enqueues cannot tell
    /// the published case from the unpublished one.)
    fn deliver(store: &SellerStore, job: &str, mode: PaymentMode) {
        store.record_offer(&offer_row(job, mode), 1).expect("record offer");
        store
            .claim_and_enqueue(
                job,
                job,
                if mode.is_free() { None } else { Some("creqA") },
                &wire_draft(crate::gateway::JOB_CLAIM_KIND),
                1,
                9_999,
                1,
            )
            .expect("claim");
        store.record_award(&format!("award-{job}"), job, &"b".repeat(64), 2).expect("award");
        assert!(
            store
                .deliver_and_enqueue(
                    job,
                    "ref",
                    mode,
                    &wire_draft(crate::gateway::JOB_RESULT_KIND),
                    3,
                    9_999,
                    3,
                )
                .expect("deliver"),
            "{job} must deliver"
        );
        confirm_result(store, job);
    }

    /// Confirm the `result:{job}` outbox row the way the publisher does: find the pending row and
    /// call `mark_confirmed` on it. Panics if there is no such pending row (a fixture bug).
    /// (Discovery uses `now=8_000` because `pending_outbox` only returns rows with
    /// `expires_at_unix > now`; our rows expire at 9_999, so they are pending across 8_000.)
    fn confirm_result(store: &SellerStore, job: &str) {
        let pending = store.pending_outbox(8_000).expect("pending");
        let row = pending
            .iter()
            .find(|item| item.dedup_key == format!("result:{job}"))
            .unwrap_or_else(|| panic!("result:{job} must be pending before confirm"));
        store
            .mark_confirmed(row.id, &format!("evt-{job}"), 9_999)
            .expect("confirm");
    }

    /// Drive a job to a DELIVERED-but-UNCONFIRMED state (enqueue only): the result was produced and
    /// handed to the outbox, but publication has NOT been confirmed. Used for the lifecycle cases
    /// the R1 review flagged as wrongly reported (pending, failed attempt, expired-unpublished).
    fn deliver_unconfirmed(store: &SellerStore, job: &str, mode: PaymentMode) {
        store.record_offer(&offer_row(job, mode), 1).expect("record offer");
        store
            .claim_and_enqueue(
                job,
                job,
                if mode.is_free() { None } else { Some("creqA") },
                &wire_draft(crate::gateway::JOB_CLAIM_KIND),
                1,
                9_999,
                1,
            )
            .expect("claim");
        store.record_award(&format!("award-{job}"), job, &"b".repeat(64), 2).expect("award");
        assert!(
            store
                .deliver_and_enqueue(
                    job,
                    "ref",
                    mode,
                    &wire_draft(crate::gateway::JOB_RESULT_KIND),
                    3,
                    9_999,
                    3,
                )
                .expect("deliver"),
            "{job} must deliver"
        );
    }

    /// Mark the `result:{job}` outbox row EXPIRED the way the real publisher does after the retry
    /// window closes: call `expire_outbox`, which flips every still-`pending` row whose
    /// `expires_at_unix <= now` to `expired`. Publication was never confirmed. (We controlled the
    /// row's `expires_at_unix` in `deliver_unconfirmed` so this is exactly the row we mean.)
    fn expire_pending_result(store: &SellerStore, job: &str) {
        let before = store.pending_outbox(8_000).expect("pending");
        assert!(
            before.iter().any(|i| i.dedup_key == format!("result:{job}")),
            "result:{job} must be pending before expire"
        );
        // `expire_outbox` flips every still-pending row whose `expires_at_unix <= now`. Our rows
        // expire at 9_999, so `now=9_999` expires them. (Discovery uses 8_000 because
        // `pending_outbox` only returns rows with `expires_at_unix > now`.)
        let expired = store.expire_outbox(9_999).expect("expire");
        let after = store.pending_outbox(8_000).expect("pending");
        assert!(
            after.iter().all(|i| i.dedup_key != format!("result:{job}")),
            "result:{job} must be gone from pending after expire"
        );
        assert!(expired >= 1, "expire_outbox must have expired a row");
    }

    /// Settle a delivered PRICED job the only way the store does: a real receipt write that marks
    /// `jobs.state = 'paid'`.
    fn settle(store: &SellerStore, job: &str) {
        store
            .collect_receipt(
                &format!("receipt-{job}"),
                job,
                21,
                ReceiptFees { mint_fee_sats: 1, fee_bps: 1000, fee_sats: 2 },
                4,
            )
            .expect("collect");
    }

    /// The job_ids the supported arrears method reports, in the order it reports them.
    fn arrears_ids(store: &SellerStore) -> Vec<String> {
        store
            .arrears()
            .expect("arrears")
            .into_iter()
            .map(|a| a.job_id)
            .collect()
    }

    /// Insert a LEGACY delivery row that was actually CONFIRMED-PUBLISHED: a priced job delivered
    /// BEFORE `deliveries.payment` existed, so `payment` is NULL and every reader must resolve it
    /// to PRICED, never free. Because R2 only reports jobs with RECORDED confirmed publication
    /// proof, this also writes a CONFIRMED `result:{job}` outbox row — the legacy row carries the
    /// proof it needs to be asserted owed.
    fn legacy_delivery(path: &std::path::Path, job: &str) {
        let conn = Connection::open(path).expect("reopen raw");
        conn.execute(
            "INSERT INTO jobs (job_id, offer_id, state, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, 'delivered', 1, 1)",
            params![job, format!("offer-{job}")],
        )
        .expect("insert legacy job");
        conn.execute(
            "INSERT INTO deliveries (job_id, result_ref, delivered_at_unix, payment)
             VALUES (?1, 'legacy-ref', 1, NULL)",
            [job],
        )
        .expect("insert legacy delivery");
        conn.execute(
            "INSERT INTO nostr_event_outbox
                (dedup_key, draft_json, created_at_unix, state, attempts,
                 expires_at_unix, published_event_id, updated_at_unix)
             VALUES (?1, '{}', 1, 'confirmed', 1, 9999, ?2, 1)",
            params![format!("result:{job}"), format!("legacy-evt-{job}")],
        )
        .expect("insert confirmed publication proof");
        drop(conn);
    }

    /// Insert a LEGACY delivery row WITHOUT publication proof: a priced job delivered before
    /// `deliveries.payment` existed (payment NULL = PRICED) whose result publication was NEVER
    /// confirmed. Under R2 this must NOT be asserted owed — there is no recorded proof the work
    /// reached the world, so the "debt" may never have been incurred.
    fn legacy_delivery_unconfirmed(path: &std::path::Path, job: &str) {
        let conn = Connection::open(path).expect("reopen raw");
        conn.execute(
            "INSERT INTO jobs (job_id, offer_id, state, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, 'delivered', 1, 1)",
            params![job, format!("offer-{job}")],
        )
        .expect("insert legacy job");
        conn.execute(
            "INSERT INTO deliveries (job_id, result_ref, delivered_at_unix, payment)
             VALUES (?1, 'legacy-ref-unconfirmed', 1, NULL)",
            [job],
        )
        .expect("insert legacy delivery");
        // NO outbox row: no recorded publication proof. The legacy delivery is deliberately not
        // asserted owed.
        drop(conn);
    }

    /// A FREE job is delivered but never owed money — `payment='none'` must keep it out of arrears.
    ///
    /// NON-VACUOUS: if the query forgot the `payment` filter and counted every delivered-unpaid
    /// row as owed, the free job would appear here and the set would shift; if it wrongly excluded
    /// ALL unpaid rows, the priced job would vanish. Both are caught by the exact-set assertion.
    #[test]
    fn arrears_excludes_free_jobs() {
        let path = temp_db("arrears-free");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver(&store, "free-job", PaymentMode::None);
        deliver(&store, "priced-job", PaymentMode::Sat);
        assert_eq!(
            arrears_ids(&store),
            vec!["priced-job".to_owned()],
            "only the priced delivered-unpaid job is owed; the free job is not debt"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// A LEGACY `payment = NULL` delivery is a priced job (the fail-closed default), so it counts.
    ///
    /// NON-VACUOUS: if a reader resolved NULL as free (or as anything to exclude), the legacy job
    /// would disappear from the result and the exact-set assertion would fail.
    #[test]
    fn arrears_includes_legacy_null_as_priced() {
        let path = temp_db("arrears-legacy");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver(&store, "priced-job", PaymentMode::Sat);
        legacy_delivery(&path, "legacy-job");
        assert_eq!(
            arrears_ids(&store),
            vec!["legacy-job".to_owned(), "priced-job".to_owned()],
            "legacy NULL and explicit 'sat' both resolve PRICED and both count"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// A delivered job that HAS a receipt is paid — not arrears.
    ///
    /// NON-VACUOUS: if the query dropped the anti-join against `receipts` (or forgot to exclude
    /// paid), the settled job would appear as owed and the exact-set assertion would fail.
    #[test]
    fn arrears_excludes_paid_jobs() {
        let path = temp_db("arrears-paid");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver(&store, "paid-job", PaymentMode::Sat);
        settle(&store, "paid-job");
        deliver(&store, "unpaid-job", PaymentMode::Sat);
        assert_eq!(
            arrears_ids(&store),
            vec!["unpaid-job".to_owned()],
            "the settled job is excluded; only the delivered-unpaid job is owed"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// A job that never reached an actual delivery is not owed money at all.
    ///
    /// NON-VACUOUS: the method is anchored on the `deliveries` table, so a claimed/awarded job with
    /// no delivery row (and thus no publication) cannot be reported as an unpaid delivery. If the
    /// method instead walked `jobs` and treated any non-`paid` terminal state as owed, this awarded
    /// job would leak in and the exact-set assertion would fail.
    #[test]
    fn arrears_excludes_non_delivered_jobs() {
        let path = temp_db("arrears-nondelivered");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        // A job claimed and awarded but never delivered — no `deliveries` row exists.
        store.record_offer(&offer_row("not-delivered", PaymentMode::Sat), 1).expect("record offer");
        store
            .claim_and_enqueue(
                "not-delivered",
                "not-delivered",
                Some("creq"),
                &wire_draft(crate::gateway::JOB_CLAIM_KIND),
                1,
                9_999,
                1,
            )
            .expect("claim");
        store.record_award("award-not-delivered", "not-delivered", &"b".repeat(64), 2).expect("award");
        deliver(&store, "delivered-job", PaymentMode::Sat);
        assert_eq!(
            arrears_ids(&store),
            vec!["delivered-job".to_owned()],
            "an awarded-but-never-delivered job owes nothing"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// Mixed rows return EXACT IDs and counts.
    #[test]
    fn arrears_mixed_rows_return_exact_ids_and_counts() {
        let path = temp_db("arrears-mixed");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver(&store, "free-job", PaymentMode::None);
        deliver(&store, "priced-unpaid", PaymentMode::Sat);
        legacy_delivery(&path, "legacy-unpaid");
        deliver(&store, "priced-paid", PaymentMode::Sat);
        settle(&store, "priced-paid");
        store.record_offer(&offer_row("never-delivered", PaymentMode::Sat), 1).expect("record offer");
        store
            .claim_and_enqueue(
                "never-delivered",
                "never-delivered",
                Some("creq"),
                &wire_draft(crate::gateway::JOB_CLAIM_KIND),
                1,
                9_999,
                1,
            )
            .expect("claim");
        store.record_award("award-never", "never-delivered", &"b".repeat(64), 2).expect("award");

        let ids = arrears_ids(&store);
        assert_eq!(
            ids,
            vec!["legacy-unpaid".to_owned(), "priced-unpaid".to_owned()],
            "exactly the delivered, unpaid, priced rows — free, paid and never-delivered excluded"
        );
        assert_eq!(ids.len(), 2, "exact count of owed jobs");
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// The anti-join cannot fan out: a single delivered unpaid job yields exactly ONE row even when
    /// other jobs carry MULTIPLE receipt rows, and `deliveries.job_id` being a PRIMARY KEY means one
    /// delivery row per job.
    ///
    /// NON-VACUOUS: a naive cross `JOIN deliveries x receipts` would emit one output row per
    /// (delivery × receipt) pair — so a delivered job sitting next to several receipts on other jobs
    /// would be repeated and the count would inflate. The exact single-entry assertion catches that.
    #[test]
    fn arrears_does_not_fan_out_on_receipts() {
        let path = temp_db("arrears-cardinality");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver(&store, "owed-job", PaymentMode::Sat);
        // Several rows on OTHER jobs (a legacy/edge store could hold more than one receipt per job);
        // none reference owed-job, so a loose join must not multiply owed-job out.
        let conn = store.lock().expect("lock");
        for i in 0..3 {
            conn.execute(
                "INSERT INTO receipts (receipt_id, job_id, amount_sats, received_at_unix)
                 VALUES (?1, ?2, 21, 5)",
                params![format!("receipt-{i}"), format!("other-job-{i}")],
            )
            .expect("insert receipt");
        }
        drop(conn);
        let ids = arrears_ids(&store);
        assert_eq!(
            ids,
            vec!["owed-job".to_owned()],
            "one delivered unpaid job is reported exactly once, however many receipt rows sit beside it"
        );
        assert_eq!(ids.len(), 1, "count equals distinct owed jobs, not delivery x receipts");
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// R2 F1 lifecycle — PENDING (enqueued-but-not-yet-published): a priced job whose result was
    /// produced and handed to the outbox but whose publication is still `pending`. It is NOT owed:
    /// a delivery row means ENQUEUED, not published, so no debt is asserted until the relay
    /// confirms the result reached the world.
    ///
    /// NON-VACUOUS: this is precisely the defect R1 flagged. If the query treated enqueue as
    /// proof (the old behaviour), this job would be reported owed and the exact-set assertion
    /// would fail. The fixture only enqueues — it cannot tell the bug from the fix unless the
    /// query is corrected, which is the point.
    #[test]
    fn arrears_excludes_pending_unpublished_result() {
        let path = temp_db("arrears-pending");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver_unconfirmed(&store, "pending-unpublished", PaymentMode::Sat);
        deliver(&store, "confirmed", PaymentMode::Sat);
        assert_eq!(
            arrears_ids(&store),
            vec!["confirmed".to_owned()],
            "a pending-unpublished result is not owed; only the confirmed-published job is"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// R2 F1 lifecycle — FAILED ATTEMPT: the publisher tried to send the result and recorded a
    /// failed attempt (the row stays `pending`, `attempts` bumped, retry pending). Publication was
    /// never confirmed, so the job is NOT owed.
    ///
    /// NON-VACUOUS: if the query ignored the outbox and only looked at `deliveries` + `receipts`,
    /// this job would be reported owed. The exact-set assertion catches that.
    #[test]
    fn arrears_excludes_failed_attempt_unpublished_result() {
        let path = temp_db("arrears-failed-attempt");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver_unconfirmed(&store, "failed-attempt", PaymentMode::Sat);
        // Simulate a failed send attempt: the row stays pending, attempts bumped via record_attempt.
        let row = store
            .pending_outbox(8_000)
            .expect("pending")
            .into_iter()
            .find(|i| i.dedup_key == "result:failed-attempt")
            .expect("pending row");
        store.record_attempt(row.id, 8_000).expect("record attempt");
        let updated = store.outbox_row("result:failed-attempt").expect("row").expect("exists");
        assert_eq!(updated.0, "pending", "attempt leaves the row pending");
        deliver(&store, "confirmed", PaymentMode::Sat);
        assert_eq!(
            arrears_ids(&store),
            vec!["confirmed".to_owned()],
            "a failed-attempt (still pending) result is not owed; only confirmed is"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// R2 F1 lifecycle — EXPIRED-UNPUBLISHED: the relay retry window closed and the publisher gave
    /// up; the outbox row is `expired` and publication was NEVER confirmed. The job is NOT owed.
    ///
    /// NON-VACUOUS: this is the strongest R1 counterexample — the delivery row persists and no
    /// receipt exists, yet publication never happened. If the query treated enqueue as proof, this
    /// job would be reported owed forever.
    #[test]
    fn arrears_excludes_expired_unpublished_result() {
        let path = temp_db("arrears-expired");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver_unconfirmed(&store, "expired-unpublished", PaymentMode::Sat);
        expire_pending_result(&store, "expired-unpublished");
        // Confirm the row is now expired, not pending.
        assert_eq!(
            store.outbox_row("result:expired-unpublished").expect("row").expect("exists").0,
            "expired",
            "expire_outbox must have flipped the row to expired"
        );
        deliver(&store, "confirmed", PaymentMode::Sat);
        assert_eq!(
            arrears_ids(&store),
            vec!["confirmed".to_owned()],
            "an expired-unpublished result is not owed; only confirmed is"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// R2 F1 — CONFIRMED INCLUDED: a priced job whose result publication is recorded CONFIRMED is
    /// owed. This is the positive case that must appear in arrears.
    #[test]
    fn arrears_includes_confirmed_result() {
        let path = temp_db("arrears-confirmed");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver(&store, "confirmed-job", PaymentMode::Sat);
        assert_eq!(
            arrears_ids(&store),
            vec!["confirmed-job".to_owned()],
            "a confirmed-published unpaid priced job is owed"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// R2 — a CONFIRMED FREE job stays EXCLUDED: even with publication proof, `payment='none'`
    /// means not a debt. This preserves the already-accepted rule under the new publication gate.
    #[test]
    fn arrears_excludes_confirmed_free_job() {
        let path = temp_db("arrears-confirmed-free");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        deliver(&store, "free-job", PaymentMode::None);
        deliver(&store, "priced-job", PaymentMode::Sat);
        assert_eq!(
            arrears_ids(&store),
            vec!["priced-job".to_owned()],
            "a confirmed free job is still not a debt"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// R2 — a LEGACY delivery (payment NULL = PRICED) WITHOUT recorded publication proof is NOT
    /// asserted owed. The old fixture only enqueued (the R1 defect); this one has NO outbox row at
    /// all, so there is no recorded proof the work reached the world — the "debt" may never have
    /// been incurred, and R2 requires we not assert it.
    #[test]
    fn arrears_excludes_legacy_unconfirmed() {
        let path = temp_db("arrears-legacy-unconfirmed");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        legacy_delivery_unconfirmed(&path, "legacy-unpublished");
        legacy_delivery(&path, "legacy-confirmed");
        assert_eq!(
            arrears_ids(&store),
            vec!["legacy-confirmed".to_owned()],
            "a legacy delivery WITHOUT publication proof is not asserted owed; only the confirmed one is"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}

