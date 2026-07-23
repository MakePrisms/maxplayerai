//! Wallet-backed payment policy, adapters, and authenticity checks.

use std::collections::HashSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cashu::nuts::nut18::PaymentRequestPayload;
use cashu::{
    Amount, CheckStateRequest, CurrencyUnit, MintUrl, PublicKey as CashuPublicKey, SecretKey,
    SpendingConditions, State, Token,
};
use cdk::wallet::{
    HttpClient, KeysetFilter, MintConnector, ReceiveOptions, SendOptions, Wallet,
};
use cdk::wallet::types::{SendSagaState, TransactionDirection, WalletSagaState};
use nostr_sdk::PublicKey as NostrPublicKey;

use crate::gateway::ParsedOffer;
use crate::payment::{
    AttemptId, EffectError, LockedPayment, PaymentEffects, PaymentKey, PaymentTerms,
    ReceiptEvidence,
};
use crate::payment_send::{PaymentPayload, PaymentSend, PaymentSent};
use crate::wallet::{TradeLock, VerifiedPayment, verify_trade_p2pk_with_connector};

const ATTEMPT_METADATA: &str = "mobee_attempt_id";

/// Default bound for the mint-touching legs of the buyer money path.
///
/// A live keyset/fee fetch against a dead or unroutable mint would otherwise hang
/// past the 15s MCP tool deadline; we bound each such leg and refuse fast instead.
pub const MINT_TOUCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Reason code surfaced when a dead mint blocks the post-time dust guard.
pub const MINT_UNREACHABLE_POST: &str = "mint_unreachable";

/// Reason code surfaced when a dead mint blocks the pay path.
pub const MINT_UNREACHABLE_PAY: &str = "mint_unreachable_pay";

/// Outcome of retiring incomplete send sagas that are safe to clean up.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetireReport {
    /// `Send(ProofsReserved)` sagas cancelled after mint Unspent proof.
    pub retired: usize,
    /// Mapped `Send(TokenCreated)` pending claims left alone (not a wedge).
    pub mapped_token_created: usize,
    /// Stranded Swap sagas rolled back after mint-truth said the reserved inputs
    /// were all UNSPENT — the swap never executed.
    pub swap_rolled_back: usize,
    /// Stranded Swap sagas completed after mint-truth said the reserved inputs
    /// were all SPENT — the swap executed; inputs marked spent and outputs
    /// re-derived from the wallet seed via NUT-13 `Wallet::restore`.
    pub swap_recovered: usize,
}

#[derive(Debug)]
/// Failure in a wallet-backed payment operation.
pub enum PaymentWalletError {
    Policy(String),
    Wallet(String),
    Reconcile(String),
    Verify(String),
    /// Predicted mint fee did not match the post-swap net credit.
    ///
    /// Wallet credit from the swap is left intact; callers must not journal or
    /// publish a receipt for this attempt.
    FeeMismatch {
        face: Amount,
        received: Amount,
        predicted_fee: Amount,
    },
    /// The configured mint could not be reached within the bounded timeout — a
    /// dead/unroutable mint (transport failure) or an elapsed deadline.
    ///
    /// The buyer money path fails fast with this instead of hanging past the MCP
    /// tool deadline. `reason` is a stable code (`mint_unreachable` for the
    /// post-time dust guard, `mint_unreachable_pay` for the pay path) and `mint`
    /// names the unreachable mint URL.
    MintUnreachable {
        reason: &'static str,
        mint: String,
        detail: String,
    },
}

impl std::fmt::Display for PaymentWalletError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy(message) => write!(formatter, "payment policy rejected: {message}"),
            Self::Wallet(message) => write!(formatter, "wallet operation failed: {message}"),
            Self::Reconcile(message) => {
                write!(formatter, "wallet reconciliation refused: {message}")
            }
            Self::Verify(message) => write!(formatter, "payment verification failed: {message}"),
            Self::FeeMismatch {
                face,
                received,
                predicted_fee,
            } => write!(
                formatter,
                "fee mismatch after swap: face={face} received={received} predicted_fee={predicted_fee} (wallet credit intact; do not journal)"
            ),
            Self::MintUnreachable {
                reason,
                mint,
                detail,
            } => write!(
                formatter,
                "{reason}: mint {mint} unreachable within bound ({detail})"
            ),
        }
    }
}

impl std::error::Error for PaymentWalletError {}

/// Constructs typed payment terms under an explicit mint allowlist.
pub struct PaymentPolicy {
    allowed_mints: HashSet<MintUrl>,
}

impl PaymentPolicy {
    /// Creates a policy from the complete allowed test-mint set.
    pub fn new(allowed_mints: impl IntoIterator<Item = MintUrl>) -> Self {
        Self {
            allowed_mints: allowed_mints.into_iter().collect(),
        }
    }

    /// Maps a validated offer + accepted seller into shared typed terms, at the *realized* mint
    /// the buyer actually paid at.
    ///
    /// The mint is not read off the offer (`offer.mint_url` is dead here).
    /// It is the mint the buyer declared in its NUT-18 payload (`payload.mint`) — the seller pins
    /// the redeem terms to what was actually paid. `amount`/`unit` are still copied from the offer,
    /// which is exactly what the seller-authored `creq` copied (`creq.a`/`creq.u`), so checking a
    /// redeem against these terms IS checking it against the creq. The realized mint must be one
    /// the seller advertised (`∈ accepted_mints == allowed_mints`), else `wrong_mint`.
    pub fn terms_for_offer(
        &self,
        realized_mint: MintUrl,
        offer: &ParsedOffer,
        accepted_seller: &str,
    ) -> Result<PaymentTerms, PaymentWalletError> {
        offer
            .assert_seller_matches(accepted_seller)
            .map_err(|error| PaymentWalletError::Policy(error.to_string()))?;
        let unit = CurrencyUnit::from_str(&offer.unit).map_err(|error| {
            PaymentWalletError::Policy(format!(
                "unsupported payment unit {:?}: {error}",
                offer.unit
            ))
        })?;
        if unit != CurrencyUnit::Sat {
            return Err(PaymentWalletError::Policy(format!(
                "unsupported payment unit {:?}",
                offer.unit
            )));
        }
        if !self.allowed_mints.contains(&realized_mint) {
            return Err(PaymentWalletError::Policy(format!(
                "wrong_mint: realized mint {realized_mint} is outside the seller's accepted_mints"
            )));
        }
        let seller_nostr_pubkey = NostrPublicKey::parse(accepted_seller).map_err(|error| {
            PaymentWalletError::Policy(format!("invalid accepted seller key: {error}"))
        })?;
        let seller_p2pk_lock =
            CashuPublicKey::from_str(&format!("02{}", seller_nostr_pubkey.to_hex())).map_err(
                |error| PaymentWalletError::Policy(format!("invalid seller P2PK lock: {error}")),
            )?;

        Ok(PaymentTerms::new(
            realized_mint,
            Amount::from(offer.amount),
            unit,
            seller_nostr_pubkey,
            seller_p2pk_lock,
        ))
    }
}

/// Redeem guard: the paid token's mint must be one the seller advertised in its
/// `creq` (`∈ accepted_mints`) AND must equal the mint the buyer declared in its NUT-18 payload
/// (`payload.mint`). A token from any other mint is refused `wrong_mint` — no swap runs, so no
/// funds move; the buyer re-pays from a listed mint.
pub fn assert_redeem_mint(
    token_mint: &MintUrl,
    payload_mint: &MintUrl,
    accepted_mints: &HashSet<MintUrl>,
) -> Result<(), PaymentWalletError> {
    if !accepted_mints.contains(payload_mint) {
        return Err(PaymentWalletError::Policy(format!(
            "wrong_mint: payload mint {payload_mint} is not in the seller's accepted_mints"
        )));
    }
    if token_mint != payload_mint {
        return Err(PaymentWalletError::Policy(format!(
            "wrong_mint: token mint {token_mint} does not equal payload mint {payload_mint}"
        )));
    }
    Ok(())
}

/// Buyer wallet adapter backed by CDK's persisted send sagas.
pub struct CdkBuyerMint<'a> {
    wallet: &'a Wallet,
}

impl<'a> CdkBuyerMint<'a> {
    /// Creates an adapter over one mint-and-unit wallet.
    pub fn new(wallet: &'a Wallet) -> Self {
        Self { wallet }
    }

