//! Registry identity CLI; dry-run never contacts HTTP or Nostr.
use std::io::Write;

fn usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage: maxplayer address <claim <name>|show|release> [--home <path>] [--dry-run]\nClaim your registry identity (NIP-05); no Lightning address is published."
    );
}
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if crate::cli::is_help_request(args) {
        usage(out);
        return 0;
    }
    match execute(args) {
        Ok(value) => {
            let _ = writeln!(out, "{value}");
            0
        }
        Err(e) => {
            let _ = writeln!(err, "{e}");
            2
        }
    }
}
#[cfg(not(feature = "wallet"))]
fn execute(_: &[String]) -> Result<String, String> {
    Err("address requires the wallet feature".into())
}
#[cfg(feature = "wallet")]
fn execute(args: &[String]) -> Result<String, String> {
    use maxplayer_core::{
        address::{Registry, normalize_name},
        home, profile,
    };
    let command = args
        .first()
        .map(String::as_str)
        .ok_or("expected claim, show or release")?;
    let (name, mut i) = match command {
        "claim" => (
            Some(normalize_name(args.get(1).ok_or("claim requires a name")?)?),
            2,
        ),
        "show" | "release" => (None, 1),
        _ => return Err("expected claim, show or release".into()),
    };
    let mut root = None;
    let mut dry_run = false;
    while i < args.len() {
        match args[i].as_str() {
            "--home" => {
                i += 1;
                root = Some(std::path::PathBuf::from(
                    args.get(i).ok_or("--home requires a path")?,
                ));
            }
            "--dry-run" => dry_run = true,
            _ => return Err(format!("unknown argument {}", args[i])),
        }
        i += 1;
    }
    let root = root
        .map(Ok)
        .unwrap_or_else(home::default_home_dir)
        .map_err(|e| e.to_string())?;
    if !home::is_initialized(&root) {
        return Err(format!(
            "address refused: no initialized home at {}",
            root.display()
        ));
    }
    let home = home::bootstrap(&root).map_err(|e| e.to_string())?;
    let registry = Registry::for_home(&home)?;
    let pubkey = home::public_key_hex(&home).map_err(|e| e.to_string())?;
    if dry_run {
        return Ok(serde_json::json!({"dry_run":true,"home":root,"pubkey":pubkey,"action":command,"handle":name,"domain":registry.domain}).to_string());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        if command == "show" {
            let names = registry.lookup(&[pubkey.clone()]).await?;
            return Ok(names.get(&pubkey).map(|n| format!("{n} ({n}@{})", registry.domain)).unwrap_or_else(|| "no handle".into()));
        }
        let secret = home::read_secret_key_hex(&home).map_err(|e| e.to_string())?;
        let keys = nostr_sdk::Keys::parse(&secret).map_err(|_| "invalid seat key")?;
        let address = if let Some(name) = name {
            let name = registry.claim(&keys, &name).await?;
            Some(format!("{name}@{}", registry.domain))
        } else { registry.release(&keys).await?; None };
        profile::set_nip05_async(&home, address.as_deref()).await.map_err(|e| format!("registry {command} succeeded, but kind-0 update failed: {e}. Retry the same command to repair advertisement."))?;
        Ok(address.unwrap_or_else(|| "handle released".into()))
    })
}

#[cfg(all(test, feature = "wallet"))]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn address_help_and_invalid_input_need_no_home() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(run(&args(&["--help"]), &mut out, &mut err), 0);
        assert!(String::from_utf8(out).unwrap().contains("--dry-run"));
        assert!(
            execute(&args(&["claim", "a+b"]))
                .unwrap_err()
                .contains("invalid handle")
        );
    }
    #[test]
    fn address_dry_run_reads_config_and_never_initializes_a_missing_home() {
        let dir = std::env::temp_dir().join(format!(
            "address-cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("seat");
        let path = root.to_str().unwrap();
        assert!(execute(&args(&["claim", "alice", "--home", path, "--dry-run"])).is_err());
        assert!(!root.exists());
        let mut home = maxplayer_core::home::bootstrap(&root).unwrap();
        maxplayer_core::home::save_config(&mut home, |c| {
            c.relay_url = "ws://127.0.0.1:1".into();
            c.nip05_domain = "handles.example.org".into();
        })
        .unwrap();
        for command in [vec!["claim", "ALICE"], vec!["show"], vec!["release"]] {
            let mut a = command;
            a.extend(["--home", path, "--dry-run"]);
            let result: serde_json::Value =
                serde_json::from_str(&execute(&args(&a)).unwrap()).unwrap();
            assert_eq!(result["domain"], "handles.example.org");
            assert_eq!(result["dry_run"], true);
            assert_eq!(result["pubkey"].as_str().unwrap().len(), 64);
            if result["action"] == "claim" {
                assert_eq!(result["handle"], "alice");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
