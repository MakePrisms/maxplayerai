//! `maxplayer stub-pay` — exercise the config-bound budget gate over a mock authorization (allow/deny)
//! without touching the real pay path. Caps bind from `~/.mobee` config only; the amount is
//! authorized against them and the durable spent counter advances on allow.

use std::io::Write;
use std::path::PathBuf;

use mobee_core::budget::BudgetGate;
use mobee_core::home;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

/// Entry from `cli::run` for `maxplayer stub-pay <amount> [--home <path>]`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut amount: Option<u64> = None;
    let mut root: Option<PathBuf> = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--home" => {
                idx += 1;
                match args.get(idx) {
                    Some(path) => root = Some(PathBuf::from(path)),
                    None => return usage(err),
                }
            }
            other if other.starts_with('-') => return usage(err),
            other => match other.parse::<u64>() {
                Ok(value) if amount.is_none() => amount = Some(value),
                _ => return usage(err),
            },
        }
        idx += 1;
    }
    let Some(amount) = amount else {
        return usage(err);
    };

    let root = match root {
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
    let mut gate = match BudgetGate::from_home(&home) {
        Ok(gate) => gate,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    match gate.authorize_then(amount, || "stub_ok") {
        Ok(stub) => {
            let _ = writeln!(
                out,
                "{stub} amount_sats={amount} spent_total_sats={} remaining_sats={} per_job_cap_sats={} total_cap_sats={}",
                gate.spent(),
                gate.remaining(),
                gate.per_job_cap(),
                gate.total_cap()
            );
            SUCCESS
        }
        Err(refuse) => {
            let _ = writeln!(err, "{refuse}");
            RUNTIME_ERROR
        }
    }
}

fn usage(err: &mut dyn Write) -> i32 {
    let _ = writeln!(
        err,
        "Usage:\n  maxplayer stub-pay <amount_sats> [--home <path>]\n\nExit codes: 0 success, 1 usage error, 2 runtime error"
    );
    USAGE_ERROR
}
