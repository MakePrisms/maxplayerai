use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use cashu::{
    Amount, CheckStateRequest, CurrencyUnit, MintUrl, ProofState, PublicKey, SpendingConditions,
    State, Token,
};
use cdk::wallet::MintConnector;

/// Trade facts the token must match before payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradeLock {
    pub mint: MintUrl,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub seller_lock: PublicKey,
}

/// Trade binding and unspent state; not mint authenticity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedPayment {
    pub mint: MintUrl,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub proof_ys: Vec<PublicKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalletVerifyError {
    InvalidToken(String),
    AmountMismatch { expected: Amount, actual: Amount },
    MintMismatch { expected: MintUrl, actual: MintUrl },
    UnitMismatch {
        expected: CurrencyUnit,
        actual: Option<CurrencyUnit>,
    },
    LockMismatch { expected: PublicKey },
    /// The proof carries P2PK spending conditions the seller key ALONE cannot satisfy (a
    /// multisig `n_sigs`, an additional required pubkey, a locktime/refund clawback, or a
    /// sig-flag the seller cannot meet). Canonical accepted shape: a single seller pubkey with no
    /// extra conditions.
    UnsatisfiableLock(String),
    DuplicateProofY(PublicKey),
    DuplicateState(PublicKey),
    MissingState(PublicKey),
    NotUnspent { proof_y: PublicKey, state: State },
    MintConnector(String),
}

impl fmt::Display for WalletVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToken(error) => write!(f, "invalid Cashu token: {error}"),
            Self::AmountMismatch { expected, actual } => {
                write!(f, "amount mismatch: expected {expected}, got {actual}")
            }
            Self::MintMismatch { expected, actual } => {
                write!(f, "mint mismatch: expected {expected}, got {actual}")
            }
            Self::UnitMismatch { expected, actual } => match actual {
                Some(actual) => write!(
                    f,
                    "currency unit mismatch: expected {expected}, got {actual}"
                ),
                None => write!(f, "currency unit mismatch: expected {expected}, got none"),
            },
            Self::LockMismatch { expected } => {
                write!(f, "P2PK lock does not match seller key {expected}")
            }
            Self::UnsatisfiableLock(reason) => {
                write!(f, "P2PK lock not seller-satisfiable: {reason}")
            }
            Self::DuplicateProofY(y) => write!(f, "duplicate proof y {y}"),
            Self::DuplicateState(y) => write!(f, "duplicate NUT-07 state for proof {y}"),
            Self::MissingState(y) => write!(f, "missing NUT-07 state for proof {y}"),
            Self::NotUnspent { proof_y, state } => {
                write!(f, "proof {proof_y} is not unspent: {state}")
            }
            Self::MintConnector(error) => write!(f, "NUT-07 spend-state check failed: {error}"),
        }
    }
}

impl std::error::Error for WalletVerifyError {}

/// Verifies mint, amount, unit, per-proof seller lock, and unspent state; not mint authenticity.
pub fn verify_trade_p2pk(
    token: &Token,
    lock: &TradeLock,
    states: &[ProofState],
) -> Result<VerifiedPayment, WalletVerifyError> {
    let actual_amount = token
        .value()
        .map_err(|error| WalletVerifyError::InvalidToken(error.to_string()))?;
    if actual_amount != lock.amount {
        return Err(WalletVerifyError::AmountMismatch {
            expected: lock.amount,
            actual: actual_amount,
        });
    }

    let actual_mint = token
        .mint_url()
        .map_err(|error| WalletVerifyError::InvalidToken(error.to_string()))?;
    if actual_mint != lock.mint {
        return Err(WalletVerifyError::MintMismatch {
            expected: lock.mint.clone(),
            actual: actual_mint,
        });
    }

    let actual_unit = token.unit();
    if actual_unit.as_ref() != Some(&lock.unit) {
        return Err(WalletVerifyError::UnitMismatch {
            expected: lock.unit.clone(),
            actual: actual_unit,
        });
    }

    require_seller_lock(token, lock.seller_lock)?;

    let proof_ys = token_ys(token)?;
    reject_duplicate_ys(&proof_ys)?;
    require_unspent(&proof_ys, states)?;

    Ok(VerifiedPayment {
        mint: actual_mint,
        amount: actual_amount,
        unit: lock.unit.clone(),
        proof_ys,
    })
}

