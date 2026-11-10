//! The wallet database as cdk's own saga recovery sees it: every saga except the ones maxplayer keeps.
//!
//! cdk 0.17.2's `Wallet::recover_incomplete_sagas` is all-or-nothing over `get_incomplete_sagas`.
//! Two of its resume branches act without asking the mint, or destroy the only record a crashed
//! send or melt left (PR #1263 review r3 H1, r4 H1/M3): a swap that stores blinded messages and
//! reserves nothing (a send's or melt's `swap_no_reserve`) is deleted on an empty `/restore` or has
//! its outputs credited on top of inputs already released, and a `ProofsReserved` send or melt is
//! compensated with no NUT-07 check. maxplayer resolves (or keeps) those itself
//! (`payment_wallet::retire_eligible_incomplete_sagas_in`). Holding cdk recovery for the whole
//! wallet while one of them exists stalled every unrelated saga, including the hop sweep's (r4 M3),
//! so cdk runs over this view instead: `get_incomplete_sagas` leaves out the listed ids, and every
//! other call goes to the real database unchanged. Read-only filtering, no new state.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bitcoin::bip32::DerivationPath;
use cdk::cdk_database::{Error, WalletDatabase};
use cdk::mint_url::MintUrl;
use cdk::nuts::{
    CurrencyUnit, Id, KeySet, KeySetInfo, Keys, MintInfo, PublicKey, SpendingConditions, State,
};
use cdk::wallet::Wallet;
use cdk::wallet::types::{
    MeltQuote, MintQuote, P2PKSigningKey, ProofInfo, Transaction, TransactionDirection,
    TransactionId, WalletSaga,
};

type Store = Arc<dyn WalletDatabase<Error> + Send + Sync>;

/// `wallet` with a database view that hides the sagas in `hidden` from `get_incomplete_sagas`.
pub(crate) fn without_sagas(wallet: &Wallet, hidden: HashSet<uuid::Uuid>) -> Wallet {
    let mut view = wallet.clone();
    view.localstore = Arc::new(HidingStore {
        inner: wallet.localstore.clone(),
        hidden,
        #[cfg(test)]
        list_transactions_ok: None,
    });
    view
}

/// Test only: `wallet` whose `list_transactions` answers the first `ok_calls` calls and then fails
/// (a database read error), with nothing hidden.
#[cfg(test)]
pub(crate) fn failing_list_transactions_after(wallet: &Wallet, ok_calls: usize) -> Wallet {
    let mut view = wallet.clone();
    view.localstore = Arc::new(HidingStore {
        inner: wallet.localstore.clone(),
        hidden: HashSet::new(),
        list_transactions_ok: Some(Arc::new(std::sync::atomic::AtomicUsize::new(ok_calls))),
    });
    view
}

#[derive(Debug)]
struct HidingStore {
    inner: Store,
    hidden: HashSet<uuid::Uuid>,
    /// Test only: calls of `list_transactions` left before it fails.
    #[cfg(test)]
    list_transactions_ok: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

#[async_trait]
impl WalletDatabase<Error> for HidingStore {
    async fn get_incomplete_sagas(&self) -> Result<Vec<WalletSaga>, Error> {
        Ok(self
            .inner
            .get_incomplete_sagas()
            .await?
            .into_iter()
            .filter(|saga| !self.hidden.contains(&saga.id))
            .collect())
    }

    // ---- Everything below is the real database, unchanged. ----

