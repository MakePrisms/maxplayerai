//! `maxplayer issuer` — this seat's OWN Cashu mint (`docs/protocol-v1.md` §4.2 "Issuer mint").
//!
//! Four verbs, and the boundary between them is what the seat is allowed to do to its own currency:
//!
//! - `init` writes files. LOCAL ONLY — no relay, no wallet, no network. It does not install
//!   `cdk-mintd`, does not spawn it and does not supervise it: it prints the exact command and the
//!   operator runs it.
//! - `status` reads the counters back out of the mint.
//! - `issue` writes the seat's own IOU into its own wallet.
//! - `retire` takes some of that IOU back and burns it.
//!
//! ⛔ **The seed is never printed here, and no line below can print it.** `init` reports the seed's
//! PATH and whether it created one; nothing in this module ever reads the file's contents, and the
//! core module that writes it never returns the phrase to a caller.

use std::io::Write;
#[cfg(any(feature = "wallet", test))]
use std::path::PathBuf;

#[cfg(feature = "wallet")]
use maxplayer_core::home::{self, MaxplayerHome};
#[cfg(feature = "wallet")]
use maxplayer_core::issuer;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
// Every runtime failure here comes from a wallet-gated verb; a buyer-only build reaches none of
// them, so the constant is gated with the code that can return it rather than left dead.
#[cfg(feature = "wallet")]
const RUNTIME_ERROR: i32 = 2;

/// The install line from the 3 Sep prove-out (REPORT §1), reproduced verbatim where it matters:
/// `protoc` is a HARD build dependency of `cdk-signatory 0.17.2` even with grpc off
/// (`build.rs:14` panics without it), and it is undeclared, so an operator without it gets a build
/// failure with no hint about what is missing.
#[cfg(feature = "wallet")]
const INSTALL_HINT: &str = "\
  PROTOC=/path/to/protoc \\
    cargo install cdk-mintd --version 0.17.2 --locked \\
      --no-default-features --features fakewallet,sqlite \\
      --root <install-root>
  (protoc is a HARD, undeclared build dependency of cdk-signatory 0.17.2 even with grpc off)";

#[cfg(any(feature = "wallet", test))]
#[derive(Debug, Default)]
struct CommonOpts {
    home: Option<PathBuf>,
    listen_host: Option<String>,
    listen_port: Option<u16>,
    json: bool,
}

/// Entry from `cli::run` for `maxplayer issuer ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` at any level prints usage to STDOUT and exits 0 BEFORE parsing options
    // or taking any side effect — no home bootstrap, no file written, no mint contacted.
    if crate::cli::is_help_request(args) {
        issuer_usage(out);
        return SUCCESS;
    }
    match args.first().map(String::as_str) {
        Some("init") => cmd_init(&args[1..], out, err),
        Some("status") => cmd_status(&args[1..], out, err),
        Some("issue") => cmd_issue(&args[1..], out, err),
        Some("retire") => cmd_retire(&args[1..], out, err),
        _ => {
            issuer_usage(err);
            USAGE_ERROR
        }
    }
}

fn issuer_usage(sink: &mut dyn Write) {
    let _ = writeln!(
        sink,
        "Usage:\n\
         \x20 maxplayer issuer init [--listen-host <host>] [--listen-port <port>] [--home <path>]\n\
         \x20\x20\x20# writes <home>/mint-seed (0600), <home>/mint/mintd-config.toml, and wires config.toml.\n\
         \x20\x20\x20# LOCAL FILES ONLY: no relay, no wallet, no network. Prints the command to run the sidecar.\n\
         \x20 maxplayer issuer status [--json] [--home <path>]\n\
         \x20\x20\x20# url, issued, redeemed, outstanding, retired, last_seen and the work dir, read from the mint.\n\
         \x20 maxplayer issuer issue <sats> [--home <path>]      # mint this seat's own currency at its own mint\n\
         \x20 maxplayer issuer retire <sats> [--home <path>]     # take that currency back and BURN it\n\
         \n\
         The sidecar is `cdk-mintd 0.17.2` in fake-wallet mode, started by YOU, never by this command.\n\
         An issuer mint has no Lightning: `wallet fund` and `wallet melt` refuse it, by design.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

#[cfg(any(feature = "wallet", test))]
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
            "--listen-host" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--listen-host requires a host".to_owned())?;
                opts.listen_host = Some(value.clone());
            }
            "--listen-port" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--listen-port requires a port".to_owned())?;
                opts.listen_port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("invalid port: {value}"))?,
                );
            }
            "--json" => opts.json = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag: {flag}")),
            other => positional.push(other.to_owned()),
        }
        index += 1;
    }
    Ok((opts, positional))
}

#[cfg(feature = "wallet")]
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

