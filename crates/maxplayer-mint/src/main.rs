use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use maxplayer_mint::home::{MintHome, mint_url};
use maxplayer_mint::{backend, server};

const USAGE: &str = "usage: maxplayer-mint <init|run>
  init   create <home>/mint/ and print the nostr:// mint URL
  run    serve the mint over its relays until interrupted
<home> is $MAXPLAYER_HOME, or ~/.maxplayer";

#[tokio::main]
async fn main() -> Result<()> {
    let command = std::env::args().nth(1);
    let home = MintHome::at(&home_dir()?);
    match command.as_deref() {
        Some("init") => init(&home).await,
        Some("run") => run(&home).await,
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

async fn init(home: &MintHome) -> Result<()> {
    let keys = home.init()?;
    let url = mint_url(&keys)?;
    let secrets = home.load()?;
    // Creates mint.sqlite and the first keyset.
    backend::open(&home.db_path(), &secrets.seed, &url).await?;
    println!("Created {}", home.dir().display());
    println!("Mint URL: {url}");
    println!("Add it to accepted_mints in config.toml:  \"{url}\"");
    println!();
    println!(
        "BACK UP {} NOW, and keep exactly one live copy.",
        home.dir().display()
    );
    println!("Losing it makes every credit this mint issued worthless.");
    println!("Restoring an OLD copy can let credits that were already spent be spent again.");
    Ok(())
}

async fn run(home: &MintHome) -> Result<()> {
    let secrets = home.load().with_context(|| {
        format!(
            "load {} (run `maxplayer-mint init` first)",
            home.dir().display()
        )
    })?;
    let url = mint_url(&secrets.keys)?;
    let mint = backend::open(&home.db_path(), &secrets.seed, &url).await?;
    let relays = secrets.config.effective_relays();
    let server =
        server::Server::connect(mint, secrets.keys, &relays, secrets.config.rate_limit).await?;
    eprintln!("maxplayer-mint: serving {url} on {relays:?}");
    tokio::select! {
        result = server.serve() => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
