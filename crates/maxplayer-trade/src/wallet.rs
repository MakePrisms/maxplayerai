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
async fn info(mint: &str) -> Result<serde_json::Value> {
    crate::Asset::new(mint)?.fence()?;
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
    Ok(info)
}
pub async fn action_time(mint: &str) -> Result<u64> {
    let info = info(mint).await?;
    Ok(cdk::util::unix_time().max(info["time"].as_u64().context("missing mint time")?))
}
pub async fn refund_time(mint: &str) -> Result<u64> {
    // This clock is used for refund eligibility; the mint enforces locktime.
    // Missing info must not strand an otherwise working swap endpoint.
    let local = cdk::util::unix_time();
    Ok(match info(mint).await {
        Ok(info) => local.max(info["time"].as_u64().unwrap_or(local)),
        Err(_) => local,
    })
}
pub async fn bounded<T>(
    f: impl std::future::Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout(std::time::Duration::from_secs(15), f).await
}
pub async fn preflight(mint: &str) -> Result<()> {
    preflight_for(mint, None).await
}
pub async fn preflight_for(mint: &str, operation: Option<&str>) -> Result<()> {
    let info = info(mint).await?;
    ensure!(
        info["nuts"]["9"]["supported"] == true && info["nuts"]["12"]["supported"] == true,
        "mint does not advertise NUT-09 and NUT-12"
    );
    ensure!(
        info["nuts"]["7"]["supported"] == true && info["nuts"]["14"]["supported"] == true,
        "mint does not advertise NUT-07 and NUT-14"
    );
    ensure!(
        info["nuts"]["11"]["supported"] == true,
        "mint does not advertise NUT-11"
    );
    if let Some(operation) = operation {
        let nut = if operation == "fund" { "4" } else { "5" };
        ensure!(
            info["nuts"][nut]["disabled"] == false
                && info["nuts"][nut]["methods"]
                    .as_array()
                    .is_some_and(|methods| methods
                        .iter()
                        .any(|m| m["method"] == "bolt11" && m["unit"] == "sat")),
            "mint does not enable required bolt11/sat NUT-{nut}"
        );
        if operation == "fund" {
            ensure!(
                info["nuts"]["20"]["supported"] == true,
                "mint does not advertise NUT-20"
            );
        }
    }
    let now = cdk::util::unix_time();
    let time = info["time"]
        .as_u64()
        .context("mint clock uncertainty: missing info time")?;
    ensure!(
        now.abs_diff(time) <= 60,
        "mint clock skew exceeds 60 seconds"
    );
    let keysets: serde_json::Value = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(format!("{}/v1/keysets", mint.trim_end_matches('/')))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        keysets["keysets"].as_array().is_some_and(|sets| sets
            .iter()
            .any(|k| k["unit"] == "sat" && k["active"] == true)),
        "mint has no active sat keyset"
    );
    eprintln!(
        "{}",
        serde_json::json!({"mint":mint,"version":info["version"],"nut07":true,"nut07_witnesses":"unverified: requires a known spent HTLC; advertisement is not emission evidence","nut14":true,"clock_skew_seconds":now.abs_diff(time)})
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

/// Read-only SQLite snapshot: no seed creation, migrations, recovery or owner lock.
pub fn read_balance(home: &std::path::Path, mint: &str) -> Result<u64> {
    let asset = hex::encode(Sha256::digest(format!("{mint}|sat")));
    let path = home.join(format!("{asset}.sqlite"));
    if !path.exists() {
        return Ok(0);
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(db.query_row("SELECT COALESCE(SUM(amount),0) FROM proof WHERE mint_url=?1 AND unit='sat' AND state='UNSPENT' AND used_by_operation IS NULL AND spending_condition IS NULL", [mint], |r| r.get(0))?)
}
pub fn read_status(home: &std::path::Path) -> Result<serde_json::Value> {
    let path = home.join("trade.sqlite");
    if !path.exists() {
        return Ok(serde_json::json!({"records":[]}));
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    let mut stmt = db.prepare("SELECT secondary_namespace, value FROM kv_store WHERE primary_namespace='trade-v1' AND secondary_namespace IN ('swap','funding','withdrawal')")?;
    let mut records = vec![];
    for row in stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
    })? {
        let (kind, bytes) = row?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        let public = if kind == "withdrawal" {
            serde_json::from_slice::<crate::money::Withdrawal>(&bytes)?.summary()
        } else {
            serde_json::json!({"kind":kind,"id":v["id"],"state":v["state"],"done":v["done"],"expired_unpaid":v["expired_unpaid"]})
        };
        records.push(public);
    }
    Ok(serde_json::json!({"records":records}))
}
