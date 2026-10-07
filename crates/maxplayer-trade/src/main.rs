use anyhow::{Context, Result, ensure};
use cdk::{
    amount::SplitTarget,
    nuts::{CurrencyUnit, PaymentMethod},
    wallet::{Wallet, WalletBuilder},
};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
};
#[derive(Parser)]
struct Cli {
    #[arg(long)]
    home: PathBuf,
    #[arg(long)]
    allow_real_mint: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Preflight {
        mint: String,
    },
    Fund {
        mint: String,
        #[arg(long)]
        amount: u64,
        #[arg(long)]
        quote: Option<String>,
    },
    Balance {
        mint: String,
    },
}
async fn preflight(mint: &str) -> Result<()> {
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
async fn wallet(home: &std::path::Path, mint: &str) -> Result<Wallet> {
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
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    fs::create_dir_all(&cli.home)?;
    fs::set_permissions(&cli.home, fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(cli.home.join("owner.lock"))?;
    lock.try_lock_exclusive()
        .context("home is already in use")?;
    let mint_arg = match &cli.command {
        Command::Preflight { mint } | Command::Fund { mint, .. } | Command::Balance { mint } => {
            mint
        }
    };
    let asset = maxplayer_trade::Asset::new(mint_arg)?;
    asset.fence(cli.allow_real_mint)?;
    let mint = &asset.mint_url;
    if matches!(cli.command, Command::Preflight { .. }) {
        return preflight(mint).await;
    }
    let w = wallet(&cli.home, mint).await?;
    match cli.command {
        Command::Fund {
            amount, ref quote, ..
        } => {
            ensure!(
                amount > 0 && amount <= 500_000,
                "funding amount out of test range"
            );
            preflight(mint).await?;
            let q = if let Some(id) = quote {
                w.check_mint_quote(id).await?
            } else {
                w.mint_quote(PaymentMethod::BOLT11, Some(amount.into()), None, None)
                    .await?
            };
            ensure!(
                q.mint_url.to_string().trim_end_matches('/') == mint
                    && q.unit == CurrencyUnit::Sat
                    && q.amount == Some(amount.into()),
                "resumed quote does not match requested asset/amount"
            );
            println!(
                "{}",
                serde_json::json!({"quote":q.id,"amount":amount,"note":"test mint auto-pay only; no invoice is paid by this tool"})
            );
            for _ in 0..30 {
                let current = w.check_mint_quote(&q.id).await?;
                if current.state != cashu::nuts::nut23::QuoteState::Paid {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                match w.mint(&q.id, SplitTarget::default(), None).await {
                    Ok(p) => {
                        println!(
                            "{}",
                            serde_json::json!({"minted_proofs":p.len(),"balance":u64::from(w.total_balance().await?)})
                        );
                        return Ok(());
                    }
                    Err(e) if e.to_string().to_lowercase().contains("not paid") => {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            anyhow::bail!("test quote remains unpaid; retained in wallet for recovery")
        }
        Command::Balance { .. } => {
            println!(
                "{}",
                serde_json::json!({"mint":mint,"unit":"sat","balance":u64::from(w.total_balance().await?)})
            );
            Ok(())
        }
        _ => unreachable!(),
    }
}
