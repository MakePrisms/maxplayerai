//! Flexible ecash wallet ops for `maxplayer wallet` / MCP mirrors, over the packaged CDK wallet at
//! `home/.maxplayer/wallet`. This module owns the mint-fund path: [`begin_mint_async`] creates a mint
//! quote and returns the bolt11 invoice up front, then [`complete_mint_async`] mints once it is
//! paid. ([`crate::buyer_fund`] covers wallet open, seed derivation, and balance read.)
//!
//! **Funding assumption:** only the pinned testnut host ([`DEFAULT_MINT_URL`])
//! FakeWallet-auto-pays mint quotes. For other configured mints, [`begin_mint_async`]
//! returns the bolt11 and callers must pay it, then [`complete_mint_async`].

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use cashu::{MintUrl, Token};
use cdk::Amount;
use cdk::cdk_database::WalletDatabase;
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod, ProofsMethods};
use cdk::wallet::{KeysetFilter, ReceiveOptions, SendOptions, Wallet};
use cdk_sqlite::wallet::WalletSqliteDatabase;
use sha2::{Digest, Sha256};

use crate::buyer_fund::seed_from_secret_hex;
use crate::home::{self, DEFAULT_MINT_URL, HomeError, MaxplayerHome};

#[derive(Debug)]
pub enum WalletOpsError {
    Home(HomeError),
    /// The mint is not in this home's configured set (`accepted_mints`/`extra_mints`) — a
    /// MEMBERSHIP miss, cleared by `maxplayer wallet mints add`. `default_mint` carries the home's
    /// ACTUAL default (`config.default_mint()`) so the Display names it rather than the pinned
    /// testnut constant — on a real-minibits home the latter is a money-relevant lie (#506).
    MintNotAllowed {
        mint_url: String,
        default_mint: String,
    },
    /// The mint IS configured but is a real mint refused by the real-mint fence (issue #49):
    /// `allow_real_mints` is off. A POLICY block — `mints add` cannot clear it, so it must NOT
    /// borrow [`Self::MintNotAllowed`]'s remedy; the control is `MAXPLAYER_ALLOW_REAL_MINTS` (#465).
    RealMintDisallowed {
        mint_url: String,
    },
    /// `remove_mint` refuses to remove the home's pinned default mint. `mint_url` carries that
    /// actual default (`config.default_mint()`) so the message names the real pinned mint rather
    /// than a hardcoded constant — on a real-minibits home the constant would be a false-default
    /// lie (#579).
    MintPinnedDefault {
        mint_url: String,
    },
    /// A melt run under a [`MeltCeiling`] was REFUSED before any proof was selected, prepared or
    /// spent: the quote the mint raised at payment time would take more out of the wallet than the
    /// caller's hard maximum, or quoted a different invoice amount than the caller planned. The
    /// seller fee remittance's money hold (stage 2a, addendum 3 §1): the seller never pays more than
    /// the fee it accrued, enforced at the moment of spending. Nothing left the wallet.
    MeltExceedsCeiling {
        mint_url: String,
        quote_id: String,
        invoice_sats: u64,
        fee_reserve_sats: u64,
        planned_invoice_sats: u64,
        max_debit_sats: u64,
    },
    /// A melt under a [`MeltCeiling`] was REFUSED after `prepare_melt` and before `confirm`: the
    /// TOTAL the wallet would lose — invoice + fee reserve + the proof-input fee the mint charges on
    /// the selected proofs + the fee of the pre-melt swap the wallet would perform when its proofs do
    /// not fit — exceeds the caller's hard maximum (addendum 8 §1, verdict B4). The four figures are
    /// the SDK's own, read off the prepared melt (CDK 0.17.2 `PreparedMelt::input_fee` /
    /// `swap_fee`), not an estimate. The prepared melt was CANCELLED: its proofs are released in the
    /// local store and nothing was ever posted to the mint — `prepare_melt` only reads the mint's
    /// keysets and writes the wallet's own database (pinned `melt/saga/mod.rs:286–460`); the swap and
    /// the melt request both live inside `confirm` (`:687–697`, `:907–911`). Nothing left the wallet.
    MeltTotalExceedsCeiling {
        mint_url: String,
        quote_id: String,
        invoice_sats: u64,
        fee_reserve_sats: u64,
        input_fee_sats: u64,
        swap_fee_sats: u64,
        total_sats: u64,
        max_debit_sats: u64,
    },
    /// A melt under a [`MeltCeiling`] was REFUSED after `prepare_melt` and before `confirm` because
    /// `confirm` would NOT succeed (addendum 10 §1.1, [`ConfirmShortfall::TargetShort`]): the swap
    /// would yield `target_sats`, the SDK recomputes the input fee on those proofs as
    /// `actual_input_fee_sats` (its prepared estimate was `input_fee_sats`), and target < invoice +
    /// reserve + actual — pinned CDK refuses AFTER paying the swap fee (`melt/saga/mod.rs:704–712`).
    /// Caught here instead: the prepared melt was CANCELLED, no fee-bearing request was posted.
    MeltWouldNotConfirm {
        mint_url: String,
        quote_id: String,
        invoice_sats: u64,
        fee_reserve_sats: u64,
        input_fee_sats: u64,
        actual_input_fee_sats: u64,
        target_sats: u64,
        swap_fee_sats: u64,
        input_fee_ppk: u64,
    },
    Wallet(String),
}

impl std::fmt::Display for WalletOpsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Home(error) => write!(formatter, "{error}"),
            Self::MintNotAllowed {
                mint_url,
                default_mint,
            } => write!(
                formatter,
                "mint {mint_url} is not configured; add it with `maxplayer wallet mints add` (default stays {default_mint})"
            ),
            Self::RealMintDisallowed { mint_url } => write!(
                formatter,
                "mint {mint_url} not allowed: allow_real_mints is off (only {DEFAULT_MINT_URL} is permitted). \
                 Set MAXPLAYER_ALLOW_REAL_MINTS=true (or allow_real_mints in config.toml) to opt in, \
                 or use --mint {DEFAULT_MINT_URL} for dev/play-money"
            ),
            Self::MintPinnedDefault { mint_url } => write!(
                formatter,
                "cannot remove the default mint ({mint_url}); only extra_mints are removable"
            ),
            Self::MeltExceedsCeiling {
                mint_url,
                quote_id,
                invoice_sats,
                fee_reserve_sats,
                planned_invoice_sats,
                max_debit_sats,
            } => write!(
                formatter,
                "melt refused before spending: mint {mint_url} quote {quote_id} would debit {} sats \
                 ({invoice_sats} sats invoice + {fee_reserve_sats} sats fee reserve; planned invoice \
                 {planned_invoice_sats} sats) against a ceiling of {max_debit_sats} sats; nothing left the wallet",
                invoice_sats.saturating_add(*fee_reserve_sats)
            ),
            Self::MeltTotalExceedsCeiling {
                mint_url,
                quote_id,
                invoice_sats,
                fee_reserve_sats,
                input_fee_sats,
                swap_fee_sats,
                total_sats,
                max_debit_sats,
            } => write!(
                formatter,
                "melt refused before spending: mint {mint_url} quote {quote_id} would debit {total_sats} sats in total \
                 ({invoice_sats} sats invoice + {fee_reserve_sats} sats fee reserve + {input_fee_sats} sats proof input fee \
                 + {swap_fee_sats} sats swap fee) against a ceiling of {max_debit_sats} sats; the prepared melt was \
                 cancelled and its proofs released; nothing was posted to the mint"
            ),
            Self::MeltWouldNotConfirm {
                mint_url,
                quote_id,
                invoice_sats,
                fee_reserve_sats,
                input_fee_sats,
                actual_input_fee_sats,
                target_sats,
                swap_fee_sats,
                input_fee_ppk,
            } => write!(
                formatter,
                "melt refused before spending: the wallet would swap to {target_sats} sats ({:?}) for mint {mint_url} \
                 quote {quote_id} and the mint's actual proof input fee on those proofs is {actual_input_fee_sats} sats \
                 (prepared estimate {input_fee_sats} sats at {input_fee_ppk} ppk), so {invoice_sats} sats invoice + \
                 {fee_reserve_sats} sats fee reserve + {actual_input_fee_sats} sats would need {} sats and the SDK would \
                 refuse AFTER paying the {swap_fee_sats} sats swap fee; the prepared melt was cancelled before any \
                 fee-bearing request",
                binary_split(*target_sats),
                invoice_sats
                    .saturating_add(*fee_reserve_sats)
                    .saturating_add(*actual_input_fee_sats)
            ),
            Self::Wallet(message) => write!(formatter, "wallet error: {message}"),
        }
    }
}

impl std::error::Error for WalletOpsError {}

impl From<HomeError> for WalletOpsError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintBalance {
    pub mint_url: String,
    pub balance_sats: u64,
    pub is_default: bool,
    /// Whether the mint is in this home's configured set (default + `extra_mints`). Rows with
    /// `configured == false` are DISCOVERED — the shared wallet DB holds proofs or a registration
    /// for a mint the config no longer (or never) names. Display surfaces them (#266); accept-time
    /// source selection deliberately ignores them (see `crossmint::holds_at_least`).
    pub configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintOutcome {
    pub mint_url: String,
    pub invoice: String,
    pub quote_id: String,
    pub funded_sats: u64,
    pub balance_sats: u64,
}

/// Bolt11 mint quote ready for payment (invoice is available before any wait).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintQuote {
    pub mint_url: String,
    pub invoice: String,
    pub quote_id: String,
    pub amount_sats: u64,
}

/// Result of a mint attempt: auto-paid fund, or invoice awaiting external pay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintFlow {
    Funded(MintOutcome),
    /// Non-autopay mint: bolt11 surfaced; pay then [`complete_mint_async`].
    NeedsPayment(MintQuote),
}

#[derive(PartialEq, Eq)]
pub struct SendOutcome {
    pub mint_url: String,
    pub sent_sats: u64,
    pub balance_sats: u64,
    /// Bearer cashu token — spendable ecash. Never emitted by [`Debug`] (redacted below); read the
    /// field directly to hand the token to the payee.
    pub token: String,
}

// Manual Debug: the `token` field is a BEARER cashu token (spendable ecash). A derived Debug would
// print it verbatim, so any debug log of a `SendOutcome` would leak spendable funds. Redact it to a
// SHA-256 hash prefix (identifies the token for correlation without exposing spendable material).
// `Clone` is intentionally NOT derived: nothing needs to duplicate a bearer token, and each extra
// copy is another place it can leak.
impl std::fmt::Debug for SendOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SendOutcome")
            .field("mint_url", &self.mint_url)
            .field("sent_sats", &self.sent_sats)
            .field("balance_sats", &self.balance_sats)
            .field("token", &redact_secret(&self.token))
            .finish()
    }
}

/// Render a secret as `<redacted:sha256:HEX12>` — a stable 12-hex-char digest prefix that lets two
/// log lines be correlated to the same secret without exposing any spendable material. An empty
/// secret renders `<redacted:empty>` (no digest of nothing).
fn redact_secret(secret: &str) -> String {
    if secret.is_empty() {
        return "<redacted:empty>".to_string();
    }
    let digest = Sha256::digest(secret.as_bytes());
    format!("<redacted:sha256:{}>", &hex::encode(digest)[..12])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveOutcome {
    pub mint_url: String,
    pub received_sats: u64,
    pub balance_sats: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeltOutcome {
    pub mint_url: String,
    pub paid_sats: u64,
    /// CDK `FinalizedMelt::fee_paid` (pinned `melt/saga/mod.rs:139–148`): proofs sent − invoice −
    /// change returned = the Lightning fee the mint took PLUS the ACTUAL proof input fee on the
    /// proofs the melt sent. Inclusive; it does not contain the pre-melt swap's fee. Never add
    /// `input_fee_sats` to it — that fee is already in here, at its actual value.
    pub fee_sats: u64,
    /// Best-effort balance after the payment: the observational read when it succeeded, else
    /// `before − actual debit`. Legacy field kept for the operator paths; the seller fee remittance
    /// prints [`Self::balance_after_sats`] instead and says "unknown" rather than a computed number.
    pub balance_sats: u64,
    /// The observational post-payment balance read: `None` when the read failed (the payment still
    /// happened; the caller prints "unknown", never a computed figure).
    pub balance_after_sats: Option<u64>,
    /// The mint's melt quote id the payment settled under — journaled by the seller fee remittance
    /// so a settled row names the quote the mint can be asked about.
    pub quote_id: String,
    /// The mint's fee RESERVE on the paying quote: the ceiling on the LIGHTNING fee the mint may
    /// keep (NUT-05), checked against the caller's [`MeltCeiling`] before anything was spent.
    /// `fee_sats` (Lightning + actual proof input fee, CDK's inclusive `fee_paid`) can exceed it by
    /// that input fee; the reserve bounds the Lightning component only. Journaled beside the fee.
    pub fee_reserve_sats: u64,
    /// The proof-input fee the SDK's prepared melt carried (CDK `PreparedMelt::input_fee`): an
    /// ESTIMATE on the split of invoice + reserve (`melt/saga/mod.rs:383–387`), bounded under the
    /// ceiling before the fence. Informational after payment: `confirm` recomputes the actual fee on
    /// the swapped proofs and that actual fee is inside `fee_sats`. `0` where the path did not
    /// prepare through [`prepare_melt_payment_blocking`].
    pub input_fee_sats: u64,
    /// The fee of the pre-melt swap the wallet performed inside `confirm` because its proofs did not
    /// fit the amount (CDK `PreparedMelt::swap_fee`), charged at the swap; `0` when no swap was
    /// needed. Not part of `fee_sats`. Actual debit = `paid_sats` + `fee_sats` + `swap_fee_sats`.
    pub swap_fee_sats: u64,
}

/// NUT-02 (pinned `cdk/src/fees.rs:35–48`, reached from `wallet/mod.rs:319–352` and `:356`):
/// fee = ceil(ppk × count / 1000).
pub(crate) fn fee_for(input_fee_ppk: u64, count: usize) -> u64 {
    (input_fee_ppk * count as u64).div_ceil(1000)
}

/// The denominations a power-of-two keyset hands back for `amount` under `SplitTarget::None`
/// (CDK `Amount::split`, the split the swap uses for the melt's proofs at `swap/saga/mod.rs:
/// 285–301` and for the change) — one proof per set bit, largest first.
pub(crate) fn binary_split(amount: u64) -> Vec<u64> {
    (0..64)
        .rev()
        .map(|bit| 1u64 << bit)
        .filter(|denomination| amount & denomination != 0)
        .collect()
}

/// CDK's post-swap figures for a melt of `need` = invoice + reserve on a swap layout: the target the
/// wallet swaps to (`need` + the PREPARED input fee, `melt/saga/mod.rs:678`) and the ACTUAL input
/// fee the SDK recomputes on that target's binary split (`:704`). `prepared_input_fee_sats` is what
/// `prepare_melt` estimated (the fee on the split of `need`, `:383–387`); when `None` it is computed
/// the same way here (planning, before any quote's figures exist).
pub(crate) fn post_swap_figures(
    need_sats: u64,
    prepared_input_fee_sats: Option<u64>,
    input_fee_ppk: u64,
) -> (u64, u64) {
    let prepared = prepared_input_fee_sats
        .unwrap_or_else(|| fee_for(input_fee_ppk, binary_split(need_sats).len()));
    let target_sats = need_sats.saturating_add(prepared);
    let actual = fee_for(input_fee_ppk, binary_split(target_sats).len());
    (target_sats, actual)
}

/// What pinned CDK 0.17.2's `confirm` will act on for a melt of `invoice + reserve`, and the most it
/// can debit — the ONE arithmetic the seller fee remittance's planner, the wallet's prepared-melt
/// gate ([`MeltCeiling::admits_confirmable`]) and its pre-fence check all decide on (addendum 10
/// §1.1). Computed by [`confirm_bound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmBound {
    /// invoice + fee reserve.
    pub need_sats: u64,
    /// The SDK's PREPARED input fee: the fee on the binary split of `need` (`melt/saga/mod.rs:
    /// 383–387`) — an estimate on a swap layout; the fee on the selected proofs on an exact fit.
    pub prepared_input_fee_sats: u64,
    /// The amount the pre-melt swap yields (`need` + prepared input fee, `:678`); `need` itself when
    /// no swap is needed.
    pub target_sats: u64,
    /// The input fee `confirm` RECOMPUTES on the proofs the melt sends (`:704`): on the target's
    /// binary split after a swap, the prepared figure on an exact fit.
    pub actual_input_fee_sats: u64,
    /// The fee of the pre-melt swap, charged at the swap; `0` without one.
    pub swap_fee_sats: u64,
    /// The most that can leave the wallet: invoice + reserve + ACTUAL input fee + swap fee. The
    /// Lightning fee the mint keeps is at most the reserve, so the debit is at most this.
    pub worst_debit_sats: u64,
}

/// Why [`confirm_bound`] refused — each names the figures a refusal line prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmShortfall {
    /// The paying quote is not for the invoice the caller planned.
    DifferentInvoice {
        invoice_sats: u64,
        planned_invoice_sats: u64,
    },
    /// After the swap the target would not cover invoice + reserve + the recomputed input fee: the
    /// SDK would refuse AFTER paying the swap fee (`melt/saga/mod.rs:704–712`).
    TargetShort {
        bound: ConfirmBound,
        needed_after_swap_sats: u64,
    },
    /// `confirm` would succeed but the worst-case debit exceeds the ceiling.
    OverCeiling {
        bound: ConfirmBound,
        max_debit_sats: u64,
    },
}