fn require_seller_lock(token: &Token, seller_lock: PublicKey) -> Result<(), WalletVerifyError> {
    let secrets = token.token_secrets();
    if secrets.is_empty() {
        return Err(WalletVerifyError::LockMismatch {
            expected: seller_lock,
        });
    }

    for secret in secrets {
        let nut10 = cashu::nuts::nut10::Secret::try_from(secret).map_err(|error| {
            WalletVerifyError::InvalidToken(format!(
                "proof secret is not a valid P2PK spending condition: {error}"
            ))
        })?;
        let conditions = SpendingConditions::try_from(nut10.clone()).map_err(|error| {
            WalletVerifyError::InvalidToken(format!(
                "proof secret is not a valid P2PK spending condition: {error}"
            ))
        })?;
        if conditions.kind() != cashu::nuts::Kind::P2PK {
            return Err(WalletVerifyError::InvalidToken(
                "proof secret is not a P2PK spending condition".to_owned(),
            ));
        }

        let primary_lock = PublicKey::from_str(nut10.secret_data().data()).map_err(|error| {
            WalletVerifyError::InvalidToken(format!("invalid primary P2PK key: {error}"))
        })?;
        if primary_lock != seller_lock {
            return Err(WalletVerifyError::LockMismatch {
                expected: seller_lock,
            });
        }
        // Reject any additional spending conditions the seller key ALONE cannot satisfy. The
        // canonical accepted shape is a single seller pubkey with NO extra conditions; anything
        // else (multisig, a foreign required signer, a locktime/refund clawback path, or a
        // sig-flag the seller cannot meet) means the buyer could later reclaim or the seller could
        // not spend — refuse before crediting.
        if let SpendingConditions::P2PKConditions {
            conditions: Some(extra),
            ..
        } = &conditions
        {
            reject_non_seller_satisfiable(extra)?;
        }
    }

    Ok(())
}

/// Refuse P2PK [`cashu::nuts::nut11::Conditions`] the seller key alone cannot satisfy.
/// Accepted shape carries none of these; each set field is a distinct refusal.
fn reject_non_seller_satisfiable(
    conditions: &cashu::nuts::nut10::Conditions,
) -> Result<(), WalletVerifyError> {
    // >1 required signature ⇒ the single seller key cannot spend.
    if conditions.num_sigs.unwrap_or(1) > 1 {
        return Err(WalletVerifyError::UnsatisfiableLock(format!(
            "requires {} signatures (seller holds one key)",
            conditions.num_sigs.unwrap_or(1)
        )));
    }
    // Any additional required pubkey is a foreign co-signer the seller does not control.
    if conditions
        .pubkeys
        .as_ref()
        .is_some_and(|keys| !keys.is_empty())
    {
        return Err(WalletVerifyError::UnsatisfiableLock(
            "carries additional required pubkeys (foreign co-signer)".into(),
        ));
    }
    // A locktime + refund key lets the buyer (refund key holder) claw the funds back after the
    // locktime — the seller cannot rely on holding them.
    if conditions.locktime.is_some()
        || conditions
            .refund_keys
            .as_ref()
            .is_some_and(|keys| !keys.is_empty())
    {
        return Err(WalletVerifyError::UnsatisfiableLock(
            "carries a locktime/refund clawback path".into(),
        ));
    }
    // SigAll (vs the default SigInputs) demands signatures over outputs the seller does not
    // control at receive time.
    if conditions.sig_flag != cashu::nuts::nut11::SigFlag::SigInputs {
        return Err(WalletVerifyError::UnsatisfiableLock(format!(
            "carries non-default sig flag {:?}",
            conditions.sig_flag
        )));
    }
    Ok(())
}

/// Fetches NUT-07 state through an injected connector, then runs the pure verifier.
pub async fn verify_trade_p2pk_with_connector<C: MintConnector + ?Sized>(
    connector: &C,
    token: &Token,
    lock: &TradeLock,
) -> Result<VerifiedPayment, WalletVerifyError> {
    let ys = token_ys(token)?;
    reject_duplicate_ys(&ys)?;
    let response = connector
        .post_check_state(CheckStateRequest { ys })
        .await
        .map_err(|error| WalletVerifyError::MintConnector(error.to_string()))?;
    verify_trade_p2pk(token, lock, &response.states)
}

