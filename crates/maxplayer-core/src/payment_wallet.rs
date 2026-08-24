//! Wallet-backed payment policy, adapters, and authenticity checks.

use std::collections::HashSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cashu::nuts::nut18::PaymentRequestPayload;
use cashu::{
    Amount, CheckStateRequest, CurrencyUnit, MintUrl, Proofs, PublicKey as CashuPublicKey,
    SecretKey, SpendingConditions, State, Token,
};
use cdk::wallet::{
    HttpClient, KeysetFilter, MintConnector, ReceiveOptions, SendOptions, Wallet,
};
use cdk::wallet::types::{SendSagaState, TransactionDirection, WalletSagaState};
use nostr_sdk::PublicKey as NostrPublicKey;

use crate::gateway::ParsedOffer;
use crate::payment::{
    AttemptId, EffectError, LockedPayment, LockedTokenGate, PaymentEffects, PaymentKey,
    PaymentTerms, ReceiptEvidence,
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

/// Reason code surfaced when a dead mint blocks the keyset refresh that
/// [`expand_token_proofs`] needs to expand a token naming a rotated keyset.
///
/// Distinct from the two above because that refresh is reached from three paths — buyer reconcile,
/// buyer NUT-18 payload and seller receive — so neither the post nor the pay code would be true at
/// all three call sites.
pub const MINT_UNREACHABLE_KEYSETS: &str = "mint_unreachable_keysets";

/// Last-resort ceiling on ONE buyer-worker round-trip across the synchronous bridge in
/// [`CdkPaymentEffects::request`].
///
/// Every mint-touching leg the worker runs is already bounded at [`MINT_TOUCH_TIMEOUT`]; this ceiling
/// sits well above their worst-case sum (`4 × MINT_TOUCH_TIMEOUT`), so a worker that fails closed on
/// its own always surfaces that specific refusal first. The bridge timeout only fires if the worker
/// is wedged in a leg no inner bound covers — turning a would-be infinite park
/// (MakePrisms/maxplayerai#387) into a bounded, logged, fail-closed refusal that moves no money.
const BRIDGE_RECV_TIMEOUT: Duration = Duration::from_secs(20);

/// Outcome of retiring incomplete send sagas that are safe to clean up.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetireReport {
    /// `Send(ProofsReserved)` sagas cancelled after mint Unspent proof.
    pub retired: usize,
    /// Mapped `Send(TokenCreated)` sagas SEEN — a completed send, never a wedge. Counted whether
    /// or not the row was then dropped, so `mapped_token_created - retired_mapped` is the number
    /// that were recognised but refused.
    pub mapped_token_created: usize,
    /// Mapped `Send(TokenCreated)` sagas actually RETIRED (row deleted). A tally is not a
    /// retirement: this field exists because the mapped arm used to only count, which left the
    /// table growing by one per payment forever (#293).
    pub retired_mapped: usize,
    /// Stranded Swap sagas rolled back after mint-truth said the reserved inputs
    /// were all UNSPENT — the swap never executed.
    pub swap_rolled_back: usize,
    /// Stranded Swap sagas completed after mint-truth said the reserved inputs
    /// were all SPENT — the swap executed; inputs marked spent and outputs
    /// re-derived from the wallet seed via NUT-13 `Wallet::restore`.
    pub swap_recovered: usize,
    /// Stranded `Send(TokenCreated)` sagas completed after mint-truth said the
    /// reserved inputs were all SPENT — the send executed, so the inputs are
    /// recorded Spent and the saga dropped. The token's outputs are deliberately
    /// NOT re-derived: they belong to the payee (see [`complete_spent_send_saga`]).
    pub send_completed: usize,
    /// Per-saga refusals from this pass, as `"<saga id>: <reason>"`. A saga we
    /// cannot resolve is a SURVIVOR, not a fatal error — it stays in localstore for
    /// `recover_unmapped_sagas` to refuse over, so the fail-closed property is
    /// unchanged while one unresolvable saga can no longer stop the resolvable ones.
    pub unresolved: Vec<String>,
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
    /// post-time dust guard, `mint_unreachable_pay` for the pay path,
    /// `mint_unreachable_keysets` for the keyset refresh behind token expansion) and `mint`
    /// names the unreachable mint URL.
    MintUnreachable {
        reason: &'static str,
        mint: String,
        detail: String,
    },
}

/// Failure of the read-only, pre-budget mint fee probe.
///
/// This stays separate from [`EffectError`] so the one outcome the authorize path must cancel
/// distinctly is not erased at the wallet-worker bridge. All other worker commands retain the
/// existing generic effect-error plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightError {
    /// The configured mint returned no response, timed out, or answered with a server error.
    MintUnreachable {
        reason: &'static str,
        mint: String,
        detail: String,
    },
    /// Any other worker/fee-probe refusal.
    Other(String),
}

