use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};
#[derive(Parser)]
struct Cli {
    /// Private, exclusively owned journal and wallet directory. Never delete a funded home.
    #[arg(long)]
    home: PathBuf,
    /// Relay URL (repeatable); defaults to nos.lol, relay.primal.net and offchain.pub.
    #[arg(long)]
    relay: Vec<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Publish a fixed lot and reserve its inputs; preflights both mints.
    List {
        #[arg(long)]
        give_mint: String,
        #[arg(long)]
        give: u64,
        #[arg(long)]
        want_mint: String,
        #[arg(long)]
        want: u64,
        /// Maximum combined lock and claim mint fees, in sats.
        #[arg(long, default_value_t = 16)]
        max_fees: u64,
    },
    /// Discover verified available lots across all configured relays.
    Discover,
    /// Cancel an inactive lot and release its reserved inputs.
    Cancel { lot: String },
    /// Keep running to accept trades and recover existing money authorizations.
    Serve,
    /// Take a lot within explicit debit, net-receipt and fee limits.
    Take {
        lot: String,
        /// Maximum total debit including mint fees, in sats.
        #[arg(long)]
        max_give: u64,
        /// Minimum net received after mint input fees, in sats.
        #[arg(long)]
        min_receive: u64,
        /// Maximum combined lock and claim mint fees, in sats.
        #[arg(long, default_value_t = 16)]
        max_fees: u64,
    },
    /// One recovery pass; exit 2 if items remain pending. Does not wait for lock deadlines.
    Recover,
    /// Check NUT-07/09/12/14, an active sat keyset, and clock skew <= 60 seconds.
    Preflight { mint: String },
    /// Create or resume keyed funding; lifetime cap is 100,000 sats per mint/home. Never pays invoices.
    Fund {
        mint: String,
        #[arg(long)]
        amount: u64,
        #[arg(long)]
        quote: Option<String>,
    },
    /// Pay one exact BOLT11 invoice (max 100,000 sats), journaling inputs and change.
    Withdraw {
        mint: String,
        #[arg(long)]
        invoice: String,
    },
    /// Read spendable wallet balance without submitting existing money authorizations.
    Balance { mint: String },
}
use maxplayer_trade::{
    Asset, Leg, coordinator,
    journal::Journal,
    market::Market,
    wallet::{preflight, wallet},
};
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match execute().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(if error.is::<coordinator::RecoveryIncomplete>() {
                2
            } else {
                1
            })
        }
    }
}
async fn execute() -> Result<()> {
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
        let pending = maxplayer_trade::money::recover(&cli.home, &money_journal).await?;
        return coordinator::recovery_status(&money_journal, pending).await;
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
        if !matches!(cli.command, Command::Recover) {
            coordinator::recover(&cli.home, &j, &m).await?;
        }
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
        ensure!(
            amount > 0 && amount <= maxplayer_trade::real_money::CAP,
            "funding cap exceeds 100,000 sats"
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

    let w = wallet(&cli.home, mint).await?;
    match cli.command {
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