    /// Returns the existing token for an attempt or creates one seller-locked send.
    ///
    /// Crate-private: this is the raw seller-locked send primitive. Sealed so the only
    /// out-of-crate spend path stays `authorize_pay` → `PaymentService::run` (budget-gated).
    pub(crate) async fn lock_or_reconcile(
        &self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<LockedPayment, PaymentWalletError> {
        require_wallet_matches(self.wallet, terms)?;
        self.recover_unmapped_sagas().await?;
        if let Some(token) = self.reconcile(attempt_id, terms).await? {
            require_realized_locked_token(&token, terms)?;
            return Ok(LockedPayment::new(token));
        }
        // N=1 floor from live keyset (fail-closed). Input-count re-check happens
        // after prepare_send against CDK's send_fee / get_proofs_fee.
        require_fee_safe_amount(self.wallet, terms.amount).await?;
        let mut options = SendOptions {
            conditions: Some(SpendingConditions::new_p2pk(terms.seller_p2pk_lock, None)),
            ..SendOptions::default()
        };
        options
            .metadata
            .insert(ATTEMPT_METADATA.into(), attempt_id.as_str().into());
        let prepared = self
            .wallet
            .prepare_send(terms.amount, options)
            .await
            .map_err(wallet_error)?;
        // Redeem fee = CDK input-count fee on the proofs the seller will present.
        // prepared.send_fee() is that same fee API the send path uses.
        let send_fee = prepared.send_fee();
        if terms.amount <= send_fee {
            prepared.cancel().await.map_err(wallet_error)?;
            return Err(PaymentWalletError::Policy(format!(
                "dust vs mint input fee after prepare: amount={} fee={send_fee}; need amount >= fee+1",
                terms.amount
            )));
        }
        let token = match prepared.confirm(None).await {
            Ok(token) => token,
            Err(error) => {
                // Definitive confirm failure should leave no residual ProofsReserved
                // (CDK compensates). Any leftover is handled on the next recover.
                return Err(wallet_error(error));
            }
        };
        if let Err(error) = require_realized_locked_token(&token, terms) {
            // Confirm already minted TokenCreated — revoke that branch (not
            // ProofsReserved retire). Pure cleanup; no receipt / Closed.
            if let Err(revoke_error) = self.revoke_attempt_token_created(attempt_id).await {
                return Err(PaymentWalletError::Reconcile(format!(
                    "{error}; revoke after zero/mismatch realized token also failed: {revoke_error}"
                )));
            }
            return Err(error);
        }
        Ok(LockedPayment::new(token))
    }

    async fn revoke_attempt_token_created(
        &self,
        attempt_id: &AttemptId,
    ) -> Result<(), PaymentWalletError> {
        let matches = self
            .wallet
            .list_transactions(Some(TransactionDirection::Outgoing))
            .await
            .map_err(wallet_error)?
            .into_iter()
            .filter(|transaction| {
                transaction
                    .metadata
                    .get(ATTEMPT_METADATA)
                    .map(String::as_str)
                    == Some(attempt_id.as_str())
            })
            .collect::<Vec<_>>();
        let Some(transaction) = matches.first() else {
            return Err(PaymentWalletError::Reconcile(
                "zero/mismatch realized token has no outgoing transaction to revoke".into(),
            ));
        };
        let Some(saga_id) = transaction.saga_id else {
            return Err(PaymentWalletError::Reconcile(
                "zero/mismatch realized token transaction has no saga id".into(),
            ));
        };
        self.wallet
            .revoke_send(saga_id)
            .await
            .map_err(wallet_error)?;
        Ok(())
    }

    async fn reconcile(
        &self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<Option<Token>, PaymentWalletError> {
        let matches = self
            .wallet
            .list_transactions(Some(TransactionDirection::Outgoing))
            .await
            .map_err(wallet_error)?
            .into_iter()
            .filter(|transaction| {
                transaction
                    .metadata
                    .get(ATTEMPT_METADATA)
                    .map(String::as_str)
                    == Some(attempt_id.as_str())
            })
            .collect::<Vec<_>>();
        let transaction = match matches.as_slice() {
            [] => return Ok(None),
            [transaction] => transaction,
            _ => {
                return Err(PaymentWalletError::Reconcile(
                    "multiple wallet transactions claim the same payment attempt".into(),
                ));
            }
        };
        if transaction.mint_url != terms.mint
            || transaction.unit != terms.unit
            || transaction.amount != terms.amount
        {
            return Err(PaymentWalletError::Reconcile(
                "persisted wallet transaction does not match payment terms".into(),
            ));
        }
        let proofs = self
            .wallet
            .get_proofs_for_transaction(transaction.id())
            .await
            .map_err(wallet_error)?;
        let expected_ys = transaction.ys.iter().copied().collect::<HashSet<_>>();
        let actual_ys = proofs
            .iter()
            .map(|proof| proof.y())
            .collect::<Result<HashSet<_>, _>>()
            .map_err(wallet_error)?;
        if actual_ys != expected_ys {
            return Err(PaymentWalletError::Reconcile(
                "persisted payment proofs do not match the confirmed transaction".into(),
            ));
        }
        let token = Token::new(
            transaction.mint_url.clone(),
            proofs,
            transaction.memo.clone(),
            transaction.unit.clone(),
        );
        require_realized_locked_token(&token, terms).map_err(|error| {
            PaymentWalletError::Reconcile(format!(
                "persisted payment proofs fail realized-output gate: {error}"
            ))
        })?;
        Ok(Some(token))
    }

    async fn recover_unmapped_sagas(&self) -> Result<(), PaymentWalletError> {
        retire_eligible_incomplete_sagas(self.wallet).await?;
        let incomplete = self
            .wallet
            .localstore
            .get_incomplete_sagas()
            .await
            .map_err(wallet_error)?
            .into_iter()
            .filter(|saga| saga.mint_url == self.wallet.mint_url && saga.unit == self.wallet.unit)
            .collect::<Vec<_>>();
        if incomplete.is_empty() {
            return Ok(());
        }
        for saga in &incomplete {
            match &saga.state {
                WalletSagaState::Send(SendSagaState::TokenCreated)
                    if saga_has_confirmed_outgoing_tx(self.wallet, saga).await? =>
                {
                    // Mapped pending claim — not a wedge; must not block a new attempt.
                    continue;
                }
                WalletSagaState::Send(SendSagaState::ProofsReserved) => {
                    return Err(PaymentWalletError::Reconcile(
                        "wallet has an incomplete ProofsReserved operation that could not be retired safely".into(),
                    ));
                }
                WalletSagaState::Send(SendSagaState::TokenCreated) => {
                    return Err(PaymentWalletError::Reconcile(
                        "wallet has an incomplete TokenCreated operation with no matching confirmed attempt".into(),
                    ));
                }
                WalletSagaState::Send(SendSagaState::RollingBack) => {
                    return Err(PaymentWalletError::Reconcile(
                        "wallet has an in-flight RollingBack send; refuse rather than retire".into(),
                    ));
                }
                other => {
                    return Err(PaymentWalletError::Reconcile(format!(
                        "wallet has an incomplete non-eligible saga ({}); refuse rather than retire",
                        other.state_str()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Classified outcome of a bounded mint fee query.
enum BoundedFee {
    /// Live fee read within the bound.
    Fee(Amount),
    /// The mint did not answer within the bound — dead/unroutable or timed out.
    /// Carries a human-readable detail (not a reason code; the caller labels it).
    Unreachable(String),
    /// A non-transport fee-query failure (fail-closed — never default the fee).
    Failed(PaymentWalletError),
}

/// A transport-class cdk error means the mint returned no HTTP response at all —
/// connection refused, DNS/routing failure, or connect timeout: `HttpError` with
/// no status code. These are the "configured mint is down" signals.
fn is_mint_unreachable(error: &cdk::Error) -> bool {
    matches!(error, cdk::Error::HttpError(None, _))
}

fn mint_unreachable(
    wallet: &Wallet,
    reason: &'static str,
    detail: String,
) -> PaymentWalletError {
    PaymentWalletError::MintUnreachable {
        reason,
        mint: wallet.mint_url.to_string(),
        detail,
    }
}

/// Live active-keyset redeem fee for `proof_count` inputs (`ceil(Σ ppk / 1000)`),
/// raw so the bounded wrapper can classify transport failures.
async fn mint_input_fee_for_count_raw(
    wallet: &Wallet,
    proof_count: u64,
) -> Result<Amount, cdk::Error> {
    let keyset = wallet.fetch_active_keyset().await?;
    wallet.get_keyset_count_fee(&keyset.id, proof_count).await
}

/// Live active-keyset redeem fee bounded by `timeout`.
///
/// A dead/unroutable mint (transport failure) or an elapsed deadline classifies as
/// [`BoundedFee::Unreachable`] instead of hanging past the caller's MCP tool
/// deadline; other fee-query errors are [`BoundedFee::Failed`] (never defaulted).
async fn mint_input_fee_bounded(
    wallet: &Wallet,
    proof_count: u64,
    timeout: Duration,
) -> BoundedFee {
    match tokio::time::timeout(timeout, mint_input_fee_for_count_raw(wallet, proof_count)).await {
        Err(_elapsed) => BoundedFee::Unreachable(format!("fee query exceeded {timeout:?}")),
        Ok(Ok(fee)) => BoundedFee::Fee(fee),
        Ok(Err(error)) if is_mint_unreachable(&error) => {
            BoundedFee::Unreachable(format!("fee query transport failure: {error}"))
        }
        Ok(Err(error)) => {
            BoundedFee::Failed(PaymentWalletError::Wallet(format!("fee query failed: {error}")))
        }
    }
}

/// N=`proof_count` fee floor from the lowest-fee active keyset cached in the wallet
/// DB for this mint+unit — a pure localstore read (no network). `None` if no such
/// keyset is cached. Used as the post-time fallback when the mint is unreachable.
async fn cached_input_fee_floor(
    wallet: &Wallet,
    proof_count: u64,
) -> Result<Option<Amount>, PaymentWalletError> {
    let cached = wallet
        .localstore
        .get_mint_keysets(wallet.mint_url.clone())
        .await
        .map_err(|error| {
            PaymentWalletError::Wallet(format!("cached keyset read failed: {error}"))
        })?;
    let floor = cached
        .unwrap_or_default()
        .into_iter()
        .filter(|keyset| keyset.active && keyset.unit == wallet.unit)
        .map(|keyset| keyset.input_fee_ppk)
        .min();
    Ok(floor.map(|ppk| Amount::from((ppk * proof_count).div_ceil(1000))))
}

/// Refuse amounts that cannot yield a redeemable locked token after mint input fees.
///
/// Uses the N=1 floor from the live keyset (`ceil(ppk/1000)`), bounded so a dead
/// mint refuses fast with `mint_unreachable_pay` instead of hanging.
/// Callers that know the real input set must also gate on CDK
/// `get_proofs_fee` / `send_fee`.
pub async fn require_fee_safe_amount(
    wallet: &Wallet,
    amount: Amount,
) -> Result<Amount, PaymentWalletError> {
    let fee = match mint_input_fee_bounded(wallet, 1, MINT_TOUCH_TIMEOUT).await {
        BoundedFee::Fee(fee) => fee,
        BoundedFee::Failed(error) => return Err(error),
        BoundedFee::Unreachable(detail) => {
            return Err(mint_unreachable(wallet, MINT_UNREACHABLE_PAY, detail));
        }
    };
    require_amount_covers_fee(amount, fee)?;
    Ok(fee)
}

/// Post-time dust guard: same N=1 floor, but degrades to the cached keyset fee
/// floor when the mint is unreachable so posting (which needs no funds) is not
/// hard-blocked by a dead mint.
///
/// Fail-closed: a guard that can read NO fee at all — neither live nor cached —
/// refuses (fast, with `mint_unreachable`); it never silently skips the dust check.
pub async fn require_fee_safe_amount_for_post(
    wallet: &Wallet,
    amount: Amount,
) -> Result<Amount, PaymentWalletError> {
    let fee = match mint_input_fee_bounded(wallet, 1, MINT_TOUCH_TIMEOUT).await {
        BoundedFee::Fee(fee) => fee,
        BoundedFee::Failed(error) => return Err(error),
        BoundedFee::Unreachable(detail) => match cached_input_fee_floor(wallet, 1).await? {
            Some(fee) => fee,
            None => {
                return Err(mint_unreachable(
                    wallet,
                    MINT_UNREACHABLE_POST,
                    format!("{detail}; no cached keyset for a fee floor"),
                ));
            }
        },
    };
    require_amount_covers_fee(amount, fee)?;
    Ok(fee)
}

/// `amount < fee + 1` (equivalently `amount <= fee`) is economic dust.
pub fn require_amount_covers_fee(
    amount: Amount,
    fee: Amount,
) -> Result<(), PaymentWalletError> {
    if amount <= fee {
        return Err(PaymentWalletError::Policy(format!(
            "dust vs mint fee: amount={amount} fee={fee}; need amount >= fee+1"
        )));
    }
    Ok(())
}

/// Gate on the **realized** locked token after prepare_send/confirm — never input face.
fn require_realized_locked_token(
    token: &Token,
    terms: &PaymentTerms,
) -> Result<(), PaymentWalletError> {
    let mint = token.mint_url().map_err(wallet_error)?;
    let realized = token.value().map_err(wallet_error)?;
    if realized == Amount::ZERO {
        return Err(PaymentWalletError::Policy(
            "realized locked token value is zero after confirm (no materialized outputs)".into(),
        ));
    }
    if realized != terms.amount || mint != terms.mint || token.unit().as_ref() != Some(&terms.unit)
    {
        return Err(PaymentWalletError::Policy(format!(
            "realized locked token does not match terms: realized={realized} expected={}",
            terms.amount
        )));
    }
    Ok(())
}

/// Retire only enumerated-safe incomplete ops: `Send(ProofsReserved)` with no
/// confirmed attempt, non-empty reserved set, all reserved `y` NUT-07 Unspent,
/// and cancel succeeding.
///
/// Pure cleanup — no receipt, no balance credit. Idempotent (second call is a
/// no-op when nothing eligible remains). Per-saga fail-closed: Spent|Pending /
/// empty-reserved / check-state fail / cancel fail ⇒ refuse that saga's retire
/// (wedged-safer-than-double-spend). Not atomic across sagas — earlier sagas in
/// the same call may already have retired before a later refuse aborts the loop.
///
/// NUT-07 uses non-mutating `post_check_state` — never CDK `check_proofs_spent`,
/// which deletes mint-Spent `y`s from localstore and would make a second retire
/// see empty-reserved and falsely auto-retire.
///
/// **Migration edge (fail-closed):** empty-reserved is ALWAYS refused. Wallets
/// that previously ran destructive `check_proofs_spent` can hold empty sagas
/// that were Spent-then-deleted, indistinguishable from never-bound orphans —
/// auto-retiring either class would reopen the double-spend hole. Orphans stay
/// wedged-safer; operators can document/manual-clear.
pub async fn retire_eligible_incomplete_sagas(
    wallet: &Wallet,
) -> Result<RetireReport, PaymentWalletError> {
    let incomplete = wallet
        .localstore
        .get_incomplete_sagas()
        .await
        .map_err(wallet_error)?
        .into_iter()
        .filter(|saga| saga.mint_url == wallet.mint_url && saga.unit == wallet.unit)
        .collect::<Vec<_>>();

    let mut report = RetireReport::default();
    if incomplete.is_empty() {
        return Ok(report);
    }

    for saga in incomplete {
        match &saga.state {
            WalletSagaState::Send(SendSagaState::ProofsReserved) => {
                if saga_has_confirmed_outgoing_tx(wallet, &saga).await? {
                    return Err(PaymentWalletError::Reconcile(
                        "ProofsReserved saga unexpectedly has a confirmed outgoing tx; refuse retire".into(),
                    ));
                }
                retire_one_proofs_reserved(wallet, &saga).await?;
                report.retired += 1;
            }
            WalletSagaState::Send(SendSagaState::TokenCreated)
                if saga_has_confirmed_outgoing_tx(wallet, &saga).await? =>
            {
                report.mapped_token_created += 1;
            }
            // Swap-family sagas (proofs_reserved / swap_requested): resolve on mint
            // truth. A mid-swap mint outage otherwise strands these
            // forever — retire never handled them and recover_unmapped_sagas refused
            // them via its `other` arm. Resolve deletes resolvable sagas; a
            // mixed/pending/unreachable answer returns Err (fail-closed).
            WalletSagaState::Swap(_) => {
                resolve_one_swap_saga(wallet, &saga, &mut report).await?;
            }
            // TokenCreated without confirmed tx, RollingBack, other kinds: leave
            // in place for recover_unmapped_sagas to refuse. Do not retire here.
            _ => {}
        }
    }
    Ok(report)
}

async fn saga_has_confirmed_outgoing_tx(
    wallet: &Wallet,
    saga: &cdk::wallet::types::WalletSaga,
) -> Result<bool, PaymentWalletError> {
    let txs = wallet
        .list_transactions(Some(TransactionDirection::Outgoing))
        .await
        .map_err(wallet_error)?;
    Ok(txs.iter().any(|tx| tx.saga_id == Some(saga.id)))
}

/// NUT-07 via mint connector only — does not mutate localstore.
async fn nut07_check_state_non_mutating(
    wallet: &Wallet,
    ys: Vec<CashuPublicKey>,
) -> Result<Vec<cashu::ProofState>, PaymentWalletError> {
    let response = wallet
        .mint_connector()
        .post_check_state(CheckStateRequest { ys })
        .await
        .map_err(|error| {
            PaymentWalletError::Reconcile(format!(
                "check-state failed (fail-closed, no retire): {error}"
            ))
        })?;
    Ok(response.states)
}

/// Require a complete NUT-07 answer: response `Y` set == requested ys, and every
/// reported state Unspent. Empty / partial / wrong-y responses must refuse —
/// treating them as all-Unspent would false-retire possibly mint-Spent proofs
/// into local spendable (phantom credit).
fn refuse_if_not_all_unspent(
    requested_ys: &[CashuPublicKey],
    states: &[cashu::ProofState],
) -> Result<(), PaymentWalletError> {
    let requested: HashSet<_> = requested_ys.iter().copied().collect();
    let reported: HashSet<_> = states.iter().map(|proof_state| proof_state.y).collect();
    if requested.is_empty() || requested != reported {
        return Err(PaymentWalletError::Reconcile(
            "retire refused: NUT-07 response Y set incomplete or mismatched (empty/partial/wrong-y; per-saga fail-closed)"
                .into(),
        ));
    }
    if states
        .iter()
        .any(|proof_state| proof_state.state != State::Unspent)
    {
        return Err(PaymentWalletError::Reconcile(
            "retire refused: reserved proof mint state is Spent or Pending (per-saga fail-closed)"
                .into(),
        ));
    }
    Ok(())
}

async fn retire_one_proofs_reserved(
    wallet: &Wallet,
    saga: &cdk::wallet::types::WalletSaga,
) -> Result<(), PaymentWalletError> {
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;

    // Migration edge fail-closed: empty-reserved ALWAYS refused. Spent-then-
    // deleted under old check_proofs_spent is indistinguishable from a never-
    // bound orphan — auto-retire of either reopens the double-spend hole.
    if reserved.is_empty() {
        return Err(PaymentWalletError::Reconcile(
            "retire refused: empty reserved set (migration-safe fail-closed; Spent-deleted and orphan are indistinguishable; leave wedged-safer-than-double-spend)"
                .into(),
        ));
    }

    let ys = reserved.iter().map(|info| info.y).collect::<Vec<_>>();
    // Never use check_proofs_spent — it deletes mint-Spent ys from localstore.
    let states = nut07_check_state_non_mutating(wallet, ys.clone()).await?;
    refuse_if_not_all_unspent(&ys, &states)?;

    // Pre-mutate TOCTOU: re-fetch ProofsReserved ∧ no confirmed tx ∧ Unspent
    // immediately before local Unspent+delete (concurrent authorize_pay confirm).
    let fresh = wallet
        .localstore
        .get_saga(&saga.id)
        .await
        .map_err(wallet_error)?
        .ok_or_else(|| {
            PaymentWalletError::Reconcile(
                "retire refused: saga disappeared before mutate (leave wedged-safer)".into(),
            )
        })?;
    if !matches!(
        fresh.state,
        WalletSagaState::Send(SendSagaState::ProofsReserved)
    ) {
        return Err(PaymentWalletError::Reconcile(
            "retire refused: saga no longer ProofsReserved before mutate (TOCTOU)".into(),
        ));
    }
    if saga_has_confirmed_outgoing_tx(wallet, &fresh).await? {
        return Err(PaymentWalletError::Reconcile(
            "retire refused: confirmed outgoing tx appeared before mutate (TOCTOU)".into(),
        ));
    }

    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    if reserved.is_empty() {
        return Err(PaymentWalletError::Reconcile(
            "retire refused: reserved emptied before mutate (TOCTOU / migration-safe fail-closed)"
                .into(),
        ));
    }

    let ys = reserved.iter().map(|info| info.y).collect::<Vec<_>>();
    let states = nut07_check_state_non_mutating(wallet, ys.clone()).await?;
    refuse_if_not_all_unspent(&ys, &states)?;

    revert_reserved_and_delete_saga(wallet, saga).await
}

/// Revert a saga's reserved proofs to spendable and delete the saga, matching CDK
/// `compensate_send`: PendingSpent → Reserved locally, then Reserved/Pending →
/// Unspent (clearing `used_by_operation`), then delete the saga. TOCTOU: any
/// failure aborts with an error — never report success (leave wedged-safer-than-
/// double-spend). Callers MUST first prove the reserved proofs are all Unspent at
/// the mint immediately before invoking this.
async fn revert_reserved_and_delete_saga(
    wallet: &Wallet,
    saga: &cdk::wallet::types::WalletSaga,
) -> Result<(), PaymentWalletError> {
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    let mut pending_spent: Vec<_> = reserved
        .iter()
        .filter(|proof| proof.state == State::PendingSpent)
        .cloned()
        .collect();
    for proof in pending_spent.iter_mut() {
        proof.state = State::Reserved;
    }
    if !pending_spent.is_empty() {
        wallet
            .localstore
            .update_proofs(pending_spent, vec![])
            .await
            .map_err(|error| {
                PaymentWalletError::Reconcile(format!(
                    "retire cancel failed (leave wedged-safer-than-double-spend): {error}"
                ))
            })?;
    }

    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    let mut to_unspent: Vec<_> = reserved
        .into_iter()
        .filter(|proof| proof.state == State::Reserved || proof.state == State::Pending)
        .collect();
    for proof in to_unspent.iter_mut() {
        proof.state = State::Unspent;
        proof.used_by_operation = None;
    }
    if !to_unspent.is_empty() {
        wallet
            .localstore
            .update_proofs(to_unspent, vec![])
            .await
            .map_err(|error| {
                PaymentWalletError::Reconcile(format!(
                    "retire cancel failed (leave wedged-safer-than-double-spend): {error}"
                ))
            })?;
    }
    wallet
        .localstore
        .delete_saga(&saga.id)
        .await
        .map_err(|error| {
            PaymentWalletError::Reconcile(format!(
                "retire cancel failed deleting saga (leave wedged-safer-than-double-spend): {error}"
            ))
        })?;
    Ok(())
}

/// Mint-truth outcome for a stranded Swap saga's reserved INPUT proofs.
enum SwapInputTruth {
    /// Every reserved input is UNSPENT at the mint — the swap never executed.
    AllUnspent,
    /// Every reserved input is SPENT at the mint — the swap executed.
    AllSpent,
}

/// Classify a Swap saga's reserved inputs from a NUT-07 answer. Requires a
/// complete answer (response Y-set == requested ys); empty/partial/wrong-y refuses
/// exactly like [`refuse_if_not_all_unspent`]. Any mix of Spent/Unspent, or any
/// Pending, refuses fail-closed — only a unanimous answer is actionable.
fn classify_swap_inputs(
    requested_ys: &[CashuPublicKey],
    states: &[cashu::ProofState],
) -> Result<SwapInputTruth, PaymentWalletError> {
    let requested: HashSet<_> = requested_ys.iter().copied().collect();
    let reported: HashSet<_> = states.iter().map(|proof_state| proof_state.y).collect();
    if requested.is_empty() || requested != reported {
        return Err(PaymentWalletError::Reconcile(
            "swap-saga resolve refused: NUT-07 response Y set incomplete or mismatched (empty/partial/wrong-y; per-saga fail-closed)"
                .into(),
        ));
    }
    if states
        .iter()
        .all(|proof_state| proof_state.state == State::Unspent)
    {
        Ok(SwapInputTruth::AllUnspent)
    } else if states
        .iter()
        .all(|proof_state| proof_state.state == State::Spent)
    {
        Ok(SwapInputTruth::AllSpent)
    } else {
        Err(PaymentWalletError::Reconcile(
            "swap-saga resolve refused: reserved inputs neither all-unspent nor all-spent (mixed/pending; per-saga fail-closed)"
                .into(),
        ))
    }
}

/// Mint-truth resolution for a stranded Swap-family saga.
///
/// A mid-swap mint outage can leave a `swap_requested` (or `proofs_reserved`) Swap
/// saga that `retire_eligible_incomplete_sagas` never handled and that
/// `recover_unmapped_sagas` refused forever, wedging every subsequent pay. Once
/// the mint is reachable we resolve on mint truth over the reserved INPUT proofs:
///   * all UNSPENT ⇒ the swap never executed ⇒ roll the inputs back to spendable
///     and drop the saga (via [`revert_reserved_and_delete_saga`]).
///   * all SPENT   ⇒ the swap executed ⇒ re-derive the outputs from the wallet
///     seed (NUT-13 `Wallet::restore`), then mark the inputs spent and drop the
///     saga (via [`complete_spent_swap_saga`]).
///   * mixed/pending, incomplete NUT-07, empty reservation, or an unreachable mint
///     ⇒ keep refusing fail-closed.
async fn resolve_one_swap_saga(
    wallet: &Wallet,
    saga: &cdk::wallet::types::WalletSaga,
    report: &mut RetireReport,
) -> Result<(), PaymentWalletError> {
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    // Migration-safe fail-closed: an empty reservation is ambiguous (never-bound
    // orphan vs Spent-then-deleted under old check_proofs_spent) — same posture as
    // the Send path. Refuse rather than guess.
    if reserved.is_empty() {
        return Err(PaymentWalletError::Reconcile(
            "swap-saga resolve refused: empty reserved set (migration-safe fail-closed; leave wedged-safer-than-double-spend)"
                .into(),
        ));
    }
    let ys = reserved.iter().map(|info| info.y).collect::<Vec<_>>();
    // Non-mutating NUT-07 — never check_proofs_spent (it deletes Spent ys from
    // localstore). A dead mint surfaces as a Reconcile error here and propagates as
    // a fail-closed refuse.
    let states = nut07_check_state_non_mutating(wallet, ys.clone()).await?;
    match classify_swap_inputs(&ys, &states)? {
        SwapInputTruth::AllUnspent => {
            rollback_swap_saga(wallet, saga).await?;
            report.swap_rolled_back += 1;
        }
        SwapInputTruth::AllSpent => {
            complete_spent_swap_saga(wallet, saga).await?;
            report.swap_recovered += 1;
        }
    }
    Ok(())
}

/// Roll back a Swap saga whose inputs are all UNSPENT at the mint: the
/// swap never executed, so restore the reserved inputs to spendable and drop the
/// saga. TOCTOU: re-fetch and re-prove (still a Swap saga, reservation non-empty,
/// still all-unspent at the mint) immediately before mutating.
async fn rollback_swap_saga(
    wallet: &Wallet,
    saga: &cdk::wallet::types::WalletSaga,
) -> Result<(), PaymentWalletError> {
    let fresh = wallet
        .localstore
        .get_saga(&saga.id)
        .await
        .map_err(wallet_error)?
        .ok_or_else(|| {
            PaymentWalletError::Reconcile(
                "swap-saga rollback refused: saga disappeared before mutate (leave wedged-safer)"
                    .into(),
            )
        })?;
    if !matches!(fresh.state, WalletSagaState::Swap(_)) {
        return Err(PaymentWalletError::Reconcile(
            "swap-saga rollback refused: saga no longer a Swap saga before mutate (TOCTOU)".into(),
        ));
    }
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    if reserved.is_empty() {
        return Err(PaymentWalletError::Reconcile(
            "swap-saga rollback refused: reserved emptied before mutate (TOCTOU / migration-safe fail-closed)"
                .into(),
        ));
    }
    let ys = reserved.iter().map(|info| info.y).collect::<Vec<_>>();
    let states = nut07_check_state_non_mutating(wallet, ys.clone()).await?;
    if !matches!(
        classify_swap_inputs(&ys, &states)?,
        SwapInputTruth::AllUnspent
    ) {
        return Err(PaymentWalletError::Reconcile(
            "swap-saga rollback refused: inputs no longer all-unspent before mutate (TOCTOU)".into(),
        ));
    }
    revert_reserved_and_delete_saga(wallet, saga).await
}

/// Complete a Swap saga whose inputs are all SPENT at the mint: the
/// swap executed. Re-derive the outputs from the wallet seed via NUT-13
/// `Wallet::restore` — the only output-recovery path cdk 0.17.2 exposes publicly
/// (the saga-scoped `restore_outputs` is crate-private) — then mark the spent
/// inputs Spent and drop the saga. Restore runs FIRST so a restore-unreachable
/// mint aborts before any local mutation (fail-closed; the saga stays wedged for a
/// later retry rather than losing the outputs).
async fn complete_spent_swap_saga(
    wallet: &Wallet,
    saga: &cdk::wallet::types::WalletSaga,
) -> Result<(), PaymentWalletError> {
    // Re-derive the swap outputs from seed+counter (NUT-13). Idempotent: a re-run
    // rebuilds the same proofs. Any error aborts before we touch local state.
    wallet.restore().await.map_err(|error| {
        PaymentWalletError::Reconcile(format!(
            "swap-saga restore refused (leave wedged for a later retry): {error}"
        ))
    })?;

    // The inputs are confirmed Spent at the mint — record that truthfully (no
    // phantom credit) and clear their operation binding. `restore` may already have
    // swept some inputs via its own state check, so tolerate a shrunk reservation.
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    if !reserved.is_empty() {
        let mut spent = reserved;
        for proof in spent.iter_mut() {
            proof.state = State::Spent;
            proof.used_by_operation = None;
        }
        wallet
            .localstore
            .update_proofs(spent, vec![])
            .await
            .map_err(|error| {
                PaymentWalletError::Reconcile(format!(
                    "swap-saga complete failed marking inputs spent: {error}"
                ))
            })?;
    }
    wallet
        .localstore
        .delete_saga(&saga.id)
        .await
        .map_err(|error| {
            PaymentWalletError::Reconcile(format!(
                "swap-saga complete failed deleting saga: {error}"
            ))
        })?;
    Ok(())
}

struct CdkPaymentVerifier<'a, C: ?Sized> {
    connector: &'a C,
}

impl<'a, C: ?Sized> CdkPaymentVerifier<'a, C> {
    fn new(connector: &'a C) -> Self {
        Self { connector }
    }
}

impl<C: MintConnector + ?Sized> CdkPaymentVerifier<'_, C> {
    async fn verify(
        &self,
        locked: &LockedPayment,
        terms: &PaymentTerms,
    ) -> Result<VerifiedPayment, PaymentWalletError> {
        let lock = TradeLock {
            mint: terms.mint.clone(),
            amount: terms.amount,
            unit: terms.unit.clone(),
            seller_lock: terms.seller_p2pk_lock,
        };
        verify_trade_p2pk_with_connector(self.connector, locked.token(), &lock)
            .await
            .map_err(|error| PaymentWalletError::Verify(error.to_string()))
    }
}

/// Seller wallet adapter whose successful receive is the mint-authenticity gate.
pub struct CdkSellerReceive<'a> {
    wallet: &'a Wallet,
    seller_key: SecretKey,
}

enum BuyerCommand {
    Lock {
        attempt_id: AttemptId,
        terms: PaymentTerms,
        response: mpsc::SyncSender<Result<LockedPayment, PaymentWalletError>>,
    },
    Verify {
        token: Token,
        terms: PaymentTerms,
        response: mpsc::SyncSender<Result<VerifiedPayment, PaymentWalletError>>,
    },
    Send {
        /// Job id — becomes the NUT-18 payload `id` (echoes the seller request's `i`).
        job_id: String,
        /// NIP-17 gift-wrap recipient (the seller, hex).
        seller_pubkey: String,
        /// The P2PK-locked token to pay with. Decomposed to NUT-18 `proofs` in the worker (where
        /// the wallet's mint keysets are available).
        token: Token,
        response: mpsc::SyncSender<Result<PaymentSent, PaymentWalletError>>,
    },
}

/// Build the buyer's NUT-18 [`PaymentRequestPayload`] from a locked token. Decomposes the token
/// into `proofs` using the wallet's mint keysets (a TokenV4 stores short keyset ids that must be
/// expanded), and sets `mint` to the *realized* mint the token came from. Runs on the wallet
/// worker thread.
#[cfg(feature = "wallet")]
async fn build_nut18_payload(
    wallet: &Wallet,
    job_id: String,
    seller_pubkey: String,
    token: Token,
) -> Result<PaymentPayload, PaymentWalletError> {
    let keysets = wallet
        .get_mint_keysets(KeysetFilter::All)
        .await
        .map_err(wallet_error)?;
    let proofs = token.proofs(&keysets).map_err(wallet_error)?;
    let mint = token.mint_url().map_err(wallet_error)?;
    let unit = token
        .unit()
        .ok_or_else(|| PaymentWalletError::Policy("payment token carries no unit".into()))?;
    Ok(PaymentPayload {
        seller_pubkey,
        payload: PaymentRequestPayload {
            id: Some(job_id),
            memo: None,
            mint,
            unit,
            proofs,
        },
    })
}

/// Synchronous state-machine effects backed by one asynchronous wallet worker.
pub struct CdkPaymentEffects<R> {
    commands: Option<tokio::sync::mpsc::Sender<BuyerCommand>>,
    worker: Option<thread::JoinHandle<()>>,
    receipt: R,
}

impl<R> CdkPaymentEffects<R> {
    /// Starts a worker whose verifier is bound to the wallet mint.
    pub fn spawn<S>(wallet: Wallet, payment_send: S, receipt: R) -> Result<Self, PaymentWalletError>
    where
        S: PaymentSend + Send + 'static,
    {
        let connector = HttpClient::new(wallet.mint_url.clone(), None);
        Self::spawn_worker(wallet, connector, payment_send, receipt)
    }

    /// Starts a worker with an injected connector for hermetic tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_with_connector<C, S>(
        wallet: Wallet,
        connector: C,
        payment_send: S,
        receipt: R,
    ) -> Result<Self, PaymentWalletError>
    where
        C: MintConnector + Send + Sync + 'static,
        S: PaymentSend + Send + 'static,
    {
        Self::spawn_worker(wallet, connector, payment_send, receipt)
    }

    fn spawn_worker<C, S>(
        wallet: Wallet,
        connector: C,
        mut payment_send: S,
        receipt: R,
    ) -> Result<Self, PaymentWalletError>
    where
        C: MintConnector + Send + Sync + 'static,
        S: PaymentSend + Send + 'static,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(wallet_error)?;
        let (commands, mut requests) = tokio::sync::mpsc::channel(1);
        let worker = thread::Builder::new()
            .name("mobee-payment-wallet".into())
            .spawn(move || {
                runtime.block_on(async move {
                    while let Some(command) = requests.recv().await {
                        match command {
                            BuyerCommand::Lock {
                                attempt_id,
                                terms,
                                response,
                            } => {
                                let result = CdkBuyerMint::new(&wallet)
                                    .lock_or_reconcile(&attempt_id, &terms)
                                    .await;
                                let _ = response.send(result);
                            }
                            BuyerCommand::Verify {
                                token,
                                terms,
                                response,
                            } => {
                                let locked = LockedPayment::new(token);
                                let result = CdkPaymentVerifier::new(&connector)
                                    .verify(&locked, &terms)
                                    .await;
                                let _ = response.send(result);
                            }
                            BuyerCommand::Send {
                                job_id,
                                seller_pubkey,
                                token,
                                response,
                            } => {
                                let result = match build_nut18_payload(
                                    &wallet,
                                    job_id,
                                    seller_pubkey,
                                    token,
                                )
                                .await
                                {
                                    Ok(payload) => payment_send
                                        .send_payment(payload)
                                        .await
                                        .map_err(|error| {
                                            PaymentWalletError::Wallet(error.to_string())
                                        }),
                                    Err(error) => Err(error),
                                };
                                let _ = response.send(result);
                            }
                        }
                    }
                });
            })
            .map_err(wallet_error)?;
        Ok(Self {
            commands: Some(commands),
            worker: Some(worker),
            receipt,
        })
    }

    fn request<T>(
        &self,
        command: impl FnOnce(mpsc::SyncSender<Result<T, PaymentWalletError>>) -> BuyerCommand,
    ) -> Result<T, EffectError> {
        let (response, result) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .ok_or_else(|| EffectError::new("payment wallet worker is stopped"))?
            .try_send(command(response))
            .map_err(|error| {
                EffectError::new(format!("payment wallet worker unavailable: {error}"))
            })?;
        result
            .recv()
            .map_err(|_| EffectError::new("payment wallet worker dropped its response"))?
            .map_err(|error| EffectError::new(error.to_string()))
    }
}

impl<R> Drop for CdkPaymentEffects<R> {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl<R> PaymentEffects for CdkPaymentEffects<R>
where
    R: FnMut(&PaymentKey, &PaymentSent) -> Result<ReceiptEvidence, EffectError>,
{
    fn lock_or_reconcile(
        &mut self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<LockedPayment, EffectError> {
        self.request(|response| BuyerCommand::Lock {
            attempt_id: attempt_id.clone(),
            terms: terms.clone(),
            response,
        })
    }

    fn verify_payment(
        &mut self,
        _attempt_id: &AttemptId,
        terms: &PaymentTerms,
        locked: &LockedPayment,
    ) -> Result<VerifiedPayment, EffectError> {
        self.request(|response| BuyerCommand::Verify {
            token: locked.token().clone(),
            terms: terms.clone(),
            response,
        })
    }

    fn send_payment(
        &mut self,
        key: &PaymentKey,
        _attempt_id: &AttemptId,
        terms: &PaymentTerms,
        locked: &LockedPayment,
        _verified: &VerifiedPayment,
    ) -> Result<PaymentSent, EffectError> {
        if terms.unit != CurrencyUnit::Sat {
            return Err(EffectError::new(
                "NIP-17 NUT-18 payment payload supports sat only",
            ));
        }
        // The NUT-18 payload (id/mint/unit/proofs) is built on the wallet worker, where the mint
        // keysets needed to decompose the token into proofs live. `id` == the job id.
        let job_id = key.job_id.as_str().to_owned();
        let seller_pubkey = terms.seller_nostr_pubkey.to_hex();
        let token = locked.token().clone();
        self.request(|response| BuyerCommand::Send {
            job_id,
            seller_pubkey,
            token,
            response,
        })
    }

    fn publish_receipt(
        &mut self,
        key: &PaymentKey,
        payment: &PaymentSent,
    ) -> Result<ReceiptEvidence, EffectError> {
        (self.receipt)(key, payment)
    }
}

impl<'a> CdkSellerReceive<'a> {
    /// Creates a receive adapter for the seller's mint wallet and P2PK key.
    pub fn new(wallet: &'a Wallet, seller_key: SecretKey) -> Self {
        Self { wallet, seller_key }
    }

    /// Swaps the received token at its mint before returning its redeemable amount.
    ///
    /// `accepted_mints` is the seller's advertised mint set and `payload_mint` is
    /// the mint the buyer declared in its NUT-18 payload. The redeem guard refuses `wrong_mint`
    /// unless the token's mint is `∈ accepted_mints` AND equals `payload_mint`.
    pub async fn receive(
        &self,
        token: &Token,
        terms: &PaymentTerms,
        accepted_mints: &HashSet<MintUrl>,
        payload_mint: &MintUrl,
    ) -> Result<Amount, PaymentWalletError> {
        self.receive_with(token, terms, accepted_mints, payload_mint, |options| async move {
            self.wallet
                .receive(&token.to_string(), options)
                .await
                .map_err(wallet_error)
        })
        .await
    }

    async fn receive_with<F, Fut>(
        &self,
        token: &Token,
        terms: &PaymentTerms,
        accepted_mints: &HashSet<MintUrl>,
        payload_mint: &MintUrl,
        receive: F,
    ) -> Result<Amount, PaymentWalletError>
    where
        F: FnOnce(ReceiveOptions) -> Fut,
        Fut: Future<Output = Result<Amount, PaymentWalletError>>,
    {
        require_wallet_matches(self.wallet, terms)?;
        let token_mint = token.mint_url().map_err(wallet_error)?;
        let face = token.value().map_err(wallet_error)?;
        // Redeem guard: token mint ∈ accepted_mints AND == payload.mint. `terms.mint` is the
        // realized (payload) mint the seller pinned, so `token_mint != terms.mint` below is a
        // defensive re-check of the same invariant.
        assert_redeem_mint(&token_mint, payload_mint, accepted_mints)?;
        if token_mint != terms.mint || token.unit().as_ref() != Some(&terms.unit) {
            return Err(PaymentWalletError::Policy(
                "wrong_mint: received token mint/unit does not match the realized creq terms".into(),
            ));
        }
        if face != terms.amount {
            return Err(PaymentWalletError::Policy(format!(
                "amount_mismatch: token face {face} does not match creq amount {}",
                terms.amount
            )));
        }
        if self.seller_key.public_key().x_only_public_key()
            != terms.seller_p2pk_lock.x_only_public_key()
        {
            return Err(PaymentWalletError::Policy(
                "seller receive key does not match payment terms".into(),
            ));
        }

        // Fee must be predicted pre-swap: CDK receive returns net after fees.
        let keysets = self
            .wallet
            .get_mint_keysets(KeysetFilter::All)
            .await
            .map_err(wallet_error)?;
        let proofs = token.proofs(&keysets).map_err(wallet_error)?;
        let fee = self
            .wallet
            .get_proofs_fee(&proofs)
            .await
            .map_err(wallet_error)?
            .total;
        if face <= fee {
            return Err(PaymentWalletError::Policy(format!(
                "token uneconomical vs mint fee: face={face} fee={fee}"
            )));
        }

        let options = ReceiveOptions {
            p2pk_signing_keys: vec![self.seller_key.clone()],
            ..ReceiveOptions::default()
        };
        let received = receive(options).await?;
        require_received_amount_after_fee(received, face, fee)
    }
}

fn require_received_amount_after_fee(
    received: Amount,
    face: Amount,
    fee: Amount,
) -> Result<Amount, PaymentWalletError> {
    // Journal/daemon invariants expect face (== offer.amount), not wallet net.
    if received
        .checked_add(fee)
        .is_some_and(|total| total == face)
    {
        return Ok(face);
    }
    if received > Amount::ZERO {
        return Err(PaymentWalletError::FeeMismatch {
            face,
            received,
            predicted_fee: fee,
        });
    }
    Err(PaymentWalletError::Policy(
        "received amount does not match payment terms".into(),
    ))
}

fn require_wallet_matches(wallet: &Wallet, terms: &PaymentTerms) -> Result<(), PaymentWalletError> {
    if wallet.mint_url != terms.mint || wallet.unit != terms.unit {
        return Err(PaymentWalletError::Policy(
            "wallet mint or unit does not match payment terms".into(),
        ));
    }
    Ok(())
}

fn wallet_error(error: impl std::fmt::Display) -> PaymentWalletError {
    PaymentWalletError::Wallet(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use cashu::secret::Secret;
    use cashu::{
        Conditions, Id, KeySet, KeySetInfo, Keys, MintInfo, Proof, ProofState, PublicKey, State,
    };
    use cdk::cdk_database::WalletDatabase;
    use cdk::wallet::types::{ProofInfo, Transaction, TransactionDirection, WalletSaga};
    use cdk::wallet::{BaseHttpClient, HttpTransport, Wallet, WalletBuilder};
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use url::Url;

    use super::*;
    use crate::gateway::ParsedOffer;
    use crate::delivery::{
        CommitOid, DeliveryError, DeliveryVerifier, GitDelivery, VerifiedDelivery,
    };
    use crate::payment::{
        DeliveryIntegrityHash, JobHash, JobId, MemoryPaymentJournal, PaymentKey, PaymentService,
        PaymentState, ReceiptAuthority, ResultId,
    };
    use crate::payment_send::{PaymentSendError, PaymentSent};

    /// Test-only accept verifier so wallet spine tests go through `run_with_verifier`
    /// (delivery tip-bind) instead of the now module-private `advance`.
    struct AcceptDelivery;

    impl DeliveryVerifier for AcceptDelivery {
        fn verify(
            &mut self,
            delivery: &GitDelivery,
        ) -> Result<VerifiedDelivery, DeliveryError> {
            VerifiedDelivery::from_fetched_tip(delivery, delivery.commit_oid().clone())
        }
    }

    const MINT: &str = "https://testnut.cashu.space";
    const OTHER_MINT: &str = "https://real-mint.example";
    const KEYSET_ID: &str = "009a1f293253e41e";

    #[test]
    fn policy_rejects_a_realized_mint_outside_the_allowlist() {
        // The mint is the REALIZED (payload) mint, not read off the offer. A realized mint
        // the seller never advertised is `wrong_mint`.
        let seller = secret_key(1).public_key().to_string();
        let policy = PaymentPolicy::new([mint(MINT)]);
        let offer = offer(&seller);

        let error = policy
            .terms_for_offer(mint(OTHER_MINT), &offer, &seller)
            .unwrap_err();

        assert!(matches!(
            error,
            PaymentWalletError::Policy(message)
                if message.contains("wrong_mint") && message.contains("accepted_mints")
        ));
    }

    #[test]
    fn policy_maps_the_offer_once_into_typed_terms_at_the_realized_mint() {
        let seller_lock = secret_key(1).public_key();
        let seller = nostr_key_for_p2pk(seller_lock).to_hex();
        let policy = PaymentPolicy::new([mint(MINT)]);

        let terms = policy
            .terms_for_offer(mint(MINT), &offer(&seller), &seller)
            .unwrap();

        assert_eq!(terms.mint, mint(MINT));
        assert_eq!(terms.amount, Amount::from(7));
        assert_eq!(terms.unit, CurrencyUnit::Sat);
        assert_eq!(terms.seller_nostr_pubkey.to_hex(), seller);
        assert_eq!(
            terms.seller_p2pk_lock.x_only_public_key(),
            seller_lock.x_only_public_key()
        );
    }

    #[test]
    fn policy_rejects_an_unknown_unit_without_defaulting_to_sat() {
        let seller = secret_key(1).public_key().to_string();
        let policy = PaymentPolicy::new([mint(MINT)]);
        let mut offer = offer(&seller);
        offer.unit = "credit".into();

        let result = policy.terms_for_offer(mint(MINT), &offer, &seller);

        assert!(matches!(
            result,
            Err(PaymentWalletError::Policy(message))
                if message.contains("unsupported payment unit")
        ));
    }

    #[tokio::test]
    async fn confirmed_attempt_reconciles_the_exact_token_without_a_second_send() {
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let attempt_id = key.attempt_id();
        store_confirmed_attempt(&fixture.wallet, &attempt_id, &fixture.token).await;

        let locked = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&attempt_id, &fixture.terms)
            .await
            .unwrap();

        assert_eq!(locked.token(), &fixture.token);
        assert_eq!(
            fixture
                .wallet
                .list_transactions(Some(TransactionDirection::Outgoing))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn confirmed_attempt_without_its_exact_proof_refuses() {
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let attempt_id = key.attempt_id();
        store_confirmed_transaction(&fixture.wallet, &attempt_id, &fixture.proof).await;

        let result = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&attempt_id, &fixture.terms)
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::Reconcile(message))
                if message.contains("proofs do not match the confirmed transaction")
        ));
    }

    #[tokio::test]
    async fn empty_reserved_proofs_reserved_refuses_retire_migration_safe() {
        // Migration edge: empty reserved is ALWAYS refused — Spent-then-deleted
        // under old check_proofs_spent is indistinguishable from never-bound orphan.
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let saga = WalletSaga::new(
            uuid::Uuid::now_v7(),
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            fixture.terms.amount,
            fixture.terms.mint.clone(),
            fixture.terms.unit.clone(),
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: fixture.terms.amount,
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: None,
            }),
        );
        fixture.wallet.localstore.add_saga(saga).await.unwrap();

        let err = retire_eligible_incomplete_sagas(&fixture.wallet)
            .await
            .expect_err("empty-reserved must refuse (migration-safe fail-closed)");
        match &err {
            PaymentWalletError::Reconcile(message)
                if message.contains("empty reserved") && message.contains("fail-closed") => {}
            other => panic!("expected empty-reserved refuse, got: {other}"),
        }
        assert_eq!(
            fixture
                .wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1,
            "empty-reserved saga must remain"
        );
        let err2 = retire_eligible_incomplete_sagas(&fixture.wallet)
            .await
            .expect_err("empty-reserved refuse must be sticky");
        match &err2 {
            PaymentWalletError::Reconcile(message) if message.contains("empty reserved") => {}
            other => panic!("expected sticky empty-reserved refuse, got: {other}"),
        }

        // recover path still sees the incomplete saga (wedged-safer, no auto-clear).
        let result = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&key.attempt_id(), &fixture.terms)
            .await;
        match &result {
            Err(PaymentWalletError::Reconcile(message))
                if message.contains("empty reserved")
                    || message.contains("incomplete operation") => {}
            Err(other) => panic!("expected wedged refuse, got: {other}"),
            Ok(_) => panic!("expected wedged refuse, got Ok"),
        }
    }

    #[tokio::test]
    async fn proofs_reserved_with_spent_mint_state_refuses_retire() {
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        // Insert as Unspent; reserve_proofs requires Unspent → marks Reserved.
        let proof_info = ProofInfo::new(
            proof.clone(),
            mint(MINT),
            State::Unspent,
            CurrencyUnit::Sat,
        )
        .unwrap();
        let saga_id = uuid::Uuid::now_v7();
        store.update_proofs(vec![proof_info], vec![]).await.unwrap();
        store
            .reserve_proofs(vec![proof_y], &saga_id)
            .await
            .unwrap();
        let saga = WalletSaga::new(
            saga_id,
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            Amount::from(7),
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: Amount::from(7),
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: Some(vec![proof.clone()]),
            }),
        );
        store.add_saga(saga).await.unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: vec![ProofState::from((proof_y, State::Spent))],
            }),
            None,
        ));
        let wallet = WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([9; 64])
            .shared_client(connector)
            .build()
            .unwrap();

        let err = retire_eligible_incomplete_sagas(&wallet)
            .await
            .expect_err("spent must refuse");
        match &err {
            PaymentWalletError::Reconcile(message) if message.contains("Spent or Pending") => {}
            other => panic!("expected Spent/Pending refuse, got: {other}"),
        }
        assert_eq!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1,
            "saga must remain when retire refused"
        );
        // Non-mutating NUT-07: reserved proofs must still be present after Spent refuse.
        assert_eq!(
            wallet
                .localstore
                .get_reserved_proofs(&saga_id)
                .await
                .unwrap()
                .len(),
            1,
            "check_proofs_spent must not be used (would delete Spent ys)"
        );

        // Stickiness RED triad: 2nd Err ∧ saga len==1 ∧ no phantom Unspent credit.
        let err2 = retire_eligible_incomplete_sagas(&wallet)
            .await
            .expect_err("spent refuse must be sticky on second retire");
        match &err2 {
            PaymentWalletError::Reconcile(message) if message.contains("Spent or Pending") => {}
            other => panic!("expected sticky Spent/Pending refuse, got: {other}"),
        }
        assert_eq!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1,
            "saga must remain after second spent refuse"
        );
        let unspent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
            .unwrap();
        assert!(
            unspent.iter().all(|info| info.y != proof_y),
            "Spent refuse must not phantom-credit proof as Unspent/spendable"
        );
        assert_eq!(
            wallet
                .localstore
                .get_reserved_proofs(&saga_id)
                .await
                .unwrap()
                .len(),
            1,
            "Spent proof must remain reserved (not returned to spendable)"
        );
    }

    #[tokio::test]
    async fn proofs_reserved_with_pending_mint_state_refuses_retire_sticky() {
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let proof_info = ProofInfo::new(
            proof.clone(),
            mint(MINT),
            State::Unspent,
            CurrencyUnit::Sat,
        )
        .unwrap();
        let saga_id = uuid::Uuid::now_v7();
        store.update_proofs(vec![proof_info], vec![]).await.unwrap();
        store
            .reserve_proofs(vec![proof_y], &saga_id)
            .await
            .unwrap();
        let saga = WalletSaga::new(
            saga_id,
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            Amount::from(7),
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: Amount::from(7),
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: Some(vec![proof.clone()]),
            }),
        );
        store.add_saga(saga).await.unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: vec![ProofState::from((proof_y, State::Pending))],
            }),
            None,
        ));
        let wallet = WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([11; 64])
            .shared_client(connector)
            .build()
            .unwrap();

        let err = retire_eligible_incomplete_sagas(&wallet)
            .await
            .expect_err("pending must refuse");
        match &err {
            PaymentWalletError::Reconcile(message) if message.contains("Spent or Pending") => {}
            other => panic!("expected Spent/Pending refuse, got: {other}"),
        }
        let err2 = retire_eligible_incomplete_sagas(&wallet)
            .await
            .expect_err("pending refuse must be sticky");
        match &err2 {
            PaymentWalletError::Reconcile(message) if message.contains("Spent or Pending") => {}
            other => panic!("expected sticky Pending refuse, got: {other}"),
        }
        assert_eq!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1
        );
        let unspent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
            .unwrap();
        assert!(
            unspent.iter().all(|info| info.y != proof_y),
            "Pending refuse must not phantom-credit proof as Unspent/spendable"
        );
    }

    #[tokio::test]
    async fn empty_reserved_with_bound_proofs_refuses_retire() {
        // Spent-then-deleted localstore gap (old check_proofs_spent): reserved
        // empty even with bound op proofs ⇒ refuse, never auto-retire.
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let saga_id = uuid::Uuid::now_v7();
        let saga = WalletSaga::new(
            saga_id,
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            Amount::from(7),
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: Amount::from(7),
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: Some(vec![proof]),
            }),
        );
        store.add_saga(saga).await.unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse { states: vec![] }),
            None,
        ));
        let wallet = WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([12; 64])
            .shared_client(connector)
            .build()
            .unwrap();

        let err = retire_eligible_incomplete_sagas(&wallet)
            .await
            .expect_err("empty-reserved must refuse");
        match &err {
            PaymentWalletError::Reconcile(message) if message.contains("empty reserved") => {}
            other => panic!("expected empty-reserved refuse, got: {other}"),
        }
        assert_eq!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1,
            "saga must remain when empty reserved"
        );
        let _ = retire_eligible_incomplete_sagas(&wallet)
            .await
            .expect_err("empty-reserved refuse must be sticky");
        assert_eq!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// Shared setup: one ProofsReserved saga with a reserved proof; mint returns `states`.
    async fn reserved_saga_with_nut07_states(
        seed: u8,
        states: Vec<ProofState>,
    ) -> (Wallet, uuid::Uuid, CashuPublicKey) {
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let proof_info = ProofInfo::new(
            proof.clone(),
            mint(MINT),
            State::Unspent,
            CurrencyUnit::Sat,
        )
        .unwrap();
        let saga_id = uuid::Uuid::now_v7();
        store.update_proofs(vec![proof_info], vec![]).await.unwrap();
        store
            .reserve_proofs(vec![proof_y], &saga_id)
            .await
            .unwrap();
        let saga = WalletSaga::new(
            saga_id,
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            Amount::from(7),
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: Amount::from(7),
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: Some(vec![proof]),
            }),
        );
        store.add_saga(saga).await.unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse { states }),
            None,
        ));
        let wallet = WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([seed; 64])
            .shared_client(connector)
            .build()
            .unwrap();
        (wallet, saga_id, proof_y)
    }

    async fn assert_nut07_incomplete_refuses(wallet: &Wallet, saga_id: uuid::Uuid, proof_y: CashuPublicKey) {
        let err = retire_eligible_incomplete_sagas(wallet)
            .await
            .expect_err("incomplete NUT-07 must refuse");
        match &err {
            PaymentWalletError::Reconcile(message)
                if message.contains("Y set incomplete") || message.contains("mismatched") => {}
            other => panic!("expected Y-set incomplete refuse, got: {other}"),
        }
        assert_eq!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1,
            "saga must remain"
        );
        assert_eq!(
            wallet
                .localstore
                .get_reserved_proofs(&saga_id)
                .await
                .unwrap()
                .len(),
            1,
            "reserved must remain"
        );
        let unspent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
            .unwrap();
        assert!(
            unspent.iter().all(|info| info.y != proof_y),
            "incomplete NUT-07 must not phantom-credit as Unspent"
        );
    }

    #[tokio::test]
    async fn nut07_empty_states_refuses_retire() {
        // An empty NUT-07 states list must not pass refuse_if_not_all_unspent.
        let (wallet, saga_id, proof_y) = reserved_saga_with_nut07_states(20, vec![]).await;
        assert_nut07_incomplete_refuses(&wallet, saga_id, proof_y).await;
    }

    #[tokio::test]
    async fn nut07_wrong_y_states_refuses_retire() {
        let wrong_y = p2pk_proof(3, secret_key(2).public_key()).y().unwrap();
        let (wallet, saga_id, proof_y) = reserved_saga_with_nut07_states(
            21,
            vec![ProofState::from((wrong_y, State::Unspent))],
        )
        .await;
        assert_nut07_incomplete_refuses(&wallet, saga_id, proof_y).await;
    }

    #[tokio::test]
    async fn nut07_partial_states_refuses_retire() {
        // Two reserved ys; mint returns only one → refuse.
        let seller = secret_key(1).public_key();
        let proof_a = p2pk_proof(4, seller);
        let proof_b = p2pk_proof(3, seller);
        let y_a = proof_a.y().unwrap();
        let y_b = proof_b.y().unwrap();
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let info_a = ProofInfo::new(proof_a.clone(), mint(MINT), State::Unspent, CurrencyUnit::Sat)
            .unwrap();
        let info_b = ProofInfo::new(proof_b.clone(), mint(MINT), State::Unspent, CurrencyUnit::Sat)
            .unwrap();
        let saga_id = uuid::Uuid::now_v7();
        store
            .update_proofs(vec![info_a, info_b], vec![])
            .await
            .unwrap();
        store
            .reserve_proofs(vec![y_a, y_b], &saga_id)
            .await
            .unwrap();
        let saga = WalletSaga::new(
            saga_id,
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            Amount::from(7),
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: Amount::from(7),
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: Some(vec![proof_a, proof_b]),
            }),
        );
        store.add_saga(saga).await.unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                // Partial: only y_a, missing y_b.
                states: vec![ProofState::from((y_a, State::Unspent))],
            }),
            None,
        ));
        let wallet = WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([22; 64])
            .shared_client(connector)
            .build()
            .unwrap();

        let err = retire_eligible_incomplete_sagas(&wallet)
            .await
            .expect_err("partial NUT-07 must refuse");
        match &err {
            PaymentWalletError::Reconcile(message)
                if message.contains("Y set incomplete") || message.contains("mismatched") => {}
            other => panic!("expected Y-set incomplete refuse, got: {other}"),
        }
        assert_eq!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            wallet
                .localstore
                .get_reserved_proofs(&saga_id)
                .await
                .unwrap()
                .len(),
            2,
            "both reserved proofs must remain"
        );
        let unspent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
            .unwrap();
        assert!(
            unspent
                .iter()
                .all(|info| info.y != y_a && info.y != y_b),
            "partial NUT-07 must not phantom-credit reserved proofs"
        );
    }

    #[tokio::test]
    async fn proofs_reserved_all_unspent_retires_and_returns_spendable() {
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        // Insert as Unspent; reserve_proofs requires Unspent → marks Reserved.
        let proof_info = ProofInfo::new(
            proof.clone(),
            mint(MINT),
            State::Unspent,
            CurrencyUnit::Sat,
        )
        .unwrap();
        let saga_id = uuid::Uuid::now_v7();
        store.update_proofs(vec![proof_info], vec![]).await.unwrap();
        store
            .reserve_proofs(vec![proof_y], &saga_id)
            .await
            .unwrap();
        let saga = WalletSaga::new(
            saga_id,
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            Amount::from(7),
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: Amount::from(7),
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: None,
            }),
        );
        store.add_saga(saga).await.unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: vec![ProofState::from((proof_y, State::Unspent))],
            }),
            None,
        ));
        let wallet = WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([10; 64])
            .shared_client(connector)
            .build()
            .unwrap();

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(report.retired, 1);
        assert!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .is_empty()
        );
        let unspent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
            .unwrap();
        assert_eq!(unspent.len(), 1);
        assert_eq!(unspent[0].used_by_operation, None);

        let report2 = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(report2.retired, 0);
    }

    #[tokio::test]
    async fn token_created_without_confirmed_tx_is_not_retired() {
        let fixture = wallet_fixture().await;
        let saga = WalletSaga::new(
            uuid::Uuid::now_v7(),
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::TokenCreated,
            ),
            fixture.terms.amount,
            fixture.terms.mint.clone(),
            fixture.terms.unit.clone(),
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: fixture.terms.amount,
                memo: None,
                counter_start: None,
                counter_end: None,
                token: Some("cashuBplaceholder".into()),
                proofs: None,
            }),
        );
        fixture.wallet.localstore.add_saga(saga).await.unwrap();

        let report = retire_eligible_incomplete_sagas(&fixture.wallet)
            .await
            .unwrap();
        assert_eq!(report.retired, 0);
        assert_eq!(
            fixture
                .wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1
        );

        let key = payment_key(&fixture.terms);
        let result = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&key.attempt_id(), &fixture.terms)
            .await;
        match &result {
            Err(PaymentWalletError::Reconcile(message)) if message.contains("TokenCreated") => {}
            Err(other) => panic!("expected TokenCreated refuse, got: {other}"),
            Ok(_) => panic!("expected TokenCreated refuse, got Ok"),
        }
    }

    #[test]
    fn amount_covers_fee_refuses_dust_and_accepts_fee_plus_one() {
        require_amount_covers_fee(Amount::from(1), Amount::from(1)).unwrap_err();
        require_amount_covers_fee(Amount::from(0), Amount::from(1)).unwrap_err();
        require_amount_covers_fee(Amount::from(2), Amount::from(1)).unwrap();
    }

    // An unroutable mint URL that refuses the TCP connect instantly, so
    // the bounded fee query returns a transport error well inside the timeout — the
    // deterministic stand-in for a down mint (no live network, no real hang wait).
    const DEAD_MINT: &str = "https://127.0.0.1:1";

    /// Post-time dust guard fails fast with `mint_unreachable` (not a hang / generic
    /// deadline) when the mint is down and NO keyset is cached to fall back on.
    #[tokio::test]
    async fn post_dust_guard_fails_fast_with_mint_unreachable_when_mint_down() {
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let wallet = Wallet::new(DEAD_MINT, CurrencyUnit::Sat, store, [7; 64], None).unwrap();

        let started = std::time::Instant::now();
        let error = require_fee_safe_amount_for_post(&wallet, Amount::from(10))
            .await
            .expect_err("dead mint with no cached keyset must refuse the post-time dust guard");
        let elapsed = started.elapsed();

        match &error {
            PaymentWalletError::MintUnreachable { reason, mint, .. } => {
                assert_eq!(*reason, MINT_UNREACHABLE_POST);
                assert!(mint.contains("127.0.0.1"), "reason names the mint: {mint}");
            }
            other => panic!("expected MintUnreachable, got: {other}"),
        }
        assert!(
            elapsed < MINT_TOUCH_TIMEOUT,
            "must fail fast, took {elapsed:?}"
        );
    }

    /// Pay path fails fast with `mint_unreachable_pay` (before any spend state) when
    /// the mint is down — no cached fallback: the pay leg genuinely needs the mint.
    #[tokio::test]
    async fn pay_dust_guard_fails_fast_with_mint_unreachable_pay_when_mint_down() {
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let wallet = Wallet::new(DEAD_MINT, CurrencyUnit::Sat, store, [7; 64], None).unwrap();

        let started = std::time::Instant::now();
        let error = require_fee_safe_amount(&wallet, Amount::from(10))
            .await
            .expect_err("dead mint must refuse the pay-path dust guard");
        let elapsed = started.elapsed();

        match &error {
            PaymentWalletError::MintUnreachable { reason, mint, .. } => {
                assert_eq!(*reason, MINT_UNREACHABLE_PAY);
                assert!(mint.contains("127.0.0.1"), "reason names the mint: {mint}");
            }
            other => panic!("expected MintUnreachable, got: {other}"),
        }
        assert!(
            elapsed < MINT_TOUCH_TIMEOUT,
            "must fail fast, took {elapsed:?}"
        );
    }

    /// When the mint is down but a keyset is cached in the wallet DB, the post-time
    /// dust guard degrades to the cached fee floor and STILL runs the dust check
    /// (fail-closed) rather than skipping it: dust refuses, fee+1 passes.
    #[tokio::test]
    async fn post_dust_guard_falls_back_to_cached_keyset_when_mint_unreachable() {
        // input_fee_ppk = 1000 ⇒ N=1 floor = ceil(1000/1000) = 1 sat.
        let keyset = test_keyset_with_fee(1000);
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        store
            .add_mint(mint(DEAD_MINT), Some(MintInfo::new()))
            .await
            .unwrap();
        store
            .add_mint_keysets(
                mint(DEAD_MINT),
                vec![KeySetInfo {
                    id: keyset.id,
                    unit: keyset.unit.clone(),
                    active: true,
                    input_fee_ppk: keyset.input_fee_ppk,
                    final_expiry: keyset.final_expiry,
                }],
            )
            .await
            .unwrap();
        let wallet = Wallet::new(DEAD_MINT, CurrencyUnit::Sat, store, [7; 64], None).unwrap();

        // Cached floor is 1; the dust check still runs on that floor.
        let dust = require_fee_safe_amount_for_post(&wallet, Amount::from(1))
            .await
            .expect_err("amount == cached fee floor is dust and must refuse");
        assert!(
            matches!(dust, PaymentWalletError::Policy(_)),
            "cached fallback runs the dust check (Policy), got: {dust}"
        );

        let fee = require_fee_safe_amount_for_post(&wallet, Amount::from(2))
            .await
            .expect("amount above the cached fee floor must pass via the cached fallback");
        assert_eq!(fee, Amount::from(1), "fallback used the cached N=1 fee floor");
    }

    #[tokio::test]
    async fn confirmed_attempt_with_empty_reserved_orphan_refuses_reconcile() {
        // Empty-reserved orphan blocks even when a confirmed attempt exists —
        // migration-safe fail-closed (Spent-deleted vs orphan indistinguishable).
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let attempt_id = key.attempt_id();
        store_confirmed_attempt(&fixture.wallet, &attempt_id, &fixture.token).await;
        let saga = WalletSaga::new(
            uuid::Uuid::now_v7(),
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            fixture.terms.amount,
            fixture.terms.mint.clone(),
            fixture.terms.unit.clone(),
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: fixture.terms.amount,
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: None,
            }),
        );
        fixture.wallet.localstore.add_saga(saga).await.unwrap();

        let result = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&attempt_id, &fixture.terms)
            .await;
        match &result {
            Err(PaymentWalletError::Reconcile(message)) if message.contains("empty reserved") => {}
            Err(other) => panic!("expected empty-reserved refuse, got: {other}"),
            Ok(_) => panic!("expected empty-reserved refuse, got Ok"),
        }
        assert_eq!(
            fixture
                .wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1,
            "empty orphan must remain (not auto-retired)"
        );
    }

    #[tokio::test]
    async fn seller_receive_rejects_an_inflated_proof_at_the_mint_swap() {
        let seller_key = secret_key(1);
        let keyset = test_keyset();
        let proof = p2pk_proof_for_keyset(7, seller_key.public_key(), keyset.id);
        let proof_y = proof.y().unwrap();
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let transport = InflatedSwapTransport::new(proof_y, Amount::from(1));
        let swap_calls = transport.swap_calls.clone();
        let presented_amount = transport.presented_amount.clone();
        let wallet = seller_wallet(transport, keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );

        let result = CdkSellerReceive::new(&wallet, seller_key)
            .receive(&token, &terms, &accepted(&[MINT]), &mint(MINT))
            .await;

        assert!(matches!(result, Err(PaymentWalletError::Wallet(_))));
        assert_eq!(swap_calls.load(Ordering::SeqCst), 1);
        assert_eq!(presented_amount.load(Ordering::SeqCst), 7);
    }

    #[tokio::test]
    async fn seller_receive_rejects_an_authentic_underpay_before_the_mint_swap() {
        let seller_key = secret_key(1);
        let keyset = test_keyset();
        let proof = p2pk_proof_for_keyset(1, seller_key.public_key(), keyset.id);
        let proof_y = proof.y().unwrap();
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let transport = InflatedSwapTransport::new(proof_y, Amount::from(7));
        let swap_calls = transport.swap_calls.clone();
        let wallet = seller_wallet(transport, keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );

        let result = CdkSellerReceive::new(&wallet, seller_key)
            .receive(&token, &terms, &accepted(&[MINT]), &mint(MINT))
            .await;

        assert!(matches!(result, Err(PaymentWalletError::Policy(_))));
        assert_eq!(swap_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn seller_receive_rejects_dust_as_uneconomical_before_swap() {
        let seller_key = secret_key(1);
        let keyset = test_keyset_with_fee(1_000); // 1 proof → fee = 1
        let proof = p2pk_proof_for_keyset(1, seller_key.public_key(), keyset.id);
        let proof_y = proof.y().unwrap();
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let transport = InflatedSwapTransport::new(proof_y, Amount::from(1));
        let swap_calls = transport.swap_calls.clone();
        let wallet = seller_wallet(transport, keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(1),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );

        let result = CdkSellerReceive::new(&wallet, seller_key)
            .receive(&token, &terms, &accepted(&[MINT]), &mint(MINT))
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::Policy(message))
                if message.contains("uneconomical vs mint fee")
        ));
        assert_eq!(swap_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn seller_receive_returns_face_when_net_plus_fee_matches() {
        let seller_key = secret_key(1);
        let keyset = test_keyset_with_fee(1_000); // 1 proof → fee = 1
        let proof = p2pk_proof_for_keyset(2, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let wallet = seller_wallet(InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(2),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        // CDK receive returns net after fees (face 2 − fee 1 = 1).
        let amount = adapter
            .receive_with(&token, &terms, &accepted(&[MINT]), &mint(MINT), |_| async { Ok(Amount::from(1)) })
            .await
            .unwrap();

        assert_eq!(amount, Amount::from(2));
    }

    // Z2 (multi-mint redeem at realized mint): when the buyer pays at a NON-default accepted mint,
    // the seller opens its wallet at that REALIZED mint, so redeem succeeds. Wallet bound to MINT2
    // (non-default) + terms/token/payload at MINT2 + accepted={MINT,MINT2} → receive returns face.
    #[tokio::test]
    async fn seller_receive_succeeds_when_wallet_opened_at_non_default_realized_mint() {
        const MINT2: &str = "https://alt-testnut.example";
        let seller_key = secret_key(1);
        let keyset = test_keyset(); // fee = 0
        let proof = p2pk_proof_for_keyset(5, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT2), vec![proof], None, CurrencyUnit::Sat);
        // Seller wallet bound to the realized (non-default) mint, as the daemon now opens it.
        let wallet = seller_wallet_at(MINT2, InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT2),
            Amount::from(5),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        let amount = adapter
            .receive_with(&token, &terms, &accepted(&[MINT, MINT2]), &mint(MINT2), |_| async {
                Ok(Amount::from(5))
            })
            .await
            .expect("redeem at realized non-default mint must succeed");
        assert_eq!(amount, Amount::from(5));
    }

    // Z2 pre-fix symptom: a wallet opened at the DEFAULT mint refuses a payment realized at a
    // different accepted mint — the "wallet mint ... does not match terms" failure the fix removes.
    #[tokio::test]
    async fn seller_receive_refuses_when_wallet_mint_differs_from_realized_terms() {
        const MINT2: &str = "https://alt-testnut.example";
        let seller_key = secret_key(1);
        let keyset = test_keyset();
        let proof = p2pk_proof_for_keyset(5, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT2), vec![proof], None, CurrencyUnit::Sat);
        // Wallet at the seller DEFAULT mint (the pre-fix behavior) vs terms at the realized MINT2.
        let wallet = seller_wallet_at(MINT, InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT2),
            Amount::from(5),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        let result = adapter
            .receive_with(&token, &terms, &accepted(&[MINT, MINT2]), &mint(MINT2), |_| async {
                Ok(Amount::from(5))
            })
            .await;
        assert!(
            matches!(&result, Err(PaymentWalletError::Policy(msg)) if msg.contains("wallet mint")),
            "expected wallet-mint mismatch policy refusal, got {result:?}"
        );
    }

    #[tokio::test]
    async fn seller_receive_surfaces_fee_mismatch_without_treating_as_underpay() {
        let seller_key = secret_key(1);
        let keyset = test_keyset_with_fee(1_000); // 1 proof → fee = 1
        let proof = p2pk_proof_for_keyset(2, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let wallet = seller_wallet(InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(2),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        let result = adapter
            .receive_with(&token, &terms, &accepted(&[MINT]), &mint(MINT), |_| async { Ok(Amount::from(2)) })
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::FeeMismatch {
                face,
                received,
                predicted_fee,
            }) if face == Amount::from(2)
                && received == Amount::from(2)
                && predicted_fee == Amount::from(1)
        ));
    }

    #[tokio::test]
    async fn seller_receive_rejects_when_wallet_returns_a_mismatched_amount() {
        let seller_key = secret_key(1);
        let keyset = test_keyset(); // fee = 0
        let proof = p2pk_proof_for_keyset(7, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let wallet = seller_wallet(InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        let result = adapter
            .receive_with(&token, &terms, &accepted(&[MINT]), &mint(MINT), |_| async {
                Ok(Amount::from(1))
            })
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::FeeMismatch {
                face,
                received,
                predicted_fee,
            }) if face == Amount::from(7)
                && received == Amount::from(1)
                && predicted_fee == Amount::ZERO
        ));
    }

    // A payload whose mint ∉ the seller's creq `m` list is refused
    // `wrong_mint`, and the token mint must equal the payload's declared mint.
    #[test]
    fn pay_matches_creq() {
        let listed = mint(MINT);
        let unlisted = mint(OTHER_MINT);
        let creq_mints = accepted(&[MINT]);

        // payload.mint is not in the creq `m` list → wrong_mint, before any swap.
        let err = assert_redeem_mint(&unlisted, &unlisted, &creq_mints).unwrap_err();
        assert!(matches!(
            err,
            PaymentWalletError::Policy(message)
                if message.contains("wrong_mint") && message.contains("accepted_mints")
        ));

        // payload.mint is listed, but the token came from a different mint → wrong_mint.
        let err = assert_redeem_mint(&unlisted, &listed, &creq_mints).unwrap_err();
        assert!(matches!(
            err,
            PaymentWalletError::Policy(message)
                if message.contains("wrong_mint") && message.contains("does not equal payload mint")
        ));

        // token mint == payload.mint ∈ creq `m` → accepted.
        assert!(assert_redeem_mint(&listed, &listed, &creq_mints).is_ok());
    }

    // The seller redeem accepts a token from a listed mint that equals
    // the payload's mint, and refuses otherwise — the guard fails BEFORE the mint swap (no funds
    // move on refusal).
    #[tokio::test]
    async fn redeem_guard() {
        let seller_key = secret_key(1);
        let keyset = test_keyset(); // fee = 0
        let proof = p2pk_proof_for_keyset(7, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let wallet = seller_wallet(InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        // Accepts: token mint == payload.mint == MINT ∈ accepted_mints.
        let amount = adapter
            .receive_with(&token, &terms, &accepted(&[MINT]), &mint(MINT), |_| async {
                Ok(Amount::from(7))
            })
            .await
            .unwrap();
        assert_eq!(amount, Amount::from(7));

        // Refuses: payload.mint (OTHER_MINT) is not in accepted_mints → wrong_mint, no swap.
        let swap_calls = Arc::new(AtomicUsize::new(0));
        let counter = swap_calls.clone();
        let err = adapter
            .receive_with(
                &token,
                &terms,
                &accepted(&[MINT]),
                &mint(OTHER_MINT),
                move |_| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(Amount::from(7))
                    }
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PaymentWalletError::Policy(message) if message.contains("wrong_mint")
        ));
        assert_eq!(
            swap_calls.load(Ordering::SeqCst),
            0,
            "redeem guard must refuse before the mint swap"
        );
    }

    #[test]
    fn worker_wires_reconcile_verify_and_send_into_the_real_state_machine() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let fixture = runtime.block_on(wallet_fixture());
        let key = payment_key(&fixture.terms);
        runtime.block_on(store_confirmed_attempt(
            &fixture.wallet,
            &key.attempt_id(),
            &fixture.token,
        ));
        let ys = [fixture.proof.y().unwrap()];
        let connector = BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: ys
                    .iter()
                    .copied()
                    .map(|y| ProofState::from((y, State::Unspent)))
                    .collect(),
            }),
            None,
        );
        let send_count = Arc::new(AtomicUsize::new(0));
        let sender = CountingSend(send_count.clone());
        let authority = authority();
        let mut effects = CdkPaymentEffects::spawn_with_connector(
            fixture.wallet.clone(),
            connector,
            sender,
            move |key: &PaymentKey, _: &PaymentSent| Ok(cosigned_receipt(key)),
        )
        .unwrap();
        let journal = MemoryPaymentJournal::default();
        let delivery = git_delivery_for_key(&key);
        let mut verifier = AcceptDelivery;

        let state = PaymentService::new(&journal)
            .run_with_verifier(
                &delivery,
                &mut verifier,
                &key,
                &fixture.terms,
                &authority,
                &mut effects,
            )
            .unwrap();

        assert!(matches!(state, PaymentState::Closed { .. }));
        assert_eq!(send_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn worker_sends_to_the_nostr_identity_not_the_odd_parity_p2pk_lock() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let seller_key = odd_secret_key();
        let fixture = runtime.block_on(wallet_fixture_for_seller(seller_key.public_key()));
        let key = payment_key(&fixture.terms);
        runtime.block_on(store_confirmed_attempt(
            &fixture.wallet,
            &key.attempt_id(),
            &fixture.token,
        ));
        let proof_y = fixture.proof.y().unwrap();
        let connector = BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: vec![ProofState::from((proof_y, State::Unspent))],
            }),
            None,
        );
        let authority = authority();
        let mut effects = CdkPaymentEffects::spawn_with_connector(
            fixture.wallet.clone(),
            connector,
            NostrRecipientSend,
            move |key: &PaymentKey, _: &PaymentSent| Ok(cosigned_receipt(key)),
        )
        .unwrap();
        let journal = MemoryPaymentJournal::default();
        let delivery = git_delivery_for_key(&key);
        let mut verifier = AcceptDelivery;

        let state = PaymentService::new(&journal)
            .run_with_verifier(
                &delivery,
                &mut verifier,
                &key,
                &fixture.terms,
                &authority,
                &mut effects,
            )
            .unwrap();

        assert!(matches!(state, PaymentState::Closed { .. }));
    }

    #[test]
    fn worker_rejects_a_wrong_seller_lock_through_the_real_verify_adapter() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let fixture = runtime.block_on(wallet_fixture());
        let proof_y = fixture.proof.y().unwrap();
        let connector = BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: vec![ProofState::from((proof_y, State::Unspent))],
            }),
            None,
        );
        let mut effects = CdkPaymentEffects::spawn_with_connector(
            fixture.wallet.clone(),
            connector,
            CountingSend(Arc::new(AtomicUsize::new(0))),
            |_: &PaymentKey, _: &PaymentSent| unreachable!(),
        )
        .unwrap();
        let wrong_terms = wallet_terms(secret_key(2).public_key());
        let locked = LockedPayment::new(fixture.token.clone());

        let result = effects.verify_payment(
            &payment_key(&wrong_terms).attempt_id(),
            &wrong_terms,
            &locked,
        );

        assert!(result.is_err());
    }

    struct WalletFixture {
        wallet: Wallet,
        terms: PaymentTerms,
        token: Token,
        proof: Proof,
    }

    async fn wallet_fixture() -> WalletFixture {
        wallet_fixture_for_seller(secret_key(1).public_key()).await
    }

    async fn wallet_fixture_for_seller(seller: PublicKey) -> WalletFixture {
        let proof = p2pk_proof(7, seller);
        let token = Token::new(mint(MINT), vec![proof.clone()], None, CurrencyUnit::Sat);
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let wallet = Wallet::new(MINT, CurrencyUnit::Sat, store, [7; 64], None).unwrap();
        WalletFixture {
            wallet,
            terms: wallet_terms(seller),
            token,
            proof,
        }
    }

    async fn seller_wallet(transport: InflatedSwapTransport, keyset: KeySet) -> Wallet {
        seller_wallet_at(MINT, transport, keyset).await
    }

    /// Build a seller wallet bound to an EXPLICIT mint url (multi-mint tests). Mirrors
    /// `seller_wallet` but lets the wallet bind to a non-default mint.
    async fn seller_wallet_at(
        mint_url: &str,
        transport: InflatedSwapTransport,
        keyset: KeySet,
    ) -> Wallet {
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        store
            .add_mint(mint(mint_url), Some(MintInfo::new()))
            .await
            .unwrap();
        store
            .add_mint_keysets(
                mint(mint_url),
                vec![KeySetInfo {
                    id: keyset.id,
                    unit: keyset.unit.clone(),
                    active: true,
                    input_fee_ppk: keyset.input_fee_ppk,
                    final_expiry: keyset.final_expiry,
                }],
            )
            .await
            .unwrap();
        store.add_keys(keyset).await.unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(mint(mint_url), transport, None));
        WalletBuilder::new()
            .mint_url(mint(mint_url))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([8; 64])
            .shared_client(connector)
            .build()
            .unwrap()
    }

    fn test_keyset() -> KeySet {
        test_keyset_with_fee(0)
    }

    fn test_keyset_with_fee(input_fee_ppk: u64) -> KeySet {
        let keys = [1_u64, 2, 4, 8]
            .into_iter()
            .map(|amount| {
                (
                    Amount::from(amount),
                    secret_key(amount as u8 + 10).public_key(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let keys = Keys::new(keys);
        KeySet {
            id: Id::v1_from_keys(&keys),
            unit: CurrencyUnit::Sat,
            active: Some(true),
            keys,
            input_fee_ppk,
            final_expiry: None,
        }
    }

    async fn store_confirmed_attempt(wallet: &Wallet, attempt_id: &AttemptId, token: &Token) {
        let proof = match token {
            Token::TokenV4(token) => token.token[0].proofs[0]
                .clone()
                .into_proof(&Id::from_str(KEYSET_ID).unwrap()),
            Token::TokenV3(_) => panic!("fixture uses v4 token"),
        };
        let proof_info = ProofInfo::new(
            proof.clone(),
            wallet.mint_url.clone(),
            State::PendingSpent,
            wallet.unit.clone(),
        )
        .unwrap();
        wallet
            .localstore
            .update_proofs(vec![proof_info], vec![])
            .await
            .unwrap();
        store_confirmed_transaction(wallet, attempt_id, &proof).await;
    }

    async fn store_confirmed_transaction(wallet: &Wallet, attempt_id: &AttemptId, proof: &Proof) {
        let mut metadata = HashMap::new();
        metadata.insert(ATTEMPT_METADATA.into(), attempt_id.as_str().into());
        wallet
            .localstore
            .add_transaction(Transaction {
                mint_url: wallet.mint_url.clone(),
                direction: TransactionDirection::Outgoing,
                amount: Amount::from(7),
                fee: Amount::ZERO,
                unit: wallet.unit.clone(),
                ys: vec![proof.y().unwrap()],
                timestamp: 1,
                memo: None,
                metadata,
                quote_id: None,
                payment_request: None,
                payment_proof: None,
                payment_method: None,
                saga_id: Some(uuid::Uuid::now_v7()),
            })
            .await
            .unwrap();
    }

    fn payment_key(terms: &PaymentTerms) -> PaymentKey {
        PaymentKey::new(
            JobId::new("job").unwrap(),
            ResultId::new("result").unwrap(),
            DeliveryIntegrityHash::from_hex("11".repeat(32)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            terms,
            None,
        )
    }

    fn git_delivery_for_key(key: &PaymentKey) -> GitDelivery {
        GitDelivery::new(
            "https://example.invalid/repo.git",
            "mobee/job",
            CommitOid::parse(key.delivery_integrity_hash.as_str()).unwrap(),
        )
        .unwrap()
    }

    fn offer(seller: &str) -> ParsedOffer {
        ParsedOffer {
            task: "task".into(),
            output: "text/plain".into(),
            amount: 7,
            unit: "sat".into(),
            deadline_unix: 1,
            seller_pubkey: Some(seller.into()),
        }
    }

    fn authority() -> ReceiptAuthority {
        // External anchors are nostr identities; the receipt co-signatures verify against
        // these (never the receipt's own p-tags).
        ReceiptAuthority {
            buyer: receipt_buyer_keys().public_key(),
            seller: receipt_seller_keys().public_key(),
        }
    }

    fn receipt_buyer_keys() -> nostr_sdk::Keys {
        nostr_sdk::Keys::parse(&"21".repeat(32)).unwrap()
    }

    fn receipt_seller_keys() -> nostr_sdk::Keys {
        nostr_sdk::Keys::parse(&"11".repeat(32)).unwrap()
    }

    /// A real co-signed kind-3400 receipt over the trade preimage (both schnorr sigs by
    /// the anchored buyer/seller nostr keys) — what a real buyer publishes.
    fn cosigned_receipt(key: &PaymentKey) -> ReceiptEvidence {
        let preimage = crate::receipt::ReceiptPreimage {
            job_hash: key.job_hash.as_str().to_owned(),
            offer_id: key.job_id.as_str().to_owned(),
            amount: key.amount.to_u64(),
            unit: key.unit.to_string(),
            buyer_pubkey: receipt_buyer_keys().public_key().to_hex(),
            seller_pubkey: receipt_seller_keys().public_key().to_hex(),
            delivery_integrity_hash: key.delivery_integrity_hash.as_str().to_owned(),
            delivery_kind: "fork".to_owned(),
            exec_metadata_commitment: crate::receipt::EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
            creq_hash: key.creq_hash.clone(),
        };
        let message = nostr_sdk::secp256k1::Message::from_digest(preimage.digest_bytes());
        ReceiptEvidence {
            receipt_id: "aa".repeat(32),
            author: receipt_buyer_keys().public_key(),
            seller_signature: receipt_seller_keys().sign_schnorr(&message).to_string(),
            buyer_signature: receipt_buyer_keys().sign_schnorr(&message).to_string(),
            preimage,
            relay_success: vec!["memory://relay".into()],
        }
    }

    fn p2pk_proof(amount: u64, seller: PublicKey) -> Proof {
        p2pk_proof_for_keyset(amount, seller, Id::from_str(KEYSET_ID).unwrap())
    }

    fn p2pk_proof_for_keyset(amount: u64, seller: PublicKey, keyset_id: Id) -> Proof {
        let secret = Secret::try_from(SpendingConditions::new_p2pk(
            seller,
            Some(Conditions::default()),
        ))
        .unwrap();
        Proof::new(
            Amount::from(amount),
            keyset_id,
            secret,
            secret_key(9).public_key(),
        )
    }

    fn secret_key(byte: u8) -> SecretKey {
        SecretKey::from_slice(&[byte; 32]).unwrap()
    }

    fn odd_secret_key() -> SecretKey {
        (1..=u8::MAX)
            .map(secret_key)
            .find(|key| key.public_key().to_string().starts_with("03"))
            .expect("an odd-parity test key exists")
    }

    fn nostr_key_for_p2pk(key: PublicKey) -> NostrPublicKey {
        let compressed = key.to_string();
        NostrPublicKey::from_hex(&compressed[2..]).unwrap()
    }

    fn wallet_terms(seller: PublicKey) -> PaymentTerms {
        PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller),
            seller,
        )
    }

    fn mint(url: &str) -> MintUrl {
        MintUrl::from_str(url).unwrap()
    }

    fn accepted(urls: &[&str]) -> HashSet<MintUrl> {
        urls.iter().map(|url| mint(url)).collect()
    }

    struct CountingSend(Arc<AtomicUsize>);

    struct NostrRecipientSend;

    impl PaymentSend for NostrRecipientSend {
        async fn send_payment(
            &mut self,
            payload: PaymentPayload,
        ) -> Result<PaymentSent, PaymentSendError> {
            nostr_sdk::PublicKey::parse(&payload.seller_pubkey).map_err(|error| {
                PaymentSendError::Transport(format!("invalid Nostr recipient: {error}"))
            })?;
            Ok(PaymentSent {
                payment_id: "payment".into(),
                relay_success: vec!["memory://relay".into()],
                relay_failed: Vec::new(),
            })
        }
    }

    impl PaymentSend for CountingSend {
        async fn send_payment(
            &mut self,
            _payload: PaymentPayload,
        ) -> Result<PaymentSent, PaymentSendError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PaymentSent {
                payment_id: "payment".into(),
                relay_success: vec!["memory://relay".into()],
                relay_failed: Vec::new(),
            })
        }
    }

    #[derive(Clone, Debug, Default)]
    struct CheckStateTransport {
        response: serde_json::Value,
    }

    impl CheckStateTransport {
        fn new(response: cashu::CheckStateResponse) -> Self {
            Self {
                response: serde_json::to_value(response).unwrap(),
            }
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for CheckStateTransport {
        fn with_proxy(
            &mut self,
            _proxy: Url,
            _host_matcher: Option<&str>,
            _accept_invalid_certs: bool,
        ) -> Result<(), cdk::Error> {
            Ok(())
        }

        async fn http_get<R>(
            &self,
            _url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
        ) -> Result<R, cdk::Error>
        where
            R: DeserializeOwned,
        {
            Err(cdk::Error::Custom("unexpected GET".into()))
        }

        async fn http_post<P, R>(
            &self,
            url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
            _payload: &P,
        ) -> Result<R, cdk::Error>
        where
            P: Serialize + ?Sized + Send + Sync,
            R: DeserializeOwned,
        {
            if !url.path().ends_with("/v1/checkstate") {
                return Err(cdk::Error::Custom("unexpected POST".into()));
            }
            serde_json::from_value(self.response.clone())
                .map_err(|error| cdk::Error::Custom(error.to_string()))
        }
    }

    #[derive(Clone, Debug)]
    struct InflatedSwapTransport {
        authoritative_y: PublicKey,
        authoritative_amount: Amount,
        swap_calls: Arc<AtomicUsize>,
        presented_amount: Arc<AtomicU64>,
    }

    impl InflatedSwapTransport {
        fn new(authoritative_y: PublicKey, authoritative_amount: Amount) -> Self {
            Self {
                authoritative_y,
                authoritative_amount,
                swap_calls: Arc::new(AtomicUsize::new(0)),
                presented_amount: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    impl Default for InflatedSwapTransport {
        fn default() -> Self {
            Self::new(secret_key(31).public_key(), Amount::ZERO)
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for InflatedSwapTransport {
        fn with_proxy(
            &mut self,
            _proxy: Url,
            _host_matcher: Option<&str>,
            _accept_invalid_certs: bool,
        ) -> Result<(), cdk::Error> {
            Ok(())
        }

        async fn http_get<R>(
            &self,
            _url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
        ) -> Result<R, cdk::Error>
        where
            R: DeserializeOwned,
        {
            Err(cdk::Error::Custom("unexpected GET".into()))
        }

        async fn http_post<P, R>(
            &self,
            url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
            payload: &P,
        ) -> Result<R, cdk::Error>
        where
            P: Serialize + ?Sized + Send + Sync,
            R: DeserializeOwned,
        {
            if !url.path().ends_with("/v1/swap") {
                return Err(cdk::Error::Custom("unexpected POST".into()));
            }
            let request: cashu::SwapRequest = serde_json::from_value(
                serde_json::to_value(payload)
                    .map_err(|error| cdk::Error::Custom(error.to_string()))?,
            )
            .map_err(|error| cdk::Error::Custom(error.to_string()))?;
            let presented = request
                .input_amount()
                .map_err(|error| cdk::Error::Custom(error.to_string()))?;
            let presented_y = request
                .inputs()
                .first()
                .ok_or_else(|| cdk::Error::Custom("swap has no input".into()))?
                .y()
                .map_err(|error| cdk::Error::Custom(error.to_string()))?;
            if presented_y != self.authoritative_y || presented == self.authoritative_amount {
                return Err(cdk::Error::Custom(
                    "swap did not present the expected inflated unspent proof".into(),
                ));
            }
            self.swap_calls.fetch_add(1, Ordering::SeqCst);
            self.presented_amount
                .store(presented.to_u64(), Ordering::SeqCst);
            Err(cdk::Error::TransactionUnbalanced(
                self.authoritative_amount.to_u64(),
                presented.to_u64(),
                0,
            ))
        }
    }

    /// Fake mint for stranded-Swap-saga resolution. Answers NUT-07
    /// `/v1/checkstate` with configured input states and NUT-13 `/v1/restore` with
    /// an empty batch (no output signatures) — the existing scaffolding does not
    /// simulate a signing mint, so the AllSpent branch exercises the completion
    /// path (mark inputs Spent + drop saga) with a no-op output re-derivation.
    #[derive(Clone, Debug, Default)]
    struct SwapReconcileTransport {
        checkstate: serde_json::Value,
    }

    impl SwapReconcileTransport {
        fn new(states: Vec<ProofState>) -> Self {
            Self {
                checkstate: serde_json::to_value(cashu::CheckStateResponse { states }).unwrap(),
            }
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for SwapReconcileTransport {
        fn with_proxy(
            &mut self,
            _proxy: Url,
            _host_matcher: Option<&str>,
            _accept_invalid_certs: bool,
        ) -> Result<(), cdk::Error> {
            Ok(())
        }

        async fn http_get<R>(
            &self,
            _url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
        ) -> Result<R, cdk::Error>
        where
            R: DeserializeOwned,
        {
            Err(cdk::Error::Custom("unexpected GET".into()))
        }

        async fn http_post<P, R>(
            &self,
            url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
            _payload: &P,
        ) -> Result<R, cdk::Error>
        where
            P: Serialize + ?Sized + Send + Sync,
            R: DeserializeOwned,
        {
            if url.path().ends_with("/v1/checkstate") {
                return serde_json::from_value(self.checkstate.clone())
                    .map_err(|error| cdk::Error::Custom(error.to_string()));
            }
            if url.path().ends_with("/v1/restore") {
                let empty = cashu::nuts::nut09::RestoreResponse {
                    outputs: vec![],
                    signatures: vec![],
                };
                return serde_json::from_value(serde_json::to_value(empty).unwrap())
                    .map_err(|error| cdk::Error::Custom(error.to_string()));
            }
            Err(cdk::Error::Custom("unexpected POST".into()))
        }
    }

    /// Build a wallet holding a single stranded Swap saga in `SwapRequested` state
    /// with `proofs` reserved under it, keysets seeded so NUT-13 restore stays
    /// local, and a [`SwapReconcileTransport`] reporting `states` for the inputs.
    async fn swap_saga_wallet(
        seed: u8,
        proofs: Vec<Proof>,
        states: Vec<ProofState>,
    ) -> (Wallet, uuid::Uuid, Vec<CashuPublicKey>) {
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        store
            .add_mint(mint(MINT), Some(MintInfo::new()))
            .await
            .unwrap();
        let keyset = test_keyset();
        store
            .add_mint_keysets(
                mint(MINT),
                vec![KeySetInfo {
                    id: keyset.id,
                    unit: keyset.unit.clone(),
                    active: true,
                    input_fee_ppk: keyset.input_fee_ppk,
                    final_expiry: keyset.final_expiry,
                }],
            )
            .await
            .unwrap();
        store.add_keys(keyset).await.unwrap();

        let ys = proofs
            .iter()
            .map(|proof| proof.y().unwrap())
            .collect::<Vec<_>>();
        let infos = proofs
            .iter()
            .map(|proof| {
                ProofInfo::new(proof.clone(), mint(MINT), State::Unspent, CurrencyUnit::Sat)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        store.update_proofs(infos, vec![]).await.unwrap();
        let saga_id = uuid::Uuid::now_v7();
        store.reserve_proofs(ys.clone(), &saga_id).await.unwrap();
        let total: u64 = proofs.iter().map(|proof| proof.amount.to_u64()).sum();
        let saga = WalletSaga::new(
            saga_id,
            cdk::wallet::types::WalletSagaState::Swap(
                cdk::wallet::types::SwapSagaState::SwapRequested,
            ),
            Amount::from(total),
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Swap(cdk::wallet::types::SwapOperationData {
                input_amount: Amount::from(total),
                output_amount: Amount::from(total),
                counter_start: Some(0),
                counter_end: Some(1),
                blinded_messages: None,
            }),
        );
        store.add_saga(saga).await.unwrap();

        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            SwapReconcileTransport::new(states),
            None,
        ));
        let wallet = WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([seed; 64])
            .shared_client(connector)
            .build()
            .unwrap();
        (wallet, saga_id, ys)
    }

    #[tokio::test]
    async fn swap_saga_all_inputs_unspent_rolls_back() {
        // A stranded swap_requested saga whose reserved inputs are all
        // UNSPENT at the mint never executed — roll it back to spendable.
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let (wallet, saga_id, ys) = swap_saga_wallet(
            40,
            vec![proof],
            vec![ProofState::from((proof_y, State::Unspent))],
        )
        .await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(report.swap_rolled_back, 1, "unspent inputs must roll back");
        assert_eq!(report.swap_recovered, 0);
        assert!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .is_empty(),
            "saga must be dropped after rollback"
        );
        assert!(
            wallet
                .localstore
                .get_reserved_proofs(&saga_id)
                .await
                .unwrap()
                .is_empty(),
            "inputs must be unreserved after rollback"
        );
        let unspent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
            .unwrap();
        assert!(
            ys.iter()
                .all(|y| unspent.iter().any(|info| info.y == *y)),
            "rolled-back inputs must be spendable (Unspent) again"
        );
    }

    #[tokio::test]
    async fn swap_saga_all_inputs_spent_recovers_via_restore() {
        // A stranded swap_requested saga whose reserved inputs are all
        // SPENT at the mint executed — complete it: re-derive outputs via NUT-13
        // restore (no-op against the fake), mark inputs Spent, drop the saga.
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let (wallet, saga_id, _ys) = swap_saga_wallet(
            41,
            vec![proof],
            vec![ProofState::from((proof_y, State::Spent))],
        )
        .await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(report.swap_recovered, 1, "spent inputs must complete via restore");
        assert_eq!(report.swap_rolled_back, 0);
        assert!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .is_empty(),
            "saga must be dropped after completion"
        );
        assert!(
            wallet
                .localstore
                .get_reserved_proofs(&saga_id)
                .await
                .unwrap()
                .is_empty(),
            "inputs must be released from the saga after completion"
        );
        // Truthful: spent inputs must NOT reappear as spendable (no phantom credit).
        let unspent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
            .unwrap();
        assert!(
            unspent.iter().all(|info| info.y != proof_y),
            "spent inputs must never be credited back as Unspent"
        );
    }

    #[tokio::test]
    async fn swap_saga_mixed_input_states_refuses() {
        // A partial/mixed answer (one input Spent, one Unspent) is
        // inconsistent — keep refusing fail-closed, leave the saga wedged.
        let seller = secret_key(1).public_key();
        let proof_a = p2pk_proof(4, seller);
        let proof_b = p2pk_proof(3, seller);
        let y_a = proof_a.y().unwrap();
        let y_b = proof_b.y().unwrap();
        let (wallet, saga_id, _ys) = swap_saga_wallet(
            42,
            vec![proof_a, proof_b],
            vec![
                ProofState::from((y_a, State::Spent)),
                ProofState::from((y_b, State::Unspent)),
            ],
        )
        .await;

        let err = retire_eligible_incomplete_sagas(&wallet)
            .await
            .expect_err("mixed input states must refuse");
        match &err {
            PaymentWalletError::Reconcile(message)
                if message.contains("neither all-unspent nor all-spent") => {}
            other => panic!("expected mixed-state refuse, got: {other}"),
        }
        assert_eq!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .len(),
            1,
            "saga must remain wedged on a mixed answer"
        );
        assert_eq!(
            wallet
                .localstore
                .get_reserved_proofs(&saga_id)
                .await
                .unwrap()
                .len(),
            2,
            "reserved inputs must remain untouched on refuse"
        );
    }
}
