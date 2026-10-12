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
    home: Option<PathBuf>,
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
    /// One read-only info request to an explicit mint/relay pair; no wallet or journal.
    Doctor {
        mint: String,
        #[arg(long = "mint-relay")]
        mint_relay: String,
        #[arg(long)]
        json: bool,
        /// Read-only probe budget; does not change monetary request windows.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=300))]
        timeout_seconds: u64,
    },
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
        /// Print both legs' expected fees, gross amounts and total debit; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Discover verified available lots (lock-free, read-only; works while serve runs).
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
        /// Print both legs' expected fees, gross amounts and total debit; change nothing.
        #[arg(long)]
        dry_run: bool,
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
    /// Export exactly --amount sats as a Cashu token file (0600, never overwritten; max 100,000), or --reclaim an unredeemed send.
    Send {
        #[arg(required_unless_present = "reclaim", conflicts_with = "reclaim")]
        mint: Option<String>,
        #[arg(long, required_unless_present = "reclaim", conflicts_with = "reclaim")]
        amount: Option<u64>,
        /// New token file; the token is never printed.
        #[arg(long, required_unless_present = "reclaim", conflicts_with = "reclaim")]
        out: Option<PathBuf>,
        /// Maximum mint input fee, in sats; required unless the mint charges none.
        #[arg(long)]
        max_fees: Option<u64>,
        /// Send attempt id whose still-unspent proofs return to this wallet.
        #[arg(long)]
        reclaim: Option<String>,
    },
    /// Summarize lots, swaps and money records without locking the home or advancing payments.
    Status {
        /// Print the raw public records (machine-readable) instead of the summary.
        #[arg(long)]
        json: bool,
        /// Also ask each mint (NUT-07) whether sent tokens were redeemed.
        #[arg(long)]
        check_sends: bool,
    },
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
            if let Some(f) = error.downcast_ref::<DoctorExit>() {
                return std::process::ExitCode::from(f.0);
            }
            if let Some(f) = error.downcast_ref::<maxplayer_trade::serve::Forwarded>() {
                return std::process::ExitCode::from(f.0);
            }
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
    if let Command::Doctor {
        mint,
        mint_relay,
        json,
        timeout_seconds,
    } = &cli.command
    {
        return doctor(mint, mint_relay, *json, *timeout_seconds).await;
    }
    let home = cli
        .home
        .as_ref()
        .context("--home is required for this command")?;
    maxplayer_trade::transport::configure_mint_relays(&cli.mint_relay)?;
    // No home, lock, journal or intent is touched for a path a nostr:// mint cannot serve.
    match &cli.command {
        Command::Fund { mint, .. } => maxplayer_trade::transport::require_http(mint, "fund")?,
        Command::Withdraw { mint, .. } => {
            maxplayer_trade::transport::require_http(mint, "withdraw")?
        }
        _ => {}
    }
    // A zero amount can never succeed: refuse it before any cap check, home, lock or mint.
    let zero = match &cli.command {
        Command::Fund { amount, .. }
        | Command::Send {
            amount: Some(amount),
            ..
        } => (*amount == 0).then_some("--amount"),
        Command::List { give, want, .. } => (*give == 0)
            .then_some("--give")
            .or((*want == 0).then_some("--want")),
        Command::Take { max_give, .. } => (*max_give == 0).then_some("--max-give"),
        Command::Withdraw { max_debit, .. } => (*max_debit == Some(0)).then_some("--max-debit"),
        _ => None,
    };
    if let Some(flag) = zero {
        anyhow::bail!("amount must be at least 1 sat ({flag} 0)");
    }
    // Read-only observations remain available while serve owns the writer lock.
    if let Command::Balance { ref mint } = cli.command {
        let asset = Asset::from_cli(mint)?;
        println!(
            "{}",
            serde_json::json!({"mint":asset.mint_url,"unit":"sat","balance":maxplayer_trade::wallet::read_balance(home, &asset.mint_url)?})
        );
        return Ok(());
    }
    if let Command::Status { json, check_sends } = cli.command {
        if json {
            println!("{}", maxplayer_trade::wallet::read_status(home)?);
        } else {
            println!("{}", maxplayer_trade::report::render(home)?);
        }
        if check_sends {
            maxplayer_trade::send::check_redeemed(home).await?;
        }
        return Ok(());
    }
    let relays: Vec<String> = if cli.relay.is_empty() {
        maxplayer_trade::market::DEFAULT_RELAYS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        cli.relay.clone()
    };
    // Lock-free, journal-free commands: they only read relays, mints and SQLite snapshots.
    match &cli.command {
        Command::Discover => return lockfree::discover(&relays).await,
        Command::List {
            give_mint,
            give,
            want_mint,
            want,
            max_fees,
            dry_run: true,
        } => {
            return lockfree::list(home, give_mint, *give, want_mint, *want, *max_fees).await;
        }
        Command::Take {
            lot,
            max_give,
            min_receive,
            max_fees,
            dry_run: true,
        } => {
            return lockfree::take(home, &relays, lot, *max_give, *min_receive, *max_fees).await;
        }
        _ => {}
    }
    // A running serve owns the home: hand list/take/cancel to it, with these exact bounds.
    let handoff = match &cli.command {
        Command::List {
            give_mint,
            give,
            want_mint,
            want,
            max_fees,
            ..
        } => Some(maxplayer_trade::serve::Command::List {
            give_mint: give_mint.clone(),
            give: *give,
            want_mint: want_mint.clone(),
            want: *want,
            max_fees: *max_fees,
        }),
        Command::Take {
            lot,
            max_give,
            min_receive,
            max_fees,
            ..
        } => Some(maxplayer_trade::serve::Command::Take {
            lot: lot.clone(),
            max_give: *max_give,
            min_receive: *min_receive,
            max_fees: *max_fees,
        }),
        Command::Cancel { lot } => {
            Some(maxplayer_trade::serve::Command::Cancel { lot: lot.clone() })
        }
        _ => None,
    }
    .map(|command| maxplayer_trade::serve::Request {
        v: maxplayer_trade::serve::PROTOCOL_V,
        relays: relays.clone(),
        command,
    });
    if let Some(request) = &handoff {
        if home.is_dir() {
            if let Some(code) = maxplayer_trade::serve::forward(home, request).await? {
                return forwarded(code);
            }
        }
    }
    fs::create_dir_all(home)?;
    fs::set_permissions(home, fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(home.join("owner.lock"))?;
    if let Err(busy) = lock.try_lock_exclusive() {
        // Another process owns the home: a serve (possibly still binding its socket) gets the
        // request; a short-lived writer such as recover is waited out. Never two writers.
        let Some(request) = &handoff else {
            return Err(busy).context("home is already in use");
        };
        let end = tokio::time::Instant::now() + maxplayer_trade::serve::CONNECT_WAIT;
        loop {
            if let Some(code) = maxplayer_trade::serve::forward(home, request).await? {
                return forwarded(code);
            }
            if lock.try_lock_exclusive().is_ok() {
                break;
            }
            if tokio::time::Instant::now() >= end {
                return Err(busy).context("home is already in use");
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    #[cfg(feature = "lab")]
    let _trace = lab_trace::Writer::start();
    // Bind while holding owner.lock (an existing socket file is stale) and before the startup
    // recovery pass, so hand-offs queue instead of failing while serve recovers.
    let mut jobs = if matches!(cli.command, Command::Serve) {
        Some(maxplayer_trade::serve::bind(home, relays.clone())?)
    } else {
        None
    };
    // `recover` is one bounded pass: it waits for mint pacing instead of skipping an item.
    maxplayer_trade::backoff::set_wait_for_pacing(matches!(cli.command, Command::Recover));
    let money_journal = Journal::open(home).await?;
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
        let pending = maxplayer_trade::money::recover(home, &money_journal).await?;
        return coordinator::recovery_status(&money_journal, pending).await;
    }
    if !matches!(
        &cli.command,
        Command::Preflight { .. }
            | Command::Fund { .. }
            | Command::Balance { .. }
            | Command::Withdraw { .. }
            | Command::Receive { .. }
            | Command::Send { .. }
    ) {
        let keys = maxplayer_trade::wallet::identity(home)?;
        let j = Journal::open(home).await?;
        let mut m = Market::connect(keys, &relays).await?;
        if !matches!(cli.command, Command::Recover) {
            coordinator::recover(home, &j, &m).await?;
        }
        return match cli.command {
            Command::List {
                give_mint,
                give,
                want_mint,
                want,
                max_fees,
                ..
            } => {
                coordinator::list(
                    home,
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
            Command::Cancel { lot } => coordinator::cancel(home, &j, &m, &lot).await,
            Command::Take {
                lot,
                max_give,
                min_receive,
                max_fees,
                ..
            } => {
                let id =
                    coordinator::start_take(home, &j, &m, &lot, max_give, min_receive, max_fees)
                        .await?;
                coordinator::run(home, &j, &mut m, Some(&id)).await
            }
            Command::Serve => coordinator::run_with(home, &j, &mut m, None, jobs.take()).await,
            Command::Recover => coordinator::recover_until_settled(home, &j, &mut m).await,
            _ => unreachable!(),
        };
    }
    if let Command::Send {
        mint,
        amount,
        out,
        max_fees,
        reclaim,
    } = &cli.command
    {
        return maxplayer_trade::send::command(
            home,
            &money_journal,
            mint.as_deref(),
            *amount,
            out.as_deref(),
            *max_fees,
            reclaim.as_deref(),
        )
        .await;
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
            home,
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
        let r = maxplayer_trade::receive::receive(home, &money_journal, mint, &raw).await;
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
            maxplayer_trade::money::fund(home, &money_journal, mint, amount).await?
        };
        println!(
            "{}",
            serde_json::json!({"quote":f.quote,"invoice":f.invoice,"amount":f.amount,"note":"pay externally; tool never auto-pays"})
        );
        // Quote polling: one status GET per FUND_POLL (about 30 s in total).
        for _ in 0..maxplayer_trade::money::FUND_POLLS {
            maxplayer_trade::money::resume_fund(home, &money_journal, &mut f).await?;
            if f.done {
                return Ok(());
            }
            tokio::time::sleep(maxplayer_trade::money::fund_poll(&f.mint)).await;
        }
        anyhow::bail!("funding remains pending; invoice and intent retained; run recover");
    }

    unreachable!()
}

fn forwarded(code: u8) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(maxplayer_trade::serve::Forwarded(code).into())
    }
}

/// Commands that never take owner.lock, never open the journal and never create files.
mod lockfree {
    use anyhow::{Context, Result, bail, ensure};
    use maxplayer_trade::{Asset, dryrun, market::Market, parse_lot, wallet};
    use std::path::Path;
    /// Ephemeral relay identity: discovery needs no trade key and must not create one.
    async fn market(relays: &[String]) -> Result<Market> {
        Market::connect(nostr_sdk::Keys::generate(), relays).await
    }
    pub async fn discover(relays: &[String]) -> Result<()> {
        let m = market(relays).await?;
        let lots = m.discover(None).await?;
        let now = maxplayer_trade::coordinator::now();
        let terms = lots
            .iter()
            .map(|e| parse_lot(e, now))
            .collect::<Result<Vec<_>>>()?;
        let fees = dryrun::discover_fees(&terms).await;
        let listings = lots.iter().zip(fees).map(|(e, fees)| {
            Ok(serde_json::json!({"lot_id":e.id,"maker":e.pubkey,"terms":serde_json::from_str::<serde_json::Value>(&e.content)?,"expected_fees":fees}))
        }).collect::<Result<Vec<_>>>()?;
        println!("{}", serde_json::json!({"status":"ok","listings":listings}));
        Ok(())
    }
    async fn preflight(mints: [&str; 2]) -> Result<[dryrun::Keysets; 2]> {
        maxplayer_trade::diagnostics::notice();
        for mint in mints {
            Asset::new(mint)?.fence()?;
        }
        let (a, b) = tokio::join!(
            wallet::preflight_admission(mints[0]),
            wallet::preflight_admission(mints[1])
        );
        Ok([dryrun::parse_keysets(&a?)?, dryrun::parse_keysets(&b?)?])
    }
    fn finish(out: serde_json::Value, refusal: Option<String>) -> Result<()> {
        println!("{out}");
        match refusal {
            Some(r) => bail!("dry run: the real command would refuse: {r}"),
            None => Ok(()),
        }
    }
    pub async fn list(
        home: &Path,
        give_mint: &str,
        give: u64,
        want_mint: &str,
        want: u64,
        max_fees: u64,
    ) -> Result<()> {
        let (give_asset, want_asset) = (Asset::new(give_mint)?, Asset::new(want_mint)?);
        ensure!(give_asset != want_asset, "same-asset swap");
        let [gks, wks] = preflight([&give_asset.mint_url, &want_asset.mint_url]).await?;
        let p = dryrun::plan(
            &dryrun::spendable(home, &give_asset.mint_url)?,
            &gks,
            give,
            max_fees,
        )?;
        let incoming = dryrun::leg(
            want,
            wks.active()
                .context("want mint has no active sat keyset")?
                .1,
        );
        let mut refusal = p.error.clone();
        if let Some(c) = incoming["claim_fee"].as_u64() {
            if c > max_fees && refusal.is_none() {
                refusal = Some(format!(
                    "every taker would be refused: incoming claim fee {c} exceeds --max-fees {max_fees}"
                ));
            }
        } else if refusal.is_none() {
            refusal = incoming["error"].as_str().map(str::to_owned);
        }
        finish(
            serde_json::json!({"dry_run":true,"command":"list",
                "you_give":p.json(&give_asset.mint_url),
                "you_receive":{"mint":want_asset.mint_url,"leg":incoming,"note":"the taker funds your claim fee; you net exactly the wanted amount"},
                "max_total_debit":p.debit,"max_fees":max_fees,
                "fees_total":p.lock_fee.map(|l| l + p.claim_fee),
                "side_effects":"none: nothing journaled, reserved, locked or published"}),
            refusal,
        )
    }
    pub async fn take(
        home: &Path,
        relays: &[String],
        lot: &str,
        max_give: u64,
        min_receive: u64,
        max_fees: u64,
    ) -> Result<()> {
        let m = market(relays).await?;
        let e = m
            .discover(Some(lot.parse()?))
            .await?
            .into_iter()
            .next()
            .context("lot unavailable, invalid, expired or cancelled")?;
        let l = parse_lot(&e, maxplayer_trade::coordinator::now())?;
        // Self-trade check without creating a trade key.
        let own = std::fs::read_to_string(home.join("trade.key"))
            .ok()
            .and_then(|k| nostr_sdk::Keys::parse(&k).ok());
        ensure!(own.is_none_or(|k| k.public_key() != e.pubkey), "self trade");
        let [gks, wks] = preflight([&l.give.asset.mint_url, &l.want.asset.mint_url]).await?;
        let p = dryrun::plan(
            &dryrun::spendable(home, &l.want.asset.mint_url)?,
            &wks,
            l.want.net,
            max_fees,
        )?;
        let incoming = dryrun::leg(
            l.give.net,
            gks.active()
                .context("give mint has no active sat keyset")?
                .1,
        );
        let mut refusal = None;
        if l.give.net < min_receive {
            refusal = Some(format!(
                "minimum receive cap: lot gives {} net < --min-receive {min_receive}",
                l.give.net
            ));
        }
        refusal = refusal.or(p.error.clone());
        if refusal.is_none() {
            if let Some(d) = p.debit.filter(|d| *d > max_give) {
                refusal = Some(format!(
                    "maximum give cap: debit {d} > --max-give {max_give}"
                ));
            }
        }
        if refusal.is_none() {
            match incoming["claim_fee"].as_u64() {
                Some(c) if c > max_fees => {
                    refusal = Some(format!(
                        "incoming claim fee {c} exceeds taker admission cap --max-fees {max_fees}"
                    ))
                }
                Some(_) => {}
                None => refusal = incoming["error"].as_str().map(str::to_owned),
            }
        }
        finish(
            serde_json::json!({"dry_run":true,"command":"take","lot_id":e.id,
                "you_give":p.json(&l.want.asset.mint_url),
                "you_receive":{"mint":l.give.asset.mint_url,"leg":incoming,"note":"the maker funds your claim fee; you net exactly this amount"},
                "max_total_debit":p.debit,"max_give":max_give,"min_receive":min_receive,"max_fees":max_fees,
                "fees_total":p.lock_fee.map(|lf| lf + p.claim_fee),
                "side_effects":"none: nothing journaled, reserved, locked, published or sent to the maker"}),
            refusal,
        )
    }
}

/// Lab-only writer trace for the single-writer race test: one line per owner.lock tenure.
#[cfg(feature = "lab")]
mod lab_trace {
    use std::io::Write;
    pub struct Writer(Option<std::path::PathBuf>);
    fn line(path: &std::path::Path, what: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let _ = writeln!(f, "{what} {} {t}", std::process::id());
        }
    }
    impl Writer {
        pub fn start() -> Self {
            let path = std::env::var_os("TRADE_LAB_WRITER_TRACE").map(std::path::PathBuf::from);
            if let Some(p) = &path {
                line(p, "acquire");
            }
            Self(path)
        }
    }
    impl Drop for Writer {
        fn drop(&mut self) {
            if let Some(p) = &self.0 {
                line(p, "release");
            }
        }
    }
}

