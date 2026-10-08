use anyhow::{Context, Result, ensure};
use cdk::{
    amount::SplitTarget,
    nuts::{CurrencyUnit, PaymentMethod},
};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};
#[derive(Parser)]
struct Cli {
    #[arg(long)]
    home: PathBuf,
    #[arg(long)]
    relay: Vec<String>,
    #[arg(long, global = true)]
    real_mint_allow: Vec<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    List {
        #[arg(long)]
        give_mint: String,
        #[arg(long)]
        give: u64,
        #[arg(long)]
        want_mint: String,
        #[arg(long)]
        want: u64,
        #[arg(long, default_value_t = 16)]
        max_fees: u64,
    },
    Discover,
    Cancel {
        lot: String,
    },
    Serve,
    Take {
        lot: String,
        #[arg(long)]
        max_give: u64,
        #[arg(long)]
        min_receive: u64,
        #[arg(long, default_value_t = 16)]
        max_fees: u64,
    },
    Recover,
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
    Withdraw {
        mint: String,
        #[arg(long)]
        invoice: String,
    },
    Balance {
        mint: String,
    },
}
use maxplayer_trade::{
    Asset, Leg, coordinator,
    journal::Journal,
    market::Market,
    wallet::{preflight, wallet},
};
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    maxplayer_trade::real_money::configure(cli.real_mint_allow.clone())?;
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
    let money_journal = Journal::open(&cli.home).await?;
    // Inspection must not submit an earlier payment authorization. Newly credited
    // money proofs remain reserved until explicit recovery settles their journal.
    if matches!(cli.command, Command::Recover)
        && money_journal
            .all::<coordinator::Swap>("swap")
            .await?
            .is_empty()
        && money_journal
            .all::<coordinator::Listing>("listing")
            .await?
            .is_empty()
    {
        while maxplayer_trade::money::recover(&cli.home, &money_journal).await? {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        return Ok(());
    }
    if !matches!(
        &cli.command,
        Command::Preflight { .. }
            | Command::Fund { .. }
            | Command::Balance { .. }
            | Command::Withdraw { .. }
    ) {
        let keys = maxplayer_trade::wallet::identity(&cli.home)?;
        let relays = if cli.relay.is_empty() {
            maxplayer_trade::market::DEFAULT_RELAYS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            cli.relay.clone()
        };
        let j = Journal::open(&cli.home).await?;
        let mut m = Market::connect(keys, &relays).await?;
        coordinator::recover(&cli.home, &j, &m).await?;
        return match cli.command {
            Command::List {
                give_mint,
                give,
                want_mint,
                want,
                max_fees,
            } => {
                coordinator::list(
                    &cli.home,
                    &j,
                    &m,
                    Leg {
                        asset: Asset::new(&give_mint)?,
                        net: give,
                    },
                    Leg {
                        asset: Asset::new(&want_mint)?,
                        net: want,
                    },
                    max_fees,
                )
                .await?;
                Ok(())
            }
            Command::Discover => {
                let lots = m.discover(None).await?;
                let listings = lots.iter().map(|e| {
                    Ok(serde_json::json!({"lot_id":e.id,"maker":e.pubkey,"terms":serde_json::from_str::<serde_json::Value>(&e.content)?}))
                }).collect::<Result<Vec<_>>>()?;
                println!("{}", serde_json::json!({"status":"ok","listings":listings}));
                Ok(())
            }
            Command::Cancel { lot } => coordinator::cancel(&cli.home, &j, &m, &lot).await,
            Command::Take {
                lot,
                max_give,
                min_receive,
                max_fees,
            } => {
                let id = coordinator::start_take(
                    &cli.home,
                    &j,
                    &m,
                    &lot,
                    max_give,
                    min_receive,
                    max_fees,
                )
                .await?;
                coordinator::run(&cli.home, &j, &mut m, Some(&id)).await
            }
            Command::Serve => coordinator::run(&cli.home, &j, &mut m, None).await,
            Command::Recover => coordinator::recover_until_settled(&cli.home, &j, &mut m).await,
            _ => unreachable!(),
        };
    }
    let mint_arg = match &cli.command {
        Command::Preflight { mint }
        | Command::Fund { mint, .. }
        | Command::Balance { mint }
        | Command::Withdraw { mint, .. } => mint,
        _ => unreachable!(),
    };
    let asset = maxplayer_trade::Asset::new(mint_arg)?;
    asset.fence()?;
    let mint = &asset.mint_url;
    if matches!(cli.command, Command::Preflight { .. }) {
        return preflight(mint).await;
    }
    if let Command::Withdraw { ref invoice, .. } = cli.command {
        preflight(mint).await?;
        let a = maxplayer_trade::money::withdraw(&cli.home, &money_journal, mint, invoice).await?;
        println!("{}", a.summary());
        return Ok(());
    }
    if let Command::Fund {
        amount, ref quote, ..
    } = cli.command
    {
        if maxplayer_trade::real_money::allows(mint) {
            ensure!(
                amount > 0 && amount <= maxplayer_trade::real_money::CAP,
                "funding cap exceeds 500 sats"
            );
            preflight(mint).await?;
            let mut f = if let Some(id) = quote {
                money_journal
                    .all::<maxplayer_trade::money::Funding>("funding")
                    .await?
                    .into_iter()
                    .find(|f| f.quote.as_ref() == Some(id) && f.mint == *mint && f.amount == amount)
                    .context("quote not journaled for this mint and amount")?
            } else {
                maxplayer_trade::money::fund(&cli.home, &money_journal, mint, amount).await?
            };
            println!(
                "{}",
                serde_json::json!({"quote":f.quote,"invoice":f.invoice,"amount":f.amount,"note":"pay externally; tool never auto-pays"})
            );
            for _ in 0..30 {
                maxplayer_trade::money::resume_fund(&cli.home, &money_journal, &mut f).await?;
                if f.done {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            anyhow::bail!("funding remains pending; invoice and intent retained; run recover");
        }
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