/// **Addendum 10 §1.1, the actual-confirmability bound:** for `invoice + reserve` under the given
/// fee metadata, `confirm` succeeds iff on a swap layout `target ≥ need + actual_input_fee(target
/// split)`, and the payment fits iff `need + actual_input_fee + swap_fee ≤ max_debit_sats`. On an
/// exact-fit layout (`requires_swap == false`) the selected proofs already cover the prepared fee, so
/// only the debit bound applies with `actual = prepared`. `prepared_input_fee_sats` is the SDK's
/// figure when a preparation exists, `None` when planning (computed the same way, on the split of
/// `need`). Pure; the same function answers the planner (`Ok` ⇒ this invoice is confirmable), the
/// wallet's gate and the pre-fence check, so they cannot disagree on fixed metadata.
pub fn confirm_bound(
    invoice_sats: u64,
    fee_reserve_sats: u64,
    prepared_input_fee_sats: Option<u64>,
    swap_fee_sats: u64,
    input_fee_ppk: u64,
    requires_swap: bool,
    max_debit_sats: u64,
) -> Result<ConfirmBound, ConfirmShortfall> {
    let need_sats = invoice_sats.saturating_add(fee_reserve_sats);
    let (prepared_input_fee_sats, target_sats, actual_input_fee_sats) = if requires_swap {
        let (target_sats, actual) =
            post_swap_figures(need_sats, prepared_input_fee_sats, input_fee_ppk);
        (target_sats.saturating_sub(need_sats), target_sats, actual)
    } else {
        let prepared = prepared_input_fee_sats.unwrap_or(0);
        (prepared, need_sats, prepared)
    };
    let worst_debit_sats = need_sats
        .saturating_add(actual_input_fee_sats)
        .saturating_add(swap_fee_sats);
    let bound = ConfirmBound {
        need_sats,
        prepared_input_fee_sats,
        target_sats,
        actual_input_fee_sats,
        swap_fee_sats,
        worst_debit_sats,
    };
    let needed_after_swap_sats = need_sats.saturating_add(actual_input_fee_sats);
    if requires_swap && target_sats < needed_after_swap_sats {
        return Err(ConfirmShortfall::TargetShort {
            bound,
            needed_after_swap_sats,
        });
    }
    if worst_debit_sats > max_debit_sats {
        return Err(ConfirmShortfall::OverCeiling {
            bound,
            max_debit_sats,
        });
    }
    Ok(bound)
}

/// A hard bound a caller places on a melt. Two checks share it: [`Self::admits`] bounds the two
/// figures a QUOTE carries (invoice + fee reserve) and is taken before any proof is selected — the
/// operator's [`melt_within_async`] takes only this one (its reserve-only ceiling is kept as is,
/// addendum 8 §6); [`Self::admits_confirmable`] is the actual-confirmability bound on a PREPARED
/// melt (invoice + fee reserve + the input fee the SDK will RECOMPUTE on the proofs it sends + its
/// pre-melt swap fee, addendum 10 §1.1) and is taken between `prepare_melt` and `confirm` by
/// [`prepare_melt_payment_blocking`] — the seller fee remittance's path, whose ceiling therefore
/// bounds the ENTIRE wallet debit (addendum 8 §1, verdict B4). The remittance's money hold (stage
/// 2a, addendum 3 §1): the plan's estimate is not the quote the spend runs under — the mint quotes
/// again when the payment is made, and its fee reserve can differ — so the ceiling is enforced at
/// the moment of spending, not estimated beforehand or regretted afterwards. A quote that does not
/// fit is refused as [`WalletOpsError::MeltExceedsCeiling`], a clean failure with nothing moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeltCeiling {
    /// The most that may leave the wallet for this melt — invoice amount and fee reserve together.
    /// For the remittance this is the unremitted accrued gross being discharged.
    pub max_debit_sats: u64,
    /// The invoice amount the caller planned; the paying quote must quote exactly this.
    pub invoice_sats: u64,
    /// The melt quote the plan was journaled against, for the record. The spend re-validates the
    /// quote it actually pays under and reports it in [`MeltOutcome::quote_id`].
    pub planned_quote_id: Option<String>,
}

impl MeltCeiling {
    /// Whether a quote of `invoice_sats` with `fee_reserve_sats` fits under this ceiling. Pure, so
    /// the bound is unit-tested without a mint.
    pub fn admits(&self, invoice_sats: u64, fee_reserve_sats: u64) -> bool {
        invoice_sats == self.invoice_sats
            && invoice_sats.saturating_add(fee_reserve_sats) <= self.max_debit_sats
    }

    /// **The one gate on a PREPARED melt (addendum 10 §1.1).** The paying quote must be for the
    /// planned invoice, and [`confirm_bound`] must hold for the SDK's prepared figures under
    /// `max_debit_sats`: `confirm` will succeed and its worst-case debit (invoice + reserve + the
    /// input fee it RECOMPUTES on the proofs it sends + swap fee) fits. Replaces round 8's bound on
    /// the PREPARED total — an estimate that can exceed the final debit and refused fitting
    /// remittances (verdict 4714623 §3.2). Pure, so unit-tested without a mint.
    pub fn admits_confirmable(
        &self,
        invoice_sats: u64,
        fee_reserve_sats: u64,
        prepared_input_fee_sats: Option<u64>,
        swap_fee_sats: u64,
        input_fee_ppk: u64,
        requires_swap: bool,
    ) -> Result<ConfirmBound, ConfirmShortfall> {
        if invoice_sats != self.invoice_sats {
            return Err(ConfirmShortfall::DifferentInvoice {
                invoice_sats,
                planned_invoice_sats: self.invoice_sats,
            });
        }
        confirm_bound(
            invoice_sats,
            fee_reserve_sats,
            prepared_input_fee_sats,
            swap_fee_sats,
            input_fee_ppk,
            requires_swap,
            self.max_debit_sats,
        )
    }

    /// The four parts summed, saturating — the figure a refusal names, with the input fee at the
    /// value the caller has (the ACTUAL one where [`confirm_bound`] computed it).
    pub fn total_debit(
        invoice_sats: u64,
        fee_reserve_sats: u64,
        input_fee_sats: u64,
        swap_fee_sats: u64,
    ) -> u64 {
        invoice_sats
            .saturating_add(fee_reserve_sats)
            .saturating_add(input_fee_sats)
            .saturating_add(swap_fee_sats)
    }
}

/// A melt quote and nothing more: what the mint would charge to pay `bolt11`, read without paying
/// it. The dry-run half of the seller fee remittance; see [`melt_quote_async`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeltEstimate {
    pub mint_url: String,
    pub quote_id: String,
    /// The invoice amount the mint quoted, in sats.
    pub amount_sats: u64,
    /// The mint's fee RESERVE for this melt: the ceiling on the LIGHTNING fee it may keep (NUT-05);
    /// the unused part returns as change. It does not bound the proof input fee: the inclusive
    /// melt fee (Lightning + actual proof input fee, CDK's `fee_paid`) can exceed the reserve by
    /// that input fee, which [`confirm_bound`] accounts for separately.
    pub fee_reserve_sats: u64,
    /// When the quote expires at the mint (unix seconds), as the quote states it. A quote is paid
    /// by id ([`prepare_melt_payment_blocking`] → confirm; the retained [`pay_melt_quote_async`]
    /// likewise) only while it is live; the seller fee remittance binds its
    /// admission to this quote and refuses to pay it inside its spending margin of expiry
    /// (addendum 5 §1, rule 1).
    pub expiry_unix: u64,
    /// The proof fees the SDK would charge on top of amount + reserve if THIS wallet paid this quote
    /// now, computed from the wallet's own unspent proofs and the mint's keyset `input_fee_ppk` the
    /// way `prepare_melt` computes them (addendum 8 §1.3) — the proof-input fee on an exact-fit
    /// selection, or the estimated input fee plus the swap fee on a layout that needs a pre-melt
    /// swap — WITHOUT reserving anything. Sizes the plan so a payment that can fit is planned and one
    /// that never can is refused at planning; the hard bound is still taken on the prepared melt's
    /// own figures at payment ([`prepare_melt_payment_blocking`]). `0` when the estimate could not
    /// be made (e.g. the wallet cannot cover amount + reserve at estimate time); the reason is in
    /// [`Self::expected_fees_note`].
    pub expected_fees_sats: u64,
    /// The part of `expected_fees_sats` that is the pre-melt SWAP's fee (`0` on an exact-fit
    /// layout); the rest is the SDK's ESTIMATED melt-input fee. Planning needs them apart: the
    /// input fee is recomputed by `confirm` on the swapped proofs, the swap fee is not.
    pub expected_swap_fee_sats: u64,
    /// The active keyset's `input_fee_ppk` (NUT-02), so the planner can run the SDK's post-swap
    /// input-fee recomputation ahead of time (addendum 9 §1.2). `0` when it could not be read; the
    /// reason is in [`Self::expected_fees_note`].
    pub input_fee_ppk: u64,
    /// Why `expected_fees_sats` is `0` by default rather than measured, when it is; `None` when the
    /// estimate was made.
    pub expected_fees_note: Option<String>,
}

/// The SDK's figures for ONE prepared melt — read off CDK 0.17.2's `PreparedMelt` after
/// `prepare_melt` selected and reserved proofs in the LOCAL store and before `confirm` performs any
/// swap or posts the melt request. The bound in [`MeltCeiling::admits_confirmable`] is taken on these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeltPreparation {
    pub mint_url: String,
    pub quote_id: String,
    pub invoice_sats: u64,
    pub fee_reserve_sats: u64,
    /// `PreparedMelt::input_fee` — on the swap layout this is the SDK's estimate for the proofs the
    /// swap will yield (pinned `melt/saga/mod.rs:384–399`); on the exact-fit layout it is the fee on
    /// the selected proofs (`:359`).
    pub input_fee_sats: u64,
    /// `PreparedMelt::swap_fee` — the fee on the proofs the pre-melt swap consumes; `0` when no swap.
    pub swap_fee_sats: u64,
    /// `PreparedMelt::requires_swap` — whether `confirm` will perform a pre-melt swap first.
    pub requires_swap: bool,
    /// The active keyset's `input_fee_ppk` (NUT-02) at preparation — what `confirm` will charge per
    /// proof when it RECOMPUTES the input fee on the swapped proofs (pinned `melt/saga/mod.rs:704`);
    /// read as the fee on 1000 proofs, which is exactly ppk (`fees.rs:35–48`).
    pub input_fee_ppk: u64,
    /// The four parts summed (saturating) — what `admits_confirmable` compared against the ceiling: invoice + reserve + ACTUAL input fee + swap fee.
    pub total_debit_sats: u64,
    pub expiry_unix: u64,
}

enum PreparedCommand {
    Confirm,
    Cancel,
}

