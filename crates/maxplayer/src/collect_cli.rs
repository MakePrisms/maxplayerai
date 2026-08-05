//! `maxplayer collect` — single-call buyer collect: verify integrity + pay + materialize.
//!
//! Thin adapter over [`maxplayer_core::collect`]. The money authority (spend gate, budget,
//! single-redeem, mint-compat) lives in the core path; this only parses args and renders the
//! outcome. Never echoes the secret key.

use std::io::Write;
use std::path::PathBuf;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

struct Opts {
    job_id: String,
    out: Option<String>,
    home: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut job_id: Option<String> = None;
    let mut out: Option<String> = None;
    let mut home: Option<PathBuf> = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--out" => {
                idx += 1;
                out = Some(args.get(idx).ok_or("--out requires a value")?.clone());
            }
            "--home" => {
                idx += 1;
                home = Some(PathBuf::from(args.get(idx).ok_or("--home requires a value")?));
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other}")),
            other if job_id.is_none() => job_id = Some(other.to_owned()),
            other => return Err(format!("unexpected argument {other}")),
        }
        idx += 1;
    }
    Ok(Opts {
        job_id: job_id.ok_or("collect requires <job_id>")?,
        out,
        home,
    })
}

fn usage(err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "Usage:\n\
         \x20 maxplayer collect <job_id> [--out <folder>] [--home <path>]\n\
         \n\
         Accepts the delivered claim if no bind exists yet, verifies the delivery integrity\n\
         (delivered branch must tip at the accepted commit), pays the seller through the sealed\n\
         money path, then materializes the files under <home>/results/<job_id>. Idempotent:\n\
         re-collecting an already-paid job re-materializes without a second payment.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

/// Entry from `cli::run` for `maxplayer collect ...`.
///
/// Money ops are owned by the buyer daemon (exclusive home lock, single wallet + budget + reservation
/// ledger). This command routes `collect` over the daemon socket (connect-or-spawn) rather than
/// opening the wallet itself — so a CLI collect can never pay outside the daemon path or race a
/// concurrent MCP/daemon melt.
#[cfg(feature = "wallet")]
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::home;

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
    let home = match home::bootstrap(&root) {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };

    let params = serde_json::json!({ "job_id": opts.job_id, "out": opts.out });
    let body = match crate::daemon::ensure_then_call(&home, "collect", params) {
        Ok(body) => body,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            return RUNTIME_ERROR;
        }
    };

    let rendered = body.to_string();
    // Defense in depth: never let the secret key appear on stdout (the daemon never returns it).
    if let Ok(secret) = home::read_secret_key_hex(&home) {
        if !secret.is_empty() && rendered.contains(&secret) {
            let _ = writeln!(err, "collect refused: response would echo secret key");
            return RUNTIME_ERROR;
        }
    }
    let _ = writeln!(out, "{rendered}");
    SUCCESS
}

#[cfg(not(feature = "wallet"))]
pub fn run(args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if parse(args).is_err() {
        usage(err);
        return USAGE_ERROR;
    }
    let _ = writeln!(err, "maxplayer collect requires the wallet feature");
    USAGE_ERROR
}
