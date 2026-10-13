//! Dependency bug reproducer, NOT evidence that recovery works.
//! Mirrors CDK 0.17.2 `inner_check_mint_quote_status`'s orphan branch:
//! release_mint_quote(op), then add_mint_quote(the still-reserved in-memory quote).
use cdk::{
    cdk_database::WalletDatabase,
    nuts::{CurrencyUnit, PaymentMethod},
    wallet::MintQuote,
};
#[tokio::test]
async fn pinned_orphan_recovery_writeback_resurrects_reservation() {
    let db = cdk_sqlite::wallet::memory::empty().await.unwrap();
    let q = MintQuote::new(
        "test-quote".into(),
        "http://127.0.0.1:1234".parse().unwrap(),
        PaymentMethod::BOLT11,
        Some(128.into()),
        CurrencyUnit::Sat,
        "not-a-real-invoice".into(),
        cdk::util::unix_time() + 600,
        None,
    );
    db.add_mint_quote(q).await.unwrap();
    let operation = uuid::Uuid::new_v4();
    db.reserve_mint_quote("test-quote", &operation)
        .await
        .unwrap();
    assert!(db.get_saga(&operation).await.unwrap().is_none());
    let mut stale = db.get_mint_quote("test-quote").await.unwrap().unwrap();
    stale.state = cashu::nuts::nut23::QuoteState::Paid;
    stale.amount_paid = 128.into();
    db.release_mint_quote(&operation).await.unwrap();
    assert!(
        db.get_mint_quote("test-quote")
            .await
            .unwrap()
            .unwrap()
            .used_by_operation
            .is_none()
    );
    db.add_mint_quote(stale).await.unwrap();
    let reread = db.get_mint_quote("test-quote").await.unwrap().unwrap();
    assert_eq!(reread.used_by_operation, Some(operation.to_string()));
    let error = db
        .reserve_mint_quote("test-quote", &uuid::Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Quote already in use"),
        "{error}"
    );
}
