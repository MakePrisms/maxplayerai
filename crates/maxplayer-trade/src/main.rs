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
    /// Relay for nostr:// mint requests only (kinds 23410/23411; repeatable, max 8; wss://, or ws://
    /// on loopback). Defaults to wss://relay.maxplayer.ai, wss://relay.ditto.pub and
    /// wss://nostr-pub.wellorder.net. Never used for market traffic; relay.maxplayer.ai carries
    /// mint traffic only and is refused as a --relay.
    #[arg(long = "mint-relay")]
    mint_relay: Vec<String>,
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
    /// One recovery pass; exit 3 if items remain pending. Does not wait for lock deadlines.
    Recover,
    /// Check NUT-07/09/11/12/14, an active sat keyset, and clock skew <= 60 seconds.
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
        /// Maximum invoice amount + input fee + Lightning reserve, in sats.
        #[arg(long)]
        max_debit: Option<u64>,
    },
    /// Import one plain-sat Cashu token (max 100,000 sats) from a file or stdin; never argv.
    Receive {
        mint: String,
        /// File holding the token; omit to read the token from stdin.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Read public journal states without locking the home or advancing payments.
    Status,
    /// Read spendable wallet balance without submitting existing money authorizations.
    Balance { mint: String },
}
use maxplayer_trade::{
    Asset, Leg, coordinator, journal::Journal, market::Market, wallet::preflight,
};
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match execute().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(if error.is::<coordinator::RecoveryIncomplete>() {
                3
            } else if error.is::<coordinator::ManualRecovery>() {
                4
            } else {
                1
            })
        }
    }
}
async fn execute() -> Result<()> {
    let cli = Cli::parse();
    maxplayer_trade::transport::configure_mint_relays(&cli.mint_relay)?;
    // No home, lock, journal or intent is touched for a path a nostr:// mint cannot serve.
    match &cli.command {
        Command::Fund { mint, .. } => maxplayer_trade::transport::require_http(mint, "fund")?,
        Command::Withdraw { mint, .. } => {
            maxplayer_trade::transport::require_http(mint, "withdraw")?
        }
        _ => {}
    }
    // Read-only observations remain available while serve owns the writer lock.
    if let Command::Balance { ref mint } = cli.command {
        let asset = Asset::from_cli(mint)?;
        println!(
            "{}",
            serde_json::json!({"mint":asset.mint_url,"unit":"sat","balance":maxplayer_trade::wallet::read_balance(&cli.home, &asset.mint_url)?})
        );
        return Ok(());
    }
    if matches!(cli.command, Command::Status) {
        println!("{}", maxplayer_trade::wallet::read_status(&cli.home)?);
        return Ok(());
    }
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
            | Command::Receive { .. }
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
                        asset: Asset::from_cli(&give_mint)?,
                        net: give,
                    },
                    Leg {
                        asset: Asset::from_cli(&want_mint)?,
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
        | Command::Withdraw { mint, .. }
        | Command::Receive { mint, .. } => mint,
        _ => unreachable!(),
    };
    let asset = maxplayer_trade::Asset::from_cli(mint_arg)?;
    asset.fence()?;
    let mint = &asset.mint_url;

    if matches!(cli.command, Command::Preflight { .. }) {
        return preflight(mint).await;
    }
    if let Command::Withdraw {
        ref invoice,
        max_debit,
        ..
    } = cli.command
    {
        let a = match maxplayer_trade::money::withdraw_bounded(
            &cli.home,
            &money_journal,
            mint,
            invoice,
            max_debit,
        )
        .await
        {
            Ok(a) => a,
            Err(error) => {
                // A submitted withdrawal whose resolution failed (mint unreachable after
                // POST, change not yet restorable) is unresolved work, never a refusal.
                if maxplayer_trade::money::submitted_unresolved(&money_journal, invoice)
                    .await
                    .unwrap_or(true)
                {
                    eprintln!("{error}");
                    return Err(coordinator::RecoveryIncomplete.into());
                }
                return Err(error);
            }
        };
        println!("{}", a.summary());
        if !a.terminal() {
            return Err(coordinator::RecoveryIncomplete.into());
        }
        ensure!(
            a.state == maxplayer_trade::money::MeltState::Done,
            "withdrawal not paid; terminal {:?}",
            a.state
        );
        return Ok(());
    }
    if let Command::Receive { ref token_file, .. } = cli.command {
        use std::io::Read;
        let limit = maxplayer_trade::receive::MAX_TOKEN_BYTES as u64 + 1;
        let mut raw = String::new();
        match token_file {
            Some(path) => fs::File::open(path)
                .context("cannot open token file")?
                .take(limit)
                .read_to_string(&mut raw)
                .context("token file is not readable UTF-8")?,
            None => std::io::stdin()
                .take(limit)
                .read_to_string(&mut raw)
                .context("stdin token is not readable UTF-8")?,
        };
        let r = maxplayer_trade::receive::receive(&cli.home, &money_journal, mint, &raw).await;
        drop(raw);
        let r = r?;
        println!("{}", r.summary());
        use maxplayer_trade::receive::ReceiveState::*;
        return match r.state {
            Done if r.settled() => Ok(()),
            // Journaled done but the credited rows are not yet released: run recover.
            Done => Err(coordinator::RecoveryIncomplete.into()),
            Quarantined => Err(coordinator::ManualRecovery.into()),
            Refused => anyhow::bail!(
                "receive refused by the mint ({}); final: the token was NOT imported, nothing \
                 was credited or charged to the cap, and it can be redeemed elsewhere",
                r.refusal_code
                    .map_or("NUT code not journaled".to_owned(), |c| format!(
                        "NUT code {c}"
                    ))
            ),
            AlreadySpent => anyhow::bail!("token already spent; nothing credited"),
            Prepared | Submitted => Err(coordinator::RecoveryIncomplete.into()),
        };
    }
    if let Command::Fund {
        amount, ref quote, ..
    } = cli.command
    {
        ensure!(
            amount > 0 && amount <= maxplayer_trade::real_money::CAP,
            "funding cap exceeds 100,000 sats"
        );
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

    unreachable!()
}
