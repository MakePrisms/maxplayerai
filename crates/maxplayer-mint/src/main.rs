use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use maxplayer_mint::home::{MintHome, mint_url};
use maxplayer_mint::{backend, issue, server};

const USAGE: &str = "usage: maxplayer-mint <init [--name <name>]|name <name>|run|issue [amount]>
  init            create <home>/mint/ and print the nostr:// mint URL
  name <name>     set the name wallets show next to the mint URL (restart `run` to serve it)
  run             serve the mint over its relays until interrupted
  issue [amount]  finish any interrupted issue, then issue <amount> sat into a token file
<home> is $MAXPLAYER_HOME, or ~/.maxplayer";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
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
                "Restart `maxplayer-mint run` to serve it (systemctl --user restart maxplayer-mint)."
            );
            println!(
                "Wallets show it after they next talk to the mint, or re-run `maxplayer wallet mints add <url>`."
            );
            Ok(())
        }
        ["run"] => run(&home).await,
        ["issue"] => issue_cmd(&home, None).await,
        ["issue", amount] => issue_cmd(&home, Some(amount.to_string())).await,
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
    println!(
        "BACK UP {} NOW, and keep exactly one live copy.",
        home.dir().display()
    );
    println!("Losing nostr.key or seed makes every credit this mint issued worthless.");
    println!(
        "An OLD or MISSING mint.sqlite can let spent credits be spent again: never delete it."
    );
    println!("Stop `maxplayer-mint run` before you copy the directory.");
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

async fn run(home: &MintHome) -> Result<()> {
    let secrets = home.load().with_context(|| {
        format!(
            "load {} (run `maxplayer-mint init` first)",
            home.dir().display()
        )
    })?;
    // Before `open`: its startup saga recovery must not run under a listener that is serving.
    let _lock = home.lock_run()?;
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
    tokio::select! {
        result = server.serve() => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
