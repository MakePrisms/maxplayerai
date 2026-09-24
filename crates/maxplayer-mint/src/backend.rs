//! The cdk mint behind the Nostr transport: one `sat` keyset, sqlite store, no Lightning.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use cdk::Mint;
use cdk::mint::{MintBuilder, UnitConfig};
use cdk::nuts::CurrencyUnit;
use cdk_sqlite::MintSqliteDatabase;

use crate::dispatch::MAX_IO;

/// Name the mint advertises in NUT-06 info.
pub const MINT_NAME: &str = "Maxplayer credits";

/// Open (creating on first use) the mint in `db_path`, keyed from `seed`, advertising `url`.
pub async fn open(db_path: &Path, seed: &[u8; 64], url: &str) -> Result<Mint> {
    let db = Arc::new(
        MintSqliteDatabase::new(db_path.to_path_buf())
            .await
            .with_context(|| format!("open {}", db_path.display()))?,
    );
    let mut builder = MintBuilder::new(db.clone());
    builder
        .configure_unit(CurrencyUnit::Sat, UnitConfig::default())
        .context("configure sat unit")?;
    let mint = builder
        .with_name(MINT_NAME.to_owned())
        .with_urls(vec![url.to_owned()])
        .with_limits(MAX_IO, MAX_IO)
        .build_with_seed(db, seed)
        .await
        .context("build mint")?;
    mint.start().await.context("start mint")?;
    Ok(mint)
}
