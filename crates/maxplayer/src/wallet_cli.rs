//! `maxplayer wallet` — flexible ecash wallet management (CLI).
//!
//! Never echoes the secret key. Token/bolt11 may appear on argv per subcommand
//! surface but are not written to durable logs here.

use std::io::Write;
use std::path::PathBuf;

use maxplayer_core::home::{self, MaxplayerHome, DEFAULT_MINIBITS_MINT_URL, DEFAULT_MINT_URL};
#[cfg(feature = "wallet")]
use maxplayer_core::wallet_ops;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

#[derive(Debug, Default)]
struct CommonOpts {
    home: Option<PathBuf>,
    mint: Option<String>,
    amount: Option<u64>,
}

/// Default amount `maxplayer wallet setup` asks the mint for (mirrors the old setup_wallet MCP tool).
/// On the shipped default mint that is an invoice for 21 REAL sats, not a gift.
const SETUP_FUND_SATS: u64 = 21;

/// Entry from `cli::run` for `maxplayer wallet ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match args.first().map(String::as_str) {
        Some("setup") => cmd_setup(&args[1..], out, err),
        Some("balance") => cmd_balance(&args[1..], out, err),
        Some("mint") => cmd_mint(&args[1..], out, err),
        Some("mint-complete") => cmd_mint_complete(&args[1..], out, err),
        Some("send") => cmd_send(&args[1..], out, err),
        Some("receive") => cmd_receive(&args[1..], out, err),
        Some("melt") => cmd_melt(&args[1..], out, err),
        Some("invoice") => cmd_invoice(&args[1..], out, err),
        Some("mints") => cmd_mints(&args[1..], out, err),
        Some("reconcile") => cmd_reconcile(&args[1..], out, err),
        Some("complete-locked") => cmd_complete_locked(&args[1..], out, err),
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
         \x20 maxplayer wallet setup [<amount>] [--mint <url>] [--home <path>]   # bootstrap ~/.maxplayer, then invoice <amount> (default 21) from the mint\n\
         \x20 maxplayer wallet balance [--mint <url>] [--home <path>]\n\
         \x20 maxplayer wallet mint <amount> [--mint <url>] [--home <path>]\n\
         \x20 maxplayer wallet mint-complete <quote_id> [--amount <sats>] [--mint <url>] [--home <path>]\n\
         \x20 maxplayer wallet send <amount> [--mint <url>] [--home <path>]\n\
         \x20 maxplayer wallet receive <token> [--home <path>]\n\
         \x20 maxplayer wallet melt <bolt11> [--mint <url>] [--home <path>]\n\
         \x20 maxplayer wallet invoice <amount> [--mint <url>] [--home <path>]\n\
         \x20 maxplayer wallet mints list [--home <path>]\n\
         \x20 maxplayer wallet mints add <url> [--home <path>]\n\
         \x20 maxplayer wallet mints remove <url> [--home <path>]\n\
         \x20 maxplayer wallet reconcile [--home <path>]   # retire eligible incomplete send sagas (no receipt/credit)\n\
         \x20 maxplayer wallet complete-locked --job-id <id> --result-id <id> --delivery-integrity-hash <hex> --job-hash <hex> --seller-pubkey <hex> --amount <sats> --seller-signature <hex> [--creq-hash <hex>] [--accepted-mint <url> ...] [--realized-mint <url>] [--home <path>]\n\
         \x20\x20\x20# operator: complete ONE payment wedged at Locked by proof-gated REUSE of the already-minted token (never re-mints; STOPS + alarms if the token reads spent)\n"
    );
    // The mint line names the constant the code actually ships rather than restating it. A literal
    // here is what let this text go on saying "testnut" for a release whose default had moved to a
    // real mint (#378, #447) — a reader believed they were on play money while funding with sats.
    let _ = writeln!(
        err,
        "Default mint is {DEFAULT_MINIBITS_MINT_URL} — a REAL mint. `setup` and `mint` print a \
         Lightning invoice you pay with REAL sats; nothing is auto-funded.\n\
         Play money is a dev-only opt-in: --mint {DEFAULT_MINT_URL} (its invoices settle \
         themselves). Extra mints are opt-in via `mints add`.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

fn bootstrap_home(opts: &CommonOpts, err: &mut dyn Write) -> Result<MaxplayerHome, i32> {
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
    let _ = writeln!(err, "maxplayer wallet requires the wallet feature");
    USAGE_ERROR
}
#[cfg(not(feature = "wallet"))]
fn cmd_setup(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_reconcile(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_balance(_args, _out, err)
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
#[cfg(not(feature = "wallet"))]
fn cmd_complete_locked(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
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
            "no balance row for mint={} (configured mints only; check `maxplayer wallet mints list`)",
            filter.as_deref().unwrap_or("")
        );
        return RUNTIME_ERROR;
    }
    let _ = writeln!(out, "total_sats={total}");
    SUCCESS
}

#[cfg(feature = "wallet")]
fn cmd_setup(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            wallet_usage(err);
            return USAGE_ERROR;
        }
    };
    let amount = match positional.as_slice() {
        [] => SETUP_FUND_SATS,
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
    // Bootstrap ~/.maxplayer (config + autogen key + wallet dir), then fund. `home::bootstrap` never
    // prints the secret key.
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    match wallet_ops::mint_blocking(&home, amount, opts.mint.as_deref()) {
        Ok(wallet_ops::MintFlow::Funded(outcome)) => {
            let _ = writeln!(
                out,
                "status=funded home={} funded_sats={} balance_sats={} mint={}",
                home.root.display(),
                outcome.funded_sats,
                outcome.balance_sats,
                outcome.mint_url
            );
            SUCCESS
        }
        Ok(wallet_ops::MintFlow::NeedsPayment(quote)) => {
            // The ordinary path on the shipped default: a real mint invoices, and the sats are the
            // user's. Only a dev test mint settles its own invoice and lands in `Funded` above.
            let _ = writeln!(
                err,
                "status=needs_payment amount_sats={} mint={} quote_id={} (pay the invoice below with real sats, then `maxplayer wallet mint-complete {}`)",
                quote.amount_sats, quote.mint_url, quote.quote_id, quote.quote_id
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
fn cmd_reconcile(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::{buyer_fund, payment_wallet};

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
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "reconcile runtime: {error}");
            return RUNTIME_ERROR;
        }
    };
    let report = runtime.block_on(async {
        let wallet = buyer_fund::open_wallet_async(&home)
            .await
            .map_err(|error| error.to_string())?;
        payment_wallet::retire_eligible_incomplete_sagas(&wallet)
            .await
            .map_err(|error| error.to_string())
    });
    match report {
        Ok(report) => {
            let _ = writeln!(
                out,
                "retired={} mapped_token_created={} swap_rolled_back={} swap_recovered={}",
                report.retired,
                report.mapped_token_created,
                report.swap_rolled_back,
                report.swap_recovered
            );
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

/// Distinct exit code for the STOP+alarm path: the recovered token read spent at the mint. Kept
/// apart from the ordinary runtime-error code so a monitoring wrapper can page a human on the
/// accounting gap specifically.
#[cfg(feature = "wallet")]
const COMPLETE_LOCKED_SPENT_ALARM: i32 = 3;

/// `maxplayer wallet complete-locked` — operator-invoked completion of ONE payment wedged at a
/// recovered `Locked`. Proof-gates the already-minted P2PK-locked token at the mint and, only if
/// every proof is Unspent, REUSES it (never re-mints). A spent token STOPS and emits a loud ALARM.
#[cfg(feature = "wallet")]
fn cmd_complete_locked(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::authorize_pay::complete_recovered_locked_async;
    use maxplayer_core::budget::BudgetGate;

    let (home_path, request) = match parse_complete_locked(args) {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            complete_locked_usage(err);
            return USAGE_ERROR;
        }
    };
    let opts = CommonOpts {
        home: home_path,
        ..CommonOpts::default()
    };
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    let mut gate = match BudgetGate::from_home(&home) {
        Ok(gate) => gate,
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
            let _ = writeln!(err, "complete-locked runtime: {error}");
            return RUNTIME_ERROR;
        }
    };
    let result = runtime.block_on(complete_recovered_locked_async(&home, &mut gate, request));
    report_complete_locked(result, out, err)
}

/// Map the completion outcome to operator output + exit code. Pure (no I/O beyond the writers) so
/// the STOP+alarm behavior is unit-testable without a live wallet.
#[cfg(feature = "wallet")]
fn report_complete_locked(
    result: Result<
        maxplayer_core::authorize_pay::CompleteLockedOutcome,
        maxplayer_core::authorize_pay::AuthorizePayError,
    >,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    use maxplayer_core::authorize_pay::AuthorizePayError;
    use maxplayer_core::payment::PaymentError;
    match result {
        Ok(outcome) => {
            let _ = writeln!(
                out,
                "complete-locked ok state={} attempt_id={} amount_sats={} spent_total_sats={}",
                payment_state_label(&outcome.state),
                outcome.attempt_id,
                outcome.amount_sats,
                outcome.spent_total_sats,
            );
            SUCCESS
        }
        // STOP + ALARM: the recovered token's proofs read spent — the P2PK-locked proofs can only be
        // spent by the seller, so the seller was ALREADY paid by some path we did not record. Loud,
        // greppable, DISTINCT exit so a monitor can page a human. The core entrypoint left the
        // journal untouched (it never advanced), so there is no partial state to unwind.
        Err(AuthorizePayError::Payment(PaymentError::LockedTokenSpent(detail))) => {
            let _ = writeln!(
                err,
                "ALARM complete-locked spent-token: {detail}. NOT resending. VERIFY whether our own \
                 prior (interrupted) send delivered — a spent proof during interrupt-recovery is \
                 benign — versus an unaccounted redemption; escalate the accounting gap only if \
                 unexplained. Journal left unchanged."
            );
            COMPLETE_LOCKED_SPENT_ALARM
        }
        // STOP (not the accounting-gap alarm): no token reconciled for this attempt — refuse rather
        // than blind-remint a token we cannot account for.
        Err(AuthorizePayError::Payment(PaymentError::LockedTokenMissing(detail))) => {
            let _ = writeln!(
                err,
                "complete-locked refused: {detail}. No token reconciled for this attempt — refusing \
                 to remint. Journal left unchanged."
            );
            RUNTIME_ERROR
        }
        Err(error) => {
            let _ = writeln!(err, "complete-locked: {error}");
            RUNTIME_ERROR
        }
    }
}

/// Human label for a folded payment state (no field echo).
#[cfg(feature = "wallet")]
fn payment_state_label(state: &maxplayer_core::payment::PaymentState) -> &'static str {
    use maxplayer_core::payment::PaymentState;
    match state {
        PaymentState::Intent { .. } => "Intent",
        PaymentState::Locked { .. } => "Locked",
        PaymentState::Sent { .. } => "Sent",
        PaymentState::ReceiptPublished { .. } => "ReceiptPublished",
        PaymentState::Closed { .. } => "Closed",
    }
}

/// Fetch the value that must follow a flag at `args[index]`.
#[cfg(feature = "wallet")]
fn require_value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

/// Unwrap a required flag, naming it in the error.
#[cfg(feature = "wallet")]
fn required(value: Option<String>, flag: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("{flag} is required"))
}

