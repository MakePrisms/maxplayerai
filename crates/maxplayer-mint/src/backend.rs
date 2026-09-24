//! The cdk mint behind the Nostr transport: one `sat` keyset, sqlite store, no Lightning.

use std::path::Path;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::{Context, Result};
use cdk::Mint;
use cdk::amount::FeeAndAmounts;
use cdk::mint::{MintBuilder, UnitConfig};
use cdk::nuts::{CurrencyUnit, Id};
use cdk_sqlite::MintSqliteDatabase;

use crate::dispatch::MAX_IO;

/// Name the mint advertises in NUT-06 info.
pub const MINT_NAME: &str = "Maxplayer credits";

/// Open (creating on first use) and start the mint in `db_path`, keyed from `seed`, advertising
/// `url`. Starting runs cdk's saga recovery; only `run` (and `init`) may do that.
pub async fn open(db_path: &Path, seed: &[u8; 64], url: &str) -> Result<Mint> {
    let mint = build(db_path, seed, url).await?;
    mint.start().await.context("start mint")?;
    Ok(mint)
}

/// The mint without its background services (no saga recovery): safe next to a running `run`.
pub async fn build(db_path: &Path, seed: &[u8; 64], url: &str) -> Result<Mint> {
    let db = Arc::new(
        MintSqliteDatabase::new(db_path.to_path_buf())
            .await
            .with_context(|| format!("open {}", db_path.display()))?,
    );
    let mut builder = MintBuilder::new(db.clone());
    builder
        .configure_unit(CurrencyUnit::Sat, UnitConfig::default())
        .context("configure sat unit")?;
    builder
        .with_name(MINT_NAME.to_owned())
        .with_urls(vec![url.to_owned()])
        .with_limits(MAX_IO, MAX_IO)
        .build_with_seed(db, seed)
        .await
        .context("build mint")
}

/// The active `sat` keyset.
pub fn active_keyset(mint: &Mint) -> Result<Id> {
    mint.keysets()
        .keysets
        .into_iter()
        .find(|k| k.active && k.unit == CurrencyUnit::Sat)
        .map(|k| k.id)
        .ok_or_else(|| anyhow!("no active sat keyset"))
}

/// Fee and denominations of the keyset (`UnitConfig::default()`: no fee, powers of two).
pub fn fee_and_amounts() -> FeeAndAmounts {
    let config = UnitConfig::default();
    FeeAndAmounts::from((config.input_fee_ppk, config.amounts))
}
