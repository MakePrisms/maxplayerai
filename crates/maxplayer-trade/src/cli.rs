//! The `maxplayer trade` command line (formerly the standalone `maxplayer-trade` binary).
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
#[derive(Parser)]
#[command(name = "maxplayer trade")]
struct Cli {
    /// The maxplayer home to trade from (default: $MAXPLAYER_HOME, else ~/.maxplayer): its `key`
    /// signs and its wallet pays; trade's journal lives in <home>/trade. A legacy standalone
    /// trading home (wallet.seed / trade.key) is still accepted to drain it. Never delete a funded home.
    #[arg(long)]
    home: Option<PathBuf>,
    /// Market relay URL (repeatable); defaults to relay.maxplayer.ai, then nos.lol,
    /// relay.primal.net and offchain.pub as fallbacks.
    #[arg(long)]
    relay: Vec<String>,
    /// Relay for nostr:// mint requests only (kinds 23410/23411; repeatable, max 8; wss://).
    /// Defaults to wss://relay.maxplayer.ai, wss://relay.ditto.pub and
    /// wss://nostr-pub.wellorder.net. Never used for market traffic, which goes to --relay.
    #[arg(long = "mint-relay")]
    mint_relay: Vec<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// One read-only info request to an explicit mint/relay pair; no wallet or journal. Signed
    /// with the key of --home when it exists (a legacy home's trade.key; never created), else a
    /// throwaway key that a sign-in-required relay (relay.maxplayer.ai) refuses.
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
    Discover {
        /// Only lots whose maker gives this mint (same direction as on list).
        #[arg(long)]
        give_mint: Option<String>,
        /// Only lots whose maker wants this mint. With both mints, lots are sorted cheapest
        /// first for a taker (fewest wanted sats per given sat).
        #[arg(long)]
        want_mint: Option<String>,
    },
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
    /// One recovery pass; does not wait for lock deadlines. Exits 0 when settled, 3 while items remain (keep serve running or rerun), 4 when manual recovery is needed.
    Recover,
    /// Check NUT-07/09/11/12/14, an active sat keyset, and clock skew <= 60 seconds.
    Preflight { mint: String },
    /// Moved: `maxplayer wallet mint` (money in and out is `maxplayer wallet`).
    #[command(hide = true, disable_help_flag = true)]
    Fund {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
    /// Moved: `maxplayer wallet melt --max-debit`.
    #[command(hide = true, disable_help_flag = true)]
    Withdraw {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
    /// Moved: `maxplayer wallet receive --token-file`.
    #[command(hide = true, disable_help_flag = true)]
    Receive {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
    /// Drain a legacy standalone home: export exactly --amount sats as a Cashu token file (0600,
    /// never overwritten; max 100,000). Or --reclaim an unredeemed send made by trade. On a maxplayer
    /// home a new send is `maxplayer wallet send <amount> --out <file>`.
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
    /// Summarize lots, swaps and money records without locking the home or advancing payments; always exits 0.
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
use crate::{Asset, Leg, coordinator, journal::Journal, market::Market, wallet::preflight};
/// `maxplayer trade …`: parse `args` (the first item is the program name shown in usage) and run
/// one trade command to completion. Returns the process exit code. Owns its own tokio runtime, so
/// call it from synchronous code only (the `maxplayer` dispatcher, the test binary).
pub fn main_from<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return e.exit_code().clamp(0, 255) as u8;
        }
    };
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(run(cli)),
        Err(e) => {
            eprintln!("tokio runtime: {e}");
            1
        }
    }
}
async fn run(cli: Cli) -> u8 {
    // Keep ownership through the final snapshot and socket cleanup, not just execute().
    let mut owner_lock = None;
    match execute(cli, &mut owner_lock).await {
        Ok(()) => {
            crate::shutdown::finished(0, None);
            0
        }
        Err(error) => {
            if let Some(f) = error.downcast_ref::<DoctorExit>() {
                return f.0;
            }
            if let Some(f) = error.downcast_ref::<crate::serve::Forwarded>() {
                return f.0;
            }
            eprintln!("{error}");
            let code = if error.is::<coordinator::RecoveryIncomplete>() {
                3
            } else if error.is::<coordinator::ManualRecovery>() {
                4
            } else if error.is::<crate::shutdown::Stopped>() {
                crate::shutdown::EXIT_STOPPED
            } else {
                1
            };
            // serve only: the final lifecycle line (reason, exit code, unfinished work).
            crate::shutdown::finished(code, Some(&error));
            code
        }
    }
}
async fn execute(cli: Cli, owner_lock: &mut Option<fs::File>) -> Result<()> {
    if let Command::Doctor {
        mint,
        mint_relay,
        json,
        timeout_seconds,
    } = &cli.command
    {
        return doctor(
            mint,
            mint_relay,
            *json,
            *timeout_seconds,
            cli.home.as_deref(),
        )
        .await;
    }
    // Money in and out moved to `maxplayer wallet` (one wallet, one set of commands).
    match &cli.command {
        Command::Fund { .. } => anyhow::bail!(
            "{}",
            moved("fund", "maxplayer wallet mint <amount> --mint <url>")
        ),
        Command::Withdraw { .. } => anyhow::bail!(
            "{}",
            moved(
                "withdraw",
                "maxplayer wallet melt <bolt11> --max-debit <sats> --mint <url>` (its --max-debit \
                 bounds invoice + fee reserve; input fees come on top; a legacy standalone home \
                 drains with `maxplayer trade --home <legacy> send"
            )
        ),
        Command::Receive { .. } => anyhow::bail!(
            "{}",
            moved("receive", "maxplayer wallet receive --token-file <file>")
        ),
        _ => {}
    }
    // A zero amount can never succeed: refuse it before any cap check, home, lock or mint.
    let zero = match &cli.command {
        Command::Send {
            amount: Some(amount),
            ..
        } => (*amount == 0).then_some("--amount"),
        Command::List { give, want, .. } => (*give == 0)
            .then_some("--give")
            .or((*want == 0).then_some("--want")),
        Command::Take { max_give, .. } => (*max_give == 0).then_some("--max-give"),
        _ => None,
    };
    if let Some(flag) = zero {
        anyhow::bail!("amount must be at least 1 sat ({flag} 0)");
    }
    crate::transport::configure_mint_relays(&cli.mint_relay)?;
    // Argument-only mint fence (no http:// mints in a user build): before any home is resolved,
    // created or locked, any hand-off, and any mint or relay is contacted.
    let arg_mints: Vec<&String> = match &cli.command {
        Command::Preflight { mint }
        | Command::Balance { mint }
        | Command::Send {
            mint: Some(mint), ..
        } => vec![mint],
        Command::List {
            give_mint,
            want_mint,
            ..
        } => vec![give_mint, want_mint],
        _ => vec![],
    };
    for mint in arg_mints {
        Asset::from_cli(mint)?.fence()?;
    }
    // Refusals that need no write (mixed or legacy home, a mint this home may not use) come first,
    // then the home is bootstrapped: a refused command creates nothing.
    let (resolved, bootstrap) = check_home(cli.home.as_deref(), &cli.command)?;
    if bootstrap {
        bootstrap_home(&resolved)?;
    }
    let home = &resolved;
    // A maxplayer home moves money only at its own mints (default, extra_mints, the seller's
    // accepted_mints) that pass the real-mint fence.
    if let Command::Send {
        mint: Some(mint), ..
    } = &cli.command
    {
        crate::wallet::check_mint(home, &Asset::from_cli(mint)?.mint_url)?;
    }
    // Mint requests sign as this home's key: the default mint relay only admits registered keys.
    // A read-only command never creates a key (`discover` on an empty or legacy home).
    crate::transport::configure_mint_identity(home, is_read_only(&cli.command));
    // Read-only observations remain available while serve owns the writer lock.
    if let Command::Balance { ref mint } = cli.command {
        let asset = Asset::from_cli(mint)?;
        println!(
            "{}",
            serde_json::json!({"mint":asset.mint_url,"unit":"sat","balance":crate::wallet::read_balance(home, &asset.mint_url)?})
        );
        return Ok(());
    }
    if let Command::Status { json, check_sends } = cli.command {
        if json {
            println!("{}", crate::wallet::read_status(home)?);
        } else {
            println!("{}", crate::report::render(home)?);
        }
        if check_sends {
            crate::send::check_redeemed(home).await?;
        }
        return Ok(());
    }
    // Preflight only talks to the mint: no home, lock or journal (a journal created here would turn
    // an empty --home into a legacy-looking directory).
    if let Command::Preflight { ref mint } = cli.command {
        return preflight(&Asset::from_cli(mint)?.mint_url).await;
    }
    // Without flags, a maxplayer home's `[trade] relays` / `mint_relays` (what the buyer daemon or
    // seller hosting serve uses, so hand-offs match), else the built-in defaults.
    let (configured_relays, configured_mint_relays) = configured_relays(home);
    if cli.mint_relay.is_empty() && !configured_mint_relays.is_empty() {
        crate::transport::configure_mint_relays(&configured_mint_relays)
            .context("[trade] mint_relays in config.toml")?;
    }
    let relays: Vec<String> = if !cli.relay.is_empty() {
        cli.relay.clone()
    } else if !configured_relays.is_empty() {
        configured_relays
    } else {
        crate::market::DEFAULT_RELAYS
            .iter()
            .map(|s| s.to_string())
            .collect()
    };
    // Lock-free, journal-free commands: they only read relays, mints and SQLite snapshots.
    match &cli.command {
        Command::Discover {
            give_mint,
            want_mint,
        } => {
            return lockfree::discover(&relays, give_mint.as_deref(), want_mint.as_deref()).await;
        }
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
    // A partial send (no --reclaim) can never run: say what is missing before any hand-off or lock.
    if let Command::Send {
        mint,
        amount,
        out,
        reclaim: None,
        ..
    } = &cli.command
    {
        mint.as_ref().context("mint required")?;
        amount.as_ref().context("--amount required")?;
        out.as_ref().context("--out required")?;
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
        } => Some(crate::serve::Command::List {
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
        } => Some(crate::serve::Command::Take {
            lot: lot.clone(),
            max_give: *max_give,
            min_receive: *min_receive,
            max_fees: *max_fees,
        }),
        Command::Cancel { lot } => Some(crate::serve::Command::Cancel { lot: lot.clone() }),
        _ => None,
    }
    .map(|command| crate::serve::Request {
        v: crate::serve::PROTOCOL_V,
        relays: relays.clone(),
        mint_relays: crate::transport::mint_relays(),
        command,
    });
    if let Some(request) = &handoff {
        if home.is_dir() {
            if let Some(code) = crate::serve::forward(home, request).await? {
                return forwarded(code);
            }
        }
    }
    if !crate::wallet::is_maxplayer_home(home) {
        fs::create_dir_all(home)?;
        fs::set_permissions(home, fs::Permissions::from_mode(0o700))?;
    }
    let state = crate::wallet::ensure_state_dir(home)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(state.join("owner.lock"))?;
    if let Err(busy) = lock.try_lock_exclusive() {
        // Another process owns the home: a serve (possibly still binding its socket) gets the
        // request; a short-lived writer such as recover is waited out. Never two writers.
        let Some(request) = &handoff else {
            // One serve per home: a buyer daemon or seller hosting serve here (or another
            // standalone serve) holds owner.lock and answers on the socket. Say so; start nothing.
            if matches!(cli.command, Command::Serve) && crate::serve::answering(home).await {
                anyhow::bail!("{}", already_served(home));
            }
            return Err(busy).context("home is already in use");
        };
        let end = tokio::time::Instant::now() + crate::serve::CONNECT_WAIT;
        loop {
            if let Some(code) = crate::serve::forward(home, request).await? {
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
    // main retains this file until shutdown::finished has removed our socket.
    *owner_lock = Some(lock);
    #[cfg(feature = "lab")]
    let _trace = lab_trace::Writer::start();
    // Bind while holding owner.lock (an existing socket file is stale) and before the startup
    // recovery pass, so hand-offs queue instead of failing while serve recovers.
    let mut jobs = if matches!(cli.command, Command::Serve) {
        crate::shutdown::start(home, &relays)?;
        Some(crate::serve::bind(home, relays.clone())?)
    } else {
        None
    };
    // `recover` is one bounded pass: it waits for mint pacing instead of skipping an item.
    crate::backoff::set_wait_for_pacing(matches!(cli.command, Command::Recover));
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
        let pending = crate::money::recover_explicit(home, &money_journal).await?;
        return coordinator::recovery_status(&money_journal, pending).await;
    }
    if !matches!(&cli.command, Command::Balance { .. } | Command::Send { .. }) {
        let keys = crate::wallet::identity(home)?;
        let j = Journal::open(home).await?;
        let mut m = Market::connect(keys, &relays).await?;
        // serve starts the same recovery as its first scheduled units (`sched`), before any
        // message or request it admits, so one slow mint never holds the start either.
        if !matches!(cli.command, Command::Recover | Command::Serve) {
            crate::shutdown::bounded(coordinator::recover(home, &j, &m)).await?;
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
        return crate::send::command(
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
    unreachable!()
}

/// The refusal for a money command that moved to `maxplayer wallet`.
fn moved(command: &str, instead: &str) -> String {
    format!(
        "`maxplayer trade {command}` moved to `maxplayer wallet`: use `{instead}`; nothing was done"
    )
}

/// The home a command runs against. `--home` (else the maxplayer home: $MAXPLAYER_HOME, else
/// ~/.maxplayer) names either a maxplayer home (`key`/`config.toml`: shared mode) or a legacy
/// standalone trading home (`wallet.seed`/`trade.key`/root `trade.sqlite`: drain-only, warned).
/// A directory that is neither: read-only commands create nothing there; every other command
/// bootstraps it into a maxplayer home first.
/// Commands that run without a home and must create nothing in one (no home, key or journal).
fn is_read_only(command: &Command) -> bool {
    matches!(
        command,
        Command::Balance { .. }
            | Command::Status { .. }
            | Command::Discover { .. }
            | Command::Preflight { .. }
            | Command::List { dry_run: true, .. }
            | Command::Take { dry_run: true, .. }
    )
}

/// Returns the home and whether it still needs [`bootstrap_home`]; creates nothing.
fn check_home(arg: Option<&Path>, command: &Command) -> Result<(PathBuf, bool)> {
    use crate::wallet;
    let home = match arg {
        Some(path) => path.to_path_buf(),
        None => maxplayer_core::home::default_home_dir().map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    // A maxplayer home with legacy files next to it: neither layout can be used safely (the
    // legacy money sits in per-mint databases under wallet.seed, the home's in
    // wallet/cdk-wallet.sqlite). Refuse every command before anything is created or read.
    if wallet::is_mixed_dir(&home) {
        anyhow::bail!("{}", wallet::mixed_refusal(&home));
    }
    if wallet::is_legacy_dir(&home) {
        eprintln!("{}", wallet::legacy_warning(&home));
        if matches!(command, Command::List { .. } | Command::Take { .. }) {
            anyhow::bail!("{}", wallet::legacy_refusal(&home));
        }
        return Ok((home, false));
    }
    // Read-only commands never create anything: no home, key, config or journal ("SAFETY: discover
    // created a home", tests/ux.rs). They run against whatever is there.
    if is_read_only(command) {
        return Ok((home, false));
    }
    // A mint this home may not use is refused before bootstrap creates anything (the config is
    // resolved read-only; an absent config.toml means the defaults bootstrap would write).
    // A maxplayer home sends with `maxplayer wallet send`; trade's send drains legacy homes only.
    if let Command::Send { reclaim: None, .. } = command {
        anyhow::bail!(
            "{}",
            moved(
                "send",
                "maxplayer wallet send <amount> --mint <url> --out <file> (a legacy standalone \
                 home still drains with `maxplayer trade --home <legacy> send`; `send --reclaim \
                 <id>` still takes back a token trade sent)"
            )
        );
    }
    if let Command::List {
        give_mint,
        want_mint,
        ..
    } = command
    {
        for mint in [give_mint, want_mint] {
            wallet::check_shared_mint(&home, &crate::Asset::from_cli(mint)?.mint_url)?;
        }
    }
    Ok((home, true))
}

/// Every command that is not read-only can move money, so it gets a complete maxplayer home first,
/// exactly as the rest of `maxplayer` does (`home::bootstrap`: 0700 dir, config.toml, a 0600 key,
/// wallet/). Idempotent on an existing home. Bootstrap does not register the key with the relay.
fn bootstrap_home(home: &Path) -> Result<()> {
    let boot = maxplayer_core::home::bootstrap(home).map_err(|e| anyhow::anyhow!("{e}"))?;
    if boot.key_created {
        eprintln!(
            "created a new key for maxplayer home {h}; link it with `maxplayer account link --home \
             {h}` so relay.maxplayer.ai accepts its listings and mint requests",
            h = home.display()
        );
    }
    Ok(())
}

/// `[trade] relays` and `mint_relays` of a maxplayer home, read-only (both empty when unset, when
/// the config cannot be read, or for any other directory).
fn configured_relays(home: &Path) -> (Vec<String>, Vec<String>) {
    if !crate::wallet::is_maxplayer_home(home) {
        return (vec![], vec![]);
    }
    match maxplayer_core::home::resolve_config_read_only(home) {
        Ok(config) => (config.trade.relays, config.trade.mint_relays),
        Err(_) => (vec![], vec![]),
    }
}

/// The refusal of a standalone `serve` on a home whose serve already runs (exit 1).
pub fn already_served(home: &Path) -> String {
    let state = crate::wallet::state_dir(home);
    format!(
        "home is already in use: trade serve is already running on {h}: a buyer daemon, a seller or another `maxplayer \
         trade serve` holds {lock} and answers on {sock}, and list/take/cancel hand off to it. \
         Nothing was started. To run serve on its own instead, set `[trade] serve = false` in \
         {config} and restart that daemon",
        h = home.display(),
        lock = state.join("owner.lock").display(),
        sock = state.join(crate::serve::SOCKET).display(),
        config = home.join("config.toml").display(),
    )
}

fn forwarded(code: u8) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(crate::serve::Forwarded(code).into())
    }
}

/// Commands that never take owner.lock, never open the journal and never create files.
mod lockfree {
    use crate::{Asset, dryrun, market::Market, parse_lot, wallet};
    use anyhow::{Context, Result, bail, ensure};
    use std::path::Path;
    /// Discovery needs no trade key and must not create one: a read-only market that never
    /// signs in (lots and statuses are public reads).
    async fn market(relays: &[String]) -> Result<Market> {
        Market::connect_read_only(relays).await
    }
    pub async fn discover(relays: &[String], give: Option<&str>, want: Option<&str>) -> Result<()> {
        // Refuse a malformed filter before any relay traffic.
        let give = give.map(Asset::from_cli).transpose()?;
        let want = want.map(Asset::from_cli).transpose()?;
        ensure!(
            give.is_none() || give != want,
            "same-asset swap: --give-mint and --want-mint are the same mint"
        );
        let m = market(relays).await?;
        let lots = m.discover(None).await?;
        let now = crate::coordinator::now();
        let parsed = lots
            .into_iter()
            .map(|e| Ok((parse_lot(&e, now)?, e)))
            .collect::<Result<Vec<_>>>()?;
        let (terms, lots): (Vec<_>, Vec<_>) =
            crate::select_lots(parsed, give.as_ref(), want.as_ref())
                .into_iter()
                .unzip();
        let fees = dryrun::discover_fees(&terms).await;
        let listings = lots.iter().zip(fees).map(|(e, fees)| {
            Ok(serde_json::json!({"lot_id":e.id,"maker":e.pubkey,"terms":serde_json::from_str::<serde_json::Value>(&e.content)?,"expected_fees":fees}))
        }).collect::<Result<Vec<_>>>()?;
        println!("{}", serde_json::json!({"status":"ok","listings":listings}));
        Ok(())
    }
    async fn preflight(mints: [&str; 2]) -> Result<[dryrun::Keysets; 2]> {
        crate::diagnostics::notice();
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
        let l = parse_lot(&e, crate::coordinator::now())?;
        // Self-trade check without creating a trade key.
        let own = wallet::existing_identity(home).ok().flatten();
        for mint in [&l.give.asset.mint_url, &l.want.asset.mint_url] {
            wallet::check_mint(home, mint)?;
        }
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
async fn doctor(
    mint: &str,
    relay: &str,
    json_output: bool,
    seconds: u64,
    home: Option<&Path>,
) -> Result<()> {
    match doctor_inner(mint, relay, json_output, seconds, home).await {
        Err(error) if !error.is::<DoctorExit>() => {
            // Caller text stays out of the message (a relay URL may hold a token); a scheme
            // refusal is a fixed string, so name it.
            let text = format!("{error:#}");
            let reason = [
                crate::transport::HTTP_MINT_REFUSED,
                crate::transport::WS_MINT_RELAY_REFUSED,
            ]
            .into_iter()
            .find(|r| text.contains(r));
            let message = format!(
                "Diagnostic not started: check the mint identifier, the explicit relay URL and \
                 the --home key{}",
                reason.map(|r| format!(" ({r})")).unwrap_or_default()
            );
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"exit":2,"observation":{"reachability":"not_checked"},"error":message})
                );
            } else {
                eprintln!("{message}");
            }
            Err(DoctorExit(2).into())
        }
        result => result,
    }
}
async fn doctor_inner(
    mint: &str,
    relay: &str,
    json_output: bool,
    seconds: u64,
    home: Option<&Path>,
) -> Result<()> {
    use maxplayer_core::nostr_mint::NostrMintConnector;
    use nostr_sdk::prelude::ToBech32;
    use std::time::Duration;
    let asset = Asset::from_cli(mint)?;
    asset.fence()?;
    ensure!(
        crate::transport::is_nostr(&asset.mint_url),
        "doctor requires a nostr:// mint and one explicit --mint-relay"
    );
    crate::transport::configure_mint_relays(&[relay.to_owned()])?;
    let window = Duration::from_secs(seconds);
    let connector = NostrMintConnector::new(&asset.mint_url.parse()?, vec![relay.to_owned()])?
        // Resend the identical event on the production cadence, so a first delivery lost on
        // the way is retried exactly as list/take admission would retry it.
        .with_timing(
            window,
            maxplayer_core::nostr_mint::DEFAULT_RESEND_EVERY,
            Duration::from_secs(5).min(window),
        );
    // A sign-in-required relay only admits registered keys (`maxplayer account link
    // --trade-home`), so sign as the home's trade key when it has one. Never create it here.
    let identity = match home {
        Some(home) => crate::wallet::existing_identity(home).ok().flatten(),
        None => None,
    };
    let signer = match &identity {
        Some(keys) => {
            let npub = keys.public_key().to_bech32()?;
            let source = if home.is_some_and(crate::wallet::is_maxplayer_home) {
                "home key"
            } else {
                "home trade.key"
            };
            eprintln!(
                "Signing as this home's {} ({npub})",
                source.trim_start_matches("home ")
            );
            serde_json::json!({"source":source,"npub":npub})
        }
        None => {
            eprintln!(
                "Signing with a throwaway key: a sign-in-required relay (relay.maxplayer.ai) \
                 refuses it. Pass --home <maxplayer home> once its key is linked."
            );
            serde_json::json!({"source":"ephemeral"})
        }
    };
    let connector = match identity {
        Some(keys) => connector.with_identity(keys),
        None => connector,
    };
    eprintln!(
        "Waiting for mint info (read-only budget {seconds}s); no wallet, journal or trade is opened"
    );
    let (result, evidence) = connector
        .call_observed("info", serde_json::Value::Null)
        .await;
    let compatibility = result.as_ref().ok().map(crate::compatibility::assess);
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
        "slow_warning_threshold_ms":5000,"exit":exit,"signer":signer,
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