/// Parse `complete-locked` flags into `(home, request)`. All identity flags are required so the
/// SAME attempt id (and journal) as the original pay is targeted; `--creq-hash`, `--accepted-mint`
/// (repeatable), and `--realized-mint` are optional and mirror the accept-bind.
#[cfg(feature = "wallet")]
fn parse_complete_locked(
    args: &[String],
) -> Result<(Option<PathBuf>, maxplayer_core::authorize_pay::CompleteLockedRequest), String> {
    use maxplayer_core::authorize_pay::CompleteLockedRequest;

    let mut home: Option<PathBuf> = None;
    let mut job_id: Option<String> = None;
    let mut result_id: Option<String> = None;
    let mut delivery_integrity_hash: Option<String> = None;
    let mut job_hash: Option<String> = None;
    let mut seller_pubkey: Option<String> = None;
    let mut amount_sats: Option<u64> = None;
    let mut seller_signature: Option<String> = None;
    let mut creq_hash: Option<String> = None;
    let mut realized_mint: Option<String> = None;
    let mut accepted_mints: Vec<String> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--home" => {
                index += 1;
                home = Some(PathBuf::from(require_value(args, index, "--home")?));
            }
            "--job-id" => {
                index += 1;
                job_id = Some(require_value(args, index, "--job-id")?.to_owned());
            }
            "--result-id" => {
                index += 1;
                result_id = Some(require_value(args, index, "--result-id")?.to_owned());
            }
            "--delivery-integrity-hash" => {
                index += 1;
                delivery_integrity_hash =
                    Some(require_value(args, index, "--delivery-integrity-hash")?.to_owned());
            }
            "--job-hash" => {
                index += 1;
                job_hash = Some(require_value(args, index, "--job-hash")?.to_owned());
            }
            "--seller-pubkey" => {
                index += 1;
                seller_pubkey = Some(require_value(args, index, "--seller-pubkey")?.to_owned());
            }
            "--amount" => {
                index += 1;
                amount_sats = Some(parse_amount(require_value(args, index, "--amount")?)?);
            }
            "--seller-signature" => {
                index += 1;
                seller_signature =
                    Some(require_value(args, index, "--seller-signature")?.to_owned());
            }
            "--creq-hash" => {
                index += 1;
                creq_hash = Some(require_value(args, index, "--creq-hash")?.to_owned());
            }
            "--realized-mint" => {
                index += 1;
                realized_mint = Some(require_value(args, index, "--realized-mint")?.to_owned());
            }
            "--accepted-mint" => {
                index += 1;
                accepted_mints.push(require_value(args, index, "--accepted-mint")?.to_owned());
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag: {flag}")),
            other => return Err(format!("unexpected positional argument: {other}")),
        }
        index += 1;
    }

    let request = CompleteLockedRequest {
        job_id: required(job_id, "--job-id")?,
        result_id: required(result_id, "--result-id")?,
        delivery_integrity_hash: required(delivery_integrity_hash, "--delivery-integrity-hash")?,
        job_hash: required(job_hash, "--job-hash")?,
        seller_pubkey: required(seller_pubkey, "--seller-pubkey")?,
        amount_sats: amount_sats.ok_or_else(|| "--amount is required".to_owned())?,
        seller_signature: required(seller_signature, "--seller-signature")?,
        creq_hash,
        accepted_mints,
        realized_mint,
    };
    Ok((home, request))
}