/// A melt that is PREPARED — proofs selected and reserved in the wallet's own database, fees known,
/// nothing posted to the mint — and waits for the caller to [`Self::confirm`] or [`Self::cancel`].
/// Returned by [`prepare_melt_payment_blocking`] after the caller's ceiling admitted the total. The
/// seller fee remittance holds one across its store fence (addendum 8 §1.2: bound → fence →
/// confirm), so that a fee refusal happens before the row is ever bound and a fence refusal cancels
/// a melt that has cost nothing.
///
/// Lives on a thread of its own: CDK's `PreparedMelt<'a>` borrows the `Wallet`, and every wallet
/// call here runs on a fresh current-thread Tokio runtime, so a dedicated OS thread owns the
/// runtime, the wallet and the prepared melt together and waits on a channel for the verdict.
/// Dropping this without a verdict CANCELS (the thread sees the channel close and runs
/// `PreparedMelt::cancel`, which reverts the reservation and releases the quote locally — pinned
/// `melt/saga/mod.rs:817–831`).
pub struct PreparedMeltPayment {
    pub preparation: MeltPreparation,
    command: Option<std::sync::mpsc::Sender<PreparedCommand>>,
    reply: std::sync::mpsc::Receiver<Result<Option<MeltOutcome>, WalletOpsError>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for PreparedMeltPayment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedMeltPayment")
            .field("preparation", &self.preparation)
            .field("decided", &self.command.is_none())
            .finish()
    }
}

impl PreparedMeltPayment {
    /// **The payment.** `PreparedMelt::confirm` on the thread that holds it: the pre-melt swap if
    /// one is required, then the melt request; funds leave the wallet here and nowhere else on this
    /// path. An `Err` is opaque as to how far it got — on the failure paths it recognises the SDK
    /// runs its own compensations (best-effort, local; e.g. pinned `melt/saga/mod.rs:709–712` after
    /// an insufficient post-swap total) and the caller reconciles by quote id.
    pub fn confirm(mut self) -> Result<MeltOutcome, WalletOpsError> {
        self.decide(PreparedCommand::Confirm)?.ok_or_else(|| {
            WalletOpsError::Wallet(
                "the prepared melt's thread reported no outcome for a confirm; reconcile the quote by id"
                    .to_owned(),
            )
        })
    }

    /// Release the prepared melt: the SDK's compensations — proofs back to Unspent, quote released,
    /// saga row deleted — in the wallet's own database. **Best-effort**: pinned CDK `melt/mod.rs:673`
    /// → `melt/saga/mod.rs:828–830` catches and logs a compensation's own DB error (`:817–824`) and
    /// still returns `Ok`, so `Ok` means "no fee-bearing request was ever posted, so there is
    /// nothing to undo at the mint", not "every local reservation is proven released". The
    /// Drop → Cancel → join below is this wrapper's, not SDK RAII: a bare `PreparedMelt` dropped
    /// without a decision cancels nothing.
    pub fn cancel(mut self) -> Result<(), WalletOpsError> {
        self.decide(PreparedCommand::Cancel).map(|_| ())
    }

    fn decide(&mut self, command: PreparedCommand) -> Result<Option<MeltOutcome>, WalletOpsError> {
        let Some(sender) = self.command.take() else {
            return Err(WalletOpsError::Wallet(
                "the prepared melt was already decided".to_owned(),
            ));
        };
        let what = match command {
            PreparedCommand::Confirm => "confirm",
            PreparedCommand::Cancel => "cancel",
        };
        sender.send(command).map_err(|_| {
            WalletOpsError::Wallet(format!(
                "the prepared melt's thread is gone before the {what}; no fee-bearing request was posted by this call; a local proof reservation may remain — opening the wallet does not run CDK recover_incomplete_sagas on this path; a supported recovery path is owed"
            ))
        })?;
        let reply = self.reply.recv().map_err(|_| {
            WalletOpsError::Wallet(format!(
                "the prepared melt's thread ended without reporting the {what}; reconcile the quote by id"
            ))
        });
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        reply?
    }
}

impl Drop for PreparedMeltPayment {
    fn drop(&mut self) {
        if let Some(sender) = self.command.take() {
            // Undecided: cancel. A send failure means the thread is already gone (it cancels on a
            // closed channel too); either way nothing was posted.
            let _ = sender.send(PreparedCommand::Cancel);
            let _ = self.reply.recv();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

/// The mint's melt-quote lifecycle, re-exported so a CLI caller can match on it without depending
/// on `cdk` directly.
pub use cdk::nuts::MeltQuoteState;

/// The mint's answer about a melt quote raised earlier for a given invoice. Used to reconcile an
/// interrupted remittance without paying again; see [`melt_status_for_invoice_async`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeltQuoteStatus {
    pub mint_url: String,
    pub quote_id: String,
    pub state: MeltQuoteState,
    pub amount_sats: u64,
    pub fee_reserve_sats: u64,
    /// When the quote expires at the mint (unix seconds), as the quote itself states. An UNPAID
    /// quote past this is one the mint will never pay — terminal, like FAILED.
    pub expiry_unix: u64,
}

impl MeltQuoteStatus {
    /// Whether the quote's expiry is behind `now_unix` (a clock before the epoch never expires
    /// anything: fail-closed toward "still live").
    pub fn expired_at(&self, now_unix: i64) -> bool {
        u64::try_from(now_unix).is_ok_and(|now| now > self.expiry_unix)
    }
}

fn sqlite_path(wallet_dir: &Path) -> std::path::PathBuf {
    wallet_dir.join("cdk-wallet.sqlite")
}

/// Normalize a mint URL (trim, strip trailing `/`, parse as [`MintUrl`]).
pub fn normalize_mint_url(raw: &str) -> Result<String, WalletOpsError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(WalletOpsError::Wallet("mint URL is empty".into()));
    }
    let parsed = MintUrl::from_str(trimmed)
        .map_err(|error| WalletOpsError::Wallet(format!("invalid mint URL: {error}")))?;
    Ok(parsed.to_string())
}

fn is_autopay_mint(mint_url: &str) -> bool {
    normalize_mint_url(mint_url).ok().as_deref() == Some(DEFAULT_MINT_URL)
}

/// Money class a mint moves, derived purely from the mint URL. The pinned testnut host
/// ([`DEFAULT_MINT_URL`]) FakeWallet-auto-pays its own invoices — play money — while every other
/// mint invoices for real sats. Internal: it gates the #445 fail-closed refusal of silently
/// auto-funding play money and drives a play-money marker on dev rows. Ordinary mints carry no
/// money-class label in user output — a mint is a mint, identified by its URL (#577).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoneyType {
    /// A real mint — its invoices move real sats.
    Real,
    /// The testnut dev/play mint — auto-pays its own invoices with fake sats.
    Play,
}

impl MoneyType {
    /// Classify a mint URL. A URL that does not normalize is treated as [`Self::Real`] — the
    /// fail-safe direction, so an unrecognized mint is never mislabeled play money.
    pub fn of_mint(mint_url: &str) -> Self {
        if is_autopay_mint(mint_url) {
            Self::Play
        } else {
            Self::Real
        }
    }
}

/// Configured mints: default `mint_url` first, then opt-in `extra_mints` (deduped).
pub fn configured_mints(home: &MaxplayerHome) -> Result<Vec<String>, WalletOpsError> {
    let mut out = Vec::new();
    let default = normalize_mint_url(home.config.default_mint())?;
    out.push(default.clone());
    for extra in &home.config.extra_mints {
        let normalized = normalize_mint_url(extra)?;
        if !out.iter().any(|existing| existing == &normalized) {
            out.push(normalized);
        }
    }
    Ok(out)
}

fn mint_is_allowed(home: &MaxplayerHome, mint_url: &str) -> Result<String, WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let allowed = configured_mints(home)?;
    if allowed.iter().any(|entry| entry == &normalized) {
        Ok(normalized)
    } else {
        Err(WalletOpsError::MintNotAllowed {
            mint_url: normalized,
            default_mint: home.config.default_mint().to_string(),
        })
    }
}

/// Resolve the reported post-confirm balance from a balance-read result (finding U). A cashu
/// `confirm` is the effect boundary — the ecash has already moved — so a read failure, or a
/// stale/equal balance, must NEVER make the caller discard the confirmed token/outcome: report the
/// read balance when available, otherwise a best-effort `before - spent` estimate (the authoritative
/// record is the returned token / paid+fee). `op` is `"send"`/`"melt"` for the diagnostic. Pure so
/// "a read failure still yields the outcome" is unit-testable without a mint.
fn post_confirm_balance(read: Result<u64, String>, before: u64, spent_sats: u64, op: &str) -> u64 {
    match read {
        Ok(balance) => {
            if balance >= before {
                eprintln!(
                    "wallet {op} WARN: post-confirm balance did not decrease (before={before} \
                     after={balance}); returning the confirmed outcome anyway ({op} already happened)"
                );
            }
            balance
        }
        Err(error) => {
            eprintln!(
                "wallet {op} WARN: post-confirm balance read failed (returning the confirmed outcome \
                 anyway; {op} already happened): {error}"
            );
            before.saturating_sub(spent_sats)
        }
    }
}

/// Resolve the reported post-receive balance from a balance-read result (finding X, sibling of
/// finding U). A successful `receive` is the effect boundary — the token's proofs are already
/// redeemed into the wallet — so a read failure, or a stale/non-increasing balance, must NEVER make
/// the caller discard the credited outcome (a discarded outcome retries into an already-spent
/// token): report the read balance when available, otherwise a best-effort `before + received`
/// estimate. Pure so "a read failure still yields the outcome" is unit-testable without a mint.
fn post_receive_balance(read: Result<u64, String>, before: u64, received_sats: u64) -> u64 {
    match read {
        Ok(balance) => {
            if balance <= before {
                eprintln!(
                    "wallet receive WARN: post-receive balance did not increase (before={before} \
                     after={balance}); returning the credited outcome anyway (receive already happened)"
                );
            }
            balance
        }
        Err(error) => {
            eprintln!(
                "wallet receive WARN: post-receive balance read failed (returning the credited \
                 outcome anyway; receive already happened): {error}"
            );
            before.saturating_add(received_sats)
        }
    }
}

fn resolve_mint(
    home: &MaxplayerHome,
    mint_override: Option<&str>,
) -> Result<String, WalletOpsError> {
    match mint_override {
        Some(url) => mint_is_allowed(home, url),
        None => normalize_mint_url(home.config.default_mint()),
    }
}

/// Open the packaged CDK wallet for one allowed mint (shared sqlite + seed).
pub async fn open_wallet_async(
    home: &MaxplayerHome,
    mint_url: &str,
) -> Result<Wallet, WalletOpsError> {
    let mint_url = mint_is_allowed(home, mint_url)?;
    let secret = home::read_secret_key_hex(home)?;
    let seed =
        seed_from_secret_hex(&secret).map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let path = sqlite_path(&home.wallet_dir);
    let store = WalletSqliteDatabase::new(path)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    crate::nostr_mint::build_wallet(
        mint_url.as_str(),
        CurrencyUnit::Sat,
        Arc::new(store),
        seed,
        None,
        &home.config.relay_url,
    )
    .map_err(|error| WalletOpsError::Wallet(error.to_string()))
}

