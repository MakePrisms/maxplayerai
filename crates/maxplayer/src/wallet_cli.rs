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
/// On the shipped default mint that is an invoice for 21 sats, not a gift.
const SETUP_FUND_SATS: u64 = 21;

/// Entry from `cli::run` for `maxplayer wallet ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` at any wallet level (`wallet --help`, `wallet <sub> --help`, `wallet
    // mints <sub> --help`) prints usage to STDOUT and exits 0 here, before `parse_common` (which
    // rejects `--help` as an unknown flag) or any wallet op runs.
    if crate::cli::is_help_request(args) {
        wallet_usage(out);
        return SUCCESS;
    }
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
    // The mint line interpolates the shipped default constant rather than restating it, so this help
    // can never drift from the mint the code actually ships — a hardcoded name once let it keep
    // naming "testnut" after the default had already moved (#378, #447).
    let _ = writeln!(
        err,
        "Default mint is {DEFAULT_MINIBITS_MINT_URL}. `setup` and `mint` print a Lightning invoice \
         you pay to fund the wallet; nothing is auto-funded. Use `--mint <url>` to fund from a \
         different mint, or `mints add <url>` to configure one.\n\
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
        let _ = writeln!(out, "{}", balance_row_line(row));
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
    let mut home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    // #445 (Option 1, fail-closed): never SILENTLY auto-fund play money from invisible home state.
    // If setup resolves to the testnut play mint without the user naming it via `--mint`, refuse and
    // force the money-type decision into the open — a money act must be affirmed, not defaulted.
    if let Some(reason) = refuse_silent_play_money(opts.mint.as_deref(), home.config.default_mint()) {
        let _ = writeln!(err, "{reason}");
        return RUNTIME_ERROR;
    }
    // #506-C: make the advertised one-command form work on a fresh home — `wallet setup --mint <url>`
    // auto-adds an unconfigured mint (identical to `wallet mints add <url>`) instead of exiting 2
    // ("not configured"). Idempotent, and never changes the default (adds to extra_mints only).
    if let Some(raw) = opts.mint.as_deref() {
        if let Err(error) = wallet_ops::add_mint(&mut home, raw) {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    }
    match wallet_ops::mint_blocking(&home, amount, opts.mint.as_deref()) {
        Ok(wallet_ops::MintFlow::Funded(outcome)) => {
            let _ = writeln!(out, "{}", setup_funded_summary(&home.root, &outcome));
            SUCCESS
        }
        Ok(wallet_ops::MintFlow::NeedsPayment(quote)) => {
            // The ordinary path on the shipped default: a real mint invoices, and the sats are the
            // user's. Only a dev test mint settles its own invoice and lands in `Funded` above.
            let _ = writeln!(err, "{}", setup_needs_payment_summary(&quote));
            let _ = writeln!(out, "{}", quote.invoice);
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

/// #445 (Option 1): decide whether `wallet setup` must REFUSE rather than silently auto-fund play
/// money. Returns `Some(reason)` to refuse — exactly when the mint would resolve to the testnut play
/// mint AND the user did not name a mint via `--mint`, so play money would be funded from invisible
/// home state. A mint named explicitly (real OR testnut) is an affirmation and always proceeds. Pure
/// so every branch is unit-tested without a live mint.
#[cfg(feature = "wallet")]
fn refuse_silent_play_money(explicit_mint: Option<&str>, default_mint: &str) -> Option<String> {
    if explicit_mint.is_some() {
        return None;
    }
    if wallet_ops::MoneyType::of_mint(default_mint) != wallet_ops::MoneyType::Play {
        return None;
    }
    Some(format!(
        "refusing to auto-fund PLAY money: this home's default mint is the testnut dev/play mint \
         ({DEFAULT_MINT_URL}), which silently self-funds fake sats. To fund play money, affirm it \
         explicitly with `--mint {DEFAULT_MINT_URL}`; for real money, `maxplayer wallet mints add \
         <real-mint>` (or set a real default). Nothing was funded."
    ))
}

/// A loud marker for play (testnut dev) money, empty for every ordinary mint. The testnut dev mint
/// self-mints fake sats; surfacing that keeps a dev home from mistaking play money for real funds.
/// Ordinary mints carry no class label — a mint is a mint, identified by its URL (#577). Returns a
/// leading-space field so callers append it directly; empty means the field is simply absent.
#[cfg(feature = "wallet")]
fn play_money_marker(mint_url: &str) -> &'static str {
    match wallet_ops::MoneyType::of_mint(mint_url) {
        wallet_ops::MoneyType::Play => " play_money=true",
        wallet_ops::MoneyType::Real => "",
    }
}

/// Summary line for a `wallet setup` that FUNDED. Only the testnut dev mint auto-pays its own invoice,
/// so this row is play money in practice and carries the play-money marker; an ordinary mint would
/// carry none (#577).
#[cfg(feature = "wallet")]
fn setup_funded_summary(home_root: &std::path::Path, outcome: &wallet_ops::MintOutcome) -> String {
    format!(
        "status=funded home={} funded_sats={} balance_sats={} mint={}{}",
        home_root.display(),
        outcome.funded_sats,
        outcome.balance_sats,
        outcome.mint_url,
        play_money_marker(&outcome.mint_url),
    )
}

/// Summary line for a `wallet setup` that returned an invoice to pay (the ordinary path: the mint
/// invoices and the sats are the user's). A play-money marker appears only for a dev play mint;
/// ordinary mints carry no class label (#577).
#[cfg(feature = "wallet")]
fn setup_needs_payment_summary(quote: &wallet_ops::MintQuote) -> String {
    format!(
        "status=needs_payment amount_sats={} mint={}{} quote_id={} (pay the invoice below, then `maxplayer wallet mint-complete {}`)",
        quote.amount_sats,
        quote.mint_url,
        play_money_marker(&quote.mint_url),
        quote.quote_id,
        quote.quote_id,
    )
}

/// One `wallet balance` row: mint URL, role, balance. A play-money marker trails the row only for a
/// dev play mint; ordinary mints carry no class label — the URL is the mint's identity (#577).
#[cfg(feature = "wallet")]
fn balance_row_line(row: &wallet_ops::MintBalance) -> String {
    format!(
        "mint={} role={} balance_sats={}{}",
        row.mint_url,
        if row.is_default { "default" } else { "extra" },
        row.balance_sats,
        play_money_marker(&row.mint_url),
    )
}

/// One `wallet mints list` row: like [`balance_row_line`] without a balance (list never reads one).
#[cfg(feature = "wallet")]
fn mints_list_row_line(row: &wallet_ops::MintBalance) -> String {
    format!(
        "mint={} role={}{}",
        row.mint_url,
        if row.is_default { "default" } else { "extra" },
        play_money_marker(&row.mint_url),
    )
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
    // #445 (Option 1, fail-closed): `wallet mint` tops up via the same auto-funding mint_blocking
    // path as `setup`, so a SILENT testnut auto-fund is refused identically — a money act is affirmed
    // with `--mint`, never defaulted from invisible home state. Same predicate, same placement.
    if let Some(reason) = refuse_silent_play_money(opts.mint.as_deref(), home.config.default_mint()) {
        let _ = writeln!(err, "{reason}");
        return RUNTIME_ERROR;
    }
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
                "status=needs_payment amount_sats={} mint={} quote_id={} (pay the invoice below, then `maxplayer wallet mint-complete {}`)",
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
    // #445 (Option 1, fail-closed): `wallet invoice` auto-funds via invoice_blocking (the testnut
    // dev mint self-settles), so a SILENT testnut auto-fund is refused identically to `setup`/`mint` —
    // the money-type decision is forced open with `--mint`, never defaulted. Same predicate, same placement.
    if let Some(reason) = refuse_silent_play_money(opts.mint.as_deref(), home.config.default_mint()) {
        let _ = writeln!(err, "{reason}");
        return RUNTIME_ERROR;
    }
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
                        let _ = writeln!(out, "{}", mints_list_row_line(&row));
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

    // #447 + #595: the help once named testnut as the default for a whole release whose default had
    // already moved (#378) — a string nothing checks. This binds the wording to the shipped constant
    // AND, per gudnuf's mints-are-mints ruling, forbids forking mint classes or saying "real" in the
    // copy. RED-ON-REVERT: re-adding "REAL"/"real sats" (or naming testnut as the default) reds this.
    #[test]
    fn wallet_usage_names_the_shipped_default_mint_and_never_calls_it_testnut() {
        let mut err = Vec::new();
        wallet_usage(&mut err);
        let text = String::from_utf8(err).expect("utf8");

        assert!(
            text.contains(DEFAULT_MINIBITS_MINT_URL),
            "help must name the mint a fresh config actually uses:\n{text}"
        );
        assert!(
            !text.contains("Default mint is testnut") && !text.contains("default testnut"),
            "help still presents testnut as the default:\n{text}"
        );
        // #595 (mints-are-mints): a mint is a mint — the help must not fork classes or say "real".
        assert!(
            !text.contains("REAL") && !text.contains("real sats") && !text.contains("real money"),
            "help must not fork mint classes or say 'real':\n{text}"
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

    // ---- #445 + #506 setup-UX fold ----

    #[cfg(feature = "wallet")]
    fn ux_test_home(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-setupux-{label}-{}-{id}",
            std::process::id()
        ))
    }

    /// Seed a home whose DEFAULT mint is the testnut PLAY mint — the "reused testnut home" of #445 —
    /// on disk, so the CLI's own bootstrap loads it.
    #[cfg(feature = "wallet")]
    fn seed_testnut_default_home(root: &std::path::Path) {
        let mut home = home::bootstrap(root).expect("bootstrap seed home");
        home::save_config(&mut home, |config| {
            config.accepted_mints = vec![DEFAULT_MINT_URL.to_owned()];
        })
        .expect("seed testnut default on disk");
    }

    // #445 (Option 1, fail-closed): `wallet setup` on a home whose default is the testnut PLAY mint,
    // with NO explicit `--mint`, must REFUSE rather than silently auto-fund fake sats. The refusal is
    // reached before any mint round-trip, so this holds offline. RED-ON-REVERT: without the gate the
    // command proceeds to the mint path (auto-funding testnut / erroring on the network) — never this
    // refusal with empty stdout.
    #[cfg(feature = "wallet")]
    #[test]
    fn setup_refuses_silent_play_money_without_explicit_mint() {
        let home = ux_test_home("refuse-silent-play");
        let _ = std::fs::remove_dir_all(&home);
        seed_testnut_default_home(&home);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            &[
                "setup".into(),
                "--home".into(),
                home.to_string_lossy().into_owned(),
            ],
            &mut out,
            &mut err,
        );
        let out = String::from_utf8(out).expect("utf8");
        let err = String::from_utf8(err).expect("utf8");

        assert_eq!(
            code, RUNTIME_ERROR,
            "must refuse, not fund:\nstdout={out}\nstderr={err}"
        );
        assert!(out.is_empty(), "nothing funded => empty stdout:\n{out}");
        assert!(err.contains("PLAY money"), "refusal must name play money:\n{err}");
        assert!(
            err.contains(DEFAULT_MINT_URL),
            "refusal must name the testnut mint (the --mint affirmation):\n{err}"
        );
        assert!(
            err.contains("mints add"),
            "refusal must offer the real-money remedy:\n{err}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // #445 (extension): `wallet mint <sats>` tops up via the same auto-funding mint_blocking path as
    // `setup`. On a testnut-default home with NO explicit `--mint` it must REFUSE the silent play-money
    // auto-fund, reached before any mint round-trip (offline). RED-ON-REVERT: drop the guard and it
    // proceeds to mint_blocking (auto-funds testnut / networks) instead of this empty-stdout refusal.
    // The explicit `--mint` arm shows the guard does NOT over-block: it passes through to the mint
    // layer (which then fails for a DIFFERENT reason — an unconfigured mint — never the play refusal).
    #[cfg(feature = "wallet")]
    #[test]
    fn mint_refuses_silent_play_money_without_explicit_mint() {
        let home = ux_test_home("mint-refuse-silent-play");
        let _ = std::fs::remove_dir_all(&home);
        seed_testnut_default_home(&home);
        let home_str = home.to_string_lossy().into_owned();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            &["mint".into(), "100".into(), "--home".into(), home_str.clone()],
            &mut out,
            &mut err,
        );
        let out = String::from_utf8(out).expect("utf8");
        let err = String::from_utf8(err).expect("utf8");
        assert_eq!(code, RUNTIME_ERROR, "must refuse, not fund:\nstdout={out}\nstderr={err}");
        assert!(out.is_empty(), "nothing funded => empty stdout:\n{out}");
        assert!(err.contains("PLAY money"), "refusal must name play money:\n{err}");
        assert!(err.contains(DEFAULT_MINT_URL), "refusal must name the testnut mint:\n{err}");
        assert!(err.contains("mints add"), "refusal must offer the other-mint remedy:\n{err}");

        // Allowed path: an explicit `--mint` affirms the choice, so the guard passes through.
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = run(
            &[
                "mint".into(),
                "100".into(),
                "--mint".into(),
                "https://real-mint.example/".into(),
                "--home".into(),
                home_str,
            ],
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).expect("utf8");
        assert!(!err.contains("PLAY money"), "explicit --mint must pass the guard:\n{err}");
        let _ = std::fs::remove_dir_all(&home);
    }

    // #445 (extension): `wallet invoice <sats>` auto-funds via invoice_blocking (the testnut dev mint
    // self-settles), so the same silent play-money refusal applies. RED-ON-REVERT: drop the guard and
    // it proceeds to invoice_blocking instead of this empty-stdout refusal. The explicit `--mint` arm
    // shows pass-through (fails later on the unconfigured mint, never the play refusal).
    #[cfg(feature = "wallet")]
    #[test]
    fn invoice_refuses_silent_play_money_without_explicit_mint() {
        let home = ux_test_home("invoice-refuse-silent-play");
        let _ = std::fs::remove_dir_all(&home);
        seed_testnut_default_home(&home);
        let home_str = home.to_string_lossy().into_owned();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            &["invoice".into(), "100".into(), "--home".into(), home_str.clone()],
            &mut out,
            &mut err,
        );
        let out = String::from_utf8(out).expect("utf8");
        let err = String::from_utf8(err).expect("utf8");
        assert_eq!(code, RUNTIME_ERROR, "must refuse, not fund:\nstdout={out}\nstderr={err}");
        assert!(out.is_empty(), "nothing funded => empty stdout:\n{out}");
        assert!(err.contains("PLAY money"), "refusal must name play money:\n{err}");
        assert!(err.contains(DEFAULT_MINT_URL), "refusal must name the testnut mint:\n{err}");
        assert!(err.contains("mints add"), "refusal must offer the other-mint remedy:\n{err}");

        // Allowed path: explicit `--mint` passes the guard (then fails on the unconfigured mint).
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = run(
            &[
                "invoice".into(),
                "100".into(),
                "--mint".into(),
                "https://real-mint.example/".into(),
                "--home".into(),
                home_str,
            ],
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).expect("utf8");
        assert!(!err.contains("PLAY money"), "explicit --mint must pass the guard:\n{err}");
        let _ = std::fs::remove_dir_all(&home);
    }

    // The #445 refuse predicate, unit-tested on every branch without a live mint. The integration
    // test above can only exercise the refuse branch offline; here the AFFIRMED-play and real-default
    // branches are deterministic too.
    #[cfg(feature = "wallet")]
    #[test]
    fn setup_money_gate_refuses_only_silent_play() {
        // testnut default + no --mint => the silent-play surprise => refuse.
        assert!(refuse_silent_play_money(None, DEFAULT_MINT_URL).is_some());
        // testnut default + explicit --mint testnut => affirmed => proceed.
        assert!(refuse_silent_play_money(Some(DEFAULT_MINT_URL), DEFAULT_MINT_URL).is_none());
        // real minibits default + no --mint => a real invoice, never silent play => proceed.
        assert!(refuse_silent_play_money(None, DEFAULT_MINIBITS_MINT_URL).is_none());
        // explicit real --mint (whatever the default) => proceed.
        assert!(
            refuse_silent_play_money(Some("https://real-mint.example"), DEFAULT_MINT_URL).is_none()
        );
        // explicit --mint testnut on a real-default home => affirmed play => proceed.
        assert!(
            refuse_silent_play_money(Some(DEFAULT_MINT_URL), DEFAULT_MINIBITS_MINT_URL).is_none()
        );
    }

    // #577: a play-money marker appears ONLY on a testnut (play) row; an ordinary/real row carries no
    // money-class label at all. Pure formatters so both a testnut-resolving (play) and a
    // minibits-resolving (real) setup are asserted without a live mint. The `!contains("money_type")`
    // / `!contains("REAL")` / `!contains("real sats")` asserts are the red-on-revert for gudnuf's
    // ruling: re-adding the class fork or the "real sats" qualifier reds them.
    #[cfg(feature = "wallet")]
    #[test]
    fn setup_summaries_mark_play_money_only() {
        use maxplayer_core::wallet_ops::{MintOutcome, MintQuote};
        // Funded via testnut auto-pay => play money => marker present, alongside the mint URL.
        let play = setup_funded_summary(
            std::path::Path::new("/tmp/h"),
            &MintOutcome {
                mint_url: DEFAULT_MINT_URL.to_owned(),
                invoice: String::new(),
                quote_id: "q".to_owned(),
                funded_sats: 21,
                balance_sats: 21,
            },
        );
        assert!(play.contains("play_money=true"), "{play}");
        assert!(play.contains(&format!("mint={DEFAULT_MINT_URL}")), "{play}");
        // A real mint in the Funded arm carries NO class label — no marker, no money_type, no "REAL".
        let real_funded = setup_funded_summary(
            std::path::Path::new("/tmp/h"),
            &MintOutcome {
                mint_url: DEFAULT_MINIBITS_MINT_URL.to_owned(),
                invoice: String::new(),
                quote_id: "q".to_owned(),
                funded_sats: 21,
                balance_sats: 21,
            },
        );
        assert!(!real_funded.contains("play_money"), "{real_funded}");
        assert!(!real_funded.contains("money_type"), "{real_funded}");
        assert!(!real_funded.contains("REAL"), "{real_funded}");
        // NeedsPayment is the ordinary invoice path: no class label, and no "real sats" qualifier.
        let real = setup_needs_payment_summary(&MintQuote {
            mint_url: DEFAULT_MINIBITS_MINT_URL.to_owned(),
            invoice: "lnbc-invoice".to_owned(),
            quote_id: "q".to_owned(),
            amount_sats: 21,
        });
        assert!(!real.contains("play_money"), "{real}");
        assert!(!real.contains("money_type"), "{real}");
        assert!(!real.contains("real sats"), "{real}");
        assert!(
            real.contains(&format!("mint={DEFAULT_MINIBITS_MINT_URL}")),
            "{real}"
        );
    }

    // #577: `wallet balance` and `wallet mints list` rows mark play money ONLY on a testnut (play)
    // row; an ordinary/real row carries no money-class label. Pure row formatters, asserted for a
    // play (testnut) and a real row. The absence asserts red-on-revert if the class fork returns.
    #[cfg(feature = "wallet")]
    #[test]
    fn wallet_rows_mark_play_money_only() {
        use maxplayer_core::wallet_ops::MintBalance;
        let testnut_default = MintBalance {
            mint_url: DEFAULT_MINT_URL.to_owned(),
            balance_sats: 5,
            is_default: true,
        };
        let real_extra = MintBalance {
            mint_url: "https://real-mint.example".to_owned(),
            balance_sats: 0,
            is_default: false,
        };
        let balance = balance_row_line(&testnut_default);
        assert!(
            balance.contains("role=default")
                && balance.contains("play_money=true")
                && balance.contains("balance_sats=5"),
            "{balance}"
        );
        let real_balance = balance_row_line(&real_extra);
        assert!(
            !real_balance.contains("play_money")
                && !real_balance.contains("money_type")
                && !real_balance.contains("REAL"),
            "{real_balance}"
        );
        let listed = mints_list_row_line(&testnut_default);
        assert!(
            listed.contains("role=default") && listed.contains("play_money=true"),
            "{listed}"
        );
        let real_listed = mints_list_row_line(&real_extra);
        assert!(
            !real_listed.contains("play_money")
                && !real_listed.contains("money_type")
                && !real_listed.contains("REAL"),
            "{real_listed}"
        );
    }

    // End-to-end, offline (mints list never opens a wallet): a testnut-default home with a real extra
    // mint marks the default (play) row and leaves the real extra row unmarked — the play marker is
    // per-row and appears in real command output, not only the pure formatter.
    #[cfg(feature = "wallet")]
    #[test]
    fn mints_list_command_marks_play_money_per_row() {
        let home = ux_test_home("mints-list-labels");
        let _ = std::fs::remove_dir_all(&home);
        seed_testnut_default_home(&home);
        let home_str = home.to_string_lossy().into_owned();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let add = run(
            &[
                "mints".into(),
                "add".into(),
                "https://real-mint.example/".into(),
                "--home".into(),
                home_str.clone(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(add, SUCCESS, "add extra mint: {}", String::from_utf8_lossy(&err));

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            &["mints".into(), "list".into(), "--home".into(), home_str],
            &mut out,
            &mut err,
        );
        let out = String::from_utf8(out).expect("utf8");
        assert_eq!(code, SUCCESS, "stderr: {}", String::from_utf8_lossy(&err));

        let default_row = out
            .lines()
            .find(|line| line.contains("role=default"))
            .expect("a default row");
        assert!(
            default_row.contains(&format!("mint={DEFAULT_MINT_URL}"))
                && default_row.contains("play_money=true"),
            "default row:\n{default_row}\nfull:\n{out}"
        );
        let extra_row = out
            .lines()
            .find(|line| line.contains("role=extra"))
            .expect("an extra row");
        assert!(
            !extra_row.contains("play_money")
                && !extra_row.contains("money_type")
                && !extra_row.contains("REAL"),
            "extra row:\n{extra_row}\nfull:\n{out}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // #506-C: `wallet setup --mint <url>` on a fresh home must AUTO-ADD the mint (so the advertised
    // one-command form works) instead of exiting 2 "is not configured". The add persists before any
    // network mint round-trip, so we assert the durable side effect via `mints list` (offline) and
    // that setup never emitted the membership error. RED-ON-REVERT: without auto-add, `mint_is_allowed`
    // refuses BEFORE the network (offline), so `mints list` would lack the mint and setup would print
    // "is not configured". A reserved `.example` host keeps the later network step a fast, offline NXDOMAIN.
    #[cfg(feature = "wallet")]
    #[test]
    fn setup_mint_flag_auto_adds_unconfigured_mint() {
        let home = ux_test_home("setup-auto-add");
        let _ = std::fs::remove_dir_all(&home);
        let home_str = home.to_string_lossy().into_owned();
        let extra = "https://real-mint.example/";

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _code = run(
            &[
                "setup".into(),
                "--mint".into(),
                extra.into(),
                "--home".into(),
                home_str.clone(),
            ],
            &mut out,
            &mut err,
        );
        let err = String::from_utf8(err).expect("utf8");
        assert!(
            !err.contains("is not configured"),
            "auto-add must clear the membership refusal:\n{err}"
        );

        // The mint is now configured (durably added) — the advertised form is usable.
        let mut out = Vec::new();
        let mut err2 = Vec::new();
        let code = run(
            &["mints".into(), "list".into(), "--home".into(), home_str],
            &mut out,
            &mut err2,
        );
        let out = String::from_utf8(out).expect("utf8");
        assert_eq!(code, SUCCESS, "stderr: {}", String::from_utf8_lossy(&err2));
        assert!(
            out.contains("https://real-mint.example"),
            "setup --mint must auto-add the mint:\n{out}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
