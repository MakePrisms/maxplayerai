//! `mobee wallet` — flexible ecash wallet management (CLI).
//!
//! Never echoes the secret key. Token/bolt11 may appear on argv per subcommand
//! surface but are not written to durable logs here.

use std::io::Write;
use std::path::PathBuf;

use mobee_core::home::{self, MobeeHome};
#[cfg(feature = "wallet")]
use mobee_core::wallet_ops;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

#[derive(Debug, Default)]
struct CommonOpts {
    home: Option<PathBuf>,
    mint: Option<String>,
    amount: Option<u64>,
}

/// Entry from `cli::run` for `mobee wallet ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match args.first().map(String::as_str) {
        Some("balance") => cmd_balance(&args[1..], out, err),
        Some("mint") => cmd_mint(&args[1..], out, err),
        Some("mint-complete") => cmd_mint_complete(&args[1..], out, err),
        Some("send") => cmd_send(&args[1..], out, err),
        Some("receive") => cmd_receive(&args[1..], out, err),
        Some("melt") => cmd_melt(&args[1..], out, err),
        Some("invoice") => cmd_invoice(&args[1..], out, err),
        Some("mints") => cmd_mints(&args[1..], out, err),
        _ => {
            wallet_usage(err);
            USAGE_ERROR
        }
    }
}

fn wallet_usage(err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "Usage:\n\
         \x20 mobee wallet balance [--mint <url>] [--home <path>]\n\
         \x20 mobee wallet mint <amount> [--mint <url>] [--home <path>]\n\
         \x20 mobee wallet mint-complete <quote_id> [--amount <sats>] [--mint <url>] [--home <path>]\n\
         \x20 mobee wallet send <amount> [--mint <url>] [--home <path>]\n\
         \x20 mobee wallet receive <token> [--home <path>]\n\
         \x20 mobee wallet melt <bolt11> [--mint <url>] [--home <path>]\n\
         \x20 mobee wallet invoice <amount> [--mint <url>] [--home <path>]\n\
         \x20 mobee wallet mints list [--home <path>]\n\
         \x20 mobee wallet mints add <url> [--home <path>]\n\
         \x20 mobee wallet mints remove <url> [--home <path>]\n\
         \n\
         Default mint is testnut (pinned). Extra mints are opt-in via `mints add`.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

fn bootstrap_home(opts: &CommonOpts, err: &mut dyn Write) -> Result<MobeeHome, i32> {
    let root = match opts.home.clone() {
        Some(path) => path,
        None => home::default_home_dir().map_err(|error| {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        })?,
    };
    home::bootstrap(&root).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })
}

fn parse_common(args: &[String]) -> Result<(CommonOpts, Vec<String>), String> {
    let mut opts = CommonOpts::default();
    let mut positional = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--home" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--home requires a path".to_owned())?;
                opts.home = Some(PathBuf::from(value));
            }
            "--mint" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--mint requires a url".to_owned())?;
                opts.mint = Some(value.clone());
            }
            "--amount" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--amount requires a value".to_owned())?;
                opts.amount = Some(parse_amount(value)?);
            }
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag: {flag}"));
            }
            other => positional.push(other.to_owned()),
        }
        index += 1;
    }
    Ok((opts, positional))
}

fn parse_amount(raw: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|_| format!("invalid amount: {raw}"))
        .and_then(|amount| {
            if amount == 0 {
                Err("amount must be > 0".into())
            } else {
                Ok(amount)
            }
        })
}