#[cfg(feature = "wallet")]
fn parse_sats(raw: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|_| format!("invalid amount: {raw}"))
        .and_then(|sats| {
            if sats == 0 {
                Err("amount must be > 0".into())
            } else {
                Ok(sats)
            }
        })
}

#[cfg(not(feature = "wallet"))]
fn cmd_init(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "maxplayer issuer requires the wallet feature");
    USAGE_ERROR
}
#[cfg(not(feature = "wallet"))]
fn cmd_status(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_init(args, out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_issue(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_init(args, out, err)
}
#[cfg(not(feature = "wallet"))]
fn cmd_retire(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cmd_init(args, out, err)
}

#[cfg(feature = "wallet")]
fn cmd_init(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return USAGE_ERROR;
        }
    };
    if !positional.is_empty() {
        let _ = writeln!(err, "issuer init takes no positional arguments");
        return USAGE_ERROR;
    }
    let mut options = issuer::InitOptions::default();
    if let Some(host) = opts.listen_host.clone() {
        options.listen_host = host;
    }
    if let Some(port) = opts.listen_port {
        options.listen_port = port;
    }
    // Refuse a host cdk cannot bind BEFORE touching the home: a bad `--listen-host` must not leave
    // half a sidecar on disk.
    if let Err(error) = issuer::validate_listen_host(&options.listen_host) {
        let _ = writeln!(err, "{error}");
        return USAGE_ERROR;
    }

    let mut home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    let report = match issuer::init(&mut home, &options) {
        Ok(report) => report,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };

    let _ = writeln!(out, "issuer mint: {}", report.mint_url);
    let _ = writeln!(out, "work dir:    {}", report.work_dir.display());
    let _ = writeln!(out, "mintd config: {}", report.mintd_config.display());
    // The PATH, and whether one was created. Never the phrase.
    let _ = writeln!(
        out,
        "mint seed:   {} ({})",
        report.seed_path.display(),
        if report.seed_created {
            "created, mode 0600 — back it up; a lost mint seed is lost money-shaped state"
        } else {
            "already existed — KEPT, not overwritten"
        }
    );
    let _ = writeln!(
        out,
        "config.toml: issuer_mint set{}{}",
        if report.added_to_accepted_mints {
            ", appended to accepted_mints"
        } else {
            ", already in accepted_mints"
        },
        if report.added_to_extra_mints {
            ", appended to extra_mints"
        } else {
            ", already in extra_mints"
        }
    );

    let binary = which_cdk_mintd();
    let _ = writeln!(out, "\nNow start the sidecar yourself — this command does not:");
    let _ = writeln!(
        out,
        "  {} --work-dir {} --config {} --seed-file {}",
        binary.as_deref().unwrap_or("cdk-mintd"),
        report.work_dir.display(),
        report.mintd_config.display(),
        report.seed_path.display()
    );
    if binary.is_none() {
        let _ = writeln!(
            out,
            "\ncdk-mintd is not on PATH. Build it with:\n{INSTALL_HINT}"
        );
    }
    let _ = writeln!(
        out,
        "\nNote: cdk-mintd logs at DEBUG to a daily-rotated file under {}/logs and ships no\n\
         retention setting — they grow unbounded, so reap them.",
        report.work_dir.display()
    );
    SUCCESS
}

/// Whether `cdk-mintd` is on PATH, and where. A plain PATH walk: this command must not execute the
/// binary to find out (running an unknown binary to check whether it exists is a worse trade than
/// printing a hint).
#[cfg(feature = "wallet")]
fn which_cdk_mintd() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("cdk-mintd"))
        .find(|candidate| candidate.is_file())
        .map(|found| found.display().to_string())
}

#[cfg(feature = "wallet")]
fn cmd_status(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional) = match parse_common(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return USAGE_ERROR;
        }
    };
    if !positional.is_empty() {
        let _ = writeln!(err, "issuer status takes no positional arguments");
        return USAGE_ERROR;
    }
    let home = match bootstrap_home(&opts, err) {
        Ok(home) => home,
        Err(code) => return code,
    };
    let status = match issuer::status(&home) {
        Ok(status) => status,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    if opts.json {
        match serde_json::to_string(&status) {
            Ok(json) => {
                let _ = writeln!(out, "{json}");
            }
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return RUNTIME_ERROR;
            }
        }
        return SUCCESS;
    }
    let _ = writeln!(out, "url:         {}", status.mint_url);
    let _ = writeln!(out, "work dir:    {}", status.work_dir);
    let _ = writeln!(out, "issued:      {} sat", status.issued_sats);
    let _ = writeln!(out, "redeemed:    {} sat", status.redeemed_sats);
    let _ = writeln!(out, "outstanding: {} sat", status.outstanding_sats);
    // Labelled as OURS, because the mint cannot attribute a burn to whoever presented the proofs.
    let _ = writeln!(
        out,
        "retired:     {} sat (this seat's own count; the mint cannot attribute a burn)",
        status.retired_sats
    );
    let _ = writeln!(out, "last_seen:   {}", status.last_seen);
    if !status.in_accepted_mints {
        let _ = writeln!(
            out,
            "\nWARNING: {} is NOT in accepted_mints, so every reader treats this seat's\n\
             issuer_mint tag as UNSTATED and the beat omits it. Run `maxplayer issuer init`.",
            status.mint_url
        );
    }
    SUCCESS
}