#[derive(Debug)]
struct DoctorExit(u8);
impl std::fmt::Display for DoctorExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "doctor exit {}", self.0)
    }
}
impl std::error::Error for DoctorExit {}
async fn doctor(mint: &str, relay: &str, json_output: bool, seconds: u64) -> Result<()> {
    use maxplayer_core::nostr_mint::NostrMintConnector;
    use std::time::Duration;
    let asset = Asset::from_cli(mint)?;
    ensure!(
        maxplayer_trade::transport::is_nostr(&asset.mint_url),
        "doctor requires a nostr:// mint and one explicit --mint-relay"
    );
    maxplayer_trade::transport::configure_mint_relays(&[relay.to_owned()])?;
    let window = Duration::from_secs(seconds);
    let connector = NostrMintConnector::new(&asset.mint_url.parse()?, vec![relay.to_owned()])?
        .with_timing(
            window,
            window + Duration::from_secs(1),
            Duration::from_secs(5).min(window),
        );
    eprintln!(
        "Waiting for mint info (read-only budget {seconds}s); no wallet, journal or trade is opened"
    );
    let (result, evidence) = connector
        .call_observed("info", serde_json::Value::Null)
        .await;
    let compatibility = result
        .as_ref()
        .ok()
        .map(maxplayer_trade::compatibility::assess);
    let incompatible = evidence.outcome == "unsupported_protocol"
        || compatibility
            .as_ref()
            .is_some_and(|c| c["status"] == "incompatible");
    let usable = result
        .as_ref()
        .is_ok_and(|v| v.is_object() && v["nuts"].is_object());
    let exit = if incompatible {
        1
    } else if usable {
        0
    } else {
        2
    };
    let slow = evidence.reply_ms.is_some_and(|ms| ms > 5000);
    let report = serde_json::json!({"observation":evidence,"compatibility":compatibility,
        "latency":if evidence.reply_ms.is_none() {"unknown"} else if slow {"slow"} else {"normal"},
        "slow_warning_threshold_ms":5000,"exit":exit,
        "scope":"info only; keysets, swap, restore and witness emission not tested"});
    if json_output {
        println!("{report}");
    } else {
        println!("{}", evidence.message());
        if let Some(c) = compatibility {
            println!(
                "{}",
                c["message"].as_str().unwrap_or("compatibility unknown")
            );
        }
        if slow {
            println!("WARNING: slow reply (warning threshold 5s); this is not a refusal");
        }
        println!("Info only; no guarantee of later claims or refunds.");
    }
    if exit == 0 {
        Ok(())
    } else {
        Err(DoctorExit(exit).into())
    }
}