#[cfg(not(feature = "wallet"))]
fn cmd_balance(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "mobee wallet requires the wallet feature");
    USAGE_ERROR
}
#[cfg(not(feature = "wallet"))]
fn cmd_mint(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_mint_complete(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_send(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_receive(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_melt(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_invoice(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_mints(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
}

#[cfg(feature = "wallet")]
fn cmd_balance(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    if !positional.is_empty() {
        wallet_usage(err);
        return USAGE_ERROR;
    }
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    let rows = match wallet_ops::balances_blocking(&home) {
        Ok(rows) => rows,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let filter = match opts.mint.as_deref() {
        Some(raw) => match wallet_ops::normalize_mint_url(raw) {
            Ok(url) => Some(url),
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return RUNTIME_ERROR;
            }
        },
        None => None,
    };
    let mut total = 0u64;
    let mut matched = 0u64;
    for row in &rows {
        if let Some(filter) = filter.as_deref() {
            if row.mint_url != filter {
                continue;
            }
        }
        matched = matched.saturating_add(1);
        total = total.saturating_add(row.balance_sats);
        let marker = if row.is_default { "default" } else { "extra" };
        let _ = writeln!(
            out,
            "mint={} role={} balance_sats={}",
            row.mint_url, marker, row.balance_sats
        );
    }
    if filter.is_some() && matched == 0 {
        let _ = writeln!(
            err,
            "no balance row for mint={} (configured mints only; check `mobee wallet mints list`)",
            filter.as_deref().unwrap_or("")
        );
        return RUNTIME_ERROR;
    }
    let _ = writeln!(out, "total_sats={total}");
    SUCCESS
}

#[cfg(feature = "wallet")]
fn cmd_mint(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let amount = match positional.as_slice() {
        [raw] => match parse_amount(raw) {
            Ok(amount) => amount,
            Err(message) => {
                let _ = writeln!(err, "{message}");
                return USAGE_ERROR;
            }
        },
        _ => {
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    match wallet_ops::mint_blocking(&home, amount, opts.mint.as_deref()) {
        Ok(wallet_ops::MintFlow::Funded(outcome)) => {
            let _ = writeln!(
                out,
                "minted_sats={} balance_sats={} mint={} quote_id={}",
                outcome.funded_sats, outcome.balance_sats, outcome.mint_url, outcome.quote_id
            );
            SUCCESS
        }
        Ok(wallet_ops::MintFlow::NeedsPayment(quote)) => {
            // Bolt11 before any poll — payer must fund, then complete_mint.
            let _ = writeln!(
                err,
                "status=needs_payment amount_sats={} mint={} quote_id={} (testnut auto-pays; extras require external pay + complete)",
                quote.amount_sats, quote.mint_url, quote.quote_id
            );
            let _ = writeln!(out, "{}", quote.invoice);
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

#[cfg(feature = "wallet")]
fn cmd_mint_complete(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let quote_id = match positional.as_slice() {
        [quote_id] => quote_id.as_str(),
        _ => {
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    match wallet_ops::complete_mint_by_id_blocking(&home, quote_id, opts.amount, opts.mint.as_deref())
    {
        Ok(outcome) => {
            let _ = writeln!(
                out,
                "funded_sats={} balance_sats={} mint={} quote_id={}",
                outcome.funded_sats, outcome.balance_sats, outcome.mint_url, outcome.quote_id
            );
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

#[cfg(feature = "wallet")]
fn cmd_send(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let amount = match positional.as_slice() {
        [raw] => match parse_amount(raw) {
            Ok(amount) => amount,
            Err(message) => {
                let _ = writeln!(err, "{message}");
                return USAGE_ERROR;
            }
        },
        _ => {
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    match wallet_ops::send_blocking(&home, amount, opts.mint.as_deref()) {
        Ok(outcome) => {
            // Token alone on stdout for piping; summary on stderr.
            let _ = writeln!(
                err,
                "sent_sats={} balance_sats={} mint={}",
                outcome.sent_sats, outcome.balance_sats, outcome.mint_url
            );
            let _ = writeln!(out, "{}", outcome.token);
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

#[cfg(feature = "wallet")]
fn cmd_receive(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let token = match positional.as_slice() {
        [raw] => raw.as_str(),
        _ => {
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    match wallet_ops::receive_blocking(&home, token) {
        Ok(outcome) => {
            let _ = writeln!(
                out,
                "received_sats={} balance_sats={} mint={}",
                outcome.received_sats, outcome.balance_sats, outcome.mint_url
            );
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

#[cfg(feature = "wallet")]
fn cmd_melt(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let bolt11 = match positional.as_slice() {
        [raw] => raw.as_str(),
        _ => {
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    match wallet_ops::melt_blocking(&home, bolt11, opts.mint.as_deref()) {
        Ok(outcome) => {
            let _ = writeln!(
                out,
                "paid_sats={} fee_sats={} balance_sats={} mint={}",
                outcome.paid_sats, outcome.fee_sats, outcome.balance_sats, outcome.mint_url
            );
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

#[cfg(feature = "wallet")]
fn cmd_invoice(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let amount = match positional.as_slice() {
        [raw] => match parse_amount(raw) {
            Ok(amount) => amount,
            Err(message) => {
                let _ = writeln!(err, "{message}");
                return USAGE_ERROR;
            }
        },
        _ => {
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    match wallet_ops::invoice_blocking(&home, amount, opts.mint.as_deref()) {
        Ok(wallet_ops::MintFlow::Funded(outcome)) => {
            let _ = writeln!(
                err,
                "status=funded funded_sats={} balance_sats={} mint={} quote_id={}",
                outcome.funded_sats, outcome.balance_sats, outcome.mint_url, outcome.quote_id
            );
            let _ = writeln!(out, "{}", outcome.invoice);
            SUCCESS
        }
        Ok(wallet_ops::MintFlow::NeedsPayment(quote)) => {
            let _ = writeln!(
                err,
                "status=needs_payment amount_sats={} mint={} quote_id={}",
                quote.amount_sats, quote.mint_url, quote.quote_id
            );
            let _ = writeln!(out, "{}", quote.invoice);
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

#[cfg(feature = "wallet")]
fn cmd_mints(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(sub) = args.first().map(String::as_str) else {
        wallet_usage(err);
        return USAGE_ERROR;
    };
    let (opts, positional) = match parse_common(&args[1..]) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let mut home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    match sub {
        "list" => {
            if !positional.is_empty() {
                wallet_usage(err);
                return USAGE_ERROR;
            }
            match wallet_ops::list_mints(&home) {
                Ok(rows) => {
                    for row in rows {
                        let marker = if row.is_default { "default" } else { "extra" };
                        let _ = writeln!(out, "mint={} role={}", row.mint_url, marker);
                    }
                    SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                    RUNTIME_ERROR
                }
            }
        }
        "add" => {
            let url = match positional.as_slice() {
                [raw] => raw.as_str(),
                _ => {
                    wallet_usage(err);
                    return USAGE_ERROR;
                }
            };
            match wallet_ops::add_mint(&mut home, url) {
                Ok(normalized) => {
                    let _ = writeln!(out, "added mint={normalized}");
                    SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                    RUNTIME_ERROR
                }
            }
        }
        "remove" => {
            let url = match positional.as_slice() {
                [raw] => raw.as_str(),
                _ => {
                    wallet_usage(err);
                    return USAGE_ERROR;
                }
            };
            match wallet_ops::remove_mint(&mut home, url) {
                Ok(()) => {
                    let _ = writeln!(out, "removed mint={url}");
                    SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                    RUNTIME_ERROR
                }
            }
        }
        _ => {
            wallet_usage(err);
            USAGE_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_common_accepts_amount_flag_alongside_positional() {
        let (opts, positional) =
            parse_common(&["--amount".into(), "21".into(), "quote-123".into()]).expect("parse");
        assert_eq!(opts.amount, Some(21));
        assert_eq!(positional, vec!["quote-123".to_owned()]);
    }

    #[test]
    fn parse_common_amount_flag_rejects_non_numeric() {
        let err = parse_common(&["--amount".into(), "abc".into()]).expect_err("reject");
        assert!(err.contains("invalid amount"));
    }

    #[test]
    fn parse_common_amount_flag_requires_value() {
        let err = parse_common(&["--amount".into()]).expect_err("reject");
        assert!(err.contains("--amount requires a value"));
    }

    #[test]
    fn mint_complete_without_quote_id_is_usage_error() {
        // No positional quote_id => usage error, before any wallet op runs.
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&["mint-complete".into()], &mut out, &mut err);
        assert_eq!(code, USAGE_ERROR);
    }
}
