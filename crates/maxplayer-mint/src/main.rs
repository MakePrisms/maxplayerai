use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use maxplayer_mint::home::{MintHome, mint_url};
use maxplayer_mint::{backend, backup, issue, server};

const USAGE: &str = "usage: maxplayer-mint <init [--name <name>]|name <name>|serve|issue [amount]|restore [--from <dir>]>
  init            create <home>/mint/ and print the nostr:// mint URL
  name <name>     set the name wallets show next to the mint URL (restart `serve` to serve it)
  serve           serve the mint over its relays until interrupted; backs itself up automatically
  issue [amount]  finish any interrupted issue, then issue <amount> sat into a token file
  restore         with `serve` stopped, put back the NEWEST backup as mint.sqlite (from backup_dir,
                  or --from <dir>; recreates <home>/mint/ from the backup if it is gone)
  version         print `maxplayer-mint <version> (<commit>)` (also --version); needs no home
  run             deprecated alias for `serve` (still works)
<home> is $MAXPLAYER_HOME, or ~/.maxplayer";

/// The one stderr line the deprecated `run` prints before serving anyway. Same shape as the
/// `maxplayer` daemons' deprecation line, printed right after [`start_line`], so it never becomes
/// the first line of the mint's output.
const RUN_DEPRECATED: &str = "maxplayer-mint: `maxplayer-mint run` is deprecated and still works; use `maxplayer-mint serve`";

/// The line both `--version` and `version` print. Same grammar as `maxplayer --version`
/// (`<name> <version> (<40-hex commit>)`), because `scripts/verify-release-version.sh` holds every
/// released artifact to it; the mint ships in lockstep with `maxplayer` at the same version.
fn version_line() -> String {
    format!(
        "maxplayer-mint {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("MINT_SIDECAR_BUILD")
    )
}