#[cfg(feature = "wallet")]
fn complete_locked_usage(err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "Usage:\n  maxplayer wallet complete-locked --job-id <id> --result-id <id> \
         --delivery-integrity-hash <hex> --job-hash <hex> --seller-pubkey <hex> --amount <sats> \
         --seller-signature <hex> [--creq-hash <hex>] [--accepted-mint <url> ...] \
         [--realized-mint <url>] [--home <path>]\n\n\
         Completes ONE payment wedged at Locked by proof-gated REUSE of the already-minted token \
         (never re-mints). If the token reads spent at the mint it STOPS, emits an ALARM, and \
         leaves the journal unchanged.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error, 3 SPENT alarm (accounting gap)"
    );
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
                "status=needs_payment amount_sats={} mint={} quote_id={} (pay the invoice below with real sats, then `maxplayer wallet mint-complete {}`)",
                quote.amount_sats, quote.mint_url, quote.quote_id, quote.quote_id
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

    // #447: the help text told users the default mint was testnut for the whole of the release
    // whose default had already moved to a real minibits mint (#378). Nothing failed — a string is
    // not checked by anything — so the only report was a user who had funded with real sats
    // believing they were on play money. This binds the wording to the constant the code ships.
    #[test]
    fn wallet_usage_names_the_shipped_default_mint_and_never_calls_it_testnut() {
        let mut err = Vec::new();
        wallet_usage(&mut err);
        let text = String::from_utf8(err).expect("utf8");

        assert!(
            text.contains(DEFAULT_MINIBITS_MINT_URL),
            "help must name the mint a fresh config actually uses:\n{text}"
        );
        // testnut may only appear as the dev opt-in, never as what you get by default.
        assert!(
            !text.contains("Default mint is testnut") && !text.contains("default testnut"),
            "help still presents testnut as the default:\n{text}"
        );
        assert!(
            text.contains("REAL"),
            "help must say plainly that the default mint moves real sats:\n{text}"
        );
    }

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

    #[cfg(feature = "wallet")]
    #[test]
    fn parse_complete_locked_requires_the_identity_flags() {
        // Missing --amount (a required identity flag) refuses at parse — before any home/gate/wallet.
        let args: Vec<String> = [
            "--job-id", "j", "--result-id", "r", "--delivery-integrity-hash", "aa",
            "--job-hash", "bb", "--seller-pubkey", "cc", "--seller-signature", "dd",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let err = parse_complete_locked(&args).expect_err("must refuse without --amount");
        assert!(err.contains("--amount"), "error should name the missing flag, got: {err}");
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn parse_complete_locked_collects_repeated_accepted_mints() {
        let args: Vec<String> = [
            "--job-id", "job1", "--result-id", "res1", "--delivery-integrity-hash", "11",
            "--job-hash", "22", "--seller-pubkey", "ab", "--amount", "100",
            "--seller-signature", "ff", "--accepted-mint", "https://m1", "--accepted-mint",
            "https://m2", "--realized-mint", "https://m1", "--home", "/tmp/x",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let (home, request) = parse_complete_locked(&args).expect("parse");
        assert_eq!(home, Some(PathBuf::from("/tmp/x")));
        assert_eq!(request.job_id, "job1");
        assert_eq!(request.amount_sats, 100);
        assert_eq!(request.seller_signature, "ff");
        assert_eq!(request.accepted_mints, vec!["https://m1".to_owned(), "https://m2".to_owned()]);
        assert_eq!(request.realized_mint, Some("https://m1".to_owned()));
    }

    // RED-PROVE (operator surface): a spent-token completion outcome emits a loud ALARM to stderr and
    // returns the DISTINCT alarm exit code, with nothing on stdout. Pairs with the core red-prove
    // (payment.rs) that the entrypoint refuses and leaves the journal unchanged.
    #[cfg(feature = "wallet")]
    #[test]
    fn spent_completion_emits_alarm_and_distinct_exit_code() {
        use maxplayer_core::authorize_pay::AuthorizePayError;
        use maxplayer_core::payment::PaymentError;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = report_complete_locked(
            Err(AuthorizePayError::Payment(PaymentError::LockedTokenSpent(
                "attempt 247ad2e6: a proof reads Spent at the mint".into(),
            ))),
            &mut out,
            &mut err,
        );
        assert_eq!(
            code, COMPLETE_LOCKED_SPENT_ALARM,
            "spent must return the distinct alarm exit code, not the generic runtime error"
        );
        let err_text = String::from_utf8(err).expect("utf8");
        assert!(err_text.starts_with("ALARM"), "must lead with a greppable ALARM, got: {err_text}");
        assert!(err_text.contains("NOT resending"), "alarm must state it is not resending");
        assert!(err_text.contains("Journal left unchanged"));
        assert!(out.is_empty(), "no success output on the alarm path");
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn missing_token_completion_refuses_without_the_spent_alarm() {
        use maxplayer_core::authorize_pay::AuthorizePayError;
        use maxplayer_core::payment::PaymentError;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = report_complete_locked(
            Err(AuthorizePayError::Payment(PaymentError::LockedTokenMissing(
                "no confirmed send transaction".into(),
            ))),
            &mut out,
            &mut err,
        );
        assert_eq!(code, RUNTIME_ERROR, "missing is a refuse, not the spent alarm");
        let err_text = String::from_utf8(err).expect("utf8");
        assert!(!err_text.starts_with("ALARM"), "missing must NOT masquerade as the accounting-gap alarm");
        assert!(err_text.contains("refusing to remint"), "got: {err_text}");
    }
}