impl std::fmt::Display for PreflightError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MintUnreachable { reason, mint, detail } => write!(
                formatter,
                "{reason}: mint {mint} unreachable or erroring ({detail})"
            ),
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PreflightError {}

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
        // Bounded like every other mint touch (MINT_TOUCH_TIMEOUT). The P2PK send options force cdk's
        // `force_swap` branch, whose mint HTTP cdk leaves un-timed; a stalled mint here would otherwise
        // park the worker (and, through the bridge, the caller) forever (#387). On timeout we fail
        // closed: no `prepared`, no proofs committed, no money moved.
        let prepared = match tokio::time::timeout(
            MINT_TOUCH_TIMEOUT,
            self.wallet.prepare_send(terms.amount, options),
        )
        .await
        {
            Ok(result) => result.map_err(wallet_error)?,
            Err(_elapsed) => {
                return Err(mint_unreachable(
                    self.wallet,
                    MINT_UNREACHABLE_PAY,
                    format!("prepare_send exceeded {MINT_TOUCH_TIMEOUT:?}"),
                ));
            }
        };
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
        // Same bound: confirm settles the swap over mint HTTP cdk leaves un-timed. On timeout we fail
        // closed and return NO token, so no money reaches the seller this run. A ProofsReserved left
        // mid-swap is compensated exactly as a definitive confirm failure is — the ATTEMPT_METADATA tag
        // lets the next recover/reconcile map it, so a later retry never double-spends.
        let token = match tokio::time::timeout(MINT_TOUCH_TIMEOUT, prepared.confirm(None)).await {
            Ok(Ok(token)) => token,
            Ok(Err(error)) => {
                // Definitive confirm failure should leave no residual ProofsReserved
                // (CDK compensates). Any leftover is handled on the next recover.
                return Err(wallet_error(error));
            }
            Err(_elapsed) => {
                return Err(mint_unreachable(
                    self.wallet,
                    MINT_UNREACHABLE_PAY,
                    format!("confirm exceeded {MINT_TOUCH_TIMEOUT:?}"),
                ));
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

    /// Reconcile the already-minted P2PK-locked token for `attempt_id` and gate it on a LIVE mint
    /// proof-state check. Returns the token IFF EVERY proof reads `Unspent` at the mint; otherwise a
    /// DISTINCT [`LockedTokenGate`].
    ///
    /// REUSE, never re-mint: this only reads the existing confirmed send transaction (via
    /// [`Self::reconcile`]) and asks the mint about its proofs — it NEVER calls
    /// `prepare_send`/`confirm`, so it debits nothing. The proof-state query is the non-mutating
    /// NUT-07 [`nut07_check_state_non_mutating`] every retire/reconcile path uses; CDK
    /// `check_proofs_spent` is forbidden here (it deletes mint-Spent `y`s from localstore).
    ///
    /// Classification (the recovered-`Locked` discriminator, design §4):
    /// - complete answer, ALL `Unspent` ⇒ `Ok(token)` — the seller never redeemed, safe to deliver;
    /// - complete answer, ANY proof not `Unspent` (Spent/Pending/unknown) ⇒ [`LockedTokenGate::Spent`]
    ///   — the proofs are P2PK-locked to the seller, so only the seller can spend them; a non-unspent
    ///   proof means the seller already redeemed by some path ⇒ STOP + alarm the accounting gap;
    /// - no confirmed transaction ⇒ [`LockedTokenGate::Missing`] — STOP, never blind-remint;
    /// - incomplete/mismatched NUT-07 answer or any transport failure ⇒ [`LockedTokenGate::Effect`]
    ///   — cannot verify, fail closed (never treated as all-`Unspent`; not a false spend alarm).
    async fn reconcile_locked_token_if_unspent(
        &self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<LockedPayment, LockedTokenGate> {
        require_wallet_matches(self.wallet, terms).map_err(gate_effect)?;
        let token = match self.reconcile(attempt_id, terms).await {
            Ok(Some(token)) => token,
            Ok(None) => {
                return Err(LockedTokenGate::Missing(format!(
                    "no confirmed send transaction for attempt {}",
                    attempt_id.as_str()
                )));
            }
            Err(error) => return Err(gate_effect(error)),
        };
        // Decompose the reconciled token into proofs (needs the wallet's mint keysets to expand a
        // TokenV4's short keyset ids) and compute each proof Y for the NUT-07 query — the same
        // `token.proofs` + `proof.y()` calculation the send payload build and `reconcile` use.
        let ys = token_proof_ys(self.wallet, &token).await.map_err(gate_effect)?;
        // Non-mutating NUT-07 — NEVER `check_proofs_spent` (it deletes mint-Spent `y`s).
        let states = nut07_check_state_non_mutating(self.wallet, ys.clone())
            .await
            .map_err(gate_effect)?;
        let requested: HashSet<_> = ys.iter().copied().collect();
        let reported: HashSet<_> = states.iter().map(|proof_state| proof_state.y).collect();
        if requested.is_empty() || requested != reported {
            // Incomplete/partial/wrong-y answer: we cannot verify the token is unspent. Fail closed
            // as an Effect error (a check we could not complete), NOT a false "seller redeemed"
            // alarm — treating this as all-`Unspent` is exactly the phantom-credit hazard the retire
            // path refuses.
            return Err(LockedTokenGate::Effect(EffectError::new(
                "NUT-07 proof-state answer incomplete/mismatched; cannot verify locked token unspent (fail-closed, no resend)",
            )));
        }
        if states
            .iter()
            .all(|proof_state| proof_state.state == State::Unspent)
        {
            Ok(LockedPayment::new(token))
        } else {
            Err(LockedTokenGate::Spent(format!(
                "a proof is not Unspent at the mint for attempt {} — the P2PK-locked proofs were \
                 redeemed by the seller. Do NOT resend. This may be BENIGN (our own prior, \
                 interrupted send delivered and the seller redeemed it) OR an unaccounted \
                 redemption; verify which before treating it as an accounting gap.",
                attempt_id.as_str()
            )))
        }
    }

    async fn recover_unmapped_sagas(&self) -> Result<(), PaymentWalletError> {
        let report = retire_eligible_incomplete_sagas(self.wallet).await?;
        // A fail-closed refuse that only lands in `report.unresolved` is silent on this
        // path — the daemon already runs this every pay attempt, and nothing else prints
        // the vector. Surface the resolver's own reason so a wedged wallet names why.
        if !report.unresolved.is_empty() {
            crate::opline!(
                "buyer wallet: saga resolve refused: {}",
                report.unresolved.join("; ")
            );
        }
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
                    return Err(refuse_unmapped(
                        "wallet has an incomplete ProofsReserved operation that could not be retired safely",
                        &report,
                    ));
                }
                WalletSagaState::Send(SendSagaState::TokenCreated) => {
                    return Err(refuse_unmapped(
                        "wallet has an incomplete TokenCreated operation with no matching confirmed attempt",
                        &report,
                    ));
                }
                WalletSagaState::Send(SendSagaState::RollingBack) => {
                    return Err(refuse_unmapped(
                        "wallet has an in-flight RollingBack send; refuse rather than retire",
                        &report,
                    ));
                }
                other => {
                    return Err(refuse_unmapped(
                        &format!(
                            "wallet has an incomplete non-eligible saga ({}); refuse rather than retire",
                            other.state_str()
                        ),
                        &report,
                    ));
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

/// A transport-class cdk error or a 5xx response means the configured mint is unavailable or
/// erroring. A 4xx remains a protocol/request failure and must not be clean-cancelled as downtime.
pub(crate) fn is_mint_unreachable(error: &cdk::Error) -> bool {
    matches!(error, cdk::Error::HttpError(None, _))
        || matches!(error, cdk::Error::HttpError(Some(status), _) if (500..=599).contains(status))
}

/// Start a one-shot, same-process mint endpoint that answers its first request with HTTP 502.
#[cfg(test)]
pub(crate) fn http_502_mint() -> (String, thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 502 mint responder");
    let address = listener.local_addr().expect("read 502 responder address");
    let responder = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept mint request");
        std::io::Write::write_all(
            &mut stream,
            b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .expect("write 502 mint response");
    });
    (format!("http://{address}"), responder)
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

/// Live input fee for `proof_count` inputs at this mint, bounded and fail-closed.
///
/// The same reader and the same bound as the dust guard below, for callers that must PRICE a spend
/// whose input count they have already bounded rather than test the N=1 floor. Never defaults the
/// fee: a mint that will not answer refuses the spend.
pub(crate) async fn bounded_input_fee(
    wallet: &Wallet,
    proof_count: u64,
) -> Result<Amount, PaymentWalletError> {
    match mint_input_fee_bounded(wallet, proof_count, MINT_TOUCH_TIMEOUT).await {
        BoundedFee::Fee(fee) => Ok(fee),
        BoundedFee::Failed(error) => Err(error),
        BoundedFee::Unreachable(detail) => {
            Err(mint_unreachable(wallet, MINT_UNREACHABLE_PAY, detail))
        }
    }
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
            // A `TokenCreated` send WITH a confirmed outgoing transaction is a send that
            // demonstrably completed. Counting it and moving on is what left this table growing by
            // exactly one row per payment, forever (#293) — so the row is dropped here.
            WalletSagaState::Send(SendSagaState::TokenCreated)
                if saga_has_confirmed_outgoing_tx(wallet, &saga).await? =>
            {
                report.mapped_token_created += 1;
                // A refusal is recorded and the pass continues: one saga that cannot be retired
                // must not stop the others from being, and leaving the row is always the safe
                // outcome here (it is inert, not a wedge — `recover_unmapped_sagas` tolerates it).
                match retire_one_mapped_send(wallet, &saga).await {
                    Ok(()) => report.retired_mapped += 1,
                    Err(refusal) => report.unresolved.push(format!("{}: {refusal}", saga.id)),
                }
            }
            // Unmapped `Send(TokenCreated)`: resolve on mint truth. Left unresolved these
            // wedge the wallet permanently — `recover_unmapped_sagas` refuses them
            // wallet-wide, so one stuck saga blocks every outbound payment regardless of
            // amount. Send interprets the mint answer more conservatively than Swap does;
            // see [`resolve_one_send_saga`].
            WalletSagaState::Send(SendSagaState::TokenCreated) => {
                if let Err(refusal) = resolve_one_send_saga(wallet, &saga, &mut report).await {
                    report.unresolved.push(format!("{}: {refusal}", saga.id));
                }
            }
            // Swap-family sagas (proofs_reserved / swap_requested): resolve on mint
            // truth. A mid-swap mint outage otherwise strands these
            // forever — retire never handled them and recover_unmapped_sagas refused
            // them via its `other` arm. Resolve deletes resolvable sagas.
            WalletSagaState::Swap(_) => {
                if let Err(refusal) = resolve_one_swap_saga(wallet, &saga, &mut report).await {
                    report.unresolved.push(format!("{}: {refusal}", saga.id));
                }
            }
            // RollingBack and other kinds: leave in place for recover_unmapped_sagas to
            // refuse. Do not retire here.
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

/// Any `transactions` row whose `saga_id` is this saga — direction does not matter.
///
/// `get_reserved_proofs` only asks about proof bindings. A healthy wallet also
/// records a `saga_id` on every transaction. Together the two signals distinguish
/// a never-bound orphan (neither) from Spent-then-deleted under old
/// `check_proofs_spent` (transaction row still present).
async fn saga_has_referencing_transaction(
    wallet: &Wallet,
    saga: &cdk::wallet::types::WalletSaga,
) -> Result<bool, PaymentWalletError> {
    let txs = wallet.list_transactions(None).await.map_err(wallet_error)?;
    Ok(txs.iter().any(|tx| tx.saga_id == Some(saga.id)))
}

/// Fold resolver refusals into the wallet-wide recover error so the daemon retry
/// log names the actual cause instead of only the downstream "incomplete saga" line.
fn refuse_unmapped(message: &str, report: &RetireReport) -> PaymentWalletError {
    if report.unresolved.is_empty() {
        PaymentWalletError::Reconcile(message.into())
    } else {
        PaymentWalletError::Reconcile(format!(
            "{message}; resolver: {}",
            report.unresolved.join("; ")
        ))
    }
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

/// Fold a wallet/reconcile failure into the fail-closed [`LockedTokenGate::Effect`] arm — a check
/// we could not complete, distinct from a proof that verifiably read spent.
fn gate_effect(error: PaymentWalletError) -> LockedTokenGate {
    LockedTokenGate::Effect(EffectError::new(error.to_string()))
}

/// Is this expansion failure specifically "the token names a short keyset id absent from the set we
/// passed"?
///
/// Matched on the typed variant, never on the message. A string match would start silently passing
/// (or silently retrying an unrelated fault) the first time cashu edits its error text, and nothing
/// in our suite would say so.
fn is_unknown_short_keyset_id(error: &cashu::nuts::nut00::Error) -> bool {
    matches!(
        error,
        cashu::nuts::nut00::Error::NUT02(cashu::nuts::nut02::Error::UnknownShortKeysetId)
    )
}

/// Expand a token into proofs against the mint's keysets, refreshing once if the token names a
/// keyset this wallet has not seen.
///
/// A `TokenV4` carries short keyset ids that only mean something against the mint's keyset set, and
/// that set is served from a local cache. So a mint that rotated its keysets after the cache was
/// populated leaves the expansion permanently unsatisfiable: `get_mint_keysets` filters by unit and
/// cannot know which ids the token needs, so it answers `Ok` from the stale cache and the miss
/// surfaces inside `Token::proofs`. cdk's own TTL does not rescue this — `MintMetadataCache::load`
/// prefers any populated database row and re-stamps its timestamp, so the cache never ages into a
/// mint fetch. [`Wallet::refresh_keysets`] is the only forced fetch, so on that one failure we call
/// it, re-read the keysets and retry the expansion exactly once.
///
/// Three details this depends on, each load-bearing:
///
/// - The retry re-reads with [`KeysetFilter::All`] instead of using `refresh_keysets`' return value,
///   which is filtered to *active* keysets. A token may legitimately carry proofs from a rotated,
///   inactive keyset — mints still redeem those — so expanding against the active-only set would
///   break exactly the tokens this path exists to recover.
/// - `refresh_keysets` reports `Err` when the mint serves no *active* keyset for our unit, and it
///   does so **after** writing the fetched data to the store. Its error therefore does not mean the
///   refresh did not happen, so we reload and retry regardless and keep the error for the diagnosis
///   only.
/// - Only `UnknownShortKeysetId` refreshes. Any other expansion failure is a decode fault that a
///   mint round-trip cannot repair, so it returns untouched rather than buying a pointless
///   round-trip on every malformed token.
///
/// Exactly one retry: a second miss means the mint does not serve that keyset at all, and looping
/// would turn a fail-closed refusal into an unbounded stall against a live mint.
async fn expand_token_proofs(wallet: &Wallet, token: &Token) -> Result<Proofs, PaymentWalletError> {
    let keysets = wallet
        .get_mint_keysets(KeysetFilter::All)
        .await
        .map_err(wallet_error)?;
    let first = match token.proofs(&keysets) {
        Ok(proofs) => return Ok(proofs),
        Err(error) => error,
    };
    if !is_unknown_short_keyset_id(&first) {
        return Err(wallet_error(first));
    }

    // Bounded like every other mint touch: this is a live fetch against a mint that may be dead,
    // and it sits on the buyer money path behind the MCP tool deadline.
    let refresh_error = match tokio::time::timeout(MINT_TOUCH_TIMEOUT, wallet.refresh_keysets()).await
    {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_elapsed) => {
            return Err(mint_unreachable(
                wallet,
                MINT_UNREACHABLE_KEYSETS,
                format!("refresh_keysets exceeded {MINT_TOUCH_TIMEOUT:?} recovering from: {first}"),
            ));
        }
    };

    let keysets = wallet
        .get_mint_keysets(KeysetFilter::All)
        .await
        .map_err(wallet_error)?;
    token.proofs(&keysets).map_err(|second| {
        // Keep the whole causal chain: collapsing this into `second` alone loses that a refresh was
        // attempted, and collapsing it into `first` hides that the mint still does not serve the
        // keyset after being asked directly.
        let refreshed = match &refresh_error {
            Some(error) => format!("; refresh reported: {error}"),
            None => String::new(),
        };
        wallet_error(format!(
            "token names a keyset the mint does not serve after refresh: {second} \
             (first attempt: {first}{refreshed})"
        ))
    })
}

/// Compute the NUT-07 `Y` for every proof in a reconciled token. Expands the token into proofs
/// using the wallet's mint keysets (a TokenV4 stores short keyset ids that must be expanded), then
/// `proof.y()` each — the SAME decomposition [`build_nut18_payload`] performs and the same `y()`
/// [`CdkBuyerMint::reconcile`] validates against the confirmed transaction.
async fn token_proof_ys(
    wallet: &Wallet,
    token: &Token,
) -> Result<Vec<CashuPublicKey>, PaymentWalletError> {
    let proofs = expand_token_proofs(wallet, token).await?;
    proofs
        .iter()
        .map(|proof| proof.y())
        .collect::<Result<Vec<_>, _>>()
        .map_err(wallet_error)
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

/// Mint-truth outcome for a stranded saga's reserved INPUT proofs. Shared by the
/// Swap and Send families: the mint answer is the same question either way.
///
/// ⚠ The two families read the SAME answer at DIFFERENT strengths. `AllUnspent`
/// proves "the operation never executed" only where nothing else could hold the
/// outputs — true for Swap, FALSE for Send, whose token may already be in a payee's
/// hands. Callers must interpret, not just match (see [`resolve_one_send_saga`]).
enum ReservedInputTruth {
    /// Every reserved input is UNSPENT at the mint.
    AllUnspent,
    /// Every reserved input is SPENT at the mint — the operation executed.
    AllSpent,
}

/// Classify a saga's reserved inputs from a NUT-07 answer. Requires a
/// complete answer (response Y-set == requested ys); empty/partial/wrong-y refuses
/// exactly like [`refuse_if_not_all_unspent`]. Any mix of Spent/Unspent, or any
/// Pending, refuses fail-closed — only a unanimous answer is actionable.
fn classify_reserved_inputs(
    requested_ys: &[CashuPublicKey],
    states: &[cashu::ProofState],
) -> Result<ReservedInputTruth, PaymentWalletError> {
    let requested: HashSet<_> = requested_ys.iter().copied().collect();
    let reported: HashSet<_> = states.iter().map(|proof_state| proof_state.y).collect();
    if requested.is_empty() || requested != reported {
        return Err(PaymentWalletError::Reconcile(
            "saga resolve refused: NUT-07 response Y set incomplete or mismatched (empty/partial/wrong-y; per-saga fail-closed)"
                .into(),
        ));
    }
    if states
        .iter()
        .all(|proof_state| proof_state.state == State::Unspent)
    {
        Ok(ReservedInputTruth::AllUnspent)
    } else if states
        .iter()
        .all(|proof_state| proof_state.state == State::Spent)
    {
        Ok(ReservedInputTruth::AllSpent)
    } else {
        Err(PaymentWalletError::Reconcile(
            "saga resolve refused: reserved inputs neither all-unspent nor all-spent (mixed/pending; per-saga fail-closed)"
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
///   * mixed/pending, incomplete NUT-07, empty reservation **with** a referencing
///     transaction, or an unreachable mint ⇒ keep refusing fail-closed.
///   * empty reservation **and** no referencing transaction ⇒ never-bound orphan;
///     drop the row. Proof bindings alone cannot tell orphan from Spent-then-deleted
///     under old `check_proofs_spent`; the transaction row can. If either signal is
///     present, refuse exactly as before.
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
    // Empty reservation is ambiguous given only proof bindings (never-bound orphan
    // vs Spent-then-deleted under old check_proofs_spent). A referencing
    // `transactions.saga_id` is the discriminator: the Spent-then-deleted case
    // still has a row, so it still refuses. Neither signal ⇒ never-bound orphan.
    if reserved.is_empty() {
        if saga_has_referencing_transaction(wallet, saga).await? {
            return Err(PaymentWalletError::Reconcile(
                "swap-saga resolve refused: empty reserved set (migration-safe fail-closed; leave wedged-safer-than-double-spend)"
                    .into(),
            ));
        }
        drop_never_bound_swap_orphan(wallet, saga).await?;
        return Ok(());
    }
    let ys = reserved.iter().map(|info| info.y).collect::<Vec<_>>();
    // Non-mutating NUT-07 — never check_proofs_spent (it deletes Spent ys from
    // localstore). A dead mint surfaces as a Reconcile error here and propagates as
    // a fail-closed refuse.
    let states = nut07_check_state_non_mutating(wallet, ys.clone()).await?;
    match classify_reserved_inputs(&ys, &states)? {
        ReservedInputTruth::AllUnspent => {
            rollback_swap_saga(wallet, saga).await?;
            report.swap_rolled_back += 1;
        }
        ReservedInputTruth::AllSpent => {
            complete_spent_swap_saga(wallet, saga).await?;
            report.swap_recovered += 1;
        }
    }
    Ok(())
}

/// Drop a Swap saga that bound nothing and recorded nothing: no reserved proofs
/// and no `transactions.saga_id`. That pair is the never-bound orphan; deleting
/// the row touches no proof and no movement record.
///
/// TOCTOU: re-prove both absences immediately before mutating. If either signal
/// appears, refuse — this must not become a back door around the fail-closed
/// empty-reserved branch.
async fn drop_never_bound_swap_orphan(
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
                "swap-saga orphan drop refused: saga disappeared before mutate".into(),
            )
        })?;
    if !matches!(fresh.state, WalletSagaState::Swap(_)) {
        return Err(PaymentWalletError::Reconcile(
            "swap-saga orphan drop refused: saga no longer a Swap saga before mutate (TOCTOU)"
                .into(),
        ));
    }
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    if !reserved.is_empty() {
        return Err(PaymentWalletError::Reconcile(
            "swap-saga orphan drop refused: proofs bound before mutate (TOCTOU / fail-closed)"
                .into(),
        ));
    }
    if saga_has_referencing_transaction(wallet, &fresh).await? {
        return Err(PaymentWalletError::Reconcile(
            "swap-saga orphan drop refused: a transaction referenced this saga before mutate \
             (TOCTOU / fail-closed)"
                .into(),
        ));
    }
    wallet
        .localstore
        .delete_saga(&saga.id)
        .await
        .map_err(|error| {
            PaymentWalletError::Reconcile(format!(
                "swap-saga orphan drop failed deleting saga: {error}"
            ))
        })?;
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
        classify_reserved_inputs(&ys, &states)?,
        ReservedInputTruth::AllUnspent
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

/// Mint-truth resolution for a stranded `Send(TokenCreated)` saga.
///
/// `TokenCreated` means cdk serialized a token and marked the inputs pending-spent
/// awaiting claim. With no confirmed outgoing tx to map it to, `recover_unmapped_sagas`
/// refuses it forever — and that refusal is **wallet-wide, not job-scoped**: one stuck
/// saga blocks every subsequent outbound payment from the wallet, whatever its amount.
///
/// Resolved on mint truth over the reserved INPUT proofs:
///   * all SPENT ⇒ the send executed ⇒ record the inputs Spent and drop the saga
///     ([`complete_spent_send_saga`]).
///   * all UNSPENT ⇒ **refuse** — see below, this is where Send and Swap part.
///   * empty reservation / mixed / pending / incomplete NUT-07 / unreachable mint ⇒ keep
///     refusing fail-closed.
///
/// So this resolves exactly one case: the send that demonstrably COMPLETED. That is the
/// root-cause fix — a wallet no longer wedges on its own finished sends — and it deliberately
/// stops there. Clearing an already-stranded row is an operator decision about specific money,
/// not machinery this path should carry.
///
/// ★ Why all-UNSPENT refuses here when [`resolve_one_swap_saga`] rolls back: a Swap has
/// no counterparty, so all-unspent proves the swap never executed. **A Send does.** For a
/// Send, all-unspent proves only that the token is UNCLAIMED — indistinguishable from
/// "delivered, awaiting redemption" — and the token is bearer, recording nothing about who
/// it was for. Rolling those inputs back to spendable would queue a payee's money for
/// re-spend, first-spender-wins. Same mint answer, strictly weaker strength.
async fn resolve_one_send_saga(
    wallet: &Wallet,
    saga: &cdk::wallet::types::WalletSaga,
    report: &mut RetireReport,
) -> Result<(), PaymentWalletError> {
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    // Migration-safe fail-closed, same posture as the Swap path: an empty reservation is
    // ambiguous (never-bound orphan vs Spent-then-deleted under old `check_proofs_spent`).
    //
    // ★ Deleting such a row would touch no proof, so it is safe with respect to MONEY — but the
    // row is the only store of `SendOperationData.token`, so it is not safe with respect to the
    // RECORD: it would destroy a possibly-claimable bearer token with no chance for the operator
    // to capture it first. Clearing one inherited row is a deliberate decision about specific
    // money, not something a pay path should do to every wallet as a side effect. Refuse here and
    // leave it visible.
    if reserved.is_empty() {
        return Err(PaymentWalletError::Reconcile(
            "send-saga resolve refused: empty reserved set (migration-safe fail-closed; the row is \
             the only store of its token, so dropping it is an explicit operator decision)"
                .into(),
        ));
    }
    let ys = reserved.iter().map(|info| info.y).collect::<Vec<_>>();
    // Non-mutating NUT-07 — never check_proofs_spent (it deletes Spent ys from
    // localstore). A dead mint surfaces here and propagates as a fail-closed refuse.
    let states = nut07_check_state_non_mutating(wallet, ys.clone()).await?;
    match classify_reserved_inputs(&ys, &states)? {
        ReservedInputTruth::AllSpent => {
            complete_spent_send_saga(wallet, saga).await?;
            report.send_completed += 1;
        }
        ReservedInputTruth::AllUnspent => {
            return Err(PaymentWalletError::Reconcile(
                "send-saga resolve refused: reserved inputs all UNSPENT at the mint, which for a \
                 Send means the token is unclaimed — NOT that the send never happened. A bearer \
                 token names no payee, so rolling back could revoke a payment already owed \
                 (first-spender-wins); leave wedged-safer-than-revoking"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// Retire a `Send(TokenCreated)` saga that has a confirmed outgoing transaction: the send
/// completed, and the row is bookkeeping for a finished operation.
///
/// ★ **This is a SECOND admission predicate, not a relaxation of the first.** The empty-reserved
/// refusal in [`retire_one_proofs_reserved`] guards a `ProofsReserved` saga, where the send has NOT
/// completed and an empty reserved set cannot be told apart from a spent-then-deleted orphan.
/// Here the outgoing transaction independently establishes that the send happened, so the same
/// observation carries the opposite meaning: nothing is reserved because there is nothing left to
/// reserve. That guard is untouched, and this path never reads its way around it.
///
/// The identifier is `transactions.saga_id`, never the amount — amounts collide freely (four
/// 100-sat sends in the live wallet), so an amount match is not evidence of identity. Direction is
/// asserted too, since a saga id could otherwise be satisfied by an incoming row.
///
/// ⚠ Refuses on a NON-EMPTY reserved set. A completed send still holding reserved inputs is an
/// anomaly, and deleting the saga would strand those proofs reserved against a row that no longer
/// exists — unspendable, with nothing left to explain why. Surfacing beats tidying.
///
/// Proofs are deliberately NOT mutated. The inputs of a completed send are already in their final
/// state (that is why nothing is reserved), and the token's OUTPUT proofs belong to the payee —
/// they are unreachable from a saga in any case, because `created_by_operation` is never populated
/// (tracked separately). Retiring here is a row deletion and nothing else.
///
/// TOCTOU: re-prove state and the transaction immediately before mutating.
async fn retire_one_mapped_send(
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
                "mapped-send retire refused: saga disappeared before mutate".into(),
            )
        })?;
    if !matches!(
        fresh.state,
        WalletSagaState::Send(SendSagaState::TokenCreated)
    ) {
        return Err(PaymentWalletError::Reconcile(
            "mapped-send retire refused: saga no longer a TokenCreated send before mutate (TOCTOU)"
                .into(),
        ));
    }
    // Re-prove the admission itself, not just the state it was admitted from.
    if !saga_has_confirmed_outgoing_tx(wallet, &fresh).await? {
        return Err(PaymentWalletError::Reconcile(
            "mapped-send retire refused: no confirmed outgoing tx for this saga before mutate \
             (TOCTOU)"
                .into(),
        ));
    }
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    if !reserved.is_empty() {
        return Err(PaymentWalletError::Reconcile(format!(
            "mapped-send retire refused: send completed but {} proofs are still reserved against \
             this saga; deleting it would strand them (surface, do not tidy)",
            reserved.len()
        )));
    }
    wallet
        .localstore
        .delete_saga(&saga.id)
        .await
        .map_err(|error| {
            PaymentWalletError::Reconcile(format!("mapped-send retire failed deleting saga: {error}"))
        })?;
    Ok(())
}

/// Complete a `Send(TokenCreated)` saga whose inputs are all SPENT at the mint: the send
/// executed. Record the inputs Spent, clear their operation binding, drop the saga.
///
/// ★ Deliberately does NOT call `Wallet::restore`, which is exactly how
/// [`complete_spent_swap_saga`] recovers ITS outputs. A Send's outputs are the token, and
/// restore re-derives from this wallet's own seed+counter — so restoring here would pull
/// the outstanding token's proofs back into OUR balance, silently reclaiming money already
/// handed to a payee. That is the same violation the all-UNSPENT branch refuses, arriving
/// through a different door. The outputs staying out of this wallet IS the correct
/// accounting: the inputs are spent, and the token is someone else's claim.
///
/// TOCTOU: re-prove (still a `Send(TokenCreated)`, reservation non-empty, still all-spent
/// at the mint) immediately before mutating.
async fn complete_spent_send_saga(
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
                "send-saga complete refused: saga disappeared before mutate (leave wedged-safer)"
                    .into(),
            )
        })?;
    if !matches!(
        fresh.state,
        WalletSagaState::Send(SendSagaState::TokenCreated)
    ) {
        return Err(PaymentWalletError::Reconcile(
            "send-saga complete refused: saga no longer a TokenCreated send before mutate (TOCTOU)"
                .into(),
        ));
    }
    let reserved = wallet
        .localstore
        .get_reserved_proofs(&saga.id)
        .await
        .map_err(wallet_error)?;
    if reserved.is_empty() {
        return Err(PaymentWalletError::Reconcile(
            "send-saga complete refused: reserved emptied before mutate (TOCTOU)".into(),
        ));
    }
    let ys = reserved.iter().map(|info| info.y).collect::<Vec<_>>();
    let states = nut07_check_state_non_mutating(wallet, ys.clone()).await?;
    if !matches!(
        classify_reserved_inputs(&ys, &states)?,
        ReservedInputTruth::AllSpent
    ) {
        return Err(PaymentWalletError::Reconcile(
            "send-saga complete refused: inputs no longer all-spent before mutate (TOCTOU)".into(),
        ));
    }

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
                "send-saga complete failed marking inputs spent: {error}"
            ))
        })?;
    wallet
        .localstore
        .delete_saga(&saga.id)
        .await
        .map_err(|error| {
            PaymentWalletError::Reconcile(format!("send-saga complete failed deleting saga: {error}"))
        })
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
    /// Pre-reserve dust/liveness probe: runs `require_fee_safe_amount` ON THE WORKER runtime so a
    /// dead/hung mint refuses BEFORE the caller commits budget — and without the caller-runtime wallet
    /// HTTP that deadlocked #387. Read-only (queries the keyset fee); no proofs move.
    PreflightFee {
        amount: Amount,
        response: mpsc::SyncSender<Result<Amount, PaymentWalletError>>,
    },
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
    /// Operator-completion proof-state gate: reconcile the already-minted token for `attempt_id`
    /// and check its proofs at the mint (non-mutating NUT-07). Returns the token IFF all-`Unspent`;
    /// distinct [`LockedTokenGate`] otherwise. REUSE — never mints, never debits.
    AssertLockedUnspent {
        attempt_id: AttemptId,
        terms: PaymentTerms,
        /// Two layers: the OUTER `PaymentWalletError` is the shared [`CdkPaymentEffects::request`]
        /// transport channel (so this command rides the SAME bounded worker bridge as `Send` — no
        /// bespoke recv), and the INNER `Result` is the proof-gate verdict.
        response: mpsc::SyncSender<Result<Result<LockedPayment, LockedTokenGate>, PaymentWalletError>>,
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
    let proofs = expand_token_proofs(wallet, &token).await?;
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
    /// Ceiling on one worker round-trip at the sync bridge; see [`BRIDGE_RECV_TIMEOUT`]. Held as a
    /// field (not read straight from the const) so a hermetic test can drive the fail-closed timeout
    /// path in milliseconds.
    recv_timeout: Duration,
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
            .name("maxplayer-payment-wallet".into())
            .spawn(move || {
                runtime.block_on(async move {
                    while let Some(command) = requests.recv().await {
                        match command {
                            BuyerCommand::PreflightFee { amount, response } => {
                                // Read-only dust/liveness probe on the worker runtime; a dead mint
                                // fails closed (bounded by MINT_TOUCH_TIMEOUT) before any budget commit.
                                // Returns the N=1 active-keyset input fee so the direct pay path can
                                // fold it into the cap charge (MakePrisms/maxplayerai#185).
                                let result = require_fee_safe_amount(&wallet, amount).await;
                                let _ = response.send(result);
                            }
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
                            BuyerCommand::AssertLockedUnspent {
                                attempt_id,
                                terms,
                                response,
                            } => {
                                // The reconcile + NUT-07 proof check runs HERE, on the WORKER
                                // runtime (identical to `Send`) — never a caller-runtime mint call.
                                // The gate verdict is the inner Result; the outer `Ok` marks the
                                // request itself as delivered (transport failures surface via
                                // `request()`'s own recv).
                                let verdict = CdkBuyerMint::new(&wallet)
                                    .reconcile_locked_token_if_unspent(&attempt_id, &terms)
                                    .await;
                                let _ = response.send(Ok(verdict));
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
            recv_timeout: BRIDGE_RECV_TIMEOUT,
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
        match result.recv_timeout(self.recv_timeout) {
            Ok(inner) => inner.map_err(|error| EffectError::new(error.to_string())),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(EffectError::new("payment wallet worker dropped its response"))
            }
            // Fail closed: a worker that has not answered within the bridge ceiling is treated as
            // wedged (MakePrisms/maxplayerai#387), never awaited forever. No response means no token
            // was handed back to the caller, so no money moved.
            Err(mpsc::RecvTimeoutError::Timeout) => Err(EffectError::new(format!(
                "payment wallet worker did not respond within {:?}; fail-closed refusal, no funds moved (see MakePrisms/maxplayerai#387)",
                self.recv_timeout
            ))),
        }
    }

    /// Pre-reserve dust/liveness guard, executed on the wallet worker (never the caller runtime). A
    /// dead/hung mint refuses with a bounded fail-closed error; the pay path returns BEFORE the budget
    /// gate, so a refusal burns ZERO spend — the property the removed pre-spawn check gave, minus the
    /// #387 cross-runtime deadlock. Read-only: queries the keyset fee, no proofs move.
    ///
    /// On success returns the estimated active-keyset input fee (the N=1 floor,
    /// `ceil(input_fee_ppk / 1000)`) for the send, so the DIRECT pay path can fold it into the cap
    /// charge — the swap input fee otherwise leaves the wallet uncounted by the per-job cap
    /// (MakePrisms/maxplayerai#185). It is a floor, not the exact `send_fee`: the real input count is
    /// unknown until `prepare_send`, so the cap counts at least one input's worth of fee.
    pub fn preflight_fee(&self, amount: Amount) -> Result<u64, PreflightError> {
        let (response, result) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .ok_or_else(|| PreflightError::Other("payment wallet worker is stopped".into()))?
            .try_send(BuyerCommand::PreflightFee { amount, response })
            .map_err(|error| {
                PreflightError::Other(format!("payment wallet worker unavailable: {error}"))
            })?;
        match result.recv_timeout(self.recv_timeout) {
            Ok(Ok(fee)) => Ok(fee.to_u64()),
            Ok(Err(PaymentWalletError::MintUnreachable { reason, mint, detail })) => {
                Err(PreflightError::MintUnreachable { reason, mint, detail })
            }
            Ok(Err(error)) => Err(PreflightError::Other(error.to_string())),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(PreflightError::Other(
                "payment wallet worker dropped its response".into(),
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(PreflightError::Other(format!(
                "payment wallet worker did not respond within {:?}; fail-closed refusal, no funds moved (see MakePrisms/maxplayerai#387)",
                self.recv_timeout
            ))),
        }
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

    fn assert_locked_token_unspent(
        &mut self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<LockedPayment, LockedTokenGate> {
        // Ride the SHARED `request()` worker bridge — the SAME path `send_payment` uses — so the
        // reconcile + NUT-07 proof check runs on the WORKER runtime and this leg inherits the
        // bounded worker recv (no bespoke recv/timeout that would collide with it at rebase). A
        // transport failure (`request` → EffectError) folds into the fail-closed `Effect` arm; the
        // inner Result is the distinct gate verdict (Spent / Missing / Ok token), preserved intact.
        match self.request(|response| BuyerCommand::AssertLockedUnspent {
            attempt_id: attempt_id.clone(),
            terms: terms.clone(),
            response,
        }) {
            Ok(verdict) => verdict,
            Err(effect_error) => Err(LockedTokenGate::Effect(effect_error)),
        }
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
        let proofs = expand_token_proofs(self.wallet, token).await?;
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
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::{Arc, Mutex};
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

    #[test]
    fn is_mint_unreachable_classifies_502_through_fee_and_preflight_bridge() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // Unit-level: drive a real cdk HTTP request through the bounded fee reader. This proves
        // cdk represents the raw responder as the status-bearing error the shared predicate sees.
        let (fee_mint, fee_responder) = http_502_mint();
        let fee_store = runtime.block_on(cdk_sqlite::wallet::memory::empty()).unwrap();
        let fee_wallet = Wallet::new(
            &fee_mint,
            CurrencyUnit::Sat,
            Arc::new(fee_store),
            [7; 64],
            None,
        )
        .unwrap();
        let fee_error = runtime
            .block_on(require_fee_safe_amount(&fee_wallet, Amount::from(10)))
            .expect_err("a 502 mint must refuse the pay-path fee probe");
        fee_responder.join().unwrap();
        assert!(
            matches!(
                fee_error,
                PaymentWalletError::MintUnreachable {
                    reason: MINT_UNREACHABLE_PAY,
                    ref mint,
                    ..
                } if mint == &fee_mint
            ),
            "502 must classify as MintUnreachable, got: {fee_error}"
        );

        // Bridge-level: prove the typed result survives preflight_fee and authorize conversion.
        let (bridge_mint, bridge_responder) = http_502_mint();
        let bridge_store = runtime.block_on(cdk_sqlite::wallet::memory::empty()).unwrap();
        let bridge_wallet = Wallet::new(
            &bridge_mint,
            CurrencyUnit::Sat,
            Arc::new(bridge_store),
            [8; 64],
            None,
        )
        .unwrap();
        let effects = CdkPaymentEffects::spawn(
            bridge_wallet,
            CountingSend(Arc::new(AtomicUsize::new(0))),
            |key: &PaymentKey, _: &PaymentSent| {
                Ok::<ReceiptEvidence, EffectError>(cosigned_receipt(key))
            },
        )
        .unwrap();
        let preflight = effects
            .preflight_fee(Amount::from(10))
            .expect_err("a 502 mint must survive the worker bridge as a typed refusal");
        bridge_responder.join().unwrap();
        assert!(
            matches!(
                preflight,
                PreflightError::MintUnreachable {
                    reason: MINT_UNREACHABLE_PAY,
                    ref mint,
                    ..
                } if mint == &bridge_mint
            ),
            "502 bridge outcome must stay typed, got: {preflight}"
        );
        let authorize = crate::authorize_pay::AuthorizePayError::from(preflight);
        assert!(
            matches!(
                authorize,
                crate::authorize_pay::AuthorizePayError::CancelledMintUnreachable {
                    ref mint,
                    ..
                } if mint == &bridge_mint
            ),
            "authorize outcome must stay typed, got: {authorize}"
        );
        let line = authorize.to_string();
        assert!(line.contains(&bridge_mint), "operator text names the mint: {line}");
        assert!(line.contains("unreachable"), "operator text classifies the refusal: {line}");
    }

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

    #[tokio::test]
    async fn seller_receive_completes_a_real_swap_at_non_default_realized_mint_via_signing_mock() {
        let seller_key = secret_key(1);
        let transport = SigningSwapTransport::new(1_000); // 1 proof → fee = 1
        let keyset = transport.keyset.clone();
        let proof = signed_p2pk_proof_for_keyset(4, seller_key.public_key(), keyset.id);
        let proof_y = proof.y().unwrap();
        let token = Token::new(
            mint(OTHER_MINT),
            vec![proof],
            None,
            CurrencyUnit::Sat,
        );
        let swap_calls = transport.swap_calls.clone();
        let spent_ys = transport.spent_ys.clone();
        let wallet = seller_wallet_at(OTHER_MINT, transport, keyset).await;
        let terms = PaymentTerms::new(
            mint(OTHER_MINT),
            Amount::from(4),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );

        let amount = CdkSellerReceive::new(&wallet, seller_key)
            .receive(
                &token,
                &terms,
                &accepted(&[MINT, OTHER_MINT]),
                &mint(OTHER_MINT),
            )
            .await
            .expect("real swap at realized non-default mint must succeed");

        assert_eq!(amount, Amount::from(4));
        assert_eq!(swap_calls.load(Ordering::SeqCst), 1);
        assert_eq!(wallet.total_balance().await.unwrap(), Amount::from(3));
        assert_eq!(&*spent_ys.lock().unwrap(), &HashSet::from([proof_y]));
    }

    #[tokio::test]
    async fn signing_swap_transport_refuses_replayed_receive_token() {
        let seller_key = secret_key(1);
        let transport = SigningSwapTransport::new(1_000);
        let keyset = transport.keyset.clone();
        let proof = signed_p2pk_proof_for_keyset(4, seller_key.public_key(), keyset.id);
        let proof_y = proof.y().unwrap();
        let token = Token::new(
            mint(OTHER_MINT),
            vec![proof],
            None,
            CurrencyUnit::Sat,
        );
        let swap_calls = transport.swap_calls.clone();
        let spent_ys = transport.spent_ys.clone();
        let first_wallet =
            seller_wallet_at(OTHER_MINT, transport.clone(), keyset.clone()).await;
        let replay_wallet = seller_wallet_at(OTHER_MINT, transport, keyset).await;
        let terms = PaymentTerms::new(
            mint(OTHER_MINT),
            Amount::from(4),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );

        CdkSellerReceive::new(&first_wallet, seller_key.clone())
            .receive(
                &token,
                &terms,
                &accepted(&[MINT, OTHER_MINT]),
                &mint(OTHER_MINT),
            )
            .await
            .expect("first presentation must redeem");
        let replay = CdkSellerReceive::new(&replay_wallet, seller_key)
            .receive(
                &token,
                &terms,
                &accepted(&[MINT, OTHER_MINT]),
                &mint(OTHER_MINT),
            )
            .await;

        assert!(matches!(replay, Err(PaymentWalletError::Wallet(_))));
        assert_eq!(swap_calls.load(Ordering::SeqCst), 2);
        assert_eq!(spent_ys.lock().unwrap().len(), 1);
        assert!(spent_ys.lock().unwrap().contains(&proof_y));
        assert_eq!(replay_wallet.total_balance().await.unwrap(), Amount::ZERO);
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

    // NETWORK (#720): the injected `connector` below only answers check-state. The send leg this
    // test drives to `Closed` runs through `fixture.wallet`'s OWN http client, which fetches
    // `MINT`'s (testnut's) keysets over the public internet — so this cannot run under
    // `net: denied`. Not silenced: `live-mints` is ON in the money-path CI job, which has a
    // network. See the feature's comment in Cargo.toml.
    #[cfg(feature = "live-mints")]
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

    // RED-PROVE (#387) — when the wallet worker NEVER answers, the sync bridge must fail closed with a
    // bounded refusal, never park. Before the fix `request()` blocked on a timer-less `recv()`; a
    // caller runtime stuck there (as `collect_blocking` was) is exactly the deadlock. Here the command
    // Receiver is kept alive but never drained, so the Lock — and the std response SyncSender it
    // carries — sits buffered forever with no answer coming: the pure "worker wedged" condition.
    #[test]
    fn bridge_recv_fails_closed_when_the_worker_never_answers() {
        let terms = wallet_terms(secret_key(9).public_key());
        let attempt_id = payment_key(&terms).attempt_id();

        // Kept alive to the end of the test; never received from ⇒ nothing ever answers, and the
        // buffered command's response sender never drops (a drop would be Disconnected, a DIFFERENT
        // arm — we are proving the Timeout arm).
        let (commands, _requests) = tokio::sync::mpsc::channel::<BuyerCommand>(1);
        let recv_timeout = Duration::from_millis(150);
        let effects = CdkPaymentEffects {
            commands: Some(commands),
            worker: None,
            receipt: (),
            recv_timeout,
        };

        let start = std::time::Instant::now();
        let result: Result<LockedPayment, EffectError> =
            effects.request(|response| BuyerCommand::Lock { attempt_id, terms, response });
        let elapsed = start.elapsed();

        // Not `expect_err`: LockedPayment is intentionally not Debug (it wraps a token).
        let err = match result {
            Ok(_) => panic!("a worker that never answers must refuse, not return Ok"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("did not respond")
                && err.to_string().contains("fail-closed"),
            "must be the bridge fail-closed refusal, got: {err}"
        );
        // Bounded at ~recv_timeout: it actually waited the timeout (not an early unrelated error) and
        // it RETURNED (no park). Without the recv_timeout this line is unreachable — the test hangs.
        assert!(
            elapsed >= Duration::from_millis(120) && elapsed < Duration::from_secs(5),
            "expected a bounded return near recv_timeout, got {elapsed:?}"
        );

        // CONTROL — under the SAME condition (a live sender, nothing ever sent), the timer-less
        // `recv()` the bridge used before #387 blocks indefinitely. Prove it is still parked well past
        // the window the recv_timeout already returned in, then release it so the thread exits cleanly.
        let (ctl_tx, ctl_rx) = mpsc::sync_channel::<u8>(1);
        let returned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = returned.clone();
        let handle = std::thread::spawn(move || {
            let _ = ctl_rx.recv(); // timer-less: parks until a send or all senders drop
            flag.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(350)); // > recv_timeout above
        assert!(
            !returned.load(Ordering::SeqCst),
            "timer-less recv() must still be parked past the recv_timeout window (this is the #387 park)"
        );
        drop(ctl_tx); // Disconnected ⇒ the control thread returns and exits — no leaked thread
        handle.join().unwrap();
    }

    // PAY-PATH ZERO-SPEND + NO-PARK RED-PROVE (#387) — pins BOTH properties on the SAME
    // never-answering-mint scenario, in ONE test, so neither regresses silently (the see-saw: the
    // pre-fix code was zero-spend but PARKED; bounding the bridge alone was no-park but LEAKED).
    // Mirrors authorize_pay's real order: a PRE-RESERVE worker preflight, THEN the gated pay. A worker
    // that never answers fails the preflight, so the budget gate is never entered.
    // Non-vacuity (closes the see-saw): drop the recv_timeout ⇒ the preflight parks and THIS test
    // hangs; drop the preflight (as the deadlock fix alone did) ⇒ the never-answer reaches the gate and
    // leaks (gate.spent() != 0, the phantom spend keeper flagged).
    #[test]
    fn pay_path_timeout_refuses_bounded_without_charging_the_budget() {
        let terms = wallet_terms(secret_key(9).public_key());
        let key = payment_key(&terms);
        let attempt = key.attempt_id();
        let authority = authority();
        let journal = MemoryPaymentJournal::default();

        // Never-answering worker: command Receiver kept alive but never drained ⇒ every command is
        // buffered forever with no answer coming (the wedged-worker / dead-mint condition).
        let (commands, _requests) = tokio::sync::mpsc::channel::<BuyerCommand>(1);
        let mut effects = CdkPaymentEffects {
            commands: Some(commands),
            worker: None,
            receipt: move |key: &PaymentKey, _: &PaymentSent| Ok(cosigned_receipt(key)),
            recv_timeout: Duration::from_millis(150),
        };

        let mut gate = crate::budget::BudgetGate::new(1_000);
        let charged = 7u64; // == terms.amount (Amount::from(7))

        let start = std::time::Instant::now();
        // authorize_pay runs this PRE-RESERVE preflight on the worker, then only reserves + pays if it
        // passed. A never-answering mint fails it, so the gate below is never entered.
        let preflight = effects.preflight_fee(terms.amount);
        let mut entered_gate = false;
        if preflight.is_ok() {
            entered_gate = true;
            let _ = gate.authorize_then_attempt(attempt.as_str(), charged, || {
                PaymentService::new(&journal).run_verified(&key, &terms, &authority, &mut effects)
            });
        }
        let elapsed = start.elapsed();

        // (i) fail-closed refusal at the pre-reserve preflight (the never-answering mint).
        assert!(preflight.is_err(), "a never-answering mint must fail the pre-reserve preflight");
        assert!(!entered_gate, "a failed preflight must short-circuit BEFORE the budget gate");
        // (ii) bounded — no park (without the recv_timeout this line is unreachable; the test hangs).
        assert!(elapsed < Duration::from_secs(5), "must be bounded (no park), took {elapsed:?}");
        // (iii) ZERO SPEND — the reserve never ran, so the never-answer burns no budget.
        assert_eq!(
            gate.spent(),
            0,
            "PHANTOM SPEND (#387): a never-answer pay-path timeout charged {} sats — a hang traded for a leak",
            gate.spent()
        );
    }

    // NETWORK (#720): reaches the same real send leg as
    // `worker_wires_reconcile_verify_and_send_into_the_real_state_machine`, so it fetches the live
    // mint's keysets too. ON in the money-path CI job; see `live-mints` in Cargo.toml.
    #[cfg(feature = "live-mints")]
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

    // ---- Direct red-proves of the LIVE proof-state classifier `reconcile_locked_token_if_unspent`
    // (real reconcile + real non-mutating NUT-07 against a mock mint), independent of the fake gate.

    /// The `Y` of the token's proof AS `reconcile` will reconstruct it — extracted exactly the way
    /// [`store_confirmed_attempt`] persists it (`into_proof(KEYSET_ID)`), so the mock mint answers
    /// check-state for the SAME `Y` the classifier queries.
    fn confirmed_proof_y(token: &Token) -> CashuPublicKey {
        let proof = match token {
            Token::TokenV4(token) => token.token[0].proofs[0]
                .clone()
                .into_proof(&Id::from_str(KEYSET_ID).unwrap()),
            Token::TokenV3(_) => panic!("fixture uses v4 token"),
        };
        proof.y().unwrap()
    }

    /// Build a wallet whose mint answers NUT-07 check-state for the token's proof with `mint_state`
    /// (`None` ⇒ an EMPTY/incomplete answer), holding a confirmed send transaction for the attempt so
    /// `reconcile` finds the P2PK-locked token. The keyset is cached in the store so
    /// `token.proofs`/`get_mint_keysets` never HTTP-GET (the mock only serves `/v1/checkstate`).
    /// Mirrors the retire-path mint-state harness.
    async fn gate_wallet(mint_state: Option<State>) -> (Wallet, PaymentTerms, AttemptId) {
        let seller = secret_key(1).public_key();
        let terms = wallet_terms(seller);
        let token = Token::new(
            mint(MINT),
            vec![p2pk_proof(7, seller)],
            None,
            CurrencyUnit::Sat,
        );
        let states = match mint_state {
            Some(state) => vec![ProofState::from((confirmed_proof_y(&token), state))],
            None => vec![],
        };
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        store
            .add_mint(mint(MINT), Some(MintInfo::new()))
            .await
            .unwrap();
        store
            .add_mint_keysets(
                mint(MINT),
                vec![KeySetInfo {
                    id: Id::from_str(KEYSET_ID).unwrap(),
                    unit: CurrencyUnit::Sat,
                    active: true,
                    input_fee_ppk: 0,
                    final_expiry: None,
                }],
            )
            .await
            .unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse { states }),
            None,
        ));
        let wallet = WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([7; 64])
            .shared_client(connector)
            .build()
            .unwrap();
        let attempt_id = payment_key(&terms).attempt_id();
        store_confirmed_attempt(&wallet, &attempt_id, &token).await;
        (wallet, terms, attempt_id)
    }

    // ALL proofs Unspent ⇒ Ok(token), and REUSE — no new send/mint transaction is created (a
    // re-mint would append a second outgoing tx). Non-vacuous: if the classifier returned Spent or
    // Effect on an all-Unspent answer, this would go red.
    #[tokio::test]
    async fn live_gate_all_unspent_returns_the_reused_token() {
        let (wallet, terms, attempt_id) = gate_wallet(Some(State::Unspent)).await;
        let before = wallet
            .list_transactions(Some(TransactionDirection::Outgoing))
            .await
            .unwrap()
            .len();

        let locked = CdkBuyerMint::new(&wallet)
            .reconcile_locked_token_if_unspent(&attempt_id, &terms)
            .await
            .expect("all-Unspent must return the reused token");

        assert_eq!(
            locked.token().value().unwrap(),
            Amount::from(7),
            "the reused token's realized value matches terms"
        );
        let after = wallet
            .list_transactions(Some(TransactionDirection::Outgoing))
            .await
            .unwrap()
            .len();
        assert_eq!(
            after, before,
            "REUSE: reconcile added no outgoing transaction — the existing token is reused, not re-minted"
        );
    }

    // ANY non-Unspent proof (Spent here) ⇒ STOP with `LockedTokenGate::Spent` — the send/no-send
    // discriminator for the real payment. Non-vacuous: a Spent proof returning Ok would send on an
    // already-redeemed token; this goes red if that regressed.
    #[tokio::test]
    async fn live_gate_spent_proof_stops_with_spent() {
        let (wallet, terms, attempt_id) = gate_wallet(Some(State::Spent)).await;
        match CdkBuyerMint::new(&wallet)
            .reconcile_locked_token_if_unspent(&attempt_id, &terms)
            .await
        {
            Err(LockedTokenGate::Spent(_)) => {}
            Err(other) => panic!("a Spent proof must STOP with Spent, got {other:?}"),
            Ok(_) => panic!("a Spent proof must NOT return Ok — that would resend a redeemed token"),
        }
    }

    // A Pending proof is also "not Unspent" ⇒ Spent-class STOP (design §4). Non-vacuous mirror of
    // the Spent case over the other non-Unspent state.
    #[tokio::test]
    async fn live_gate_pending_proof_stops_with_spent() {
        let (wallet, terms, attempt_id) = gate_wallet(Some(State::Pending)).await;
        match CdkBuyerMint::new(&wallet)
            .reconcile_locked_token_if_unspent(&attempt_id, &terms)
            .await
        {
            Err(LockedTokenGate::Spent(_)) => {}
            Err(other) => panic!("a Pending proof must STOP (not-Unspent), got {other:?}"),
            Ok(_) => panic!("a Pending proof must NOT return Ok"),
        }
    }

    // An incomplete NUT-07 answer (requested Y-set != reported: here an EMPTY response) ⇒ fail-closed
    // `Effect` — NOT Spent, NOT Ok. This is the phantom-credit-hazard guard: an answer we cannot
    // verify is NEVER treated as all-Unspent (would resend) and is NOT a false accounting-gap alarm.
    // Non-vacuous: dropping the requested==reported check would make this return Ok or Spent and go red.
    #[tokio::test]
    async fn live_gate_incomplete_answer_fails_closed_as_effect() {
        let (wallet, terms, attempt_id) = gate_wallet(None).await;
        match CdkBuyerMint::new(&wallet)
            .reconcile_locked_token_if_unspent(&attempt_id, &terms)
            .await
        {
            Err(LockedTokenGate::Effect(_)) => {}
            Err(other) => panic!("an incomplete answer must fail closed as Effect, got {other:?}"),
            Ok(_) => panic!("an incomplete answer must NOT return Ok (phantom-credit hazard)"),
        }
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
    async fn seller_wallet_at<T: HttpTransport + 'static>(
        mint_url: &str,
        transport: T,
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
            "maxplayer/job",
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
            requested_agent: None,
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

    fn signed_p2pk_proof_for_keyset(
        amount: u64,
        seller: PublicKey,
        keyset_id: Id,
    ) -> Proof {
        let secret = Secret::try_from(SpendingConditions::new_p2pk(
            seller,
            Some(Conditions::default()),
        ))
        .unwrap();
        let y = cashu::dhke::hash_to_curve(secret.as_bytes()).unwrap();
        let c = cashu::dhke::sign_message(&secret_key(amount as u8 + 10), &y).unwrap();
        Proof::new(Amount::from(amount), keyset_id, secret, c)
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

    /// In-process signing mint for receive/swap tests.
    ///
    /// It enforces input/output conservation (including the configured input fee),
    /// rejects already-spent input Ys, preserves output amount/keyset metadata, and
    /// returns real `C_ = k * B_` signatures with real DLEQ proofs. It deliberately
    /// does not model keyset rotation, NUT-19 caching, auth, or quote lifecycles;
    /// those are later increments tracked in #91/#339.
    #[derive(Clone, Debug)]
    struct SigningSwapTransport {
        keyset: KeySet,
        signing_keys: BTreeMap<Amount, SecretKey>,
        spent_ys: Arc<Mutex<HashSet<PublicKey>>>,
        swap_calls: Arc<AtomicUsize>,
    }

    impl SigningSwapTransport {
        fn new(input_fee_ppk: u64) -> Self {
            let keyset = test_keyset_with_fee(input_fee_ppk);
            let signing_keys = keyset
                .keys
                .keys()
                .keys()
                .map(|amount| (*amount, secret_key(amount.to_u64() as u8 + 10)))
                .collect();
            Self {
                keyset,
                signing_keys,
                spent_ys: Arc::default(),
                swap_calls: Arc::default(),
            }
        }

        fn signing_key(&self, amount: Amount) -> Result<SecretKey, cdk::Error> {
            self.signing_keys
                .get(&amount)
                .cloned()
                .ok_or(cdk::Error::AmountKey)
        }
    }

    impl Default for SigningSwapTransport {
        fn default() -> Self {
            Self::new(0)
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for SigningSwapTransport {
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
                return Err(cdk::Error::Custom(format!(
                    "unexpected POST {}",
                    url.path()
                )));
            }
            self.swap_calls.fetch_add(1, Ordering::SeqCst);
            let request: cashu::SwapRequest = serde_json::from_value(
                serde_json::to_value(payload)
                    .map_err(|error| cdk::Error::Custom(error.to_string()))?,
            )
            .map_err(|error| cdk::Error::Custom(error.to_string()))?;

            let mut request_ys = HashSet::new();
            for proof in request.inputs() {
                if proof.keyset_id != self.keyset.id {
                    return Err(cdk::Error::KeysetUnknown(proof.keyset_id));
                }
                let y = proof
                    .y()
                    .map_err(|error| cdk::Error::Custom(error.to_string()))?;
                if !request_ys.insert(y) {
                    return Err(cdk::Error::DuplicateInputs);
                }
            }
            let mut spent = self
                .spent_ys
                .lock()
                .map_err(|_| cdk::Error::Custom("spent-Y ledger poisoned".into()))?;
            if request_ys.iter().any(|y| spent.contains(y)) {
                return Err(cdk::Error::Custom("swap input is already spent".into()));
            }

            let inputs = request
                .input_amount()
                .map_err(|error| cdk::Error::Custom(error.to_string()))?;
            let outputs = request
                .output_amount()
                .map_err(|error| cdk::Error::Custom(error.to_string()))?;
            let fee_ppk = self
                .keyset
                .input_fee_ppk
                .checked_mul(request.inputs().len() as u64)
                .ok_or(cdk::Error::AmountOverflow)?;
            let fee = fee_ppk.div_ceil(1_000);
            if outputs
                .to_u64()
                .checked_add(fee)
                .ok_or(cdk::Error::AmountOverflow)?
                != inputs.to_u64()
            {
                return Err(cdk::Error::TransactionUnbalanced(
                    inputs.to_u64(),
                    outputs.to_u64(),
                    fee,
                ));
            }

            let signatures = request
                .outputs()
                .iter()
                .map(|blinded_message| {
                    if blinded_message.keyset_id != self.keyset.id {
                        return Err(cdk::Error::KeysetUnknown(blinded_message.keyset_id));
                    }
                    let signing_key = self.signing_key(blinded_message.amount)?;
                    let c = cashu::dhke::sign_message(
                        &signing_key,
                        &blinded_message.blinded_secret,
                    )
                    .map_err(|error| cdk::Error::Custom(error.to_string()))?;
                    cashu::nuts::BlindSignature::new(
                        blinded_message.amount,
                        c,
                        self.keyset.id,
                        &blinded_message.blinded_secret,
                        signing_key,
                    )
                    .map_err(|error| cdk::Error::Custom(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;

            spent.extend(request_ys);
            let response = cashu::SwapResponse::new(signatures);
            serde_json::from_value(
                serde_json::to_value(response)
                    .map_err(|error| cdk::Error::Custom(error.to_string()))?,
            )
            .map_err(|error| cdk::Error::Custom(error.to_string()))
        }
    }

    /// Fake mint for stranded-Swap-saga resolution. Answers NUT-07
    /// `/v1/checkstate` with configured input states and NUT-13 `/v1/restore` with
    /// an empty batch (no output signatures) — the existing scaffolding does not
    /// simulate a signing mint, so the AllSpent branch exercises the completion
    /// path (mark inputs Spent + drop saga) with a no-op output re-derivation.
    #[derive(Clone, Debug, Default)]
    /// NUT-07 fake that answers **only for the ys actually requested**, from a configured
    /// map — the way a mint does.
    ///
    /// ★ The filtering is load-bearing, not politeness. [`classify_reserved_inputs`] requires
    /// the response Y-set to equal the requested set, so a fake that returns every configured
    /// state at once refuses any wallet holding more than one saga — which would make a
    /// per-saga isolation test pass with neither saga resolving.
    struct ReconcileTransport {
        states: std::collections::HashMap<CashuPublicKey, State>,
    }

    impl ReconcileTransport {
        fn new(states: Vec<ProofState>) -> Self {
            Self {
                states: states
                    .into_iter()
                    .map(|proof_state| (proof_state.y, proof_state.state))
                    .collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for ReconcileTransport {
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
            if url.path().ends_with("/v1/checkstate") {
                let request: cashu::CheckStateRequest = serde_json::from_value(
                    serde_json::to_value(payload)
                        .map_err(|error| cdk::Error::Custom(error.to_string()))?,
                )
                .map_err(|error| cdk::Error::Custom(error.to_string()))?;
                // Answer only what was asked, and only for ys we know — an unknown y is
                // simply absent from the response, which is what makes the caller refuse.
                let states = request
                    .ys
                    .iter()
                    .filter_map(|y| {
                        self.states
                            .get(y)
                            .map(|state| ProofState::from((*y, *state)))
                    })
                    .collect::<Vec<_>>();
                return serde_json::from_value(
                    serde_json::to_value(cashu::CheckStateResponse { states })
                        .map_err(|error| cdk::Error::Custom(error.to_string()))?,
                )
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
    /// local, and a [`ReconcileTransport`] reporting `states` for the inputs.
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
            ReconcileTransport::new(states),
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

    /// Build a wallet holding `proofs` (present and Unspent), keysets seeded so NUT-13
    /// restore stays local, and a [`ReconcileTransport`] reporting `states`. Adds NO saga —
    /// each test attaches the send saga(s) it needs via [`add_send_saga`], so one wallet can
    /// hold several and the isolation behaviour is observable.
    async fn send_saga_wallet(seed: u8, proofs: Vec<Proof>, states: Vec<ProofState>) -> Wallet {
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
        let infos = proofs
            .iter()
            .map(|proof| {
                ProofInfo::new(proof.clone(), mint(MINT), State::Unspent, CurrencyUnit::Sat)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        store.update_proofs(infos, vec![]).await.unwrap();

        let connector = Arc::new(BaseHttpClient::with_transport(
            mint(MINT),
            ReconcileTransport::new(states),
            None,
        ));
        WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([seed; 64])
            .shared_client(connector)
            .build()
            .unwrap()
    }

    /// Attach one `Send(TokenCreated)` saga binding `proofs`. An empty slice makes the
    /// proof-less "ghost" row — a saga that binds nothing at all.
    async fn add_send_saga(wallet: &Wallet, proofs: &[Proof]) -> uuid::Uuid {
        let saga_id = uuid::Uuid::now_v7();
        if !proofs.is_empty() {
            let ys = proofs
                .iter()
                .map(|proof| proof.y().unwrap())
                .collect::<Vec<_>>();
            wallet
                .localstore
                .reserve_proofs(ys, &saga_id)
                .await
                .unwrap();
        }
        let total: u64 = proofs.iter().map(|proof| proof.amount.to_u64()).sum();
        let saga = WalletSaga::new(
            saga_id,
            WalletSagaState::Send(SendSagaState::TokenCreated),
            Amount::from(total),
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: Amount::from(total),
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: None,
            }),
        );
        wallet.localstore.add_saga(saga).await.unwrap();
        saga_id
    }

    /// Attach one `Swap(SwapRequested)` saga that binds nothing — the never-bound
    /// orphan shape. Callers that need a referencing transaction add it separately
    /// via [`add_tx_for_saga`].
    async fn add_empty_swap_saga(wallet: &Wallet) -> uuid::Uuid {
        let saga_id = uuid::Uuid::now_v7();
        let saga = WalletSaga::new(
            saga_id,
            WalletSagaState::Swap(cdk::wallet::types::SwapSagaState::SwapRequested),
            Amount::ZERO,
            mint(MINT),
            CurrencyUnit::Sat,
            cdk::wallet::types::OperationData::Swap(cdk::wallet::types::SwapOperationData {
                input_amount: Amount::ZERO,
                output_amount: Amount::ZERO,
                counter_start: Some(0),
                counter_end: Some(1),
                blinded_messages: None,
            }),
        );
        wallet.localstore.add_saga(saga).await.unwrap();
        saga_id
    }

    async fn all_proof_ys(wallet: &Wallet) -> HashSet<CashuPublicKey> {
        wallet
            .localstore
            .get_proofs(None, None, None, None)
            .await
            .unwrap()
            .iter()
            .map(|info| info.y)
            .collect()
    }

    /// Attach a transaction to `saga_id`. Direction, amount and the id are all explicit so a test
    /// can build the NEAR-MISSES — an incoming row, or a row whose amount matches while its
    /// `saga_id` does not — which is where the admission predicate is actually decided.
    async fn add_tx_for_saga(
        wallet: &Wallet,
        saga_id: Option<uuid::Uuid>,
        direction: TransactionDirection,
        amount: u64,
    ) {
        wallet
            .localstore
            .add_transaction(Transaction {
                mint_url: wallet.mint_url.clone(),
                direction,
                amount: Amount::from(amount),
                fee: Amount::ZERO,
                unit: wallet.unit.clone(),
                ys: vec![],
                timestamp: 1,
                memo: None,
                metadata: HashMap::new(),
                quote_id: None,
                payment_request: None,
                payment_proof: None,
                payment_method: None,
                saga_id,
            })
            .await
            .unwrap();
    }

    // #293. A `TokenCreated` send WITH a confirmed outgoing transaction is a COMPLETED send. The
    // arm that recognised it used to only increment a counter, so the table grew by one row per
    // payment forever — measured live at 7 sagas against 7 outgoing transactions, 1:1.
    #[tokio::test]
    async fn a_mapped_token_created_saga_is_retired_not_merely_counted() {
        let wallet = send_saga_wallet(60, vec![], vec![]).await;
        let saga_id = add_send_saga(&wallet, &[]).await;
        add_tx_for_saga(&wallet, Some(saga_id), TransactionDirection::Outgoing, 100).await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();

        assert_eq!(report.mapped_token_created, 1, "it must still be RECOGNISED as mapped");
        assert_eq!(report.retired_mapped, 1, "and recognising it is not enough — it must be DROPPED");
        assert!(report.unresolved.is_empty(), "unexpected refusal: {:?}", report.unresolved);
        // The predicate that matters is over the artifact, not the report: the row is gone.
        assert!(
            wallet.localstore.get_saga(&saga_id).await.unwrap().is_none(),
            "the saga row must be absent after a retire — a tally is not a retirement"
        );
    }

    // RED LEGS for the admission predicate. Each is a near-miss that a sloppier join would admit,
    // and admitting any of them deletes bookkeeping for an operation that did not demonstrably
    // finish. The saga must survive every one of them.
    #[tokio::test]
    async fn a_mapped_retire_refuses_every_near_miss_and_leaves_the_saga_in_place() {
        // ★ THE IDENTIFIER CONTROL. A transaction with the SAME AMOUNT but a different `saga_id`
        // must not admit. This is the exact collinearity that made two unrelated row-sets look
        // like one during triage: `100` appears four times in the live wallet, so an amount match
        // is not evidence of identity. Only `transactions.saga_id` is.
        let wallet = send_saga_wallet(61, vec![], vec![]).await;
        let saga_id = add_send_saga(&wallet, &[]).await;
        add_tx_for_saga(
            &wallet,
            Some(uuid::Uuid::now_v7()),
            TransactionDirection::Outgoing,
            0,
        )
        .await;
        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(
            report.retired_mapped, 0,
            "a tx matching by amount but NOT saga_id must not admit a retire"
        );
        assert!(
            wallet.localstore.get_saga(&saga_id).await.unwrap().is_some(),
            "the saga must survive an amount-only match"
        );

        // Direction. A saga id can be satisfied by an INCOMING row, which says nothing about a
        // send having gone out.
        let wallet = send_saga_wallet(62, vec![], vec![]).await;
        let saga_id = add_send_saga(&wallet, &[]).await;
        add_tx_for_saga(&wallet, Some(saga_id), TransactionDirection::Incoming, 0).await;
        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(report.retired_mapped, 0, "an incoming tx must not admit a retire");
        assert!(
            wallet.localstore.get_saga(&saga_id).await.unwrap().is_some(),
            "the saga must survive an incoming-only match"
        );

        // No transaction at all — the #269 fail-closed property, which this change must not erode.
        let wallet = send_saga_wallet(63, vec![], vec![]).await;
        let saga_id = add_send_saga(&wallet, &[]).await;
        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(
            report.retired_mapped, 0,
            "an UNMAPPED token_created saga must never reach the mapped retire"
        );
        assert!(
            wallet.localstore.get_saga(&saga_id).await.unwrap().is_some(),
            "an unmapped saga must survive for recover_unmapped_sagas to refuse over"
        );
    }

    // A completed send that still holds RESERVED inputs is an anomaly, and deleting its saga would
    // strand those proofs reserved against a row that no longer exists — unspendable, with nothing
    // left to say why. Surface it instead of tidying it away.
    #[tokio::test]
    async fn a_mapped_send_still_holding_reserved_proofs_refuses_rather_than_stranding_them() {
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let wallet = send_saga_wallet(64, vec![proof.clone()], vec![]).await;
        let saga_id = add_send_saga(&wallet, &[proof]).await;
        add_tx_for_saga(&wallet, Some(saga_id), TransactionDirection::Outgoing, 7).await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();

        assert_eq!(report.mapped_token_created, 1, "still recognised as a mapped send");
        assert_eq!(report.retired_mapped, 0, "but it must NOT be retired");
        assert!(
            report.unresolved.iter().any(|line| line.contains("still reserved")),
            "the refusal must surface why, got: {:?}",
            report.unresolved
        );
        assert!(
            wallet.localstore.get_saga(&saga_id).await.unwrap().is_some(),
            "the saga must survive so the reserved proofs stay explained"
        );
    }

    #[tokio::test]
    async fn send_saga_all_inputs_spent_completes_without_reclaiming_the_token() {
        // Inputs SPENT at the mint ⇒ the send executed. Record that truthfully and drop the
        // saga so it stops wedging the wallet.
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let wallet = send_saga_wallet(
            50,
            vec![proof.clone()],
            vec![ProofState::from((proof_y, State::Spent))],
        )
        .await;
        let before = all_proof_ys(&wallet).await;
        add_send_saga(&wallet, &[proof]).await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();

        assert_eq!(report.send_completed, 1, "spent inputs must complete the send");
        assert!(
            report.unresolved.is_empty(),
            "a resolvable saga must not be recorded unresolved: {:?}",
            report.unresolved
        );
        assert!(
            wallet
                .localstore
                .get_incomplete_sagas()
                .await
                .unwrap()
                .is_empty(),
            "saga must be dropped once completed"
        );
        let spent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Spent]), None)
            .await
            .unwrap();
        assert!(
            spent.iter().any(|info| info.y == proof_y),
            "inputs must be recorded Spent, not left pending"
        );
        // ★ The no-restore property, asserted on proof identity rather than on a balance:
        // completing a send must not pull the token's outputs into OUR wallet. They are the
        // payee's. `Wallet::restore` would re-derive them from our own seed and silently
        // reclaim money already handed over — the same violation the all-UNSPENT branch
        // refuses, arriving through a different door.
        assert_eq!(
            all_proof_ys(&wallet).await,
            before,
            "no proof may APPEAR while completing a send — that would be reclaiming the token"
        );
    }

    #[tokio::test]
    async fn send_saga_all_inputs_unspent_refuses_rather_than_revoking_an_unclaimed_token() {
        // ★ Where Send parts from Swap. All-unspent proves the token is UNCLAIMED, which is
        // indistinguishable from delivered-and-awaiting-redemption; the token is bearer, so it
        // names no payee. Rolling the inputs back would queue someone else's money for
        // re-spend. Refusing keeps the wallet wedged, which is the cheaper failure.
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let wallet = send_saga_wallet(
            51,
            vec![proof.clone()],
            vec![ProofState::from((proof_y, State::Unspent))],
        )
        .await;
        let saga_id = add_send_saga(&wallet, &[proof]).await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();

        assert_eq!(report.send_completed, 0, "an unclaimed token must not complete");
        assert_eq!(
            report.unresolved.len(),
            1,
            "the refusal must be RECORDED, never swallowed"
        );
        assert!(
            report.unresolved[0].contains("all UNSPENT"),
            "the reason must name the mint answer: {}",
            report.unresolved[0]
        );
        assert_eq!(
            wallet.localstore.get_incomplete_sagas().await.unwrap().len(),
            1,
            "an unresolvable saga must survive for recover_unmapped_sagas to refuse over"
        );
        assert_eq!(
            wallet
                .localstore
                .get_reserved_proofs(&saga_id)
                .await
                .unwrap()
                .len(),
            1,
            "inputs must stay bound — never returned to spendable"
        );
    }

    #[tokio::test]
    async fn send_saga_binding_no_proofs_refuses_rather_than_destroying_its_token_record() {
        // Deleting a proof-less row would be safe with respect to MONEY — it touches no proof —
        // and unsafe with respect to the RECORD: the row is the only store of the token. So the
        // pay path refuses and leaves it visible. Clearing one is an operator decision about
        // specific money, not a side effect of trying to pay someone else.
        let seller = secret_key(1).public_key();
        let bystander = p2pk_proof(7, seller);
        let bystander_y = bystander.y().unwrap();
        let wallet = send_saga_wallet(52, vec![bystander], vec![]).await;
        add_send_saga(&wallet, &[]).await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();

        assert_eq!(report.send_completed, 0);
        assert_eq!(
            report.unresolved.len(),
            1,
            "the refusal must be recorded: {:?}",
            report.unresolved
        );
        assert!(
            report.unresolved[0].contains("empty reserved set"),
            "the reason must name the ambiguity: {}",
            report.unresolved[0]
        );
        assert_eq!(
            wallet.localstore.get_incomplete_sagas().await.unwrap().len(),
            1,
            "the row must survive — dropping it would destroy the only copy of its token"
        );
        let unspent = wallet
            .localstore
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
            .unwrap();
        assert!(
            unspent.iter().any(|info| info.y == bystander_y),
            "an unrelated proof must be left exactly as it was"
        );
    }

    #[tokio::test]
    async fn one_unresolvable_saga_does_not_veto_a_resolvable_one_in_the_same_pass() {
        // ★ The wedge this fixes. Both resolver call sites used to `?`-propagate a per-saga
        // refusal, so ONE unresolvable saga aborted the whole pass and kept every resolvable
        // one stuck — and a real wallet had exactly that mix. Per-saga isolation means a
        // refusal is recorded and the pass continues; `recover_unmapped_sagas` still refuses
        // over the survivor, so nothing about fail-closed changes.
        let seller = secret_key(1).public_key();
        let spent_proof = p2pk_proof(7, seller);
        let unspent_proof = p2pk_proof(11, seller);
        let spent_y = spent_proof.y().unwrap();
        let unspent_y = unspent_proof.y().unwrap();
        let wallet = send_saga_wallet(
            53,
            vec![spent_proof.clone(), unspent_proof.clone()],
            vec![
                ProofState::from((spent_y, State::Spent)),
                ProofState::from((unspent_y, State::Unspent)),
            ],
        )
        .await;
        let resolvable = add_send_saga(&wallet, &[spent_proof]).await;
        let unresolvable = add_send_saga(&wallet, &[unspent_proof]).await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();

        assert_eq!(
            report.send_completed, 1,
            "the resolvable saga must clear DESPITE the other refusing"
        );
        assert_eq!(
            report.unresolved.len(),
            1,
            "exactly the unresolvable one is recorded: {:?}",
            report.unresolved
        );
        let remaining = wallet.localstore.get_incomplete_sagas().await.unwrap();
        assert_eq!(remaining.len(), 1, "only the unresolvable saga may remain");
        assert_eq!(
            remaining[0].id, unresolvable,
            "the survivor must be the unresolvable saga, not the resolvable one"
        );
        assert!(
            !remaining.iter().any(|saga| saga.id == resolvable),
            "the resolvable saga must be gone"
        );
    }

    #[tokio::test]
    async fn a_recorded_refusal_still_blocks_the_pay_path() {
        // ★ Per-saga isolation must not weaken fail-closed. A refusal no longer aborts the retire
        // pass, so the property that actually protects money has to be asserted where it now
        // lives: `recover_unmapped_sagas` refuses over the SURVIVOR. Without this, "the pass
        // returns Ok" could be mistaken for "the wallet may pay", which is the whole risk of
        // downgrading an error to a record.
        let seller = secret_key(1).public_key();
        let proof = p2pk_proof(7, seller);
        let proof_y = proof.y().unwrap();
        let wallet = send_saga_wallet(
            54,
            vec![proof.clone()],
            vec![ProofState::from((proof_y, State::Unspent))],
        )
        .await;
        add_send_saga(&wallet, &[proof]).await;

        // The retire pass itself now succeeds — that is the isolation change.
        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(report.unresolved.len(), 1);

        // And the pay path still refuses, on the survivor.
        let refused = CdkBuyerMint::new(&wallet).recover_unmapped_sagas().await;
        match &refused {
            Err(PaymentWalletError::Reconcile(message))
                if message.contains("incomplete TokenCreated operation") => {}
            other => panic!("an unresolved send saga must still block the pay path, got: {other:?}"),
        }
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

        // The refusal is now RECORDED per-saga rather than aborting the pass, so one
        // unresolvable saga cannot keep resolvable ones wedged. What must not change is the
        // fail-closed outcome: the saga survives untouched, and `recover_unmapped_sagas` still
        // refuses over survivors, so the pay path stays blocked.
        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(
            report.unresolved.len(),
            1,
            "mixed input states must be recorded as unresolved: {:?}",
            report.unresolved
        );
        assert!(
            report.unresolved[0].contains("neither all-unspent nor all-spent"),
            "expected mixed-state refuse, got: {}",
            report.unresolved[0]
        );
        assert_eq!(report.swap_rolled_back, 0, "a mixed answer must not roll back");
        assert_eq!(report.swap_recovered, 0, "a mixed answer must not complete");
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

    // #748. A Swap saga with no bound proofs and no referencing transaction is a
    // never-bound orphan — nothing was reserved to it and nothing was recorded as
    // moving for it. Drop it so it stops wedging every outbound payment.
    #[tokio::test]
    async fn empty_swap_orphan_with_no_transaction_is_dropped_and_pay_resumes() {
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let attempt_id = key.attempt_id();
        store_confirmed_attempt(&fixture.wallet, &attempt_id, &fixture.token).await;
        let saga_id = add_empty_swap_saga(&fixture.wallet).await;

        let locked = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&attempt_id, &fixture.terms)
            .await
            .expect("a never-bound swap orphan must not wedge pay");

        assert_eq!(locked.token(), &fixture.token);
        assert!(
            fixture
                .wallet
                .localstore
                .get_saga(&saga_id)
                .await
                .unwrap()
                .is_none(),
            "the never-bound orphan must be dropped"
        );
    }

    // #748. The branch that protects against double-spend: empty reserved PLUS a
    // referencing transaction is the Spent-then-deleted shape. Still refuse.
    #[tokio::test]
    async fn empty_swap_saga_with_a_referencing_transaction_still_refuses() {
        let wallet = send_saga_wallet(70, vec![], vec![]).await;
        let saga_id = add_empty_swap_saga(&wallet).await;
        add_tx_for_saga(&wallet, Some(saga_id), TransactionDirection::Outgoing, 0).await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert_eq!(
            report.unresolved.len(),
            1,
            "empty-reserved + referencing tx must refuse: {:?}",
            report.unresolved
        );
        assert!(
            report.unresolved[0].contains("empty reserved set"),
            "the reason must name the empty reservation: {}",
            report.unresolved[0]
        );
        assert!(
            wallet.localstore.get_saga(&saga_id).await.unwrap().is_some(),
            "the saga must survive — this is the Spent-then-deleted fail-closed branch"
        );

        // Daemon path: the resolver reason must reach the recover error, not vanish
        // into report.unresolved while recover only says "incomplete non-eligible saga".
        let refused = CdkBuyerMint::new(&wallet).recover_unmapped_sagas().await;
        match &refused {
            Err(PaymentWalletError::Reconcile(message))
                if message.contains("empty reserved set") => {}
            other => panic!(
                "resolver refusal must surface on the recover/daemon path, got: {other:?}"
            ),
        }
        assert!(
            wallet.localstore.get_saga(&saga_id).await.unwrap().is_some(),
            "refuse must be sticky"
        );
    }

    // A transaction whose saga_id is someone else's must not keep THIS orphan wedged.
    // Same identifier control as the mapped-send near-misses: only this saga's row counts.
    #[tokio::test]
    async fn empty_swap_orphan_is_not_kept_by_a_transaction_for_a_different_saga() {
        let wallet = send_saga_wallet(71, vec![], vec![]).await;
        let saga_id = add_empty_swap_saga(&wallet).await;
        add_tx_for_saga(
            &wallet,
            Some(uuid::Uuid::now_v7()),
            TransactionDirection::Outgoing,
            0,
        )
        .await;

        let report = retire_eligible_incomplete_sagas(&wallet).await.unwrap();
        assert!(
            report.unresolved.is_empty(),
            "a tx for a different saga must not refuse this orphan: {:?}",
            report.unresolved
        );
        assert!(
            wallet.localstore.get_saga(&saga_id).await.unwrap().is_none(),
            "the orphan must still drop when the only tx names a different saga"
        );
        CdkBuyerMint::new(&wallet)
            .recover_unmapped_sagas()
            .await
            .expect("pay path must resume after dropping the orphan");
    }

    // ---- #873: token expansion must survive a mint keyset rotation ----------------------------
    //
    // Only v2 (`Version01`) keysets can reach this defect at all. `Id::from_short_keyset_id`
    // returns a v1 short id straight back without ever consulting the keyset list, so a v1 token
    // expands against any cache, stale or not. Every fixture here is therefore v2 — a v1 keyset
    // would make these tests pass while testing nothing.

    /// A v2 keyset seeded distinctly, so two of them never share a short-id prefix.
    fn v2_keyset(seed: u8) -> KeySet {
        let keys = [1_u64, 2, 4, 8]
            .into_iter()
            .map(|amount| {
                (
                    Amount::from(amount),
                    secret_key(amount as u8 + seed).public_key(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let keys = Keys::new(keys);
        KeySet {
            id: Id::v2_from_data(&keys, &CurrencyUnit::Sat, 0, None),
            unit: CurrencyUnit::Sat,
            active: Some(true),
            keys,
            input_fee_ppk: 0,
            final_expiry: None,
        }
    }

    fn keyset_info_of(keyset: &KeySet) -> KeySetInfo {
        KeySetInfo {
            id: keyset.id,
            unit: keyset.unit.clone(),
            active: true,
            input_fee_ppk: keyset.input_fee_ppk,
            final_expiry: keyset.final_expiry,
        }
    }

    /// Serves exactly the keysets it holds, and counts keyset-list fetches.
    ///
    /// The counter is the point: "expansion recovered" alone cannot distinguish a refresh from a
    /// cache that was never stale, and "expansion failed" cannot distinguish one bounded retry from
    /// a loop. Both questions are answered by how many times the mint was asked.
    #[derive(Clone, Debug, Default)]
    struct RotatingKeysetTransport {
        served: Vec<KeySet>,
        keyset_fetches: Arc<AtomicUsize>,
    }

    impl RotatingKeysetTransport {
        fn new(served: Vec<KeySet>) -> Self {
            Self {
                served,
                keyset_fetches: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn fetches(&self) -> usize {
            self.keyset_fetches.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for RotatingKeysetTransport {
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
            url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
        ) -> Result<R, cdk::Error>
        where
            R: DeserializeOwned,
        {
            let path = url.path().to_owned();
            // `/v1/keysets` contains `/v1/keys` as a substring, so it must be matched first.
            let value = if path.ends_with("/v1/keysets") {
                self.keyset_fetches.fetch_add(1, Ordering::SeqCst);
                serde_json::to_value(cashu::KeysetResponse {
                    keysets: self.served.iter().map(keyset_info_of).collect(),
                })
            } else if path.contains("/v1/keys") {
                let wanted = self
                    .served
                    .iter()
                    .filter(|keyset| path.ends_with(&keyset.id.to_string()))
                    .cloned()
                    .collect::<Vec<_>>();
                let keysets = if wanted.is_empty() && path.ends_with("/v1/keys") {
                    self.served.clone()
                } else {
                    wanted
                };
                serde_json::to_value(cashu::KeysResponse { keysets })
            } else if path.ends_with("/v1/info") {
                serde_json::to_value(MintInfo::new())
            } else {
                return Err(cdk::Error::Custom(format!("unexpected GET {path}")));
            };
            serde_json::from_value(value.map_err(|error| cdk::Error::Custom(error.to_string()))?)
                .map_err(|error| cdk::Error::Custom(error.to_string()))
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
            Err(cdk::Error::Custom(format!(
                "unexpected POST {}",
                url.path()
            )))
        }
    }

    /// A wallet whose store is seeded with `cached` only, talking to a mint that serves `served`.
    /// When `served` includes a keyset `cached` does not, the wallet is genuinely stale.
    async fn rotated_keyset_wallet(
        cached: &KeySet,
        served: Vec<KeySet>,
    ) -> (Wallet, RotatingKeysetTransport) {
        let transport = RotatingKeysetTransport::new(served);
        let wallet = seller_wallet_at(MINT, transport.clone(), cached.clone()).await;
        (wallet, transport)
    }

    fn token_for_keyset(keyset: &KeySet, seller: PublicKey) -> Token {
        Token::new(
            mint(MINT),
            vec![p2pk_proof_for_keyset(7, seller, keyset.id)],
            None,
            CurrencyUnit::Sat,
        )
    }

    #[tokio::test]
    async fn expansion_recovers_when_the_mint_rotated_its_keyset() {
        let cached = v2_keyset(20);
        let rotated = v2_keyset(60);
        assert_ne!(cached.id, rotated.id, "fixture must model a real rotation");
        let seller = secret_key(1).public_key();
        let token = token_for_keyset(&rotated, seller);
        let (wallet, transport) =
            rotated_keyset_wallet(&cached, vec![cached.clone(), rotated.clone()]).await;

        // Red-prove the premise: the unfixed shape — expand against the cached set, no refresh —
        // must actually fail here, or this test would pass without exercising the fix.
        let stale = wallet.get_mint_keysets(KeysetFilter::All).await.unwrap();
        let unfixed = token.proofs(&stale).expect_err("stale cache must refuse");
        assert!(
            is_unknown_short_keyset_id(&unfixed),
            "premise must fail with the keyset miss, not something else: {unfixed}"
        );
        assert_eq!(transport.fetches(), 0, "the stale read must not touch the mint");

        let proofs = expand_token_proofs(&wallet, &token)
            .await
            .expect("expansion must recover after refreshing the rotated keyset");

        assert_eq!(proofs.len(), 1);
        assert_eq!(
            proofs[0].keyset_id, rotated.id,
            "recovered proof must carry the ROTATED keyset's full id"
        );
        assert_eq!(
            transport.fetches(),
            1,
            "recovery must cost exactly one keyset refresh"
        );
    }

    #[tokio::test]
    async fn expansion_does_not_touch_the_mint_when_the_cache_already_serves_the_token() {
        let cached = v2_keyset(20);
        let seller = secret_key(1).public_key();
        let token = token_for_keyset(&cached, seller);
        let (wallet, transport) = rotated_keyset_wallet(&cached, vec![cached.clone()]).await;

        let proofs = expand_token_proofs(&wallet, &token)
            .await
            .expect("a token the cache can already expand must not need the mint");

        assert_eq!(proofs.len(), 1);
        assert_eq!(
            transport.fetches(),
            0,
            "the happy path must buy NO mint round-trip"
        );
    }

    #[tokio::test]
    async fn expansion_retries_exactly_once_when_the_mint_still_lacks_the_keyset() {
        let cached = v2_keyset(20);
        let missing = v2_keyset(60);
        let seller = secret_key(1).public_key();
        let token = token_for_keyset(&missing, seller);
        // The mint never serves `missing`, so the refresh cannot help. The retry must stop.
        let (wallet, transport) = rotated_keyset_wallet(&cached, vec![cached.clone()]).await;

        let error = expand_token_proofs(&wallet, &token)
            .await
            .expect_err("a keyset the mint does not serve must still refuse");

        assert_eq!(
            transport.fetches(),
            1,
            "a second miss must NOT loop: exactly one refresh, then refuse"
        );
        let text = error.to_string();
        assert!(
            text.contains("after refresh"),
            "error must say a refresh was attempted: {text}"
        );
        assert!(
            text.contains("first attempt"),
            "error must preserve the original cause: {text}"
        );
    }

    #[tokio::test]
    async fn token_proof_ys_recovers_when_the_mint_rotated_its_keyset() {
        let cached = v2_keyset(20);
        let rotated = v2_keyset(60);
        let seller = secret_key(1).public_key();
        let token = token_for_keyset(&rotated, seller);
        let (wallet, transport) =
            rotated_keyset_wallet(&cached, vec![cached.clone(), rotated.clone()]).await;

        let ys = token_proof_ys(&wallet, &token)
            .await
            .expect("buyer reconcile must route through the refreshing expansion");

        assert_eq!(ys.len(), 1);
        assert_eq!(transport.fetches(), 1);
    }

    #[tokio::test]
    async fn build_nut18_payload_recovers_when_the_mint_rotated_its_keyset() {
        let cached = v2_keyset(20);
        let rotated = v2_keyset(60);
        let seller = secret_key(1).public_key();
        let token = token_for_keyset(&rotated, seller);
        let (wallet, transport) =
            rotated_keyset_wallet(&cached, vec![cached.clone(), rotated.clone()]).await;

        let payload = build_nut18_payload(
            &wallet,
            "job-873".into(),
            seller.to_string(),
            token.clone(),
        )
        .await
        .expect("buyer NUT-18 payload must route through the refreshing expansion");

        assert_eq!(payload.payload.proofs.len(), 1);
        assert_eq!(payload.payload.proofs[0].keyset_id, rotated.id);
        assert_eq!(transport.fetches(), 1);
    }

    /// The seller caller, which is the one the field failure came from: a correctly paid seller left
    /// uncredited because fee prediction could not expand the wrap. Both fixtures carry
    /// `input_fee_ppk` 0, so the predicted fee is 0 and the injected receive returns the full face.
    #[tokio::test]
    async fn seller_receive_recovers_when_the_mint_rotated_its_keyset() {
        let cached = v2_keyset(20);
        let rotated = v2_keyset(60);
        let seller_key = secret_key(1);
        let token = token_for_keyset(&rotated, seller_key.public_key());
        let (wallet, transport) =
            rotated_keyset_wallet(&cached, vec![cached.clone(), rotated.clone()]).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        let amount = adapter
            .receive_with(&token, &terms, &accepted(&[MINT]), &mint(MINT), |_| async {
                Ok(Amount::from(7))
            })
            .await
            .expect("seller receive must route through the refreshing expansion");

        assert_eq!(amount, Amount::from(7));
        assert_eq!(
            transport.fetches(),
            1,
            "the seller path must recover by refreshing exactly once"
        );
    }

    /// The refresh is gated on ONE typed variant. A string match would silently start retrying
    /// unrelated decode faults the first time cashu edits its error text, so the gate is asserted
    /// against the neighbouring variants directly — the cases that must NOT buy a mint round-trip.
    #[test]
    fn only_the_unknown_short_keyset_id_variant_triggers_a_refresh() {
        use cashu::nuts::{nut00, nut02};

        assert!(is_unknown_short_keyset_id(&nut00::Error::NUT02(
            nut02::Error::UnknownShortKeysetId
        )));

        for other in [
            nut02::Error::MalformedShortKeysetId,
            nut02::Error::IncorrectKeysetId,
            nut02::Error::Length,
            nut02::Error::UnknownVersion,
        ] {
            let wrapped = nut00::Error::NUT02(other);
            assert!(
                !is_unknown_short_keyset_id(&wrapped),
                "must not refresh on: {wrapped}"
            );
        }

        assert!(
            !is_unknown_short_keyset_id(&nut00::Error::UnsupportedToken),
            "a non-NUT02 expansion fault must not refresh"
        );
    }
}