/// Wait for a mint quote to be paid, then issue it. Refuses a phantom credit (nothing issued) and an
/// issue that does not equal what was quoted.
pub(crate) async fn poll_and_mint(
    wallet: &Wallet,
    quote_id: &str,
    expected_sats: u64,
) -> Result<u64, WalletOpsError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let status = wallet
            .check_mint_quote(quote_id)
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
        match status.state {
            MintQuoteState::Paid | MintQuoteState::Issued => break,
            MintQuoteState::Unpaid => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(WalletOpsError::Wallet(format!(
                        "timed out waiting for mint quote {quote_id} to become paid (refusing phantom credit)"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let proofs = wallet
        .mint(quote_id, Default::default(), None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let funded = proofs
        .iter()
        .map(|proof| proof.amount.to_u64())
        .fold(0u64, |acc, value| acc.saturating_add(value));
    if funded == 0 {
        return Err(WalletOpsError::Wallet(
            "mint completed but funded amount is 0 (refusing phantom credit)".into(),
        ));
    }
    // Exact mint proofs == requested (no invented fee delta / under-over fund).
    if funded != expected_sats {
        return Err(WalletOpsError::Wallet(format!(
            "mint funded amount {funded} != requested {expected_sats} (refusing under/over fund)"
        )));
    }
    Ok(funded)
}

/// Open the shared wallet database for seedless, offline balance discovery.
async fn open_balance_store(home: &MaxplayerHome) -> Result<WalletSqliteDatabase, WalletOpsError> {
    WalletSqliteDatabase::new(sqlite_path(&home.wallet_dir))
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))
}

/// Balance per configured or wallet-database-discovered mint. The sqlite store is shared across
/// every mint, and proofs legally land at mints outside the configured set (seller redemption at
/// `accepted_mints[1..]`, cross-mint hop residue) — so the read enumerates the DB truth (proof
/// table ∪ mint registrations ∪ configured set) rather than the config filter, and tags each row
/// `configured` so callers can tell the sets apart (#266). One store open, no per-mint `Wallet`,
/// no seed, no network, and no `mint_is_allowed` fence — that fence stays load-bearing on the
/// funding paths only.
pub async fn balances_async(home: &MaxplayerHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    let default = normalize_mint_url(home.config.default_mint())?;
    let configured = configured_mints(home)?;
    let store = open_balance_store(home).await?;
    let proofs = store
        .get_proofs(
            None,
            Some(CurrencyUnit::Sat),
            Some(vec![cashu::State::Unspent]),
            None,
        )
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let registered = store
        .get_mints()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;

    let mut discovered = BTreeSet::new();
    for proof in proofs {
        discovered.insert(normalize_mint_url(&proof.mint_url.to_string())?);
    }
    for mint_url in registered.keys() {
        discovered.insert(normalize_mint_url(&mint_url.to_string())?);
    }

    let mut mint_urls = configured.clone();
    for mint_url in discovered {
        if !mint_urls.iter().any(|entry| entry == &mint_url) {
            mint_urls.push(mint_url);
        }
    }

    let mut rows = Vec::new();
    for mint_url in mint_urls {
        let balance = store
            .get_balance(
                Some(MintUrl::from_str(&mint_url).map_err(|error| {
                    WalletOpsError::Wallet(format!("invalid normalized mint URL: {error}"))
                })?),
                Some(CurrencyUnit::Sat),
                Some(vec![cashu::State::Unspent]),
            )
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
        rows.push(MintBalance {
            is_default: mint_url == default,
            configured: configured.iter().any(|entry| entry == &mint_url),
            mint_url,
            balance_sats: balance,
        });
    }
    Ok(rows)
}

/// Create a mint quote and return the bolt11 **before** any poll/wait.
pub async fn begin_mint_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintQuote, WalletOpsError> {
    if amount_sats == 0 {
        return Err(WalletOpsError::Wallet("amount must be > 0".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let amount = Amount::from(amount_sats);
    let quote = wallet
        .mint_quote(PaymentMethod::BOLT11, Some(amount), None, None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let invoice = quote.request.clone();
    if invoice.is_empty() {
        return Err(WalletOpsError::Wallet(
            "mint quote returned empty bolt11 (refusing silent fund path)".into(),
        ));
    }
    Ok(MintQuote {
        mint_url,
        invoice,
        quote_id: quote.id,
        amount_sats,
    })
}

/// Poll + mint a previously created quote. Refuses when proof total ≠ requested.
pub async fn complete_mint_async(
    home: &MaxplayerHome,
    quote: &MintQuote,
) -> Result<MintOutcome, WalletOpsError> {
    let mint_url = mint_is_allowed(home, &quote.mint_url)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let funded = poll_and_mint(&wallet, &quote.quote_id, quote.amount_sats).await?;
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    Ok(MintOutcome {
        mint_url,
        invoice: quote.invoice.clone(),
        quote_id: quote.quote_id.clone(),
        funded_sats: funded,
        balance_sats: balance,
    })
}

/// Look up a mint quote persisted in the shared CDK localstore.
///
/// The wallet sqlite is shared across every configured mint, so any opened
/// wallet's localstore sees all stored quotes. Returns `None` when the quote id
/// is unknown locally, or when the stored quote has no fixed amount (e.g.
/// variable-amount methods that cannot be completed from the id alone). Lets
/// [`complete_mint_by_id_async`] recover mint/amount/invoice from the id.
pub async fn lookup_pending_quote_async(
    home: &MaxplayerHome,
    quote_id: &str,
) -> Result<Option<MintQuote>, WalletOpsError> {
    let quote_id = quote_id.trim();
    if quote_id.is_empty() {
        return Err(WalletOpsError::Wallet("quote_id is empty".into()));
    }
    let default_mint = normalize_mint_url(home.config.default_mint())?;
    let wallet = open_wallet_async(home, &default_mint).await?;
    let stored = wallet
        .localstore
        .get_mint_quote(quote_id)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let Some(amount) = stored.amount else {
        return Ok(None);
    };
    Ok(Some(MintQuote {
        mint_url: stored.mint_url.to_string(),
        invoice: stored.request,
        quote_id: stored.id,
        amount_sats: amount.to_u64(),
    }))
}

/// Complete a paid mint quote identified only by its `quote_id`.
///
/// Recovers mint/amount/invoice from the shared CDK localstore when the quote is
/// known there (so `amount_override`/`mint_override` may be omitted). Otherwise
/// the caller must supply `amount_override` (and, optionally, `mint_override`)
/// to reconstruct the quote — the underlying cdk `mint()` still requires the
/// quote (and its NUT-20 signing key) to already live in this wallet's store, so
/// a quote this wallet never created cannot be completed here.
///
/// When both a stored value and an override are present they must agree; a
/// mismatch is refused rather than guessed, keeping the funded total exactly
/// what was quoted.
pub async fn complete_mint_by_id_async(
    home: &MaxplayerHome,
    quote_id: &str,
    amount_override: Option<u64>,
    mint_override: Option<&str>,
) -> Result<MintOutcome, WalletOpsError> {
    let quote_id = quote_id.trim();
    if quote_id.is_empty() {
        return Err(WalletOpsError::Wallet("quote_id is empty".into()));
    }
    let quote = match lookup_pending_quote_async(home, quote_id).await? {
        Some(stored) => {
            if let Some(amount) = amount_override {
                if amount != stored.amount_sats {
                    return Err(WalletOpsError::Wallet(format!(
                        "amount {amount} != stored quote amount {} for quote {quote_id} (refusing mismatched completion)",
                        stored.amount_sats
                    )));
                }
            }
            if let Some(mint) = mint_override {
                let requested = normalize_mint_url(mint)?;
                let stored_mint = normalize_mint_url(&stored.mint_url)?;
                if requested != stored_mint {
                    return Err(WalletOpsError::Wallet(format!(
                        "mint {requested} != stored quote mint {stored_mint} for quote {quote_id} (refusing mismatched completion)"
                    )));
                }
            }
            stored
        }
        None => {
            let amount_sats = amount_override.ok_or_else(|| {
                WalletOpsError::Wallet(format!(
                    "quote {quote_id} has no stored amount; pass --amount to complete it"
                ))
            })?;
            if amount_sats == 0 {
                return Err(WalletOpsError::Wallet("amount must be > 0".into()));
            }
            let mint_url = resolve_mint(home, mint_override)?;
            MintQuote {
                mint_url,
                invoice: String::new(),
                quote_id: quote_id.to_owned(),
                amount_sats,
            }
        }
    };
    complete_mint_async(home, &quote).await
}

/// Flexible/repeatable mint-fund (no `already_funded` hard-block).
///
/// Testnut ([`DEFAULT_MINT_URL`]) FakeWallet-auto-pays: begin → complete.
/// Other configured mints return [`MintFlow::NeedsPayment`] with bolt11 already
/// surfaced (caller pays, then [`complete_mint_async`]).
pub async fn mint_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    let quote = begin_mint_async(home, amount_sats, mint_override).await?;
    if is_autopay_mint(&quote.mint_url) {
        Ok(MintFlow::Funded(complete_mint_async(home, &quote).await?))
    } else {
        Ok(MintFlow::NeedsPayment(quote))
    }
}

/// Create a bolt11 invoice; on testnut, mint once FakeWallet auto-pays.
/// Non-autopay mints return [`MintFlow::NeedsPayment`] (invoice before any wait).
pub async fn invoice_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    mint_async(home, amount_sats, mint_override).await
}

/// Create/print an unlocked cashu token (ecash out).
pub async fn send_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<SendOutcome, WalletOpsError> {
    if amount_sats == 0 {
        return Err(WalletOpsError::Wallet("amount must be > 0".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    // Fail closed against the real-mint gate before opening the wallet. Operator sends are a
    // deliberate action OUTSIDE the job-pay budget gate (BudgetGate is deliberately not wired in
    // here — owner decision pending), but they must still honor `allow_real_mints`.
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    let wallet = open_wallet_async(home, &mint_url).await?;
    let before = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    if before < amount_sats {
        return Err(WalletOpsError::Wallet(format!(
            "insufficient funds: balance={before} need={amount_sats}"
        )));
    }
    let prepared = wallet
        .prepare_send(Amount::from(amount_sats), SendOptions::default())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    // `confirm` is the effect boundary: it consumes the input proofs and mints the outgoing token, so
    // past this point the ecash has left the spendable balance and the caller MUST receive the token.
    // The post-confirm balance read is observational; a read failure must never discard the token
    // (finding U — see `post_confirm_balance`).
    let token = prepared
        .confirm(None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let read = wallet
        .total_balance()
        .await
        .map(|balance| balance.to_u64())
        .map_err(|error| error.to_string());
    let balance = post_confirm_balance(read, before, amount_sats, "send");
    Ok(SendOutcome {
        mint_url,
        sent_sats: amount_sats,
        balance_sats: balance,
        token: token.to_string(),
    })
}

/// Redeem a cashu token (ecash in). Mint must already be configured.
pub async fn receive_async(
    home: &MaxplayerHome,
    token: &str,
) -> Result<ReceiveOutcome, WalletOpsError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(WalletOpsError::Wallet("token is empty".into()));
    }
    let parsed = Token::from_str(token)
        .map_err(|error| WalletOpsError::Wallet(format!("invalid cashu token: {error}")))?;
    let mint_url = parsed
        .mint_url()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_string();
    let mint_url = mint_is_allowed(home, &mint_url)?;
    // Real-mint fence (issue #49): `mint_is_allowed` only checks the mint is in the CONFIGURED list;
    // this additionally fails closed on a real mint unless the operator opted in, the same gate
    // send/melt enforce. Without it a real mint left in the configured list would redeem while
    // `allow_real_mints == false`.
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    let wallet = open_wallet_async(home, &mint_url).await?;
    let before = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    let received = wallet
        .receive(token, ReceiveOptions::default())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let received_sats = received.to_u64();
    if received_sats == 0 {
        return Err(WalletOpsError::Wallet(
            "receive credited 0 sats (refusing phantom credit)".into(),
        ));
    }
    // `receive` is the effect boundary: the token's proofs are already redeemed, so the post-receive
    // balance read is observational and must NEVER discard the credited outcome (finding X). A read
    // failure or a stale/non-increasing balance yields a best-effort figure via `post_receive_balance`;
    // the authoritative record is `received_sats`.
    let read = wallet
        .total_balance()
        .await
        .map(|balance| balance.to_u64())
        .map_err(|error| error.to_string());
    let balance = post_receive_balance(read, before, received_sats);
    Ok(ReceiveOutcome {
        mint_url,
        received_sats,
        balance_sats: balance,
    })
}

/// Pay a lightning invoice from ecash (fail-closed on insufficient / unpaid).
/// `confirm` is the effect boundary; the post-confirm balance read is observational and never
/// discards the settled outcome (finding U — see [`post_confirm_balance`]).
///
/// The unbounded form `maxplayer wallet melt` uses: [`melt_within_async`] with no ceiling. There is
/// one melt implementation; this is a name for calling it without a bound.
pub async fn melt_async(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<MeltOutcome, WalletOpsError> {
    melt_within_async(home, bolt11, mint_override, None).await
}

/// [`melt_async`] with an optional hard [`MeltCeiling`], checked against the quote the mint raises
/// here — at payment time — and BEFORE `prepare_melt` selects a single proof: quote, then the same
/// pay step [`pay_melt_quote_async`] uses. The operator's `maxplayer wallet melt` composes it. The
/// seller fee remittance does NOT pay through this (it did in stage 2a rounds 2–3): since
/// addendum 5 it raises its payment quote with [`melt_quote_async`] and binds the id in the store,
/// and since addendum 8 it prepares and confirms that quote with [`prepare_melt_payment_blocking`].
/// A fee reserve that does not fit the ceiling is refused as [`WalletOpsError::MeltExceedsCeiling`]
/// and nothing leaves the wallet. **This operator path keeps its RESERVE-ONLY ceiling** (addendum 8
/// §6, out of scope): it does not take the total bound on the prepared melt's proof-input and swap
/// fees — those are reported in the [`MeltOutcome`], not bounded. Same mint resolution, same
/// `allow_real_mints` gate, same effect boundary as the unbounded form.
pub async fn melt_within_async(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
    ceiling: Option<&MeltCeiling>,
) -> Result<MeltOutcome, WalletOpsError> {
    let bolt11 = bolt11.trim();
    if bolt11.is_empty() {
        return Err(WalletOpsError::Wallet("bolt11 invoice is empty".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    // Fail closed against the real-mint gate before opening the wallet. Operator melts are a
    // deliberate action OUTSIDE the job-pay budget gate (BudgetGate is deliberately not wired in
    // here — owner decision pending), but they must still honor `allow_real_mints`.
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    let wallet = open_wallet_async(home, &mint_url).await?;
    // The quote step: raise the payment quote (spends nothing) …
    let quote = wallet
        .melt_quote(PaymentMethod::BOLT11, bolt11, None, None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    // … then the pay step, on THAT quote by id, under the ceiling. The two steps are one call here
    // (the operator's melt has nothing to fence between them); the seller fee remittance calls them
    // separately — [`melt_quote_async`], then [`prepare_melt_payment_blocking`] →
    // [`PreparedMeltPayment::confirm`] (since addendum 8; [`pay_melt_quote_async`] is retained but
    // no longer called on that path, removal owed) — with its store fence in between, so that it
    // only ever pays the quote its admission bound (addendum 5 §1, rule 1).
    pay_quote_on_wallet(&wallet, mint_url, &quote, ceiling).await
}

/// **Pay a melt quote the wallet already holds, by id, and never raise another.** The seller fee
/// remittance's spending call in stage 2a rounds 4–6 (addendum 5 §1, rule 1); since addendum 8 the
/// remittance uses the two-phase [`prepare_melt_payment_blocking`] instead, so that the ceiling is
/// taken on the prepared melt's TOTAL before the fence — this one-shot form bounds invoice + fee
/// reserve only and is kept for [`melt_within_async`]. The quote was raised by [`melt_quote_async`]
/// AFTER the plan was journaled (the estimate quote predates the plan; this payment quote does
/// not), its id was bound to the row by the store fence, and this pays exactly that quote —
/// `prepare_melt(quote_id)` / `confirm` — after re-checking the ceiling against the quote's STORED
/// amount and fee reserve immediately before `prepare_melt`. An unknown quote id is refused before
/// the wallet touches a proof. `prepare_melt` refuses a quote whose expiry has passed on THIS
/// wallet's clock; past that check nothing here re-checks expiry or state, and the inspected CDK
/// 0.17.2 mint implementation (checksum-pinned source; no deployed mint was measured) pays an
/// UNPAID or FAILED quote regardless of its expiry — which is why the caller never treats
/// "expired" or "FAILED" as proof that a prepared payment cannot still land, never re-quotes, and
/// holds its row until the mint reports PAID (addendum 6 §1.2).
/// Same mint resolution and `allow_real_mints` gate as every melt here.
pub async fn pay_melt_quote_async(
    home: &MaxplayerHome,
    quote_id: &str,
    mint_override: Option<&str>,
    ceiling: &MeltCeiling,
) -> Result<MeltOutcome, WalletOpsError> {
    let quote_id = quote_id.trim();
    if quote_id.is_empty() {
        return Err(WalletOpsError::Wallet("melt quote id is empty".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    let wallet = open_wallet_async(home, &mint_url).await?;
    let quote = wallet
        .localstore
        .get_melt_quote(quote_id)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .ok_or_else(|| {
            WalletOpsError::Wallet(format!(
                "melt quote {quote_id} is not in this wallet; refusing to pay a quote this wallet did not raise"
            ))
        })?;
    pay_quote_on_wallet(&wallet, mint_url, &quote, Some(ceiling)).await
}

/// The pay step shared by [`melt_within_async`] (quote raised a moment ago) and
/// [`pay_melt_quote_async`] (quote bound earlier): ceiling check against THIS quote's amount and
/// reserve, balance check, `prepare_melt` on this quote's id, `confirm`.
async fn pay_quote_on_wallet(
    wallet: &Wallet,
    mint_url: String,
    quote: &cdk::wallet::MeltQuote,
    ceiling: Option<&MeltCeiling>,
) -> Result<MeltOutcome, WalletOpsError> {
    let invoice_sats = quote.amount.to_u64();
    let fee_reserve_sats = quote.fee_reserve.to_u64();
    // The money hold, at the moment of spending: THIS quote — not the plan's estimate — is what the
    // wallet would pay under, so THIS quote's amount + reserve is what the ceiling bounds. Refused
    // here, no proof has been selected, prepared or sent; the caller journals a failed attempt and
    // the balance it meant to discharge is intact.
    if let Some(ceiling) = ceiling
        && !ceiling.admits(invoice_sats, fee_reserve_sats)
    {
        return Err(WalletOpsError::MeltExceedsCeiling {
            mint_url,
            quote_id: quote.id.clone(),
            invoice_sats,
            fee_reserve_sats,
            planned_invoice_sats: ceiling.invoice_sats,
            max_debit_sats: ceiling.max_debit_sats,
        });
    }
    let need = invoice_sats.saturating_add(fee_reserve_sats);
    let before = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    if before < need {
        return Err(WalletOpsError::Wallet(format!(
            "insufficient funds for melt: balance={before} need={need} (amount+fee_reserve)"
        )));
    }
    let prepared = wallet
        .prepare_melt(&quote.id, HashMap::new())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    // Reported, not bounded: this operator path keeps its reserve-only ceiling (addendum 8 §6); the
    // seller fee remittance pays through [`prepare_melt_payment_blocking`], which bounds the total.
    let input_fee_sats = prepared.input_fee().to_u64();
    let swap_fee_sats = prepared.swap_fee().to_u64();
    let confirmed = prepared
        .confirm()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    // `confirm` is the effect boundary — the melt has settled and funds have left the wallet, so the
    // outcome (paid/fee, both read from `confirmed`) MUST be returned. The post-confirm balance read
    // is observational; a read failure must never discard the outcome (finding U).
    let paid_sats = confirmed.amount().to_u64();
    let fee_sats = confirmed.fee_paid().to_u64();
    let read = wallet
        .total_balance()
        .await
        .map(|balance| balance.to_u64())
        .map_err(|error| error.to_string());
    let balance_after_sats = read.as_ref().ok().copied();
    let balance_sats =
        post_confirm_balance(read, before, paid_sats.saturating_add(fee_sats), "melt");
    Ok(MeltOutcome {
        mint_url,
        paid_sats,
        fee_sats,
        balance_sats,
        balance_after_sats,
        quote_id: quote.id.clone(),
        fee_reserve_sats,
        input_fee_sats,
        swap_fee_sats,
    })
}

/// The proof fees THIS wallet would pay on top of `inputs_needed` (amount + fee reserve) for a melt
/// prepared now, computed exactly as pinned CDK 0.17.2 `MeltSaga::prepare` computes them
/// (`melt/saga/mod.rs:301–318` exact fit, `:377–403` swap layout) but WITHOUT reserving a proof or
/// writing a saga: keysets and unspent proofs are read, `Wallet::select_proofs` is a pure function,
/// and the fee lookups read the mint's keyset metadata (cached; a GET at most). Returns the
/// proof-input fee on an exact-fit selection, or estimated input fee + swap fee on a swap layout.
async fn expected_melt_fees(wallet: &Wallet, inputs_needed: Amount) -> Result<(u64, u64), String> {
    let active_keyset_ids: Vec<_> = wallet
        .get_mint_keysets(KeysetFilter::Active)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|keyset| keyset.id)
        .collect();
    let keyset_fees_and_amounts = wallet
        .get_keyset_fees_and_amounts()
        .await
        .map_err(|error| error.to_string())?;
    let available = wallet
        .get_unspent_proofs()
        .await
        .map_err(|error| error.to_string())?;
    let exact = Wallet::select_proofs(
        inputs_needed,
        available.clone(),
        &active_keyset_ids,
        &keyset_fees_and_amounts,
        true,
    )
    .map_err(|error| error.to_string())?;
    if exact.total_amount().map_err(|error| error.to_string())? == inputs_needed {
        return Ok((
            wallet
                .get_proofs_fee(&exact)
                .await
                .map_err(|error| error.to_string())?
                .total
                .to_u64(),
            0,
        ));
    }
    let active_keyset_id = wallet
        .get_active_keyset()
        .await
        .map_err(|error| error.to_string())?
        .id;
    let fee_and_amounts = wallet
        .get_keyset_fees_and_amounts_by_id(active_keyset_id)
        .await
        .map_err(|error| error.to_string())?;
    let estimated_output_count = inputs_needed
        .split(&fee_and_amounts)
        .map_err(|error| error.to_string())?
        .len();
    let input_fee = wallet
        .get_keyset_count_fee(&active_keyset_id, estimated_output_count as u64)
        .await
        .map_err(|error| error.to_string())?;
    let to_swap = Wallet::select_proofs(
        inputs_needed + input_fee,
        available,
        &active_keyset_ids,
        &keyset_fees_and_amounts,
        true,
    )
    .map_err(|error| error.to_string())?;
    let swap_fee = wallet
        .get_proofs_fee(&to_swap)
        .await
        .map_err(|error| error.to_string())?
        .total;
    Ok((input_fee.to_u64(), swap_fee.to_u64()))
}

/// The active keyset's NUT-02 `input_fee_ppk`, read through the SDK's fee function: the fee on
/// 1000 proofs is ceil(ppk × 1000 / 1000) = ppk (pinned `fees.rs:35–48`, `wallet/mod.rs:356`).
/// A cached metadata read — a GET at most, never a proof-bearing request.
async fn active_keyset_input_fee_ppk(wallet: &Wallet) -> Result<u64, String> {
    let active_keyset_id = wallet
        .get_active_keyset()
        .await
        .map_err(|error| error.to_string())?
        .id;
    Ok(wallet
        .get_keyset_count_fee(&active_keyset_id, 1000)
        .await
        .map_err(|error| error.to_string())?
        .to_u64())
}

/// **Prepare a melt quote this wallet already holds, by id, bound on its TOTAL cost, and hand it
/// back undecided.** The seller fee remittance's spending call since addendum 8 (§1.1–1.2): the
/// quote was raised by [`melt_quote_async`] and its id is about to be bound to the row by the store
/// fence; this
/// 1. refuses an unknown quote id before the wallet touches a proof, re-checks the ceiling against
///    the quote's STORED amount and reserve, and checks the balance covers them — as
///    [`pay_melt_quote_async`] does;
/// 2. `prepare_melt(quote_id)`: the SDK selects and RESERVES proofs in the wallet's own database and
///    computes the proof-input fee and, when the proofs do not fit, the pre-melt swap and its fee
///    (pinned `melt/saga/mod.rs:286–460`; writes only the wallet's own database; it may FETCH mint
///    metadata/keysets — `:303` → keysets → `metadata_cache.load` — a GET, never a proof-bearing or
///    fee-bearing request);
/// 3. takes the actual-confirmability bound on the SDK's figures — [`MeltCeiling::admits_confirmable`]. Refused
///    ⇒ `PreparedMelt::cancel` (best-effort local compensation: proofs back to Unspent, quote
///    released; `:817–831` logs its own DB errors and still returns Ok) and
///    [`WalletOpsError::MeltTotalExceedsCeiling`]: no fee-bearing request was posted, nothing left
///    the wallet;
/// 4. under it ⇒ returns a [`PreparedMeltPayment`] whose [`PreparedMeltPayment::confirm`] performs
///    the swap (if any) and the melt request — the only spend on this path — and whose
///    [`PreparedMeltPayment::cancel`] (or drop) releases it.
///
/// The four figures are the SDK's PREPARED ones. `input_fee` is an estimate: `confirm` swaps to
/// invoice + reserve + that estimate, recomputes the input fee on the proofs it receives and refuses
/// after the swap when they do not cover it (pinned `melt/saga/mod.rs:678`, `:704–712`). The
/// preparation therefore also carries the keyset's [`MeltPreparation::input_fee_ppk`], and the
/// caller runs `fee_remit::confirm_would_succeed` on it before its fence (addendum 9 §1.1) — this
/// function does not. Bound (§1.5): fee metadata can change between prepare and confirm and the
/// SDK takes no caller maximum; the seller fee remittance's bound-Spending hold covers that failure.
///
/// Same mint resolution and `allow_real_mints` gate as every melt here. The operator's
/// `melt_within_*` path does NOT use this: it keeps its reserve-only ceiling (addendum 8 §6).
pub fn prepare_melt_payment_blocking(
    home: &MaxplayerHome,
    quote_id: &str,
    mint_override: Option<&str>,
    ceiling: &MeltCeiling,
) -> Result<PreparedMeltPayment, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("prepare_melt_payment_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let quote_id = quote_id.trim().to_owned();
    if quote_id.is_empty() {
        return Err(WalletOpsError::Wallet("melt quote id is empty".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    let home = home.clone();
    let ceiling = ceiling.clone();
    let (prepared_tx, prepared_rx) =
        std::sync::mpsc::channel::<Result<MeltPreparation, WalletOpsError>>();
    let (command_tx, command_rx) = std::sync::mpsc::channel::<PreparedCommand>();
    let (reply_tx, reply_rx) =
        std::sync::mpsc::channel::<Result<Option<MeltOutcome>, WalletOpsError>>();
    let thread = std::thread::Builder::new()
        .name("melt-prepared".to_owned())
        .spawn(move || {
            prepared_melt_thread(
                home,
                mint_url,
                quote_id,
                ceiling,
                prepared_tx,
                command_rx,
                reply_tx,
            );
        })
        .map_err(|error| WalletOpsError::Wallet(format!("spawn melt thread: {error}")))?;
    match prepared_rx.recv() {
        Ok(Ok(preparation)) => Ok(PreparedMeltPayment {
            preparation,
            command: Some(command_tx),
            reply: reply_rx,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(WalletOpsError::Wallet(
                "the melt thread ended before reporting its preparation; nothing was posted"
                    .to_owned(),
            ))
        }
    }
}

/// The body of the thread that owns a prepared melt: runtime, wallet and `PreparedMelt` live here
/// together; the caller's verdict arrives on `command`.
fn prepared_melt_thread(
    home: MaxplayerHome,
    mint_url: String,
    quote_id: String,
    ceiling: MeltCeiling,
    prepared_tx: std::sync::mpsc::Sender<Result<MeltPreparation, WalletOpsError>>,
    command: std::sync::mpsc::Receiver<PreparedCommand>,
    reply: std::sync::mpsc::Sender<Result<Option<MeltOutcome>, WalletOpsError>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = prepared_tx.send(Err(WalletOpsError::Wallet(error.to_string())));
            return;
        }
    };
    let wallet = match runtime.block_on(open_wallet_async(&home, &mint_url)) {
        Ok(wallet) => wallet,
        Err(error) => {
            let _ = prepared_tx.send(Err(error));
            return;
        }
    };
    // Steps 1–3 as one future so an early refusal is one `Err` and the prepared melt (which borrows
    // the wallet) stays on this thread's stack for the verdict.
    let staged = runtime.block_on(async {
        let quote = wallet
            .localstore
            .get_melt_quote(&quote_id)
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
            .ok_or_else(|| {
                WalletOpsError::Wallet(format!(
                    "melt quote {quote_id} is not in this wallet; refusing to pay a quote this wallet did not raise"
                ))
            })?;
        let invoice_sats = quote.amount.to_u64();
        let fee_reserve_sats = quote.fee_reserve.to_u64();
        if !ceiling.admits(invoice_sats, fee_reserve_sats) {
            return Err(WalletOpsError::MeltExceedsCeiling {
                mint_url: mint_url.clone(),
                quote_id: quote.id.clone(),
                invoice_sats,
                fee_reserve_sats,
                planned_invoice_sats: ceiling.invoice_sats,
                max_debit_sats: ceiling.max_debit_sats,
            });
        }
        let need = invoice_sats.saturating_add(fee_reserve_sats);
        let before = wallet
            .total_balance()
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
            .to_u64();
        if before < need {
            return Err(WalletOpsError::Wallet(format!(
                "insufficient funds for melt: balance={before} need={need} (amount+fee_reserve)"
            )));
        }
        let prepared = wallet
            .prepare_melt(&quote.id, HashMap::new())
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
        let input_fee_sats = prepared.input_fee().to_u64();
        let swap_fee_sats = prepared.swap_fee().to_u64();
        let requires_swap = prepared.requires_swap();
        // The keyset's ppk, for the confirmability arithmetic (addendum 9 §1.1 / addendum 10 §1.1):
        // the fee on 1000 proofs is exactly `input_fee_ppk`. Read BEFORE the gate, which needs it;
        // a failed read cancels the preparation.
        let input_fee_ppk = match active_keyset_input_fee_ppk(&wallet).await {
            Ok(ppk) => ppk,
            Err(error) => {
                let refusal = WalletOpsError::Wallet(format!(
                    "could not read the active keyset's input fee after preparing melt quote {}: {error}",
                    quote.id
                ));
                return match prepared.cancel().await {
                    Ok(()) => Err(refusal),
                    Err(cancel_error) => Err(WalletOpsError::Wallet(format!(
                        "{refusal}; AND cancelling the prepared melt failed: {cancel_error} (no fee-bearing request was posted)"
                    ))),
                };
            }
        };
        // The one gate (addendum 10 §1.1): will `confirm` succeed, and does its worst-case debit —
        // invoice + reserve + the input fee the SDK RECOMPUTES on the proofs it sends + swap fee —
        // fit the ceiling? On a refusal, cancel FIRST — the SDK's compensations: proofs back to
        // Unspent, quote released, saga deleted, all local — then refuse, typed. Cancel is
        // best-effort (CDK logs a compensation's own DB error and still returns Ok); an Err here is
        // reported for what it is: no fee-bearing request was posted, but a local proof reservation
        // may remain — `open_wallet_async` only constructs the wallet and does not run CDK
        // `recover_incomplete_sagas` on this path (only `crossmint_hop` calls it); a supported
        // recovery path is owed, not wired here.
        let bound = match ceiling.admits_confirmable(
            invoice_sats,
            fee_reserve_sats,
            Some(input_fee_sats),
            swap_fee_sats,
            input_fee_ppk,
            requires_swap,
        ) {
            Ok(bound) => bound,
            Err(shortfall) => {
                let refusal = match shortfall {
                    ConfirmShortfall::DifferentInvoice {
                        invoice_sats,
                        planned_invoice_sats,
                    } => WalletOpsError::MeltExceedsCeiling {
                        mint_url: mint_url.clone(),
                        quote_id: quote.id.clone(),
                        invoice_sats,
                        fee_reserve_sats,
                        planned_invoice_sats,
                        max_debit_sats: ceiling.max_debit_sats,
                    },
                    ConfirmShortfall::TargetShort { bound, .. } => {
                        WalletOpsError::MeltWouldNotConfirm {
                            mint_url: mint_url.clone(),
                            quote_id: quote.id.clone(),
                            invoice_sats,
                            fee_reserve_sats,
                            input_fee_sats,
                            actual_input_fee_sats: bound.actual_input_fee_sats,
                            target_sats: bound.target_sats,
                            swap_fee_sats,
                            input_fee_ppk,
                        }
                    }
                    ConfirmShortfall::OverCeiling {
                        bound,
                        max_debit_sats,
                    } => WalletOpsError::MeltTotalExceedsCeiling {
                        mint_url: mint_url.clone(),
                        quote_id: quote.id.clone(),
                        invoice_sats,
                        fee_reserve_sats,
                        input_fee_sats: bound.actual_input_fee_sats,
                        swap_fee_sats,
                        total_sats: bound.worst_debit_sats,
                        max_debit_sats,
                    },
                };
                return match prepared.cancel().await {
                    Ok(()) => Err(refusal),
                    Err(error) => Err(WalletOpsError::Wallet(format!(
                        "{refusal}; AND cancelling the prepared melt failed: {error} (no fee-bearing request was posted; a local proof reservation may remain until a supported recovery path — owed — releases it)"
                    ))),
                };
            }
        };
        let total_debit_sats = bound.worst_debit_sats;
        let preparation = MeltPreparation {
            mint_url: mint_url.clone(),
            quote_id: quote.id.clone(),
            invoice_sats,
            fee_reserve_sats,
            input_fee_sats,
            swap_fee_sats,
            requires_swap,
            input_fee_ppk,
            total_debit_sats,
            expiry_unix: quote.expiry,
        };
        Ok((prepared, preparation, before))
    });
    let (prepared, preparation, before) = match staged {
        Ok(staged) => staged,
        Err(error) => {
            let _ = prepared_tx.send(Err(error));
            return;
        }
    };
    if prepared_tx.send(Ok(preparation.clone())).is_err() {
        // Nobody is listening: release and leave.
        let _ = runtime.block_on(prepared.cancel());
        return;
    }
    // Plain blocking wait, on a thread with no runtime driving anything else.
    let verdict = command.recv();
    let outcome = match verdict {
        Ok(PreparedCommand::Confirm) => runtime.block_on(async {
            let confirmed = prepared
                .confirm()
                .await
                .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
            // `confirm` is the effect boundary — funds have left the wallet, so the outcome MUST be
            // returned; the post-confirm balance read is observational (finding U).
            let paid_sats = confirmed.amount().to_u64();
            let fee_sats = confirmed.fee_paid().to_u64();
            let read = wallet
                .total_balance()
                .await
                .map(|balance| balance.to_u64())
                .map_err(|error| error.to_string());
            // Actual debit (addendum 9 §2.1): invoice + `fee_paid` (Lightning fee + ACTUAL proof
            // input fee, inclusive) + the swap fee charged at the swap. The PREPARED input fee is
            // an estimate the SDK replaced inside `fee_paid`; adding it again double-counts.
            let spent = paid_sats
                .saturating_add(fee_sats)
                .saturating_add(preparation.swap_fee_sats);
            let balance_after_sats = read.as_ref().ok().copied();
            let balance_sats = post_confirm_balance(read, before, spent, "melt");
            Ok(Some(MeltOutcome {
                mint_url: preparation.mint_url.clone(),
                paid_sats,
                fee_sats,
                balance_sats,
                balance_after_sats,
                quote_id: preparation.quote_id.clone(),
                fee_reserve_sats: preparation.fee_reserve_sats,
                input_fee_sats: preparation.input_fee_sats,
                swap_fee_sats: preparation.swap_fee_sats,
            }))
        }),
        Ok(PreparedCommand::Cancel) | Err(_) => runtime
            .block_on(prepared.cancel())
            .map(|()| None)
            .map_err(|error| WalletOpsError::Wallet(format!("cancel prepared melt: {error}"))),
    };
    let _ = reply.send(outcome);
}

/// Raise a melt quote for `bolt11` and return it WITHOUT paying. Same mint resolution and real-mint
/// gate as [`melt_async`]; no proofs are selected, prepared or spent. A quote is the only honest
/// estimate of the melt fee, so the seller fee remittance's dry run calls this and prints it.
pub async fn melt_quote_async(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<MeltEstimate, WalletOpsError> {
    let bolt11 = bolt11.trim();
    if bolt11.is_empty() {
        return Err(WalletOpsError::Wallet("bolt11 invoice is empty".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    let wallet = open_wallet_async(home, &mint_url).await?;
    let quote = wallet
        .melt_quote(PaymentMethod::BOLT11, bolt11, None, None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    // Fee-aware estimate (addendum 8 §1.3): the proof fees this wallet would pay on top of
    // amount + reserve, the SDK's way, reserving nothing. Not estimable ⇒ 0 and the reason.
    let (expected_fees_sats, expected_swap_fee_sats, mut expected_fees_note) =
        match expected_melt_fees(&wallet, quote.amount + quote.fee_reserve).await {
            Ok((input_fee, swap_fee)) => (input_fee + swap_fee, swap_fee, None),
            Err(reason) => (0, 0, Some(reason)),
        };
    // The keyset's ppk for the planner's post-swap recomputation (addendum 9 §1.2); a cached
    // metadata read. Unreadable ⇒ 0 and the reason, never a guess.
    let input_fee_ppk = match active_keyset_input_fee_ppk(&wallet).await {
        Ok(ppk) => ppk,
        Err(reason) => {
            let note = format!("keyset input_fee_ppk not readable: {reason}");
            expected_fees_note = Some(match expected_fees_note {
                Some(existing) => format!("{existing}; {note}"),
                None => note,
            });
            0
        }
    };
    Ok(MeltEstimate {
        mint_url,
        quote_id: quote.id,
        amount_sats: quote.amount.to_u64(),
        fee_reserve_sats: quote.fee_reserve.to_u64(),
        expiry_unix: quote.expiry,
        expected_fees_sats,
        expected_swap_fee_sats,
        input_fee_ppk,
        expected_fees_note,
    })
}

/// What the mint says about ONE melt quote this wallet raised, by id, refreshed from the mint.
/// `None` when this wallet never raised a quote with that id. The seller fee remittance reconciles a
/// SPENDING row against the quote its admission bound — this call — never against "some quote for
/// the invoice" (addendum 5 §1, rule 2). Read-only: nothing here spends.
pub async fn melt_status_for_quote_async(
    home: &MaxplayerHome,
    quote_id: &str,
    mint_override: Option<&str>,
) -> Result<Option<MeltQuoteStatus>, WalletOpsError> {
    let quote_id = quote_id.trim();
    if quote_id.is_empty() {
        return Ok(None);
    }
    let mint_url = resolve_mint(home, mint_override)?;
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    let wallet = open_wallet_async(home, &mint_url).await?;
    let known = wallet
        .localstore
        .get_melt_quote(quote_id)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    if known.is_none() {
        return Ok(None);
    }
    let quote = wallet
        .check_melt_quote_status(quote_id)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    Ok(Some(MeltQuoteStatus {
        mint_url,
        quote_id: quote.id,
        state: quote.state,
        amount_sats: quote.amount.to_u64(),
        fee_reserve_sats: quote.fee_reserve.to_u64(),
        expiry_unix: quote.expiry,
    }))
}

/// What the mint says about the melt quote(s) this wallet raised for `bolt11`, refreshed from the
/// mint. `None` when the wallet never raised a quote for that invoice — which means no melt for it
/// can have started, because [`melt_async`] persists its quote before it prepares anything. When
/// several quotes exist for one invoice (an estimate plus the payment's own), the one that says
/// PAID wins, then PENDING, so a payment that landed is never read as unpaid. Read-only: nothing
/// here spends.
pub async fn melt_status_for_invoice_async(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<Option<MeltQuoteStatus>, WalletOpsError> {
    let bolt11 = bolt11.trim();
    let mint_url = resolve_mint(home, mint_override)?;
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    let wallet = open_wallet_async(home, &mint_url).await?;
    let mine: Vec<String> = wallet
        .localstore
        .get_melt_quotes()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .into_iter()
        .filter(|quote| quote.request.trim() == bolt11)
        .map(|quote| quote.id)
        .collect();
    if mine.is_empty() {
        return Ok(None);
    }
    // The invoice may have several quotes (the remittance raises an estimate quote at plan time and
    // a payment quote at melt time). Report the one that is MOST alive: PAID over settling over a
    // live UNPAID over anything else — and among live UNPAID quotes the one expiring last. This is a
    // snapshot of the quotes this wallet holds NOW; it cannot speak for a quote raised later, and it
    // does not prove that an expired or FAILED quote cannot still be paid (the mint may accept it).
    // The remitter therefore uses it only for rows with no quote bound — a `planned` row, or a
    // legacy `spending` row from before quotes were bound — and reconciles a bound `spending` row by
    // its exact quote id through `melt_status_for_quote_async`, releasing nothing on this ranking
    // (addendum 5 §1 rule 2; addendum 6 §1.2). The fake mint's test ranking is similar in spirit
    // but not identical to this one.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let rank = |status: &MeltQuoteStatus| match status.state {
        MeltQuoteState::Paid => 0,
        MeltQuoteState::Pending | MeltQuoteState::Unknown => 1,
        MeltQuoteState::Unpaid if now_unix <= status.expiry_unix => 2,
        MeltQuoteState::Unpaid => 3,
        MeltQuoteState::Failed => 4,
    };
    let mut best: Option<MeltQuoteStatus> = None;
    for quote_id in mine {
        let quote = wallet
            .check_melt_quote_status(&quote_id)
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
        let status = MeltQuoteStatus {
            mint_url: mint_url.clone(),
            quote_id: quote.id,
            state: quote.state,
            amount_sats: quote.amount.to_u64(),
            fee_reserve_sats: quote.fee_reserve.to_u64(),
            expiry_unix: quote.expiry,
        };
        if best.as_ref().is_none_or(|current| {
            rank(&status) < rank(current)
                || (rank(&status) == rank(current) && status.expiry_unix > current.expiry_unix)
        }) {
            best = Some(status);
        }
    }
    Ok(best)
}

/// List configured mints (default first).
pub fn list_mints(home: &MaxplayerHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    let default = normalize_mint_url(home.config.default_mint())?;
    Ok(configured_mints(home)?
        .into_iter()
        .map(|mint_url| MintBalance {
            is_default: mint_url == default,
            configured: true,
            mint_url,
            balance_sats: 0,
        })
        .collect())
}

/// Opt-in add of an extra mint URL (does not invent balance).
pub fn add_mint(home: &mut MaxplayerHome, mint_url: &str) -> Result<String, WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let default = normalize_mint_url(home.config.default_mint())?;
    if normalized == default {
        return Ok(normalized);
    }
    if home
        .config
        .extra_mints
        .iter()
        .any(|entry| normalize_mint_url(entry).ok().as_deref() == Some(normalized.as_str()))
    {
        return Ok(normalized);
    }
    let to_add = normalized.clone();
    home::save_config(home, |config| {
        config.extra_mints.push(to_add);
    })?;
    Ok(normalized)
}

/// Remove an opt-in extra mint. Default mint is pinned and cannot be removed.
pub fn remove_mint(home: &mut MaxplayerHome, mint_url: &str) -> Result<(), WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let default = normalize_mint_url(home.config.default_mint())?;
    if normalized == default {
        return Err(WalletOpsError::MintPinnedDefault { mint_url: default });
    }
    let present = home
        .config
        .extra_mints
        .iter()
        .any(|entry| normalize_mint_url(entry).ok().as_deref() == Some(normalized.as_str()));
    if !present {
        return Err(WalletOpsError::MintNotAllowed {
            mint_url: normalized,
            default_mint: home.config.default_mint().to_string(),
        });
    }
    let to_remove = normalized.clone();
    home::save_config(home, |config| {
        config
            .extra_mints
            .retain(|entry| normalize_mint_url(entry).ok().as_deref() != Some(to_remove.as_str()));
    })?;
    Ok(())
}

pub fn balances_blocking(home: &MaxplayerHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("balances_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(balances_async(home))
}

pub fn mint_blocking(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("mint_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(mint_async(home, amount_sats, mint_override))
}

pub fn complete_mint_blocking(
    home: &MaxplayerHome,
    quote: &MintQuote,
) -> Result<MintOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("complete_mint_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(complete_mint_async(home, quote))
}

pub fn complete_mint_by_id_blocking(
    home: &MaxplayerHome,
    quote_id: &str,
    amount_override: Option<u64>,
    mint_override: Option<&str>,
) -> Result<MintOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("complete_mint_by_id_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(complete_mint_by_id_async(
        home,
        quote_id,
        amount_override,
        mint_override,
    ))
}

pub fn send_blocking(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<SendOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("send_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(send_async(home, amount_sats, mint_override))
}

pub fn receive_blocking(
    home: &MaxplayerHome,
    token: &str,
) -> Result<ReceiveOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("receive_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(receive_async(home, token))
}

pub fn melt_blocking(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<MeltOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("melt_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(melt_async(home, bolt11, mint_override))
}

/// [`melt_within_async`] on a runtime of its own — the seller fee remittance's spending call. Same
/// nested-runtime refusal as every `*_blocking` wrapper here.
pub fn melt_within_blocking(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
    ceiling: Option<&MeltCeiling>,
) -> Result<MeltOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("melt_within_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(melt_within_async(home, bolt11, mint_override, ceiling))
}

pub fn melt_quote_blocking(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<MeltEstimate, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("melt_quote_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(melt_quote_async(home, bolt11, mint_override))
}

pub fn melt_status_for_invoice_blocking(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<Option<MeltQuoteStatus>, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("melt_status_for_invoice_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(melt_status_for_invoice_async(home, bolt11, mint_override))
}

/// [`melt_status_for_quote_async`] on a runtime of its own.
pub fn melt_status_for_quote_blocking(
    home: &MaxplayerHome,
    quote_id: &str,
    mint_override: Option<&str>,
) -> Result<Option<MeltQuoteStatus>, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("melt_status_for_quote_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(melt_status_for_quote_async(home, quote_id, mint_override))
}

/// [`pay_melt_quote_async`] on a runtime of its own. **Retained, and DEAD on the seller fee
/// remittance path** since addendum 8: that path's spending edge is
/// [`prepare_melt_payment_blocking`] → [`PreparedMeltPayment::confirm`]; removal of this wrapper is
/// owed to the owners. Addendum 5 §1, rule 1 still holds for it: pay the bound quote by id, never
/// re-quote.
pub fn pay_melt_quote_blocking(
    home: &MaxplayerHome,
    quote_id: &str,
    mint_override: Option<&str>,
    ceiling: &MeltCeiling,
) -> Result<MeltOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("pay_melt_quote_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(pay_melt_quote_async(home, quote_id, mint_override, ceiling))
}

pub fn invoice_blocking(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("invoice_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(invoice_async(home, amount_sats, mint_override))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::bootstrap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Stage 2a, addendum 3 §1: the money hold. A ceiling admits a payment-time quote only when the
    /// invoice is the one planned AND invoice + the quote's fee reserve fit under the maximum gross
    /// debit — checked against the quote the mint raised at PAYMENT time, so a reserve that grew
    /// between the estimate and the payment is refused before any proof is consumed.
    #[test]
    fn a_melt_ceiling_admits_only_the_planned_invoice_within_the_gross_debit() {
        let ceiling = MeltCeiling {
            max_debit_sats: 15,
            invoice_sats: 13,
            planned_quote_id: Some("q-estimate".to_owned()),
        };
        assert!(ceiling.admits(13, 2), "13 + 2 = 15 fits exactly");
        assert!(ceiling.admits(13, 0), "a smaller reserve fits");
        assert!(
            !ceiling.admits(13, 4),
            "13 + 4 = 17 exceeds the 15-sat gross: the reserve grew between estimate and payment"
        );
        assert!(
            !ceiling.admits(12, 0),
            "a different invoice amount than the one planned is refused even when it fits"
        );
        assert!(!ceiling.admits(14, 0), "and so is a larger one");
        assert!(
            !ceiling.admits(u64::MAX, u64::MAX),
            "the sum saturates rather than wrapping under the ceiling"
        );
    }

    /// Addendum 10 §1.1: the bound on a PREPARED melt is the ACTUAL-confirmability bound, one
    /// arithmetic for the planner, the gate and the pre-fence check. Verdict 4714623 §3.2's witness —
    /// gross 19, reserve 2, 1000 ppk, one 32-sat proof: invoice 13 prepares input 4 (15 = 8+4+2+1)
    /// and swap 1; the PREPARED total 20 > 19, but confirm swaps to 19 = [16, 2, 1], recomputes 3,
    /// 19 ≥ 18, and debits at most 13 + 2 + 3 + 1 = 19 ≤ 19 — ADMITTED. Round 8 refused it.
    #[test]
    fn a_melt_ceiling_admits_a_prepared_melt_by_what_confirm_will_actually_debit() {
        let ceiling = MeltCeiling {
            max_debit_sats: 19,
            invoice_sats: 13,
            planned_quote_id: Some("q-estimate".to_owned()),
        };
        let bound = ceiling
            .admits_confirmable(13, 2, Some(4), 1, 1000, true)
            .expect("13 + 2 + actual 3 + swap 1 = 19 fits 19");
        assert_eq!(
            bound,
            ConfirmBound {
                need_sats: 15,
                prepared_input_fee_sats: 4,
                target_sats: 19,
                actual_input_fee_sats: 3,
                swap_fee_sats: 1,
                worst_debit_sats: 19,
            }
        );
        assert_eq!(
            MeltCeiling::total_debit(13, 2, 4, 1),
            20,
            "the PREPARED total is 20 — an estimate above the actual debit, no longer the gate"
        );
        // One sat less of gross and the same melt is over the ceiling, by the ACTUAL figures.
        let tighter = MeltCeiling {
            max_debit_sats: 18,
            ..ceiling.clone()
        };
        assert_eq!(
            tighter.admits_confirmable(13, 2, Some(4), 1, 1000, true),
            Err(ConfirmShortfall::OverCeiling {
                bound: bound.clone(),
                max_debit_sats: 18,
            })
        );
        // Record 37's schedule: invoice 12, reserve 0, prepared 2 (12 = 8+4) ⇒ target 14 = [8,4,2]
        // ⇒ actual 3 ⇒ 14 < 15: the SDK would refuse AFTER the swap — refused here, before it.
        let planned_12 = MeltCeiling {
            max_debit_sats: 20,
            invoice_sats: 12,
            planned_quote_id: None,
        };
        assert!(matches!(
            planned_12.admits_confirmable(12, 0, Some(2), 1, 1000, true),
            Err(ConfirmShortfall::TargetShort {
                needed_after_swap_sats: 15,
                bound: ConfirmBound {
                    target_sats: 14,
                    actual_input_fee_sats: 3,
                    ..
                },
            })
        ));
        // Exact fit (no swap): the selected proofs already carry the prepared fee; only the debit
        // bound applies, with actual = prepared.
        assert_eq!(
            ceiling
                .admits_confirmable(13, 2, Some(4), 0, 1000, false)
                .map(|bound| bound.worst_debit_sats),
            Ok(19)
        );
        assert!(matches!(
            ceiling.admits_confirmable(13, 2, Some(5), 0, 1000, false),
            Err(ConfirmShortfall::OverCeiling { .. })
        ));
        // A different invoice than the one planned is refused even when it fits.
        assert_eq!(
            ceiling.admits_confirmable(12, 0, Some(0), 0, 0, false),
            Err(ConfirmShortfall::DifferentInvoice {
                invoice_sats: 12,
                planned_invoice_sats: 13,
            })
        );
        // Saturating, never wrapping.
        assert!(matches!(
            ceiling.admits_confirmable(13, u64::MAX, Some(u64::MAX), u64::MAX, 1000, true),
            Err(ConfirmShortfall::OverCeiling { .. })
        ));
        assert_eq!(MeltCeiling::total_debit(u64::MAX, 1, 1, 1), u64::MAX);
    }

    /// The planner's question, through the same function: the largest invoice for which
    /// `confirm_bound` holds with the prepared fee computed on the split of `need`. Addendum 10 §1.2:
    /// 19/2/1000/[32] ⇒ 13; a consistently reserve-0 schedule at gross 20 ⇒ 15 (not 12); 20/2 ⇒ 13;
    /// 3/1 ⇒ none.
    #[test]
    fn the_confirmability_bound_selects_the_verdicts_invoices_when_searched_downward() {
        let largest = |gross: u64, reserve: u64| {
            (1..=gross.saturating_sub(reserve)).rev().find(|&invoice| {
                confirm_bound(invoice, reserve, None, 1, 1000, true, gross).is_ok()
            })
        };
        assert_eq!(largest(19, 2), Some(13));
        assert_eq!(largest(20, 0), Some(15));
        assert_eq!(largest(20, 2), Some(13));
        assert_eq!(largest(3, 1), None);
        let fifteen = confirm_bound(15, 0, None, 1, 1000, true, 20).expect("15/0 fits 20");
        assert_eq!(
            (
                fifteen.target_sats,
                fifteen.actual_input_fee_sats,
                fifteen.worst_debit_sats
            ),
            (19, 3, 19)
        );
    }

    /// The typed refusal names every part, the total and the ceiling, and says what happened to the
    /// prepared melt — the line the remittance prints.
    #[test]
    fn a_total_ceiling_refusal_names_the_parts_the_total_and_the_ceiling() {
        let refusal = WalletOpsError::MeltTotalExceedsCeiling {
            mint_url: "https://mint.example".to_owned(),
            quote_id: "q-pay".to_owned(),
            invoice_sats: 13,
            fee_reserve_sats: 2,
            input_fee_sats: 4,
            swap_fee_sats: 1,
            total_sats: 20,
            max_debit_sats: 15,
        };
        assert_eq!(
            refusal.to_string(),
            "melt refused before spending: mint https://mint.example quote q-pay would debit 20 sats in total \
             (13 sats invoice + 2 sats fee reserve + 4 sats proof input fee + 1 sats swap fee) against a ceiling \
             of 15 sats; the prepared melt was cancelled and its proofs released; nothing was posted to the mint"
        );
    }

    /// A prepared payment whose verdict never comes is cancelled on drop: the thread must have
    /// replied and exited, not hung. Exercised with a thread that models the protocol (the real
    /// body needs a wallet with a stored quote — the full path is the remittance's regression).
    #[test]
    fn dropping_an_undecided_prepared_payment_cancels_it_and_joins_its_thread() {
        let (command_tx, command_rx) = std::sync::mpsc::channel::<PreparedCommand>();
        let (reply_tx, reply_rx) =
            std::sync::mpsc::channel::<Result<Option<MeltOutcome>, WalletOpsError>>();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::clone(&cancelled);
        let thread = std::thread::spawn(move || {
            let verdict = command_rx.recv();
            if matches!(verdict, Ok(PreparedCommand::Cancel) | Err(_)) {
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = reply_tx.send(Ok(None));
        });
        let payment = PreparedMeltPayment {
            preparation: MeltPreparation {
                mint_url: "https://mint.example".to_owned(),
                quote_id: "q-pay".to_owned(),
                invoice_sats: 13,
                fee_reserve_sats: 2,
                input_fee_sats: 0,
                swap_fee_sats: 0,
                requires_swap: false,
                input_fee_ppk: 0,
                total_debit_sats: 15,
                expiry_unix: u64::MAX,
            },
            command: Some(command_tx),
            reply: reply_rx,
            thread: Some(thread),
        };
        assert!(format!("{payment:?}").contains("decided: false"));
        drop(payment);
        assert!(
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "drop without a verdict must send Cancel and wait for the thread"
        );
    }

    /// `confirm` and `cancel` each consume the payment and relay the thread's reply.
    #[test]
    fn a_prepared_payment_relays_confirm_and_cancel_verdicts() {
        fn fixture(
            script: impl FnOnce(PreparedCommand) -> Result<Option<MeltOutcome>, WalletOpsError>
            + Send
            + 'static,
        ) -> PreparedMeltPayment {
            let (command_tx, command_rx) = std::sync::mpsc::channel::<PreparedCommand>();
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let verdict = command_rx.recv().expect("a verdict");
                let _ = reply_tx.send(script(verdict));
            });
            PreparedMeltPayment {
                preparation: MeltPreparation {
                    mint_url: "https://mint.example".to_owned(),
                    quote_id: "q-pay".to_owned(),
                    invoice_sats: 13,
                    fee_reserve_sats: 2,
                    input_fee_sats: 1,
                    swap_fee_sats: 0,
                    requires_swap: false,
                    input_fee_ppk: 0,
                    total_debit_sats: 16,
                    expiry_unix: u64::MAX,
                },
                command: Some(command_tx),
                reply: reply_rx,
                thread: Some(thread),
            }
        }
        let outcome = MeltOutcome {
            mint_url: "https://mint.example".to_owned(),
            paid_sats: 13,
            fee_sats: 1,
            balance_sats: 100,
            balance_after_sats: Some(100),
            quote_id: "q-pay".to_owned(),
            fee_reserve_sats: 2,
            input_fee_sats: 1,
            swap_fee_sats: 0,
        };
        let expected = outcome.clone();
        let confirmed = fixture(move |verdict| {
            assert!(matches!(verdict, PreparedCommand::Confirm));
            Ok(Some(outcome))
        })
        .confirm()
        .expect("confirm relays the outcome");
        assert_eq!(confirmed, expected);

        fixture(|verdict| {
            assert!(matches!(verdict, PreparedCommand::Cancel));
            Ok(None)
        })
        .cancel()
        .expect("cancel relays Ok");

        let none_for_confirm = fixture(|_| Ok(None))
            .confirm()
            .expect_err("no outcome is an error");
        assert!(
            none_for_confirm
                .to_string()
                .contains("reported no outcome for a confirm")
        );

        let failed = fixture(|_| Err(WalletOpsError::Wallet("mint said no".to_owned())))
            .confirm()
            .expect_err("a failed confirm is relayed");
        assert_eq!(failed.to_string(), "wallet error: mint said no");
    }

    // Finding DD: `SendOutcome.token` is a BEARER cashu token (spendable ecash). Its `Debug` MUST
    // redact the token — a derived Debug would print it verbatim, so any debug log of a SendOutcome
    // would leak spendable funds. Assert the debug rendering contains neither the token nor any of
    // its material, and that it carries the redaction marker + non-secret fields.
    #[test]
    fn send_outcome_debug_redacts_bearer_token() {
        let token = "cashuAeyJ0b2tlbiI6c3BlbmRhYmxlLWJlYXJlci1lY2FzaC1zZWNyZXQ";
        let outcome = SendOutcome {
            mint_url: "https://testnut.cashudevkit.org".into(),
            sent_sats: 21,
            balance_sats: 100,
            token: token.into(),
        };
        let rendered = format!("{outcome:?}");
        assert!(
            !rendered.contains(token),
            "SendOutcome Debug must not contain the bearer token: {rendered}"
        );
        // No substring of the token beyond a trivial prefix leaks (guard against partial exposure).
        assert!(
            !rendered.contains("spendable-bearer-ecash-secret") && !rendered.contains(&token[6..]),
            "SendOutcome Debug must not leak token material: {rendered}"
        );
        assert!(
            rendered.contains("<redacted:sha256:"),
            "redaction marker expected: {rendered}"
        );
        // Non-secret fields remain visible for diagnostics.
        assert!(rendered.contains("sent_sats: 21") && rendered.contains("balance_sats: 100"));
    }

    // An empty token renders the empty marker (no digest of nothing) — never a bare empty string
    // that could be mistaken for "no field".
    #[test]
    fn redact_secret_empty_marks_empty() {
        assert_eq!(redact_secret(""), "<redacted:empty>");
        assert!(redact_secret("x").starts_with("<redacted:sha256:"));
    }

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-wallet-ops-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn extra_mint_add_remove_keeps_default_pinned() {
        let root = temp_home("mints");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        // Issue #378: the shipped default mint is the real minibits mint (not testnut).
        assert_eq!(
            home.config.default_mint(),
            crate::home::DEFAULT_MINIBITS_MINT_URL
        );
        let listed = list_mints(&home).expect("list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_default);

        let added = add_mint(&mut home, "https://example.mint.test").expect("add");
        assert_eq!(added, "https://example.mint.test");
        assert_eq!(list_mints(&home).expect("list2").len(), 2);

        let err = remove_mint(&mut home, crate::home::DEFAULT_MINIBITS_MINT_URL).expect_err("pin");
        assert!(matches!(err, WalletOpsError::MintPinnedDefault { .. }));

        remove_mint(&mut home, "https://example.mint.test").expect("remove");
        assert_eq!(list_mints(&home).expect("list3").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn balances_include_unconfigured_proofs_from_the_shared_database() {
        use cashu::secret::Secret;
        use cashu::{Amount, Id, Proof, SecretKey, State};
        use cdk::wallet::types::ProofInfo;

        let root = temp_home("db-truth");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let stray = MintUrl::from_str("https://stray-mint.example/").expect("stray mint URL");
        let store = WalletSqliteDatabase::new(sqlite_path(&home.wallet_dir))
            .await
            .expect("open on-disk wallet database");
        let proof = Proof::new(
            Amount::from(37),
            Id::from_str("009a1f293253e41e").expect("keyset id"),
            Secret::new("issue-266-unconfigured-db-truth"),
            SecretKey::generate().public_key(),
        );
        let proof_info = ProofInfo::new(proof, stray.clone(), State::Unspent, CurrencyUnit::Sat)
            .expect("proof info");
        store
            .update_proofs(vec![proof_info], vec![])
            .await
            .expect("seed unconfigured proof");

        let rows = balances_async(&home).await.expect("read database truth");
        let stray_row = rows
            .iter()
            .find(|row| row.mint_url == "https://stray-mint.example")
            .expect("unconfigured proof mint appears");
        assert_eq!(stray_row.balance_sats, 37);
        assert!(!stray_row.configured);
        assert!(!stray_row.is_default);

        let row_total: u64 = rows.iter().map(|row| row.balance_sats).sum();
        let db_total = store
            .get_balance(None, Some(CurrencyUnit::Sat), Some(vec![State::Unspent]))
            .await
            .expect("whole database balance");
        assert_eq!(row_total, db_total, "per-mint rows cross-foot to DB truth");
        assert_eq!(row_total, 37);
        let _ = std::fs::remove_dir_all(&root);
    }

    // #579: MintPinnedDefault's message hardcoded the testnut DEFAULT_MINT_URL, so on a
    // real-minibits-default home `wallet mints remove <minibits>` errored "cannot remove the default
    // mint (https://testnut.cashudevkit.org)" — naming testnut as the default when the mint actually
    // pinned is minibits (config.default_mint()). Display-only lie; the guard pins correctly. This
    // pins the message to the ACTUAL default. Reverting the Display fix REDS this.
    #[test]
    fn mint_pinned_default_error_names_the_real_default_not_testnut() {
        let root = temp_home("pinned-default-message");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        let default = normalize_mint_url(home.config.default_mint()).expect("normalize default");
        // The shipped default is the real minibits mint, distinct from the testnut constant — so a
        // message naming testnut here is provably wrong, not a coincidental match.
        assert_eq!(default, crate::home::DEFAULT_MINIBITS_MINT_URL);
        assert_ne!(default, crate::home::DEFAULT_MINT_URL);

        let message = remove_mint(&mut home, crate::home::DEFAULT_MINIBITS_MINT_URL)
            .expect_err("removing the pinned default must error")
            .to_string();
        assert!(
            message.contains(&default),
            "message must name the real pinned default ({default}): {message}"
        );
        assert!(
            !message.contains(crate::home::DEFAULT_MINT_URL),
            "message must not name the testnut constant as the default: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_mint_refuses_inside_runtime() {
        let root = temp_home("nested");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = mint_blocking(&home, 1, None).expect_err("nested");
        assert!(err.to_string().contains("nested block_on refused"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_mint_refused_without_inventing_credit() {
        let root = temp_home("unknown");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = mint_blocking(&home, 1, Some("https://evil.example")).expect_err("deny");
        assert!(matches!(&err, WalletOpsError::MintNotAllowed { .. }));
        // #465: a genuine membership miss KEEPS the `mints add` remedy — the distinction the fix draws.
        assert!(
            err.to_string().contains("mints add"),
            "membership miss keeps the `mints add` remedy: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding T(3): the standalone receive path fails closed on a non-allowlisted REAL mint when
    // allow_real_mints=false — even though the mint IS in the configured list (so `mint_is_allowed`
    // passes) — the same real-mint fence send/melt enforce. Reached before any wallet open, so it
    // holds offline.
    #[tokio::test(flavor = "current_thread")]
    async fn receive_refuses_real_mint_when_disallowed() {
        use std::str::FromStr;

        use cashu::secret::Secret;
        use cashu::{Amount, CurrencyUnit, Id, MintUrl, Proof, SecretKey, Token};

        let real_mint = "https://real-mint.example/";
        let root = temp_home("receive-real-mint-fence");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec![real_mint.into()];
        home.config.allow_real_mints = false;

        let proof = Proof::new(
            Amount::from(5),
            Id::from_str("009a1f293253e41e").expect("keyset id"),
            Secret::new("receive-fence-test-secret"),
            SecretKey::generate().public_key(),
        );
        let token = Token::new(
            MintUrl::from_str(real_mint).expect("mint url"),
            vec![proof],
            None,
            CurrencyUnit::Sat,
        );

        let err = receive_async(&home, &token.to_string())
            .await
            .expect_err("real mint must refuse under allow_real_mints=false");
        assert!(
            matches!(&err, WalletOpsError::RealMintDisallowed { mint_url } if mint_url.contains("real-mint.example")),
            "expected RealMintDisallowed (policy fence, not a membership miss), got {err:?}"
        );
        // #465: the policy refusal must name the ACTUAL control and never the membership remedy —
        // `mints add` cannot clear an allow_real_mints=false fence.
        let message = err.to_string();
        assert!(
            message.contains("MAXPLAYER_ALLOW_REAL_MINTS"),
            "policy refusal must name the real control (MAXPLAYER_ALLOW_REAL_MINTS), got: {message}"
        );
        assert!(
            !message.contains("mints add"),
            "policy refusal must NOT borrow the membership `mints add` remedy, got: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // #500: a funding op must persist ONLY its own change, never the in-memory, env-widened real-mint
    // fence. `save_config` writes the FILE-only view (re-reads config.toml, edits that), so an
    // `allow_real_mints = true` that exists only because MAXPLAYER_ALLOW_REAL_MINTS opened it in-process
    // can never leak to disk. The write-back class was fixed by #84 (fix/save-config-env-promotion);
    // this pins the FENCE field on the FUNDING path — which the scalar-only, direct-save
    // `save_does_not_persist_env_override_values` (home.rs) did not cover.
    #[test]
    fn funding_op_never_writes_back_the_env_widened_real_mint_fence() {
        let root = temp_home("500-funding-no-gate-writeback");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        // Durable fence CLOSED on disk — the operator's explicit opt-out.
        home::save_config(&mut home, |config| config.allow_real_mints = false)
            .expect("seed the fence closed on disk");

        // Simulate the daemon launcher's MAXPLAYER_ALLOW_REAL_MINTS=true: the env overlay opens the
        // fence IN-MEMORY only (`home.config`), while config.toml on disk stays false.
        home.config.allow_real_mints = true;

        // A funding op (adds an extra mint) — persists through `save_config`.
        let added = add_mint(&mut home, "https://real-mint.example/").expect("add an extra mint");

        let raw = std::fs::read_to_string(root.join("config.toml")).expect("read config.toml");
        let on_disk = home::parse_config_toml(&raw).expect("parse config.toml");
        // The durable fence is untouched: the env-widened in-memory value did NOT leak to disk...
        assert!(
            !on_disk.allow_real_mints,
            "a funding op must not write the env-widened real-mint fence back to disk (#500); config.toml = {raw}"
        );
        // ...while the funding op's OWN change DID persist.
        assert!(
            on_disk.extra_mints.iter().any(|entry| entry == &added),
            "the funding op's own change (the added mint) must persist to disk"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding U: `confirm` is the effect boundary, so a post-confirm balance-read FAILURE must never
    // discard the confirmed token/outcome — `post_confirm_balance` returns a best-effort estimate and
    // never errors, so the caller always returns the token. A stale/equal balance also still returns.
    #[test]
    fn post_confirm_balance_read_failure_preserves_outcome() {
        // Read failed: best-effort `before - spent`, never an error → the token is still returned.
        assert_eq!(
            post_confirm_balance(Err("boom".into()), 100, 30, "send"),
            70
        );
        // Underflow-safe when the estimate would go negative.
        assert_eq!(post_confirm_balance(Err("boom".into()), 10, 30, "send"), 0);
        // Read ok and balance decreased → report the read value.
        assert_eq!(post_confirm_balance(Ok(70), 100, 30, "melt"), 70);
        // Read ok but stale/equal (did-not-decrease) → still returned, WARN only (no discard).
        assert_eq!(post_confirm_balance(Ok(100), 100, 30, "send"), 100);
    }

    // Finding X: a successful `receive` is the effect boundary (proofs already redeemed), so a
    // post-receive balance-read FAILURE must never discard the credited outcome — `post_receive_balance`
    // returns a best-effort `before + received` estimate and never errors. A stale/non-increasing
    // read also still returns (WARN only), so the caller never retries into an already-spent token.
    #[test]
    fn post_receive_balance_read_failure_preserves_outcome() {
        // Read failed: best-effort `before + received`, never an error → the outcome is preserved.
        assert_eq!(post_receive_balance(Err("boom".into()), 100, 30), 130);
        // Read ok and balance increased → report the read value.
        assert_eq!(post_receive_balance(Ok(130), 100, 30), 130);
        // Read ok but stale/non-increasing → still returned, WARN only (no discard).
        assert_eq!(post_receive_balance(Ok(100), 100, 30), 100);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lookup_pending_quote_unknown_id_is_none() {
        // Pure local sqlite read — no live mint needed; an unknown id yields None
        // rather than inventing a quote.
        let root = temp_home("lookup-none");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let found = lookup_pending_quote_async(&home, "quote-does-not-exist")
            .await
            .expect("lookup");
        assert!(found.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_mint_by_id_unknown_quote_without_amount_refuses() {
        // No stored quote + no --amount => refuse rather than guess. Reached
        // before any mint round-trip, so this holds even with testnut down.
        let root = temp_home("complete-noamount");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = complete_mint_by_id_async(&home, "unknown-quote", None, None)
            .await
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("pass --amount"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_mint_by_id_empty_quote_id_refuses() {
        let root = temp_home("complete-empty");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = complete_mint_by_id_async(&home, "   ", Some(21), None)
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("quote_id is empty"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_complete_mint_by_id_refuses_inside_runtime() {
        let root = temp_home("complete-nested");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = complete_mint_by_id_blocking(&home, "quote", Some(21), None).expect_err("nested");
        assert!(err.to_string().contains("nested block_on refused"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_mint_url_trims_and_strips_trailing_slash() {
        let normalized =
            normalize_mint_url(" https://testnut.cashudevkit.org/ ").expect("normalize");
        assert_eq!(normalized, DEFAULT_MINT_URL);
        let err = normalize_mint_url("   ").expect_err("empty");
        assert!(matches!(err, WalletOpsError::Wallet(_)));
    }

    // #506/#577 money class: `of_mint` classifies PURELY from the mint URL — the testnut play mint is
    // Play, every other mint (including the shipped minibits default) is Real. The classification is
    // internal: it gates the #445 refusal and the play-money marker, never a surfaced money-class label.
    #[test]
    fn of_mint_classifies_testnut_play_and_others_real() {
        assert_eq!(MoneyType::of_mint(DEFAULT_MINT_URL), MoneyType::Play);
        // Trailing slash / surrounding whitespace still classify as the testnut mint (normalized).
        assert_eq!(
            MoneyType::of_mint(" https://testnut.cashudevkit.org/ "),
            MoneyType::Play
        );
        assert_eq!(
            MoneyType::of_mint(crate::home::DEFAULT_MINIBITS_MINT_URL),
            MoneyType::Real
        );
        assert_eq!(
            MoneyType::of_mint("https://real-mint.example"),
            MoneyType::Real
        );
        // Fail-safe: an unparseable URL is never classified play money.
        assert_eq!(MoneyType::of_mint("not a url"), MoneyType::Real);
    }

    // #506-A: `MintNotAllowed` must name the home's ACTUAL default (`config.default_mint()`), never
    // the pinned testnut constant. On the shipped real-minibits default home, "(default stays
    // testnut)" was a money-relevant lie (`wallet mints list` correctly shows minibits as default).
    // Red-on-revert: interpolating DEFAULT_MINT_URL again names testnut on a minibits home.
    #[test]
    fn mint_not_allowed_names_home_default_not_testnut_constant() {
        let root = temp_home("506a-default-name");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        // Precondition: the fresh home's default is the real minibits mint (#378), not testnut.
        assert_eq!(
            home.config.default_mint(),
            crate::home::DEFAULT_MINIBITS_MINT_URL
        );
        let err =
            mint_is_allowed(&home, "https://evil.example").expect_err("unconfigured mint refused");
        let message = err.to_string();
        assert!(
            message.contains(crate::home::DEFAULT_MINIBITS_MINT_URL),
            "MintNotAllowed must name the home's real default: {message}"
        );
        assert!(
            !message.contains(DEFAULT_MINT_URL),
            "MintNotAllowed must NOT name the testnut constant as the default on a minibits home: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
