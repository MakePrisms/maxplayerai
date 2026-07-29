//! `maxplayer profile set` — set the optional buyer/seller kind-0 identity (name/about) and
//! publish it to the configured relay. Never required; absent profile leaves the identity as hex.
//! Never echoes the secret key.

use std::io::Write;
use std::path::PathBuf;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

struct Opts {
    name: Option<String>,
    about: Option<String>,
    home: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut name = None;
    let mut about = None;
    let mut home = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--name" => {
                idx += 1;
                name = Some(args.get(idx).ok_or("--name requires a value")?.clone());
            }
            "--about" => {
                idx += 1;
                about = Some(args.get(idx).ok_or("--about requires a value")?.clone());
            }
            "--home" => {
                idx += 1;
                home = Some(PathBuf::from(args.get(idx).ok_or("--home requires a value")?));
            }
            other => return Err(format!("unknown argument {other}")),
        }
        idx += 1;
    }
    Ok(Opts { name, about, home })
}

fn usage(err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "Usage:\n\
         \x20 maxplayer profile set [--name <name>] [--about <about>] [--home <path>]\n\
         \n\
         Publishes/replaces the kind-0 metadata event on the configured relay. Called with no\n\
         name/about = re-publish from existing config. Never echoes the secret key.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

/// Entry from `cli::run` for `maxplayer profile ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match args.first().map(String::as_str) {
        Some("set") => cmd_set(&args[1..], out, err),
        _ => {
            usage(err);
            USAGE_ERROR
        }
    }
}

#[cfg(feature = "wallet")]
fn cmd_set(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use mobee_core::home;
    use mobee_core::profile::{self, SetProfileRequest};

    let opts = match parse(args) {
        Ok(opts) => opts,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            usage(err);
            return USAGE_ERROR;
        }
    };
    let root = match opts.home {
        Some(path) => path,
        None => match home::default_home_dir() {
            Ok(path) => path,
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return RUNTIME_ERROR;
            }
        },
    };
    let mut home = match home::bootstrap(&root) {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "profile runtime: {error}");
            return RUNTIME_ERROR;
        }
    };
    let outcome = match runtime.block_on(profile::set_profile_async(
        &mut home,
        SetProfileRequest {
            name: opts.name,
            about: opts.about,
        },
    )) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let body = serde_json::json!({
        "ok": outcome.ok,
        "pubkey": outcome.pubkey,
        "name": outcome.name,
        "about": outcome.about,
        "event_id": outcome.event_id,
        "relay_url": outcome.relay_url,
    });
    let rendered = body.to_string();
    if let Ok(secret) = home::read_secret_key_hex(&home) {
        if !secret.is_empty() && rendered.contains(&secret) {
            let _ = writeln!(err, "profile set refused: response would echo secret key");
            return RUNTIME_ERROR;
        }
    }
    let _ = writeln!(out, "{rendered}");
    SUCCESS
}

#[cfg(not(feature = "wallet"))]
fn cmd_set(args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if parse(args).is_err() {
        usage(err);
        return USAGE_ERROR;
    }
    let _ = writeln!(err, "maxplayer profile requires the wallet feature");
    USAGE_ERROR
}