#[cfg(feature = "wallet")]
fn cmd_issue(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional, sats) = match parse_amount_command(args, "issue", err) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let _ = positional;
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
            let _ = writeln!(err, "issuer issue runtime: {error}");
            return RUNTIME_ERROR;
        }
    };
    match runtime.block_on(issuer::issue(&home, sats)) {
        Ok(outcome) => {
            let _ = writeln!(
                out,
                "issued {} sat at {} (wallet balance {} sat)",
                outcome.issued_sats, outcome.mint_url, outcome.balance_sats
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
fn cmd_retire(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let (opts, positional, sats) = match parse_amount_command(args, "retire", err) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let _ = positional;
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
            let _ = writeln!(err, "issuer retire runtime: {error}");
            return RUNTIME_ERROR;
        }
    };
    match runtime.block_on(issuer::retire(&home, sats)) {
        Ok(record) => {
            let _ = writeln!(
                out,
                "retired {} sat at {} (melt quote {}); recorded in {}",
                record.sats,
                record.mint_url,
                record.quote_id,
                issuer::retired_ledger_path(&home).display()
            );
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

/// Shared parse for the two amount-taking verbs.
#[cfg(feature = "wallet")]
fn parse_amount_command(
    args: &[String],
    verb: &str,
    err: &mut dyn Write,
) -> Result<(CommonOpts, Vec<String>, u64), i32> {
    let (opts, positional) = parse_common(args).map_err(|error| {
        let _ = writeln!(err, "{error}");
        USAGE_ERROR
    })?;
    let [raw] = positional.as_slice() else {
        let _ = writeln!(err, "usage: maxplayer issuer {verb} <sats> [--home <path>]");
        return Err(USAGE_ERROR);
    };
    let sats = parse_sats(raw).map_err(|error| {
        let _ = writeln!(err, "{error}");
        USAGE_ERROR
    })?;
    Ok((opts, positional.clone(), sats))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sole `--help` prints usage to STDOUT and exits 0, at every level, before any side effect.
    #[test]
    fn help_is_answered_from_our_own_usage_before_anything_happens() {
        for args in [
            vec!["--help".to_owned()],
            vec!["init".to_owned(), "--help".to_owned()],
            vec!["status".to_owned(), "--help".to_owned()],
            vec!["retire".to_owned(), "--help".to_owned()],
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            assert_eq!(run(&args, &mut out, &mut err), SUCCESS, "{args:?}");
            let text = String::from_utf8(out).expect("utf8");
            assert!(text.contains("maxplayer issuer init"), "{text}");
            assert!(err.is_empty(), "usage went to stderr for {args:?}");
        }
    }

    /// Usage names every verb and never suggests this command runs the sidecar.
    #[test]
    fn usage_names_the_verbs_and_disclaims_supervision() {
        let mut out = Vec::new();
        issuer_usage(&mut out);
        let text = String::from_utf8(out).expect("utf8");
        for verb in ["init", "status", "issue", "retire"] {
            assert!(text.contains(&format!("maxplayer issuer {verb}")), "{text}");
        }
        assert!(text.contains("started by YOU, never by this command"), "{text}");
    }

    #[test]
    fn an_unknown_verb_is_a_usage_error() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(&["burn-it-all".to_owned()], &mut out, &mut err),
            USAGE_ERROR
        );
        assert!(out.is_empty());
    }

    #[test]
    fn flags_parse_and_an_unknown_one_is_named() {
        let (opts, positional) = parse_common(&[
            "--listen-port".to_owned(),
            "3400".to_owned(),
            "--listen-host".to_owned(),
            "[::1]".to_owned(),
            "17".to_owned(),
        ])
        .expect("parses");
        assert_eq!(opts.listen_port, Some(3400));
        assert_eq!(opts.listen_host.as_deref(), Some("[::1]"));
        assert_eq!(positional, vec!["17".to_owned()]);
        assert_eq!(
            parse_common(&["--nope".to_owned()]).expect_err("unknown flag"),
            "unknown flag: --nope"
        );
    }
}