/// The FIRST stderr line of `serve` (and of its deprecated alias `run`): `<version_line> — mint
/// starting, dir <home>/mint`. An operator who starts `maxplayer-mint serve` by bare name with an
/// older binary first on `PATH` reads from the log which build is serving. `starting`, not a verb,
/// like the `maxplayer` daemons' start line.
fn start_line(home: &MintHome) -> String {
    format!(
        "{} — mint starting, dir {}",
        version_line(),
        home.dir().display()
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Before the home is resolved: a version query must answer on a machine with no HOME and no
    // mint (the release and install checks run it in bare containers).
    if matches!(args.as_slice(), ["--version"] | ["version"]) {
        println!("{}", version_line());
        return Ok(());
    }
    let home = MintHome::at(&home_dir()?);
    match args.as_slice() {
        ["init"] => init(&home, None).await,
        ["init", "--name", name] => init(&home, Some(name.to_string())).await,
        ["name", name] => {
            home.set_name(name)?;
            println!(
                "Mint name set to {name:?} in {}",
                home.dir().join("mint.toml").display()
            );
            println!(
                "Restart `maxplayer-mint serve` to serve it (systemctl --user restart maxplayer-mint)."
            );
            println!(
                "Wallets show it after they next talk to the mint, or re-run `maxplayer wallet mints add <url>`."
            );
            Ok(())
        }
        ["serve"] => serve(&home, false).await,
        // Deprecated alias: serves exactly as `serve` does, after one stderr line naming it.
        ["run"] => serve(&home, true).await,
        ["issue"] => issue_cmd(&home, None).await,
        ["issue", amount] => issue_cmd(&home, Some(amount.to_string())).await,
        ["restore"] => restore(&home, None),
        ["restore", "--from", dir] => restore(&home, Some(PathBuf::from(dir))),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn home_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("MAXPLAYER_HOME") {
        return Ok(PathBuf::from(home));
    }
    let Some(user) = std::env::var_os("HOME") else {
        bail!("set MAXPLAYER_HOME or HOME");
    };
    Ok(PathBuf::from(user).join(".maxplayer"))
}

async fn init(home: &MintHome, name: Option<String>) -> Result<()> {
    let keys = home.init_named(name)?;
    let url = mint_url(&keys)?;
    let secrets = home.load()?;
    let name = secrets.config.effective_name(&secrets.keys);
    // Creates mint.sqlite and the first keyset.
    backend::open_named(&home.db_path(), &secrets.seed, &url, &name).await?;
    println!("Created {}", home.dir().display());
    println!("Mint URL: {url}");
    println!("Mint name: {name}");
    if secrets.config.name.is_none() {
        println!(
            "Wallets show this name next to the mint URL. Give it one people know you by (e.g. your seller name):  maxplayer-mint name <name>"
        );
    }
    println!("Add it to accepted_mints in config.toml:  \"{url}\"");
    println!("Then let your wallet spend what it earns there:  maxplayer wallet mints add {url}");
    println!();
    let backups = secrets.config.backup_dir_in(home.dir());
    println!(
        "Backups are automatic: `maxplayer-mint serve` keeps {} current (key, seed, mint.toml and the newest database snapshots).",
        backups.display()
    );
    println!(
        "ONE-TIME step: have that directory copied off this machine (or set backup_dir in mint.toml to off-box storage). Losing nostr.key or seed makes every credit this mint issued worthless."
    );
    println!(
        "Never delete mint.sqlite or run two copies. To restore, stop `serve` and use `maxplayer-mint restore` (it takes the newest backup only)."
    );
    Ok(())
}

async fn issue_cmd(home: &MintHome, amount: Option<String>) -> Result<()> {
    let amount = amount
        .map(|a| {
            a.parse::<u64>()
                .with_context(|| format!("amount {a:?} is not a whole number of sat"))
        })
        .transpose()?;
    let secrets = home.load().with_context(|| {
        format!(
            "load {} (run `maxplayer-mint init` first)",
            home.dir().display()
        )
    })?;
    let url = mint_url(&secrets.keys)?;
    let mint = backend::build(&home.db_path(), &secrets.seed, &url).await?;
    for done in issue::reconcile(&mint, home.dir(), &url).await? {
        println!(
            "Finished interrupted issue {}: {} sat in {}",
            done.id,
            done.amount,
            done.file.display()
        );
    }
    if let Some(amount) = amount {
        let done = issue::issue(&mint, home.dir(), &url, amount).await?;
        println!("Issued {} sat: {}", done.amount, done.file.display());
        println!("That file IS the credits: anyone holding it can spend them.");
    }
    Ok(())
}

async fn serve(home: &MintHome, deprecated_run: bool) -> Result<()> {
    // Before anything that can fail, so even a refused start names its binary; `run`'s deprecation
    // line comes right after it and never becomes the first line of the mint's output.
    eprintln!("{}", start_line(home));
    if deprecated_run {
        eprintln!("{RUN_DEPRECATED}");
    }
    let secrets = home.load().with_context(|| {
        format!(
            "load {} (run `maxplayer-mint init` first)",
            home.dir().display()
        )
    })?;
    // Before `open`: its startup saga recovery must not run under a listener that is serving.
    let _lock = home.lock_run()?;
    let backups = secrets.config.backup_dir_in(home.dir());
    // Before `open`, which would create a fresh, empty mint.sqlite over a missing one.
    backup::check_live(home.dir(), &home.db_path(), &backups)?;
    let url = mint_url(&secrets.keys)?;
    let name = secrets.config.effective_name(&secrets.keys);
    let mint = backend::open_named(&home.db_path(), &secrets.seed, &url, &name).await?;
    if secrets.config.name.is_none() {
        eprintln!(
            "maxplayer-mint: no name set, serving the default {name:?}; set one with `maxplayer-mint name <name>`"
        );
    }
    let relays = secrets.config.effective_relays();
    let server =
        server::Server::connect(mint, secrets.keys, &relays, secrets.config.rate_limit).await?;
    eprintln!("maxplayer-mint: serving {url} ({name:?}) on {relays:?}");
    eprintln!("maxplayer-mint: backing up automatically into {}", backups.display());
    let backups = backup::start(
        home.dir(),
        &home.db_path(),
        &backups,
        backup::Schedule::default(),
    );
    let result = tokio::select! {
        result = server.serve() => result,
        stopped = stop_signal() => stopped,
    };
    // Last snapshot after the listener stopped, so it holds every reply already sent.
    let status = backups.stop().await;
    if let Some(error) = status.last_error {
        eprintln!("maxplayer-mint: last backup attempt FAILED: {error}");
    }
    result
}

/// A clean stop: SIGINT (Ctrl-C), SIGTERM (`kill`, supervisors, containers) or SIGHUP (a closed
/// terminal or tmux session). Each still takes the last backup snapshot.
async fn stop_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("listen for SIGTERM")?;
    let mut hup = signal(SignalKind::hangup()).context("listen for SIGHUP")?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
    Ok(())
}

fn restore(home: &MintHome, from: Option<PathBuf>) -> Result<()> {
    let from = match from {
        Some(dir) => dir,
        None => home
            .config()
            .with_context(|| {
                format!(
                    "read {} (pass --from <backup dir>)",
                    home.dir().join("mint.toml").display()
                )
            })?
            .backup_dir_in(home.dir()),
    };
    let done = backup::restore(home, &from)?;
    for name in &done.copied {
        println!("Restored {name}");
    }
    if done.issued > 0 {
        println!("Restored {} issued token file(s) into issued/", done.issued);
    }
    if let Some(aside) = &done.replaced {
        println!("Moved the old mint.sqlite to {}", aside.display());
    }
    println!(
        "Restored {} (the newest backup) as {}",
        done.snapshot.display(),
        home.db_path().display()
    );
    println!("Start `maxplayer-mint serve` again.");
    Ok(())
}
