use maxplayer_chat::{DEFAULT_RELAY, Home, Result};
use nostr_sdk::prelude::ToBech32;
use std::path::PathBuf;
const HELP: &str = "maxplayer-chat [--home DIR] [--relay URL] whoami | peer add <npub|hex> --name <label> | peer remove <peer> | peer list | watch [--notify <cmd> args…] | inbox [--json] | send <peer> <text> | log <peer> [--json]";
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("maxplayer-chat: {e}");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        println!("{HELP}");
        return Ok(());
    }
    let mut root: Option<PathBuf> = None;
    let mut relay = DEFAULT_RELAY.to_string();
    let i = 0;
    while i < args.len() {
        if args[i] == "--notify" {
            break;
        }
        if args[i] == "--home" || args[i] == "--relay" {
            let flag = args.remove(i);
            if i == args.len() {
                return Err("missing global flag value".into());
            }
            let value = args.remove(i);
            if flag == "--home" {
                root = Some(value.into())
            } else {
                relay = value
            }
        } else {
            break;
        }
    }
    let argv: Vec<_> = args.iter().map(String::as_str).collect();
    let valid = match argv.as_slice() {
        ["whoami"]
        | ["peer", "add", _, "--name", _]
        | ["peer", "remove", _]
        | ["peer", "list"]
        | ["inbox"]
        | ["inbox", "--json"]
        | ["log", _]
        | ["log", _, "--json"]
        | ["send", _, _]
        | ["watch"] => true,
        ["watch", "--notify", command @ ..] => !command.is_empty(),
        _ => false,
    };
    if !valid {
        return Err(HELP.into());
    }
    let root = match root {
        Some(root) => root,
        None => maxplayer_core::home::default_home_dir()?,
    };
    let home = Home::open(root)?;
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["whoami"] => println!(
            "{}\n{}",
            home.keys.public_key().to_bech32()?,
            home.keys.public_key().to_hex()
        ),
        ["peer", "add", key, "--name", name] => home.add_peer(key, name)?,
        ["peer", "remove", peer] => home.remove_peer(peer)?,
        ["peer", "list"] => {
            for peer in home.peers()? {
                println!("{} {}", peer.pubkey, peer.name)
            }
        }
        ["inbox"] | ["inbox", "--json"] => {
            if !home.watch_running() {
                eprintln!("watch is not running; new messages are not being received");
            }
            home.print_inbox(args.len() == 2, std::io::stdout().lock())?;
        }
        ["log", peer] | ["log", peer, "--json"] => {
            home.print_log(peer, args.len() == 3, std::io::stdout().lock())?
        }
        ["send", peer, text] => println!("{}", home.send(&relay, peer, text).await?),
        ["watch"] => maxplayer_chat::watch(&home, &relay, vec![]).await?,
        ["watch", "--notify", command @ ..] if !command.is_empty() => {
            maxplayer_chat::watch(
                &home,
                &relay,
                command.iter().map(|s| s.to_string()).collect(),
            )
            .await?
        }
        _ => return Err(HELP.into()),
    }
    Ok(())
}
