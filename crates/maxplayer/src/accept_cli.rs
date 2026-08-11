//! `maxplayer accept` — lower-level buyer primitive: accept a seller claim and record the local
//! co-signed pay-bind (seller / result / commit / repo / branch / job-hash / creq_hash) for a
//! later `maxplayer collect` / authorize_pay. `collect` folds this step in automatically, so `accept`
//! is only needed to bind a specific result up front (e.g. to disambiguate multiple deliveries).
//! Never echoes the secret key.

use std::io::Write;
use std::path::PathBuf;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

struct Opts {
    job_id: String,
    claim_id: String,
    result_id: Option<String>,
    home: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut result_id = None;
    let mut home = None;
    let mut positional: Vec<String> = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--result-id" => {
                idx += 1;
                result_id = Some(args.get(idx).ok_or("--result-id requires a value")?.clone());
            }
            "--home" => {
                idx += 1;
                home = Some(PathBuf::from(args.get(idx).ok_or("--home requires a value")?));
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other}")),
            other => positional.push(other.to_owned()),
        }
        idx += 1;
    }
    let (job_id, claim_id) = match positional.as_slice() {
        [job_id, claim_id] => (job_id.clone(), claim_id.clone()),
        _ => return Err("accept requires <job_id> <claim_id>".into()),
    };
    Ok(Opts {
        job_id,
        claim_id,
        result_id,
        home,
    })
}

fn usage(err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "Usage:\n\
         \x20 maxplayer accept <job_id> <claim_id> [--result-id <id>] [--home <path>]\n\
         \n\
         Records the local co-signed pay-bind for a delivered result. Optional; `maxplayer collect`\n\
         accepts the delivered claim itself when no bind exists. Never echoes the secret key.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

/// Entry from `cli::run` for `maxplayer accept ...`.
#[cfg(feature = "wallet")]
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::home;
    use maxplayer_core::job_lifecycle::{self, AcceptClaimRequest};

    // #570: a sole `--help` prints usage to STDOUT and exits 0 before any parse or home bootstrap.
    if crate::cli::is_help_request(args) {
        usage(out);
        return SUCCESS;
    }

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
    let outcome = match job_lifecycle::accept_claim(
        &home,
        AcceptClaimRequest {
            job_id: opts.job_id,
            claim_id: opts.claim_id,
            result_id: opts.result_id,
        },
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let body = serde_json::json!({
        "ok": true,
        "accept_event_id": outcome.accept_event_id,
        "bind": outcome.bind,
    });
    let rendered = body.to_string();
    if let Ok(secret) = home::read_secret_key_hex(&home) {
        if !secret.is_empty() && rendered.contains(&secret) {
            let _ = writeln!(err, "accept refused: response would echo secret key");
            return RUNTIME_ERROR;
        }
    }
    let _ = writeln!(out, "{rendered}");
    SUCCESS
}

#[cfg(not(feature = "wallet"))]
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: `--help` succeeds to STDOUT even on a buyer-only build (help is feature-independent).
    if crate::cli::is_help_request(args) {
        usage(out);
        return SUCCESS;
    }
    if parse(args).is_err() {
        usage(err);
        return USAGE_ERROR;
    }
    let _ = writeln!(err, "maxplayer accept requires the wallet feature");
    USAGE_ERROR
}
