//! `maxplayer whoami` — print THIS seat's PUBLIC identity and nothing else.
//!
//! Prints three lines: the hex nostr public key, its npub (bech32), and the resolved home
//! directory (honoring `--home` / `MOBEE_HOME` exactly as the other commands do, via
//! [`home::default_home_dir`]).
//!
//! # Security
//!
//! Read-only, PUBLIC-ONLY. The single value read from the home is
//! [`home::public_key_hex`], which derives the nostr *public* key from the packaged secret and
//! returns it hex-encoded — it never returns the secret (see its doc: "Safe to return on MCP
//! surfaces (not secret material)"). The npub is a reversible encoding of that same public key.
//! The secret key / seed / nsec is NEVER read, derived, or written here. A whoami that leaked key
//! material would be a critical defect; the `output_never_contains_secret_key` test red-proves it.

use std::io::Write;
use std::path::PathBuf;

const USAGE_ERROR: i32 = 1;
#[cfg(feature = "wallet")]
const SUCCESS: i32 = 0;
#[cfg(feature = "wallet")]
const RUNTIME_ERROR: i32 = 2;

/// Entry point for `maxplayer whoami [--home <dir>]`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let home_override = match parse_home(args) {
        Ok(value) => value,
        Err(()) => return usage(err),
    };

    #[cfg(not(feature = "wallet"))]
    {
        let _ = (home_override, out);
        let _ = writeln!(
            err,
            "maxplayer whoami requires the wallet feature (rebuild with default features)"
        );
        USAGE_ERROR
    }

    #[cfg(feature = "wallet")]
    match run_whoami(home_override, out, err) {
        Ok(()) => SUCCESS,
        Err(code) => code,
    }
}

/// The only accepted flag is `--home <dir>`; anything else is a usage error.
fn parse_home(args: &[String]) -> Result<Option<PathBuf>, ()> {
    let mut home = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--home" => {
                index += 1;
                home = Some(PathBuf::from(args.get(index).ok_or(())?));
            }
            _ => return Err(()),
        }
        index += 1;
    }
    Ok(home)
}

#[cfg(feature = "wallet")]
fn run_whoami(
    home_override: Option<PathBuf>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), i32> {
    use maxplayer_core::home;

    let root = match home_override {
        Some(path) => path,
        None => home::default_home_dir().map_err(|error| {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        })?,
    };
    let home = home::bootstrap(&root).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;

    // PUBLIC key only — `public_key_hex` derives the nostr public key from the packaged secret and
    // returns it hex-encoded; the secret is never returned. The npub is derived from that same
    // public key, not from the secret.
    let pubkey_hex = home::public_key_hex(&home).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;
    let npub = npub_from_pubkey_hex(&pubkey_hex).map_err(|error| {
        let _ = writeln!(err, "{error}");
        RUNTIME_ERROR
    })?;

    let _ = writeln!(out, "pubkey: {pubkey_hex}");
    let _ = writeln!(out, "npub:   {npub}");
    let _ = writeln!(out, "home:   {}", home.root.display());
    Ok(())
}

/// Encode a hex public key as its npub (bech32). Operates on the PUBLIC key only.
#[cfg(feature = "wallet")]
fn npub_from_pubkey_hex(pubkey_hex: &str) -> Result<String, String> {
    use nostr_sdk::prelude::{PublicKey, ToBech32};

    let public_key =
        PublicKey::from_hex(pubkey_hex).map_err(|error| format!("pubkey parse for npub: {error}"))?;
    public_key
        .to_bech32()
        .map_err(|error| format!("npub encode: {error}"))
}

fn usage(err: &mut dyn Write) -> i32 {
    let _ = writeln!(
        err,
        "Usage:\n  maxplayer whoami [--home <dir>]   # print this seat's PUBLIC identity: hex pubkey, npub, resolved home"
    );
    USAGE_ERROR
}