    async fn get_mint(&self, mint_url: MintUrl) -> Result<Option<MintInfo>, Error> {
        self.inner.get_mint(mint_url).await
    }
    async fn get_mints(&self) -> Result<HashMap<MintUrl, Option<MintInfo>>, Error> {
        self.inner.get_mints().await
    }
    async fn get_mint_keysets(&self, mint_url: MintUrl) -> Result<Option<Vec<KeySetInfo>>, Error> {
        self.inner.get_mint_keysets(mint_url).await
    }
    async fn get_keyset_by_id(&self, keyset_id: &Id) -> Result<Option<KeySetInfo>, Error> {
        self.inner.get_keyset_by_id(keyset_id).await
    }
    async fn get_mint_quote(&self, quote_id: &str) -> Result<Option<MintQuote>, Error> {
        self.inner.get_mint_quote(quote_id).await
    }
    async fn get_mint_quotes(&self) -> Result<Vec<MintQuote>, Error> {
        self.inner.get_mint_quotes().await
    }
    async fn get_unissued_mint_quotes(&self) -> Result<Vec<MintQuote>, Error> {
        self.inner.get_unissued_mint_quotes().await
    }
    async fn get_melt_quote(&self, quote_id: &str) -> Result<Option<MeltQuote>, Error> {
        self.inner.get_melt_quote(quote_id).await
    }
    async fn get_melt_quotes(&self) -> Result<Vec<MeltQuote>, Error> {
        self.inner.get_melt_quotes().await
    }
    async fn get_keys(&self, id: &Id) -> Result<Option<Keys>, Error> {
        self.inner.get_keys(id).await
    }
    async fn get_proofs(
        &self,
        mint_url: Option<MintUrl>,
        unit: Option<CurrencyUnit>,
        state: Option<Vec<State>>,
        spending_conditions: Option<Vec<SpendingConditions>>,
    ) -> Result<Vec<ProofInfo>, Error> {
        self.inner
            .get_proofs(mint_url, unit, state, spending_conditions)
            .await
    }
    async fn get_proofs_by_ys(&self, ys: Vec<PublicKey>) -> Result<Vec<ProofInfo>, Error> {
        self.inner.get_proofs_by_ys(ys).await
    }
    async fn get_balance(
        &self,
        mint_url: Option<MintUrl>,
        unit: Option<CurrencyUnit>,
        state: Option<Vec<State>>,
    ) -> Result<u64, Error> {
        self.inner.get_balance(mint_url, unit, state).await
    }
    async fn get_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, Error> {
        self.inner.get_transaction(transaction_id).await
    }
    async fn list_transactions(
        &self,
        mint_url: Option<MintUrl>,
        direction: Option<TransactionDirection>,
        unit: Option<CurrencyUnit>,
    ) -> Result<Vec<Transaction>, Error> {
        #[cfg(test)]
        if let Some(left) = &self.list_transactions_ok {
            use std::sync::atomic::Ordering::SeqCst;
            if left.load(SeqCst) == 0 {
                return Err(Error::Database("test: list_transactions fails".into()));
            }
            left.fetch_sub(1, SeqCst);
        }
        self.inner
            .list_transactions(mint_url, direction, unit)
            .await
    }
    async fn update_proofs(
        &self,
        added: Vec<ProofInfo>,
        removed_ys: Vec<PublicKey>,
    ) -> Result<(), Error> {
        self.inner.update_proofs(added, removed_ys).await
    }
    async fn update_proofs_state(&self, ys: Vec<PublicKey>, state: State) -> Result<(), Error> {
        self.inner.update_proofs_state(ys, state).await
    }
    async fn add_transaction(&self, transaction: Transaction) -> Result<(), Error> {
        self.inner.add_transaction(transaction).await
    }
    async fn update_mint_url(
        &self,
        old_mint_url: MintUrl,
        new_mint_url: MintUrl,
    ) -> Result<(), Error> {
        self.inner.update_mint_url(old_mint_url, new_mint_url).await
    }
    async fn increment_keyset_counter(&self, keyset_id: &Id, count: u32) -> Result<u32, Error> {
        self.inner.increment_keyset_counter(keyset_id, count).await
    }
    async fn add_mint(&self, mint_url: MintUrl, mint_info: Option<MintInfo>) -> Result<(), Error> {
        self.inner.add_mint(mint_url, mint_info).await
    }
    async fn remove_mint(&self, mint_url: MintUrl) -> Result<(), Error> {
        self.inner.remove_mint(mint_url).await
    }
    async fn add_mint_keysets(
        &self,
        mint_url: MintUrl,
        keysets: Vec<KeySetInfo>,
    ) -> Result<(), Error> {
        self.inner.add_mint_keysets(mint_url, keysets).await
    }
    async fn add_mint_quote(&self, quote: MintQuote) -> Result<(), Error> {
        self.inner.add_mint_quote(quote).await
    }
    async fn remove_mint_quote(&self, quote_id: &str) -> Result<(), Error> {
        self.inner.remove_mint_quote(quote_id).await
    }
    async fn add_melt_quote(&self, quote: MeltQuote) -> Result<(), Error> {
        self.inner.add_melt_quote(quote).await
    }
    async fn remove_melt_quote(&self, quote_id: &str) -> Result<(), Error> {
        self.inner.remove_melt_quote(quote_id).await
    }
    async fn add_keys(&self, keyset: KeySet) -> Result<(), Error> {
        self.inner.add_keys(keyset).await
    }
    async fn remove_keys(&self, id: &Id) -> Result<(), Error> {
        self.inner.remove_keys(id).await
    }
    async fn remove_transaction(&self, transaction_id: TransactionId) -> Result<(), Error> {
        self.inner.remove_transaction(transaction_id).await
    }
    async fn add_saga(&self, saga: WalletSaga) -> Result<(), Error> {
        self.inner.add_saga(saga).await
    }
    async fn get_saga(&self, id: &uuid::Uuid) -> Result<Option<WalletSaga>, Error> {
        self.inner.get_saga(id).await
    }
    async fn update_saga(&self, saga: WalletSaga) -> Result<bool, Error> {
        self.inner.update_saga(saga).await
    }
    async fn delete_saga(&self, id: &uuid::Uuid) -> Result<(), Error> {
        self.inner.delete_saga(id).await
    }
    async fn reserve_proofs(
        &self,
        ys: Vec<PublicKey>,
        operation_id: &uuid::Uuid,
    ) -> Result<(), Error> {
        self.inner.reserve_proofs(ys, operation_id).await
    }
    async fn release_proofs(&self, operation_id: &uuid::Uuid) -> Result<(), Error> {
        self.inner.release_proofs(operation_id).await
    }
    async fn get_reserved_proofs(
        &self,
        operation_id: &uuid::Uuid,
    ) -> Result<Vec<ProofInfo>, Error> {
        self.inner.get_reserved_proofs(operation_id).await
    }
    async fn reserve_melt_quote(
        &self,
        quote_id: &str,
        operation_id: &uuid::Uuid,
    ) -> Result<(), Error> {
        self.inner.reserve_melt_quote(quote_id, operation_id).await
    }
    async fn release_melt_quote(&self, operation_id: &uuid::Uuid) -> Result<(), Error> {
        self.inner.release_melt_quote(operation_id).await
    }
    async fn reserve_mint_quote(
        &self,
        quote_id: &str,
        operation_id: &uuid::Uuid,
    ) -> Result<(), Error> {
        self.inner.reserve_mint_quote(quote_id, operation_id).await
    }
    async fn release_mint_quote(&self, operation_id: &uuid::Uuid) -> Result<(), Error> {
        self.inner.release_mint_quote(operation_id).await
    }
    async fn kv_read(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.inner
            .kv_read(primary_namespace, secondary_namespace, key)
            .await
    }
    async fn kv_list(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
    ) -> Result<Vec<String>, Error> {
        self.inner
            .kv_list(primary_namespace, secondary_namespace)
            .await
    }
    async fn kv_write(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
        value: &[u8],
    ) -> Result<(), Error> {
        self.inner
            .kv_write(primary_namespace, secondary_namespace, key, value)
            .await
    }
    async fn kv_remove(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
    ) -> Result<(), Error> {
        self.inner
            .kv_remove(primary_namespace, secondary_namespace, key)
            .await
    }
    async fn add_p2pk_key(
        &self,
        pubkey: &PublicKey,
        derivation_path: DerivationPath,
        derivation_index: u32,
    ) -> Result<(), Error> {
        self.inner
            .add_p2pk_key(pubkey, derivation_path, derivation_index)
            .await
    }
    async fn get_p2pk_key(&self, pubkey: &PublicKey) -> Result<Option<P2PKSigningKey>, Error> {
        self.inner.get_p2pk_key(pubkey).await
    }
    async fn list_p2pk_keys(&self) -> Result<Vec<P2PKSigningKey>, Error> {
        self.inner.list_p2pk_keys().await
    }
    async fn latest_p2pk(&self) -> Result<Option<P2PKSigningKey>, Error> {
        self.inner.latest_p2pk().await
    }
}
