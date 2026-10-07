use anyhow::{Context, Result, ensure};
use cdk::{
    nuts::CurrencyUnit,
    wallet::{Wallet, WalletBuilder},
};

use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    sync::Arc,
};
pub async fn preflight(mint: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let info: serde_json::Value = client
        .get(format!("{}/v1/info", mint.trim_end_matches('/')))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        info["nuts"]["7"]["supported"] == true && info["nuts"]["14"]["supported"] == true,
        "mint does not advertise NUT-07 and NUT-14"
    );
    let now = cdk::util::unix_time();
    let time = info["time"]
        .as_u64()
        .context("mint clock uncertainty: missing info time")?;
    ensure!(
        now.abs_diff(time) <= 60,
        "mint clock skew exceeds 60 seconds"
    );
    println!(
        "{}",
        serde_json::json!({"mint":mint,"version":info["version"],"nut07":true,"nut14":true,"clock_skew_seconds":now.abs_diff(time)})
    );
    Ok(())
}
pub async fn wallet(home: &std::path::Path, mint: &str) -> Result<Wallet> {
    crate::Asset::new(mint)?.fence()?;
    let path = home.join("wallet.seed");
    if !path.exists() {
        let a = cashu::nuts::SecretKey::generate();
        let b = cashu::nuts::SecretKey::generate();
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(&a.to_secret_bytes())?;
        f.write_all(&b.to_secret_bytes())?;
        f.sync_all()?;
        std::fs::File::open(home)?.sync_all()?;
    }
    let seed: [u8; 64] = fs::read(path)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid seed"))?;
    let asset = hex::encode(Sha256::digest(format!("{mint}|sat")));
    let db_path = home.join(format!("{asset}.sqlite"));
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&db_path)?;
    let db = cdk_sqlite::WalletSqliteDatabase::new(db_path).await?;
    Ok(WalletBuilder::new()
        .mint_url(mint.parse()?)
        .unit(CurrencyUnit::Sat)
        .localstore(Arc::new(db))
        .seed(seed)
        .build()?)
}

pub async fn database(
    home: &std::path::Path,
    mint: &str,
) -> Result<Arc<cdk_sqlite::WalletSqliteDatabase>> {
    let _ = wallet(home, mint).await?;
    let asset = hex::encode(Sha256::digest(format!("{mint}|sat")));
    Ok(Arc::new(
        cdk_sqlite::WalletSqliteDatabase::new(home.join(format!("{asset}.sqlite"))).await?,
    ))
}
pub fn identity(home: &std::path::Path) -> Result<nostr_sdk::Keys> {
    let p = home.join("trade.key");
    if !p.exists() {
        let k = nostr_sdk::Keys::generate();
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&p)?;
        f.write_all(k.secret_key().to_secret_hex().as_bytes())?;
        f.sync_all()?;
        std::fs::File::open(home)?.sync_all()?;
    }
    Ok(nostr_sdk::Keys::parse(&fs::read_to_string(p)?)?)
}
