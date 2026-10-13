use maxplayer_chat::{DEFAULT_RELAY, Home, Result};
use nostr_sdk::prelude::ToBech32;
use std::path::PathBuf;
const HELP: &str = "maxplayer-chat [--home DIR] [--relay URL] whoami | peer add <npub|hex> --name <label> | peer remove <peer> | peer list | watch [--max-failures N] [--notify <cmd> args…] | status [--json] | inbox [--json] | send <peer> <text> [--json] | log <peer> [--json]";
#[tokio::main(flavor = "current_thread")]
async fn main() {
    match run().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!(
                "maxplayer-chat: {}",
                maxplayer_chat::display_text(&e.to_string())
            );
            std::process::exit(1);
        }
    }
}
async fn run() -> Result<i32> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        println!("{HELP}");
        return Ok(0);
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
    let mut max_failures = 10;
    if args.first().map(String::as_str) == Some("watch")
        && args.get(1).map(String::as_str) == Some("--max-failures")
    {
        max_failures = args
            .get(2)
            .ok_or("missing --max-failures value")?
            .parse::<u32>()?;
        args.drain(1..3);
    }
    let argv: Vec<_> = args.iter().map(String::as_str).collect();
    let valid = match argv.as_slice() {
        ["whoami"]
        | ["peer", "add", _, "--name", _]
        | ["peer", "remove", _]
        | ["peer", "list"]
        | ["status"]
        | ["status", "--json"]
        | ["send", _, _, "--json"]
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
    if argv.first() == Some(&"status") {
        let status = maxplayer_chat::read_status(&root);
        let healthy = status.as_ref().is_ok_and(|s| s.state == "healthy");
        let value = match status {
            Ok(status) => serde_json::to_value(status)?,
            Err(e) => serde_json::json!({"state":"unhealthy", "last_error": e.to_string()}),
        };
        if args.len() == 2 {
            println!("{}", maxplayer_chat::display_json(value));
        } else {
            println!(
                "{}: {}",
                value["state"].as_str().unwrap_or("unhealthy"),
                maxplayer_chat::display_text(
                    value["last_error"]
                        .as_str()
                        .unwrap_or("authenticated subscription; status fresh")
                )
            );
        }
        return Ok(if healthy { 0 } else { 1 });
    }
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
                println!(
                    "{} {}",
                    peer.pubkey,
                    maxplayer_chat::display_text(&peer.name)
                )
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
        ["send", peer, text] | ["send", peer, text, "--json"] => {
            let outcome = home.send_outcome(&relay, peer, text).await?;
            if args.len() == 4 {
                println!(
                    "{}",
                    maxplayer_chat::display_json(serde_json::to_value(&outcome)?)
                );
            } else if outcome.exit_code() == 0 {
                println!("{}", outcome.message());
            } else {
                eprintln!("{}", outcome.message());
            }
            return Ok(outcome.exit_code());
        }
        ["watch"] => {
            maxplayer_chat::watch_with_options(
                &home,
                &relay,
                vec![],
                maxplayer_chat::WatchOptions {
                    max_failures,
                    ..Default::default()
                },
            )
            .await?
        }
        ["watch", "--notify", command @ ..] if !command.is_empty() => {
            maxplayer_chat::watch_with_options(
                &home,
                &relay,
                command.iter().map(|s| s.to_string()).collect(),
                maxplayer_chat::WatchOptions {
                    max_failures,
                    ..Default::default()
                },
            )
            .await?
        }
        _ => return Err(HELP.into()),
    }
    Ok(0)
}