fn token_ys(token: &Token) -> Result<Vec<PublicKey>, WalletVerifyError> {
    let y_only_id = cashu::Id::from_str("0000000000000000")
        .map_err(|error| WalletVerifyError::InvalidToken(error.to_string()))?;

    match token {
        Token::TokenV3(token) => token
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter())
            .map(|proof| {
                proof
                    .into_proof(&y_only_id)
                    .y()
                    .map_err(|error| WalletVerifyError::InvalidToken(error.to_string()))
            })
            .collect(),
        Token::TokenV4(token) => token
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter())
            .map(|proof| {
                proof
                    .into_proof(&y_only_id)
                    .y()
                    .map_err(|error| WalletVerifyError::InvalidToken(error.to_string()))
            })
            .collect(),
    }
}

fn reject_duplicate_ys(ys: &[PublicKey]) -> Result<(), WalletVerifyError> {
    let mut seen = HashSet::with_capacity(ys.len());
    for y in ys {
        if !seen.insert(*y) {
            return Err(WalletVerifyError::DuplicateProofY(*y));
        }
    }
    Ok(())
}

fn require_unspent(proof_ys: &[PublicKey], states: &[ProofState]) -> Result<(), WalletVerifyError> {
    let mut by_y = HashMap::with_capacity(states.len());
    for state in states {
        if by_y.insert(state.y, state.state).is_some() {
            return Err(WalletVerifyError::DuplicateState(state.y));
        }
    }

    for y in proof_ys {
        match by_y.get(y) {
            Some(State::Unspent) => {}
            Some(state) => {
                return Err(WalletVerifyError::NotUnspent {
                    proof_y: *y,
                    state: *state,
                });
            }
            None => return Err(WalletVerifyError::MissingState(*y)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use cashu::secret::Secret;
    use cashu::{
        CheckStateResponse, Conditions, CurrencyUnit, Id, Proof, SecretKey, SpendingConditions,
    };
    use cdk::wallet::{BaseHttpClient, HttpTransport};
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use url::Url;

    const MINT: &str = "https://testnut.cashu.space";
    const OTHER_MINT: &str = "https://mint.example";
    const KEYSET_ID: &str = "010000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn unsigned_prepay_token_uses_spending_conditions_not_witness_signatures() {
        let seller = public_key(1);
        let token = token(MINT, vec![proof(7, seller)]);
        let ys = token_ys(&token).unwrap();

        // The proof is deliberately unsigned. Calling Proof::verify_p2pk here
        // would return SignaturesNotProvided; native SpendingConditions are the
        // correct pre-pay lock check.
        let proof = token_proofs(&token).remove(0);
        assert!(proof.witness.is_none());
        assert!(matches!(
            proof.verify_p2pk(),
            Err(cashu::nuts::nut11::Error::SignaturesNotProvided)
        ));
        assert_eq!(
            verify_trade_p2pk(&token, &trade_lock(MINT, 7, seller), &unspent(&ys)),
            Ok(VerifiedPayment {
                mint: mint(MINT),
                amount: Amount::from(7),
                unit: CurrencyUnit::Sat,
                proof_ys: ys,
            })
        );
    }

    #[test]
    fn verify_rejects_wrong_mint_amount_lock_and_spent_state() {
        let seller = public_key(1);
        let other = public_key(2);
        let token = token(MINT, vec![proof(7, seller)]);
        let ys = token_ys(&token).unwrap();
        let states = unspent(&ys);

        assert!(matches!(
            verify_trade_p2pk(&token, &trade_lock(OTHER_MINT, 7, seller), &states),
            Err(WalletVerifyError::MintMismatch { .. })
        ));
        assert!(matches!(
            verify_trade_p2pk(&token, &trade_lock(MINT, 8, seller), &states),
            Err(WalletVerifyError::AmountMismatch { .. })
        ));
        assert!(matches!(
            verify_trade_p2pk(&token, &trade_lock(MINT, 7, other), &states),
            Err(WalletVerifyError::LockMismatch { .. })
        ));
        assert!(matches!(
            verify_trade_p2pk(
                &token,
                &trade_lock(MINT, 7, seller),
                &[ProofState::from((ys[0], State::Spent))],
            ),
            Err(WalletVerifyError::NotUnspent {
                state: State::Spent,
                ..
            })
        ));
    }

    #[test]
    fn verify_rejects_equal_numeric_amount_in_wrong_currency_unit() {
        let seller = public_key(1);
        let token = token_with_unit(MINT, vec![proof(7, seller)], CurrencyUnit::Msat);
        let ys = token_ys(&token).unwrap();

        assert!(matches!(
            verify_trade_p2pk(&token, &trade_lock(MINT, 7, seller), &unspent(&ys)),
            Err(WalletVerifyError::UnitMismatch {
                expected: CurrencyUnit::Sat,
                actual: Some(CurrencyUnit::Msat),
            })
        ));
    }

    #[test]
    fn verify_rejects_mixed_seller_and_other_primary_locks() {
        let seller = public_key(1);
        let other = public_key(2);
        let token = token(MINT, vec![proof(1, seller), proof(1, other)]);
        let ys = token_ys(&token).unwrap();

        assert!(matches!(
            verify_trade_p2pk(&token, &trade_lock(MINT, 2, seller), &unspent(&ys)),
            Err(WalletVerifyError::LockMismatch { .. })
        ));
    }

    #[test]
    fn verify_rejects_non_nut10_sibling_proof() {
        let seller = public_key(1);
        let token = token(
            MINT,
            vec![proof(1, seller), plain_secret_proof(1, "not-nut10")],
        );
        let ys = token_ys(&token).unwrap();

        let error =
            verify_trade_p2pk(&token, &trade_lock(MINT, 2, seller), &unspent(&ys)).unwrap_err();
        assert!(
            matches!(&error, WalletVerifyError::InvalidToken(message) if message.contains("P2PK")),
            "non-NUT-10 proof reached the wrong guard: {error}"
        );
    }

    #[test]
    fn duplicate_secret_and_y_are_rejected_by_native_token_value() {
        let seller = public_key(1);
        let repeated = proof(1, seller);
        let token = token(MINT, vec![repeated.clone(), repeated]);

        let error = verify_trade_p2pk(&token, &trade_lock(MINT, 2, seller), &[]).unwrap_err();
        assert!(
            matches!(&error, WalletVerifyError::InvalidToken(message) if message.to_ascii_lowercase().contains("duplicate")),
            "duplicate secret/y reached the wrong guard: {error}"
        );
    }

    #[test]
    fn amount_overflow_is_rejected_by_native_checked_sum() {
        let seller = public_key(1);
        let token = token(
            MINT,
            vec![proof(u64::MAX - 3, seller), proof(10, seller)],
        );

        let error = verify_trade_p2pk(&token, &trade_lock(MINT, 6, seller), &[]).unwrap_err();
        assert!(
            matches!(&error, WalletVerifyError::InvalidToken(message) if message.to_ascii_lowercase().contains("overflow")),
            "overflow fixture reached the wrong guard: {error}"
        );
    }

    // Finding E: refuse any P2PK spending condition the seller key ALONE cannot safely realize.
    // The canonical accepted shape is a single seller pubkey with no extra conditions; each unsafe
    // shape below is refused before the token is credited.
    #[test]
    fn verify_rejects_conditions_seller_cannot_satisfy() {
        let seller = public_key(1);
        let buyer = public_key(2);

        // Canonical shape (single seller key, no conditions) → accepted.
        let ok = token(MINT, vec![proof_with_conditions(7, seller, None)]);
        let ok_ys = token_ys(&ok).unwrap();
        assert!(
            verify_trade_p2pk(&ok, &trade_lock(MINT, 7, seller), &unspent(&ok_ys)).is_ok(),
            "canonical single-seller lock must verify"
        );

        let refuse = |conditions: Conditions, what: &str| {
            let tok = token(MINT, vec![proof_with_conditions(7, seller, Some(conditions))]);
            let ys = token_ys(&tok).unwrap();
            assert!(
                matches!(
                    verify_trade_p2pk(&tok, &trade_lock(MINT, 7, seller), &unspent(&ys)),
                    Err(WalletVerifyError::UnsatisfiableLock(_))
                ),
                "{what} must refuse as not-seller-satisfiable"
            );
        };

        // Multisig (n_sigs > 1): the single seller key cannot meet the quorum.
        refuse(
            Conditions {
                pubkeys: Some(vec![buyer]),
                num_sigs: Some(2),
                ..Default::default()
            },
            "n_sigs>1 multisig",
        );
        // Additional allowed pubkey (default n_sigs=1): the buyer could also spend the token.
        refuse(
            Conditions {
                pubkeys: Some(vec![buyer]),
                ..Default::default()
            },
            "foreign allowed pubkey",
        );
        // Locktime + refund key: the buyer (refund holder) can claw the funds back after locktime.
        refuse(
            Conditions {
                locktime: Some(1),
                refund_keys: Some(vec![buyer]),
                ..Default::default()
            },
            "locktime+refund clawback",
        );
        // Non-default sig flag (SigAll instead of SigInputs).
        refuse(
            Conditions {
                sig_flag: cashu::nuts::nut11::SigFlag::SigAll,
                ..Default::default()
            },
            "SigAll flag",
        );
    }

    #[test]
    fn connector_check_computes_ys_without_keyset_fetch() {
        let seller = public_key(1);
        // A v2 keyset id cannot be expanded from its token short-id without
        // keyset metadata. The connector path accepts no keysets and configures
        // only post_check_state, proving the pre-pay y calculation is
        // secret-only and never calls Token::proofs(&keysets).
        let token = token(MINT, vec![proof(7, seller)]);
        assert!(token.proofs(&[]).is_err());
        let ys = vec![token_proofs(&token)[0].y().expect("valid proof secret")];
        let connector = BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(CheckStateResponse {
                states: unspent(&ys),
            }),
            None,
        );

        let verified = block_on(verify_trade_p2pk_with_connector(
            &connector,
            &token,
            &trade_lock(MINT, 7, seller),
        ))
        .expect("mocked NUT-07 check succeeds without keyset metadata");

        assert_eq!(verified.proof_ys, ys);
    }

    #[derive(Clone, Debug, Default)]
    struct CheckStateTransport {
        response: serde_json::Value,
    }

    impl CheckStateTransport {
        fn new(response: CheckStateResponse) -> Self {
            Self {
                response: serde_json::to_value(response).expect("serializable NUT-07 response"),
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
            Err(cdk::Error::Custom(
                "unexpected GET in NUT-07 regression".to_owned(),
            ))
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
                return Err(cdk::Error::Custom(format!(
                    "unexpected POST path in NUT-07 regression: {}",
                    url.path()
                )));
            }
            serde_json::from_value(self.response.clone())
                .map_err(|error| cdk::Error::Custom(error.to_string()))
        }
    }

    fn trade_lock(mint_url: &str, amount: u64, seller_lock: PublicKey) -> TradeLock {
        TradeLock {
            mint: mint(mint_url),
            amount: Amount::from(amount),
            unit: CurrencyUnit::Sat,
            seller_lock,
        }
    }

    fn token(mint_url: &str, proofs: Vec<Proof>) -> Token {
        token_with_unit(mint_url, proofs, CurrencyUnit::Sat)
    }

    fn token_with_unit(mint_url: &str, proofs: Vec<Proof>, unit: CurrencyUnit) -> Token {
        Token::new(mint(mint_url), proofs, None, unit)
    }

    fn mint(url: &str) -> MintUrl {
        MintUrl::from_str(url).expect("valid mint URL")
    }

    // Canonical accepted shape: a single seller pubkey with NO extra conditions. The NUT-10
    // secret carries a random nonce, so repeated calls already yield distinct secrets.
    fn proof(amount: u64, seller: PublicKey) -> Proof {
        proof_with_conditions(amount, seller, None)
    }

    fn proof_with_conditions(
        amount: u64,
        seller: PublicKey,
        conditions: Option<Conditions>,
    ) -> Proof {
        let secret = Secret::try_from(SpendingConditions::new_p2pk(seller, conditions))
            .expect("valid P2PK spending condition");
        Proof::new(
            Amount::from(amount),
            Id::from_str(KEYSET_ID).expect("valid v2 keyset id"),
            secret,
            public_key(42),
        )
    }

    fn plain_secret_proof(amount: u64, secret: &str) -> Proof {
        Proof::new(
            Amount::from(amount),
            Id::from_str(KEYSET_ID).expect("valid v2 keyset id"),
            Secret::new(secret),
            public_key(42),
        )
    }

    fn token_proofs(token: &Token) -> Vec<Proof> {
        match token {
            Token::TokenV4(token) => token
                .token
                .iter()
                .flat_map(|entry| entry.proofs.iter())
                .map(|proof| proof.into_proof(&Id::from_str(KEYSET_ID).unwrap()))
                .collect(),
            Token::TokenV3(_) => panic!("test helper constructs v4 tokens"),
        }
    }

    fn public_key(byte: u8) -> PublicKey {
        SecretKey::from_slice(&[byte; 32])
            .expect("valid secret key")
            .public_key()
    }

    fn unspent(ys: &[PublicKey]) -> Vec<ProofState> {
        ys.iter()
            .copied()
            .map(|y| ProofState::from((y, State::Unspent)))
            .collect()
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        unsafe fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match Pin::new(&mut future).poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
