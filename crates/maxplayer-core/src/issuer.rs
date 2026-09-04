//! The seat's OWN Cashu mint — the stage-3a **issuer sidecar** (`docs/protocol-v1.md` §4.2 "Issuer
//! mint"). This module is the producer of the `issuer_mint` tag: it stands the sidecar's files up,
//! reads the counters back out of the mint, and hands the heartbeat an advertisement — or hands it
//! nothing at all, which is the case that matters most.
//!
//! ## What runs where
//!
//! The sidecar is `cdk-mintd` 0.17.2 in fake-wallet mode, an ordinary OS process the OPERATOR
//! starts. Nothing here installs it, spawns it or supervises it: [`init`] writes files and prints
//! the exact command, and every read below is a read of files that process owns. A seat whose
//! sidecar is not running is a seat that advertises no issuer mint, and that is a supported state,
//! not an error.
//!
//! ## The counters are read from the MINT, not from us
//!
//! §4.2 says the counters are read from the mint and `last_seen` is the instant of that read. There
//! is no API for them: the management RPC exposes 22 methods and not one of them retires a proof
//! (`cdk-mint-rpc-0.17.2/src/proto/cdk-mint-rpc.proto:6-22`). So the instrument is the mint's own
//! sqlite, opened READ-ONLY, with the definitions the 3 Sep prove-out measured:
//!
//! ```text
//! issued      = sum(blind_signature.amount)
//! redeemed    = sum(proof.amount where state = 'SPENT')
//! outstanding = issued - redeemed
//! ```
//!
//! `retired_sats` is NOT in that table, and cannot be: the mint burns a proof without recording who
//! presented it, so a mint that has redeemed 100 sat cannot say whether the issuer took them back or
//! a counterparty spent them onward. Retirement is therefore the SEAT's own durable count — every
//! burn this seat performed through [`retire`], appended to [`RETIRED_LEDGER_FILE`] — and it is
//! bounded by what the mint says: `retired <= redeemed` always, because every retirement is one of
//! the redemptions the mint counted.
//!
//! ## The negative is the load-bearing half
//!
//! [`advertisement`] returns `None` — the tag is ABSENT and the beat still publishes — when the
//! seat states no issuer mint, when the sidecar is down, when the sqlite will not open, when a
//! counter will not parse, and when the URL is missing from `accepted_mints`. It never returns an
//! error, never a zero standing in for an unknown, and never anything a caller could mistake for a
//! measurement. `heartbeat.rs:289-290` is the rule: an optional tag must not be able to take a
//! working seat off the market.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::heartbeat::IssuerMintAd;
use crate::home::{self, MaxplayerHome};

/// The sidecar's working directory under the home: sqlite, WAL and the mint's own logs.
pub const MINT_DIR: &str = "mint";
/// The `cdk-mintd` config this seat writes. It carries NO `mnemonic` key — the seed reaches the
/// mint by `--seed-file` (`cdk-mintd-0.17.2/src/cli.rs:30`, applied `src/lib.rs:268` →
/// `apply_seed_file` `:276-286`), and `mnemonic` is `Option` in cdk's config (`config.rs:56`), so
/// omitting it is legal.
pub const MINTD_CONFIG_FILE: &str = "mintd-config.toml";
/// The sqlite file `cdk-mintd` creates in its work dir. Named by cdk, not by us.
pub const MINT_DB_FILE: &str = "cdk-mintd.sqlite";
/// The seat's durable retirement ledger: one JSON object per line, append-only.
pub const RETIRED_LEDGER_FILE: &str = "retired.jsonl";
/// Default seed file name, beside the seat `key` in the home root.
pub const MINT_SEED_FILE: &str = "mint-seed";
/// The loopback host the sidecar binds, per the owner's stage-3 input.
pub const DEFAULT_LISTEN_HOST: &str = "127.0.0.1";
/// The port the prove-out used and the wizard defaults to.
pub const DEFAULT_LISTEN_PORT: u16 = 3338;
/// The `cdk-mintd` version this seat's config shape was measured against.
pub const CDK_MINTD_VERSION: &str = "0.17.2";

/// Anything that can go wrong standing up, reading or driving the sidecar.
///
/// ⛔ No variant carries the seed, a path to a decrypted seed's CONTENTS, or a mnemonic word. The
/// leak risk is our code, not cdk's: cdk's own config `Debug` prints a sha256 of the mnemonic
/// (`cdk-mintd-0.17.2/src/config.rs:105-118`), so nothing upstream would print it for us.
#[derive(Debug)]
pub enum IssuerError {
    /// The seat's config names no `issuer_mint`.
    NoIssuerMint,
    /// A filesystem operation failed. Carries the path and the OS error, never file CONTENTS.
    Io(String),
    /// The mint's sqlite would not open read-only, or a counter query failed.
    Counters(String),
    /// A `listen_host` this seat refuses to write.
    ListenHost(String),
    /// The retirement ledger holds a line that will not parse.
    Ledger(String),
    /// A wallet or mint operation failed.
    Wallet(String),
    /// The home layer refused.
    Home(String),
}

impl fmt::Display for IssuerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoIssuerMint => write!(
                formatter,
                "this seat runs no issuer mint (config.toml has no issuer_mint); run `maxplayer \
                 issuer init` first"
            ),
            Self::Io(detail) => write!(formatter, "issuer sidecar io: {detail}"),
            Self::Counters(detail) => write!(formatter, "issuer mint counters: {detail}"),
            Self::ListenHost(detail) => write!(formatter, "issuer sidecar listen_host: {detail}"),
            Self::Ledger(detail) => write!(formatter, "issuer retirement ledger: {detail}"),
            Self::Wallet(detail) => write!(formatter, "issuer wallet: {detail}"),
            Self::Home(detail) => write!(formatter, "issuer home: {detail}"),
        }
    }
}

impl std::error::Error for IssuerError {}

impl From<home::HomeError> for IssuerError {
    fn from(error: home::HomeError) -> Self {
        Self::Home(error.to_string())
    }
}

/// The sidecar's work dir: `<home>/mint`.
pub fn mint_dir(home: &MaxplayerHome) -> PathBuf {
    home.root.join(MINT_DIR)
}

/// The `cdk-mintd` config this seat writes: `<home>/mint/mintd-config.toml`.
pub fn mintd_config_path(home: &MaxplayerHome) -> PathBuf {
    mint_dir(home).join(MINTD_CONFIG_FILE)
}

/// The mint's sqlite: `<home>/mint/cdk-mintd.sqlite`.
pub fn mint_db_path(home: &MaxplayerHome) -> PathBuf {
    mint_dir(home).join(MINT_DB_FILE)
}

/// The seat's retirement ledger: `<home>/mint/retired.jsonl`.
pub fn retired_ledger_path(home: &MaxplayerHome) -> PathBuf {
    mint_dir(home).join(RETIRED_LEDGER_FILE)
}

/// The mint seed file: the configured `mint_seed_path`, else `<home>/mint-seed` beside the seat
/// `key`. Its CONTENTS are never read by anything in this module — only `cdk-mintd` reads them, and
/// only through `--seed-file`.
pub fn seed_path(home: &MaxplayerHome) -> PathBuf {
    match home.config.mint_seed_path() {
        Some(configured) => PathBuf::from(configured),
        None => home.root.join(MINT_SEED_FILE),
    }
}

/// The counters as the MINT states them, plus the instant they were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MintCounters {
    /// `sum(blind_signature.amount)` — everything this mint ever signed.
    pub issued_sats: u64,
    /// `sum(proof.amount where state = 'SPENT')` — everything it ever burned, by whoever.
    pub redeemed_sats: u64,
    /// `issued - redeemed`. Saturating: a mint whose redeemed somehow exceeded its issued would be
    /// reporting an impossibility, and 0 is the only honest floor for "tokens still out there".
    pub outstanding_sats: u64,
    /// Unix seconds at which the two sums above were read (§4.2: `last_seen` is the instant of the
    /// read, not of the last mint activity).
    pub last_seen: u64,
}

/// Read the counters straight out of the mint's own sqlite, READ-ONLY.
///
/// `mode=ro` is not a courtesy: this process must never be able to write the mint's ledger, and a
/// read-only handle is the enforcement rather than the intention. A WAL database that refuses a
/// read-only open is REPORTED — never worked around by relaxing a permission or copying the file,
/// because a copy would answer about a moment that has already passed.
pub fn read_counters(db_path: &Path) -> Result<MintCounters, IssuerError> {
    if !db_path.exists() {
        return Err(IssuerError::Counters(format!(
            "{} does not exist (the sidecar has not run in this work dir)",
            db_path.display()
        )));
    }
    let connection = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| {
        IssuerError::Counters(format!(
            "read-only open of {} refused: {error} (not relaxing a permission and not copying the \
             file — report this)",
            db_path.display()
        ))
    })?;

    let issued: i64 = connection
        .query_row(
            "select coalesce(sum(amount), 0) from blind_signature",
            [],
            |row| row.get(0),
        )
        .map_err(|error| IssuerError::Counters(format!("issued sum: {error}")))?;
    let redeemed: i64 = connection
        .query_row(
            "select coalesce(sum(amount), 0) from proof where state = 'SPENT'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| IssuerError::Counters(format!("redeemed sum: {error}")))?;

    let issued_sats = u64::try_from(issued)
        .map_err(|_| IssuerError::Counters(format!("issued sum is negative ({issued})")))?;
    let redeemed_sats = u64::try_from(redeemed)
        .map_err(|_| IssuerError::Counters(format!("redeemed sum is negative ({redeemed})")))?;

    Ok(MintCounters {
        issued_sats,
        redeemed_sats,
        outstanding_sats: issued_sats.saturating_sub(redeemed_sats),
        last_seen: now_unix(),
    })
}

/// One retirement this seat performed, as written to the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetirementRecord {
    /// Unix seconds at which the melt confirmed.
    pub at: u64,
    /// Sats burned — the melt quote's amount, which the mint marked PAID.
    pub sats: u64,
    /// The mint the burn happened at.
    pub mint_url: String,
    /// The mint's melt quote id, so an operator can find the row in the mint's own DB.
    pub quote_id: String,
}

/// The seat's own durable retirement total: the sum of every line in the ledger.
///
/// A ledger that does not exist is 0 retirements, not an error — a seat that has never burned
/// anything has retired nothing. A ledger line that will NOT parse IS an error, and the caller that
/// matters ([`advertisement`]) turns that into an absent tag: publishing a total derived from a
/// ledger we could not fully read would be a wrong number, which §6 forbids more strongly than it
/// forbids silence.
pub fn retired_total(ledger_path: &Path) -> Result<u64, IssuerError> {
    let raw = match fs::read_to_string(ledger_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(IssuerError::Ledger(format!(
                "{}: {error}",
                ledger_path.display()
            )));
        }
    };
    let mut total: u64 = 0;
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: RetirementRecord = serde_json::from_str(line).map_err(|error| {
            IssuerError::Ledger(format!(
                "{} line {}: {error}",
                ledger_path.display(),
                index + 1
            ))
        })?;
        total = total.checked_add(record.sats).ok_or_else(|| {
            IssuerError::Ledger(format!(
                "{} line {}: retired total overflows u64",
                ledger_path.display(),
                index + 1
            ))
        })?;
    }
    Ok(total)
}

/// Append one retirement to the ledger, fsync'd. The ledger is the ONLY record that a burn was
/// OURS, so it is written before the caller reports success.
fn append_retirement(ledger_path: &Path, record: &RetirementRecord) -> Result<(), IssuerError> {
    if let Some(parent) = ledger_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| IssuerError::Io(format!("{}: {error}", parent.display())))?;
    }
    let line = serde_json::to_string(record)
        .map_err(|error| IssuerError::Ledger(error.to_string()))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path)
        .map_err(|error| IssuerError::Io(format!("{}: {error}", ledger_path.display())))?;
    file.write_all(line.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| IssuerError::Io(format!("{}: {error}", ledger_path.display())))
}

/// Everything `maxplayer issuer status` prints, and everything the beat needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssuerStatus {
    pub mint_url: String,
    pub work_dir: String,
    pub issued_sats: u64,
    pub redeemed_sats: u64,
    pub outstanding_sats: u64,
    /// The seat's OWN count of what IT burned — see the module docs on why this cannot come from
    /// the mint.
    pub retired_sats: u64,
    pub last_seen: u64,
    /// Whether the seat's `accepted_mints` lists the issuer URL. When false the tag is UNSTATED to
    /// every reader (`IssuerMintAd::from_tags` drops a URL outside the list), so a status that hid
    /// this would describe a seat nobody can see.
    pub in_accepted_mints: bool,
}

/// Read the full issuer status: config, counters and the seat's retirement total. Errors are
/// RETURNED here — an operator running `issuer status` asked a direct question and gets the real
/// answer, including "the sidecar is not running".
pub fn status(home: &MaxplayerHome) -> Result<IssuerStatus, IssuerError> {
    let mint_url = home
        .config
        .issuer_mint()
        .ok_or(IssuerError::NoIssuerMint)?
        .to_owned();
    let counters = read_counters(&mint_db_path(home))?;
    let retired_sats = retired_total(&retired_ledger_path(home))?;
    Ok(IssuerStatus {
        in_accepted_mints: home.config.accepted_mints.iter().any(|m| m == &mint_url),
        mint_url,
        work_dir: mint_dir(home).display().to_string(),
        issued_sats: counters.issued_sats,
        redeemed_sats: counters.redeemed_sats,
        outstanding_sats: counters.outstanding_sats,
        retired_sats,
        last_seen: counters.last_seen,
    })
}

/// How long the beat will wait for the sidecar to answer before calling it down. Loopback, so this
/// is generous: the prove-out measured cold start at 0.200 s and warm start under 0.31 s.
const LIVENESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Is the sidecar actually SERVING at `mint_url`?
///
/// ⚠ **The sqlite is not the liveness signal, and reading it alone would publish a lie.** A killed
/// `cdk-mintd` leaves its database on disk, fully readable, holding the counters as of the moment it
/// died — so a beat built from the file alone would state `outstanding = N` with `last_seen = now`
/// for a mint that has been down for a week. §6 forbids exactly that ("never a wrong number"), and
/// it names "a sidecar that is down" as a case the tag must be ABSENT for.
///
/// So this asks the mint. NUT-06 `/v1/info` is the cheapest question that distinguishes "this port
/// is open" from "a cdk mint is serving this URL" — a plain TCP connect would be satisfied by any
/// process that happened to grab the port. Any answer other than a 2xx is DOWN.
async fn sidecar_serving(mint_url: &str) -> bool {
    let info_url = format!("{}/v1/info", mint_url.trim_end_matches('/'));
    let Ok(client) = reqwest::Client::builder().timeout(LIVENESS_TIMEOUT).build() else {
        return false;
    };
    matches!(client.get(&info_url).send().await, Ok(response) if response.status().is_success())
}

/// The FILE half of the advertisement: everything knowable without asking the mint whether it is up.
///
/// Split out from [`advertisement`] so each refusal is testable on its own — a test that could only
/// reach these through a live sidecar would prove nothing about them.
fn counters_advertisement(home: &MaxplayerHome) -> Option<IssuerMintAd> {
    let mint_url = home.config.issuer_mint()?;
    if !home.config.accepted_mints.iter().any(|m| m == mint_url) {
        return None;
    }
    let counters = read_counters(&mint_db_path(home)).ok()?;
    let retired_sats = retired_total(&retired_ledger_path(home)).ok()?;
    Some(IssuerMintAd {
        mint_url: mint_url.to_owned(),
        outstanding_sats: counters.outstanding_sats,
        retired_sats,
        last_seen: counters.last_seen,
    })
}

/// The `issuer_mint` advertisement for this beat, or `None` to state nothing.
///
/// ⛔ **This function cannot fail and must never be made to.** It is called from the publish path,
/// where every case below is an ordinary operating state:
///
/// 1. the seat states no `issuer_mint` — it runs no mint;
/// 2. the sidecar is DOWN — nothing answers `/v1/info` at the configured URL (and this is checked
///    BEFORE the counters are read, because the file outlives the process that wrote it);
/// 3. the sqlite will not open read-only, or a counter query fails;
/// 4. the retirement ledger holds a line that will not parse;
/// 5. the URL is not in `accepted_mints` — `IssuerMintAd::from_tags` would read the tag as UNSTATED
///    anyway, so emitting it would put bytes on the wire that no reader accepts.
///
/// Every one of them yields `None`: the tag is absent, the beat publishes, and the seat stays on
/// the market. Never a wrong number, never a boot failure (`heartbeat.rs:289-290`).
pub async fn advertisement(home: &MaxplayerHome) -> Option<IssuerMintAd> {
    let mint_url = home.config.issuer_mint()?;
    if !sidecar_serving(mint_url).await {
        return None;
    }
    counters_advertisement(home)
}

/// What [`init`] did, so the caller can print it without re-deriving any of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    pub mint_url: String,
    pub work_dir: PathBuf,
    pub mintd_config: PathBuf,
    pub seed_path: PathBuf,
    /// False when a seed file was already there. A lost mint seed is lost money-shaped state, so an
    /// existing one is KEPT and said so — the same shape as the seat key (`home.rs:1636-1642`).
    pub seed_created: bool,
    pub added_to_accepted_mints: bool,
    pub added_to_extra_mints: bool,
}

/// Options for the wizard. `listen_host` is separate from the URL because cdk builds its bind
/// address by string concatenation and the two spellings differ (see [`validate_listen_host`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    pub listen_host: String,
    pub listen_port: u16,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            listen_host: DEFAULT_LISTEN_HOST.to_owned(),
            listen_port: DEFAULT_LISTEN_PORT,
        }
    }
}

/// Refuse a `listen_host` cdk cannot bind, by name.
///
/// `cdk-mintd-0.17.2/src/lib.rs:1584` builds the address as
/// `SocketAddr::from_str(&format!("{listen_addr}:{listen_port}"))`. That is string concatenation, so
/// `"::1"` becomes `"::1:3338"`, which is not valid `SocketAddr` syntax and kills the process at
/// startup with `invalid socket address syntax` — measured, 3 Sep prove-out §5. The brackets have to
/// come from the config, so this refuses the bare form and NAMES them.
pub fn validate_listen_host(host: &str) -> Result<(), IssuerError> {
    let host = host.trim();
    if host.is_empty() {
        return Err(IssuerError::ListenHost("must not be empty".into()));
    }
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return Err(IssuerError::ListenHost(format!(
            "{host:?} is an IPv6 address without brackets; cdk-mintd builds its bind address by \
             string concatenation, so write it as \"[{host}]\" — the bare form aborts the process \
             at startup with `invalid socket address syntax`"
        )));
    }
    Ok(())
}

/// The mint URL for a bound host/port. An IPv6 literal keeps its brackets in a URL too.
fn mint_url_for(host: &str, port: u16) -> String {
    format!("http://{host}:{port}/")
}

/// Render the `cdk-mintd` config this seat runs.
///
/// The four `[ln]` bounds are NOT optional in 0.17.2 — `struct Ln` at `cdk-mintd-0.17.2/src/
/// config.rs:170` has them at `:175-178` and only `unit` above them carries `#[serde(default)]` —
/// even though the shipped `example.config.toml` comments them out. That example does not parse.
///
/// There is deliberately NO `mnemonic` key: the seed arrives by `--seed-file`.
fn render_mintd_config(host: &str, port: u16, url: &str) -> String {
    format!(
        r#"# maxplayer issuer sidecar — written by `maxplayer issuer init`.
# Run it with (the seed is NOT in this file, by design):
#   cdk-mintd --work-dir <work-dir> --config <this file> --seed-file <seed>
#
# NOTE: cdk-mintd logs at DEBUG to a daily-rotated file under the work dir and ships no
# retention setting. Somebody has to reap <work-dir>/logs.

[info]
url = "{url}"
listen_host = "{host}"
listen_port = {port}

[info.quote_ttl]
mint_ttl = 600
melt_ttl = 120

[info.http_cache]
backend = "memory"
ttl = 60
tti = 60

[mint_management_rpc]
enabled = false

[mint_info]
name = "maxplayer issuer mint"
description = "this seat's own mint: no Lightning, loopback only"

[database]
engine = "sqlite"

[ln]
ln_backend = "fakewallet"
unit = "sat"
# These four are REQUIRED in cdk-mintd {CDK_MINTD_VERSION} (config.rs:175-178 carry no serde(default)).
min_mint = 1
max_mint = 500000
min_melt = 1
max_melt = 500000

[fake_wallet]
fee_percent = 0.0
reserve_fee_min = 0
custom_payment_methods = []
min_delay_time = 0
max_delay_time = 0

[limits]
max_inputs = 1000
max_outputs = 1000
"#
    )
}

/// Generate a fresh BIP39 mnemonic and write it `0600`, or KEEP an existing one — after checking
/// that what is already there deserves to be kept.
///
/// An existing path is VALIDATED, never trusted. `0600` on the file this function creates says
/// nothing about a file some earlier hand left behind, and the earlier version of this function
/// took `path.exists()` as the whole answer: a `0644` seed, or a symlink pointing at a file the
/// operator never meant to be a seed, was adopted silently. So:
///
/// - Anything that is not a regular file is REFUSED, symlink included. The check uses
///   [`fs::symlink_metadata`], which does not follow links — `metadata` would report the target's
///   type and let a link through. A symlink is refused even when its target is a fine `0600` file,
///   because the seat cannot promise the mode of a path it does not own.
/// - On Unix a mode carrying any bit outside owner read+write is narrowed to `0600` in place —
///   the special bits `0o7000` (setuid, setgid, sticky) as well as the `0o177` below owner rw.
///   Only the mode changes: [`fs::set_permissions`] does not open the file for writing and no
///   byte is read or rewritten. A mode already at or tighter than `0600` (`0400`, say) is left
///   alone — the fix is for permissions that are too broad, not for an operator who chose to be
///   stricter. The first cut of this check masked the read with `0o7777` and then tested `0o177`,
///   so a seed at `04600` scored 0 and kept its setuid bit; that is why the mask below is spelled
///   out and why the regressions assert against `0o7777`, never `0o777`.
///
/// ⛔ The phrase is written and never returned, never printed, never logged and never put in an
/// error message — including the errors this validation adds, which name the path and the file
/// type and never open the file.
fn ensure_seed(path: &Path) -> Result<bool, IssuerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if !file_type.is_file() {
                let kind = if file_type.is_symlink() {
                    "a symlink"
                } else if file_type.is_dir() {
                    "a directory"
                } else {
                    "not a regular file"
                };
                return Err(IssuerError::Io(format!(
                    "{}: the mint seed path is {kind}; refusing to adopt it as this seat's seed. \
                     Move it aside and re-run, or point the seat at a different home.",
                    path.display(),
                )));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // DISALLOWED is every bit a seat-owned seed may not carry: 0o7000 the special
                // bits (setuid, setgid, sticky) and 0o177 everything below owner read+write
                // (owner-execute, and all of group and other). `0o600` is the one mode with
                // neither, so `mode & DISALLOWED != 0` is exactly "not 0600 or tighter".
                //
                // The special bits are in the mask because they are read out of it: `mode` below
                // is taken with `& 0o7777`, so a seed at `04600` yields 0o4600, and a mask of
                // 0o177 alone would score that 0 and leave the setuid bit standing on a file
                // holding a mint seed. Masking the read down to 0o777 instead would hide the bit
                // rather than clear it — the check must see what set_permissions will overwrite.
                const DISALLOWED: u32 = 0o7000 | 0o177;
                let mode = metadata.permissions().mode() & 0o7777;
                if mode & DISALLOWED != 0 {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(
                        |error| {
                            IssuerError::Io(format!(
                                "{}: found mode {mode:04o} on an existing mint seed and could not \
                                 narrow it to 0600: {error}",
                                path.display(),
                            ))
                        },
                    )?;
                }
            }
            return Ok(false);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(IssuerError::Io(format!("{}: {error}", path.display())));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| IssuerError::Io(format!("{}: {error}", parent.display())))?;
    }
    let mut entropy = [0u8; 32];
    getrandom::fill(&mut entropy).map_err(|error| IssuerError::Io(error.to_string()))?;
    if entropy.iter().all(|&byte| byte == 0) {
        return Err(IssuerError::Io("generated all-zero mint seed entropy".into()));
    }
    let mnemonic = bip39::Mnemonic::from_entropy(&entropy)
        .map_err(|error| IssuerError::Io(format!("mnemonic generation failed: {error}")))?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| IssuerError::Io(format!("{}: {error}", path.display())))?;
    // `write!` rather than a `let phrase = …` binding so the words are never held in a named local
    // any later line could reach for.
    write!(file, "{mnemonic}\n")
        .and_then(|()| file.sync_all())
        .map_err(|error| IssuerError::Io(format!("{}: {error}", path.display())))?;
    Ok(true)
}

/// The wizard: write the sidecar's files and wire this seat's config to it. LOCAL FILES ONLY — no
/// relay, no wallet, no network, and nothing is spawned or installed.
///
/// Idempotent in every part. An existing seed is kept (see [`ensure_seed`]); the config keys are set
/// to the same values a second run would set.
///
/// It writes THREE config keys, not two. `issuer_mint` and `accepted_mints` are what §4.2 needs for
/// the tag to be readable at all — a URL outside `accepted_mints` reads as UNSTATED
/// (`IssuerMintAd::from_tags`), so a seat with one and not the other advertises nothing. The third,
/// `extra_mints`, is what lets this seat's OWN wallet open its OWN mint: `wallet_ops::
/// configured_mints` is `accepted_mints[0]` plus `extra_mints`, so an issuer URL appended to
/// `accepted_mints` at position 1 would never reach `open_wallet_async`, and the seat could not hold
/// or retire the currency it issues.
pub fn init(home: &mut MaxplayerHome, options: &InitOptions) -> Result<InitReport, IssuerError> {
    validate_listen_host(&options.listen_host)?;
    let mint_url = mint_url_for(options.listen_host.trim(), options.listen_port);

    let work_dir = mint_dir(home);
    fs::create_dir_all(&work_dir)
        .map_err(|error| IssuerError::Io(format!("{}: {error}", work_dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&work_dir)
            .map_err(|error| IssuerError::Io(format!("{}: {error}", work_dir.display())))?
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&work_dir, permissions)
            .map_err(|error| IssuerError::Io(format!("{}: {error}", work_dir.display())))?;
    }

    let seed_path = seed_path(home);
    let seed_created = ensure_seed(&seed_path)?;

    let mintd_config = mintd_config_path(home);
    let rendered = render_mintd_config(options.listen_host.trim(), options.listen_port, &mint_url);
    crate::durable::write_atomic(&work_dir, &mintd_config, rendered.as_bytes())
        .map_err(|error| IssuerError::Io(format!("{}: {error}", mintd_config.display())))?;

    let url_for_edit = mint_url.clone();
    let mut added_to_accepted_mints = false;
    let mut added_to_extra_mints = false;
    let seed_for_config = seed_path.display().to_string();
    home::save_config(home, |config| {
        config.issuer_mint = Some(url_for_edit.clone());
        if !config.accepted_mints.iter().any(|m| m == &url_for_edit) {
            config.accepted_mints.push(url_for_edit.clone());
            added_to_accepted_mints = true;
        }
        if !config.extra_mints.iter().any(|m| m == &url_for_edit) {
            config.extra_mints.push(url_for_edit.clone());
            added_to_extra_mints = true;
        }
        config.mint_seed_path = Some(seed_for_config.clone());
    })?;

    Ok(InitReport {
        mint_url,
        work_dir,
        mintd_config,
        seed_path,
        seed_created,
        added_to_accepted_mints,
        added_to_extra_mints,
    })
}

/// What one [`issue`] produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueOutcome {
    pub mint_url: String,
    pub issued_sats: u64,
    /// The seat's wallet balance at that mint after issuance.
    pub balance_sats: u64,
}

/// The seat mints `sats` of its OWN currency at its OWN mint, into its OWN wallet.
///
/// This is NOT `wallet fund`, and it deliberately does not go through it: `wallet_ops::
/// begin_mint_async` refuses an issuer mint by name (`refuse_lightning_op_at_issuer`, op string
/// `"wallet fund"`), because FUNDING a mint means paying it over Lightning and an issuer mint has
/// no Lightning. Issuance is the opposite act — the seat writing its own IOU — so it has its own
/// surface, and the Lightning refusal on the wallet surface stays exactly where stage 2 put it.
///
/// The quote's invoice is never paid by anybody: a fake-wallet mint marks its own mint quote PAID
/// on its own say-so (prove-out §7 item 7). That is the whole point — an issuer mint issues on its
/// own authority, and the "payment proof" it returns is decoration.
pub async fn issue(home: &MaxplayerHome, sats: u64) -> Result<IssueOutcome, IssuerError> {
    if sats == 0 {
        return Err(IssuerError::Wallet("amount must be > 0".into()));
    }
    let mint_url = own_mint_url(home)?;
    let wallet = crate::wallet_ops::open_wallet_async(home, &mint_url)
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?;
    let quote = wallet
        .mint_quote(
            cdk::nuts::PaymentMethod::BOLT11,
            Some(cdk::Amount::from(sats)),
            None,
            None,
        )
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?;
    let issued_sats = crate::wallet_ops::poll_and_mint(&wallet, &quote.id, sats)
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?;
    let balance_sats = wallet
        .total_balance()
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?
        .to_u64();
    Ok(IssueOutcome {
        mint_url,
        issued_sats,
        balance_sats,
    })
}

/// The seat takes `sats` of its own currency back and BURNS them at its own mint.
///
/// ## The mechanism, and exactly what it is
///
/// Measured on this host, 4 Sep, against `cdk-mintd 0.17.2` in fake-wallet mode: a NUT-05 melt of a
/// well-formed bolt11 the mint never issued, that nothing pays, completes — and it moves every input
/// proof to `state='SPENT'` while signing NO new blind signature. 18 proofs / 100 sat went SPENT with
/// `sum(blind_signature.amount)` unchanged at 100, so outstanding fell 100 → 0. That is a burn, and
/// it is the only one available: the management RPC's 22 methods retire nothing, and a swap cannot
/// do it because `Mint::verify_transaction_balanced` requires `outputs == inputs - fee` exactly.
///
/// So the "invoice" below is a BURN INSTRUMENT, not a payment. It is built here, in-process, from a
/// random payment hash and an ephemeral key that is discarded on the next line; nothing can ever
/// route it, nothing will ever settle it, and no second mint is involved. The fake wallet's
/// `make_payment` reads the description as a `FakeInvoiceDescription` and, failing to parse one,
/// returns `Paid` for any bolt11 it is handed (`cdk-fake-wallet-0.17.2/src/lib.rs:661-673`).
///
/// ⛔ This is NOT `wallet melt`, and it must never be routed through it. The stage-2 refusal at
/// `wallet_ops.rs:775` stays: an operator asking to pay a Lightning invoice out of an issuer mint is
/// asking for something that cannot happen, and gets told so. Retirement is the issuer destroying
/// its own IOU, which is a different act on a different surface.
///
/// ⚠ `cdk-fake-wallet 0.17.2` reports `total_spent = amount + 1` unconditionally
/// (`src/lib.rs:730`), so the mint logs an "Over paid … Fee was too high" line and returns NO
/// change. The recorded retirement is the melt QUOTE's amount — what the seat asked to burn and the
/// mint confirmed PAID — never that inflated figure.
pub async fn retire(home: &MaxplayerHome, sats: u64) -> Result<RetirementRecord, IssuerError> {
    if sats == 0 {
        return Err(IssuerError::Wallet("amount must be > 0".into()));
    }
    let mint_url = own_mint_url(home)?;
    let wallet = crate::wallet_ops::open_wallet_async(home, &mint_url)
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?;

    let burn_instrument = burn_instrument(sats)?;
    let quote = wallet
        .melt_quote(
            cdk::nuts::PaymentMethod::BOLT11,
            &burn_instrument,
            None,
            None,
        )
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?;
    let quoted = quote.amount.to_u64();
    if quoted != sats {
        return Err(IssuerError::Wallet(format!(
            "the mint quoted {quoted} sat for a {sats} sat retirement; refusing to burn an amount \
             the seat did not choose"
        )));
    }
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?
        .to_u64();
    let need = sats.saturating_add(quote.fee_reserve.to_u64());
    if balance < need {
        return Err(IssuerError::Wallet(format!(
            "insufficient own currency to retire: balance={balance} need={need} \
             (amount+fee_reserve) at {mint_url}"
        )));
    }
    let quote_id = quote.id.clone();
    let prepared = wallet
        .prepare_melt(&quote.id, std::collections::HashMap::new())
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?;
    let confirmed = prepared
        .confirm()
        .await
        .map_err(|error| IssuerError::Wallet(error.to_string()))?;
    if confirmed.state() != cdk::nuts::MeltQuoteState::Paid {
        return Err(IssuerError::Wallet(format!(
            "retirement melt ended in state {:?}, not Paid; nothing recorded as retired",
            confirmed.state()
        )));
    }

    let record = RetirementRecord {
        at: now_unix(),
        sats,
        mint_url,
        quote_id,
    };
    // The ledger is the ONLY durable record that this burn was OURS, so it is written before the
    // caller may report success. A burn that happened and was not recorded understates `retired`
    // forever; the reverse would overstate it, which is why nothing is written before `confirm`.
    append_retirement(&retired_ledger_path(home), &record)?;
    Ok(record)
}

/// This seat's own issuer mint URL, or the refusal. The class-aware fence is asserted here rather
/// than assumed: `mint_class::mint_admitted` with the seat's OWN marker is what makes a loopback
/// `http://` mint passable at all, and it admits nothing else.
fn own_mint_url(home: &MaxplayerHome) -> Result<String, IssuerError> {
    let mint_url = home
        .config
        .issuer_mint()
        .ok_or(IssuerError::NoIssuerMint)?
        .to_owned();
    let issuers = crate::mint_class::IssuerMints::none().with_own(Some(mint_url.as_str()));
    if !crate::mint_class::mint_admitted(&mint_url, home.config.allow_real_mints, &issuers) {
        return Err(IssuerError::Wallet(format!(
            "{mint_url} is configured as this seat's issuer_mint but does not parse as a mint URL"
        )));
    }
    Ok(mint_url)
}

/// Build the burn instrument: a well-formed BOLT11 for `sats` that nothing can ever pay.
///
/// The payment hash is 32 fresh random bytes, so it names a preimage nobody holds — including us.
/// The signing key is generated here and dropped at the end of this function, so the "node" that
/// issued it ceases to exist before the invoice is used. The description is EMPTY on purpose: the
/// fake wallet tries to parse it as a `FakeInvoiceDescription` control object and falls back to its
/// defaults when that fails, so an empty description is the one that asks for no special behaviour.
fn burn_instrument(sats: u64) -> Result<cdk::Bolt11Invoice, IssuerError> {
    #[allow(deprecated)]
    use cdk::secp256k1::hashes::{sha256, Hash};
    use cdk::lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    use cdk::secp256k1::SecretKey;

    let amount_msat = sats
        .checked_mul(1_000)
        .ok_or_else(|| IssuerError::Wallet(format!("{sats} sat overflows msat")))?;

    let mut key_bytes = [0u8; 32];
    getrandom::fill(&mut key_bytes).map_err(|error| IssuerError::Io(error.to_string()))?;
    let signing_key = SecretKey::from_slice(&key_bytes)
        .map_err(|error| IssuerError::Wallet(format!("ephemeral key: {error}")))?;

    let mut hash_bytes = [0u8; 32];
    getrandom::fill(&mut hash_bytes).map_err(|error| IssuerError::Io(error.to_string()))?;
    let payment_hash = sha256::Hash::from_slice(&hash_bytes)
        .map_err(|error| IssuerError::Wallet(format!("payment hash: {error}")))?;

    let mut secret_bytes = [0u8; 32];
    getrandom::fill(&mut secret_bytes).map_err(|error| IssuerError::Io(error.to_string()))?;

    InvoiceBuilder::new(Currency::Bitcoin)
        .description(String::new())
        .payment_hash(payment_hash)
        .payment_secret(PaymentSecret(secret_bytes))
        .amount_milli_satoshis(amount_msat)
        .duration_since_epoch(std::time::Duration::from_secs(now_unix()))
        .min_final_cltv_expiry_delta(144)
        .build_signed(|hash| cdk::SECP256K1.sign_ecdsa_recoverable(hash, &signing_key))
        .map_err(|error| IssuerError::Wallet(format!("burn instrument: {error}")))
}

/// Unix seconds now. A clock before the epoch yields 0 rather than a panic: a beat with a wrong
/// `last_seen` is a readable statement; a panicking publish path is not.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-issuer-{tag}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn home_at(root: &Path) -> MaxplayerHome {
        home::bootstrap(root).expect("bootstrap")
    }

    /// The bracket rule, in both directions, with the message naming the fix.
    #[test]
    fn an_unbracketed_ipv6_listen_host_is_refused_by_name() {
        assert!(validate_listen_host("127.0.0.1").is_ok());
        assert!(validate_listen_host("[::1]").is_ok());
        assert!(validate_listen_host("localhost").is_ok());
        let refusal = validate_listen_host("::1").expect_err("bare ::1 is refused");
        let text = refusal.to_string();
        assert!(text.contains("[::1]"), "{text}");
        assert!(text.contains("invalid socket address syntax"), "{text}");
        assert!(validate_listen_host("").is_err());
    }

    /// The rendered config carries the four required `[ln]` bounds and NO mnemonic key.
    #[test]
    fn the_rendered_mintd_config_has_the_required_bounds_and_no_mnemonic() {
        let rendered = render_mintd_config("127.0.0.1", 3338, "http://127.0.0.1:3338/");
        for required in ["min_mint", "max_mint", "min_melt", "max_melt"] {
            assert!(rendered.contains(required), "missing {required}");
        }
        assert!(
            !rendered.contains("mnemonic"),
            "the seed must never reach the config file: {rendered}"
        );
        assert!(rendered.contains(r#"listen_host = "127.0.0.1""#), "{rendered}");
        assert!(rendered.contains(r#"ln_backend = "fakewallet""#), "{rendered}");
    }

    /// The wizard is idempotent and NEVER overwrites a seed. A lost mint seed is lost money-shaped
    /// state; the second run must keep the first run's file byte for byte.
    #[test]
    fn init_keeps_an_existing_seed_and_is_idempotent() {
        let root = temp_root("init");
        let mut home = home_at(&root);
        let first = init(&mut home, &InitOptions::default()).expect("first init");
        assert!(first.seed_created, "a fresh home has no seed yet");
        assert!(first.added_to_accepted_mints);
        assert!(first.added_to_extra_mints);
        let seed_bytes = fs::read(&first.seed_path).expect("seed readable");

        let second = init(&mut home, &InitOptions::default()).expect("second init");
        assert!(!second.seed_created, "an existing seed is KEPT");
        assert!(!second.added_to_accepted_mints, "no duplicate entry");
        assert!(!second.added_to_extra_mints, "no duplicate entry");
        assert_eq!(
            seed_bytes,
            fs::read(&second.seed_path).expect("seed still readable"),
            "the seed file was rewritten — that is lost money-shaped state"
        );

        assert_eq!(home.config.issuer_mint(), Some("http://127.0.0.1:3338/"));
        assert_eq!(
            home.config
                .accepted_mints
                .iter()
                .filter(|m| *m == "http://127.0.0.1:3338/")
                .count(),
            1
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A seed that was ALREADY on disk, and already too readable, is narrowed — not rewritten.
    ///
    /// `init_keeps_an_existing_seed_and_is_idempotent` cannot catch this: its "existing" seed is
    /// the one the first `init` wrote, which was born `0600`, so the keep path is never asked to
    /// judge a file it did not create. This starts from a `0644` file some earlier hand left and
    /// asserts the two halves separately — the bytes are IDENTICAL (a seed is money-shaped state;
    /// rewriting it is losing it) and the mode ends at `0600`.
    ///
    /// Every mode assertion here masks `0o7777`, never `0o777`. A `0o777` mask cannot witness the
    /// special bits at all: it scores `04600` as `0600` and calls a setuid seed narrowed. That is
    /// the exact hole the first cut of this test left open, and
    /// [`an_existing_setuid_seed_has_its_special_bits_cleared`] is the case that walks through it.
    #[cfg(unix)]
    #[test]
    fn an_existing_over_readable_seed_is_narrowed_to_0600_without_touching_its_bytes() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("seed-mode");
        let mut home = home_at(&root);
        let path = seed_path(&home);

        // Not a real mnemonic on purpose: this test must never depend on, or produce, a usable
        // seed. What is under test is custody of the bytes, whatever they are.
        let planted = b"planted seed bytes, not a mnemonic\n";
        fs::write(&path, planted).expect("plant a seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("make it too broad");
        let before = fs::metadata(&path).expect("planted metadata");
        let mtime_before = before.modified().expect("planted mtime");
        assert_eq!(before.permissions().mode() & 0o7777, 0o644, "precondition");

        let report = init(&mut home, &InitOptions::default()).expect("init over a planted seed");
        assert!(!report.seed_created, "an existing seed is KEPT, never regenerated");

        assert_eq!(
            fs::read(&path).expect("seed still readable"),
            planted,
            "the seed bytes changed — set_permissions must not rewrite the file"
        );
        let after = fs::metadata(&path).expect("metadata after init");
        assert_eq!(
            after.permissions().mode() & 0o7777,
            0o600,
            "mode after init was {:04o}, not 0600",
            after.permissions().mode() & 0o7777
        );
        assert_eq!(
            after.modified().expect("mtime after"),
            mtime_before,
            "the file's contents were touched, not just its mode"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// A REGULAR seed carrying a special bit is narrowed too — `04600` must not survive `init`.
    ///
    /// This is the class the first cut of the check could not see. It masked the read with
    /// `0o7777` and then tested `mode & 0o177`, and `0o4600 & 0o177 == 0`, so the branch was
    /// skipped and the setuid bit stood on a file holding a mint seed. The sibling regression
    /// could not catch it either, because it asserted `mode & 0o777 == 0o600` and
    /// `0o4600 & 0o777 == 0o600` passes. Both mistakes are the same mistake — a mask narrower
    /// than the value being judged — so this test asserts `0o7777` end to end.
    ///
    /// `04600` is a regular file, not a special one: `symlink_metadata().file_type().is_file()`
    /// is true for it, so it reaches the mode branch rather than the refusal branch. Setuid on a
    /// non-executable data file grants nothing by itself; it is cleared because a seat that
    /// promises "0600" must not leave a bit it never inspected on money-shaped state.
    #[cfg(unix)]
    #[test]
    fn an_existing_setuid_seed_has_its_special_bits_cleared() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("seed-setuid");
        let mut home = home_at(&root);
        let path = seed_path(&home);

        let planted = b"planted seed bytes, not a mnemonic\n";
        fs::write(&path, planted).expect("plant a seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o4600)).expect("plant setuid 04600");
        let before = fs::metadata(&path).expect("planted metadata");
        let mtime_before = before.modified().expect("planted mtime");
        assert_eq!(
            before.permissions().mode() & 0o7777,
            0o4600,
            "precondition: the plant must really carry the setuid bit"
        );
        assert!(
            before.file_type().is_file(),
            "precondition: 04600 is a REGULAR file, so it reaches the mode branch"
        );
        // The trap, asserted so it can never be re-introduced silently: under the old 0o177 mask
        // this mode scores zero, and under a 0o777 assertion it reads as already-correct.
        assert_eq!(0o4600 & 0o177, 0, "the old mask really was blind to this");
        assert_eq!(0o4600 & 0o777, 0o600, "the old assertion really did pass this");

        let report = init(&mut home, &InitOptions::default()).expect("init over a setuid seed");
        assert!(!report.seed_created, "an existing seed is KEPT, never regenerated");

        assert_eq!(
            fs::read(&path).expect("seed still readable"),
            planted,
            "the seed bytes changed — set_permissions must not rewrite the file"
        );
        let after = fs::metadata(&path).expect("metadata after init");
        assert_eq!(
            after.permissions().mode() & 0o7777,
            0o600,
            "mode after init was {:04o}, not 0600 — the special bits survived",
            after.permissions().mode() & 0o7777
        );
        assert_eq!(
            after.modified().expect("mtime after"),
            mtime_before,
            "the file's contents were touched, not just its mode"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// A seed PATH that is not a regular file is refused by name, and nothing is written through it.
    ///
    /// A symlink is refused even though its target here is a perfectly good `0600` file: the seat
    /// cannot promise the mode, or the identity, of a path it does not own. `symlink_metadata` is
    /// what makes this reachable — plain `metadata` would report the target and let the link pass.
    #[cfg(unix)]
    #[test]
    fn a_seed_path_that_is_not_a_regular_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("seed-type");
        let mut home = home_at(&root);
        let path = seed_path(&home);

        let target = root.join("elsewhere");
        fs::write(&target, b"target bytes\n").expect("write link target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("0600 target");
        std::os::unix::fs::symlink(&target, &path).expect("symlink into the seed path");

        let refusal = init(&mut home, &InitOptions::default()).expect_err("a symlink is refused");
        let text = refusal.to_string();
        assert!(text.contains("symlink"), "the refusal must name what it found: {text}");
        assert!(
            text.contains(&path.display().to_string()),
            "the refusal must name the path: {text}"
        );
        assert_eq!(
            fs::read(&target).expect("target still readable"),
            b"target bytes\n",
            "the refusal wrote through the link"
        );

        // And a directory in the same place is refused too, by a different branch.
        fs::remove_file(&path).expect("remove the symlink");
        fs::create_dir(&path).expect("plant a directory");
        let refusal = init(&mut home, &InitOptions::default()).expect_err("a directory is refused");
        assert!(
            refusal.to_string().contains("directory"),
            "{refusal}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// The seed is `0600` and its words appear in NO artefact this seat writes.
    #[test]
    fn the_seed_is_owner_only_and_never_leaves_its_file() {
        let root = temp_root("seed");
        let mut home = home_at(&root);
        let report = init(&mut home, &InitOptions::default()).expect("init");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&report.seed_path)
                .expect("seed metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "seed mode {mode:#o}");
        }
        let phrase = fs::read_to_string(&report.seed_path).expect("seed");
        let words: Vec<&str> = phrase.split_whitespace().collect();
        assert_eq!(words.len(), 24, "256-bit mnemonic");
        // Three consecutive seed words, every window. A SINGLE word would false-positive on prose
        // (the BIP39 list is ordinary English), while three in a row cannot occur by accident and
        // any real leak — a whole phrase, or a truncated one — contains at least one such window.
        let windows: Vec<String> = words.windows(3).map(|w| w.join(" ")).collect();

        for artefact in [report.mintd_config.clone(), home.root.join("config.toml")] {
            let text = fs::read_to_string(&artefact).expect("artefact readable");
            assert!(
                !text.contains(phrase.trim()),
                "{} carries the whole seed",
                artefact.display()
            );
            for window in &windows {
                assert!(
                    !text.contains(window.as_str()),
                    "{} carries seed words {window:?}",
                    artefact.display()
                );
            }
        }
        // The status surface never prints it either — it has no field for it.
        let rendered = serde_json::to_string(&IssuerStatus {
            mint_url: report.mint_url.clone(),
            work_dir: report.work_dir.display().to_string(),
            issued_sats: 0,
            redeemed_sats: 0,
            outstanding_sats: 0,
            retired_sats: 0,
            last_seen: 0,
            in_accepted_mints: true,
        })
        .expect("status serializes");
        for window in &windows {
            assert!(!rendered.contains(window.as_str()), "{rendered}");
        }
        // And no error this module can raise carries one either: the seed path appears in errors,
        // the seed's CONTENTS never do.
        let refusal = ensure_seed(&report.seed_path);
        assert_eq!(refusal.expect("an existing seed is kept, not an error"), false);
        let _ = fs::remove_dir_all(&root);
    }

    /// Build a mint-shaped sqlite with the two tables the counters read, so the QUERIES are under
    /// test rather than a hand-computed number. Column shapes match what `cdk-mintd 0.17.2` creates
    /// (verified against a live 0.17.2 work dir on 4 Sep: `blind_signature.amount`, `proof.amount`,
    /// `proof.state`).
    fn mint_db_with(path: &Path, signed: &[u64], proofs: &[(u64, &str)]) {
        let connection = rusqlite::Connection::open(path).expect("create");
        connection
            .execute_batch(
                "create table blind_signature (amount integer not null);
                 create table proof (y blob, amount integer not null, state text not null);",
            )
            .expect("schema");
        for amount in signed {
            connection
                .execute("insert into blind_signature (amount) values (?1)", [*amount])
                .expect("insert sig");
        }
        for (amount, state) in proofs {
            connection
                .execute(
                    "insert into proof (y, amount, state) values (randomblob(33), ?1, ?2)",
                    rusqlite::params![*amount, *state],
                )
                .expect("insert proof");
        }
    }

    /// §5's gate, with BOTH sides printed: `outstanding == issued - redeemed` is definitional, and
    /// `retired <= redeemed` is the bound that keeps the seat's own count honest — every retirement
    /// is one of the redemptions the mint counted, so a seat claiming to have burned more than the
    /// mint ever burned is claiming an impossibility.
    #[test]
    fn the_counters_balance_and_retired_never_exceeds_redeemed() {
        let root = temp_root("gate");
        let mut home = home_at(&root);
        init(&mut home, &InitOptions::default()).expect("init");
        // 100 issued; 40 of it redeemed (a PENDING proof is not redeemed and must not count).
        mint_db_with(
            &mint_db_path(&home),
            &[64, 32, 4],
            &[(32, "SPENT"), (8, "SPENT"), (16, "PENDING")],
        );
        let counters = read_counters(&mint_db_path(&home)).expect("counters");
        println!(
            "issued={} redeemed={} outstanding={}",
            counters.issued_sats, counters.redeemed_sats, counters.outstanding_sats
        );
        assert_eq!(counters.issued_sats, 100);
        assert_eq!(
            counters.redeemed_sats, 40,
            "a PENDING proof is not redeemed"
        );
        assert_eq!(
            counters.outstanding_sats,
            counters.issued_sats - counters.redeemed_sats,
            "outstanding ({}) != issued ({}) - redeemed ({})",
            counters.outstanding_sats,
            counters.issued_sats,
            counters.redeemed_sats
        );

        let ledger = retired_ledger_path(&home);
        for sats in [25_u64, 15] {
            append_retirement(
                &ledger,
                &RetirementRecord {
                    at: now_unix(),
                    sats,
                    mint_url: home.config.issuer_mint().expect("set").to_owned(),
                    quote_id: format!("q{sats}"),
                },
            )
            .expect("append");
        }
        let retired = retired_total(&ledger).expect("total");
        println!("retired={retired} redeemed={}", counters.redeemed_sats);
        assert_eq!(retired, 40);
        assert!(
            retired <= counters.redeemed_sats,
            "retired ({retired}) > redeemed ({})",
            counters.redeemed_sats
        );

        // And the beat says the same numbers.
        let ad = counters_advertisement(&home).expect("the files say so");
        assert_eq!(ad.outstanding_sats, 60);
        assert_eq!(ad.retired_sats, 40);
        assert_eq!(ad.mint_url, "http://127.0.0.1:3338/");
        assert!(ad.last_seen > 0, "last_seen is the instant of the read");

        let reported = status(&home).expect("status");
        assert_eq!(reported.issued_sats, 100);
        assert_eq!(reported.redeemed_sats, 40);
        assert_eq!(reported.outstanding_sats, 60);
        assert_eq!(reported.retired_sats, 40);
        assert!(reported.in_accepted_mints);
        let _ = fs::remove_dir_all(&root);
    }

    /// The read is READ-ONLY, and that is enforced by the handle rather than intended by the caller.
    #[test]
    fn the_counter_read_cannot_write_the_mints_ledger() {
        let root = temp_root("ro");
        let mut home = home_at(&root);
        init(&mut home, &InitOptions::default()).expect("init");
        let db = mint_db_path(&home);
        mint_db_with(&db, &[8], &[(8, "SPENT")]);
        let connection = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("read-only open");
        let refusal = connection
            .execute("insert into blind_signature (amount) values (1)", [])
            .expect_err("a read-only handle must refuse a write");
        assert!(
            refusal.to_string().to_lowercase().contains("readonly"),
            "{refusal}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// NEGATIVE 1 of 4 (§6): the seat states no issuer mint ⇒ no tag.
    #[test]
    fn a_seat_with_no_issuer_mint_advertises_nothing() {
        let root = temp_root("none");
        let home = home_at(&root);
        assert_eq!(home.config.issuer_mint(), None);
        assert!(counters_advertisement(&home).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// NEGATIVE 2 of 4 (§6): the sidecar is DOWN — the work dir has no sqlite ⇒ no tag, no error.
    #[test]
    fn a_down_sidecar_advertises_nothing_and_does_not_fail() {
        let root = temp_root("down");
        let mut home = home_at(&root);
        init(&mut home, &InitOptions::default()).expect("init");
        assert!(
            !mint_db_path(&home).exists(),
            "init must not create the mint's database — only cdk-mintd does"
        );
        assert!(counters_advertisement(&home).is_none());
        assert!(matches!(
            status(&home),
            Err(IssuerError::Counters(_)),
            ));
        let _ = fs::remove_dir_all(&root);
    }

    /// NEGATIVE 2 of 4, the half that only a LIVE check can make: a sidecar that has run, written a
    /// perfectly readable database, and then DIED. The files still answer; the mint does not. A beat
    /// built from the files alone would state `outstanding` with `last_seen = now` for a mint that
    /// is gone — a wrong number, which §6 forbids.
    #[tokio::test]
    async fn a_sidecar_that_died_leaves_readable_files_and_still_advertises_nothing() {
        let root = temp_root("dead");
        let mut home = home_at(&root);
        // A port nothing is listening on: bound, its number taken, then released.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            drop(listener);
            port
        };
        init(
            &mut home,
            &InitOptions {
                listen_host: DEFAULT_LISTEN_HOST.to_owned(),
                listen_port: port,
            },
        )
        .expect("init");
        mint_db_with(&mint_db_path(&home), &[64, 32, 4], &[(32, "SPENT")]);

        // The FILES are entirely happy...
        let from_files = counters_advertisement(&home).expect("the files read fine");
        assert_eq!(from_files.outstanding_sats, 68);
        // ...and the producer still states nothing, because nothing is serving that URL.
        assert!(
            advertisement(&home).await.is_none(),
            "a dead sidecar's stale counters must never reach the wire"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// NEGATIVE 3 of 4 (§6): a sqlite that will not parse as a mint DB ⇒ no tag.
    #[test]
    fn an_unreadable_mint_database_advertises_nothing() {
        let root = temp_root("badsql");
        let mut home = home_at(&root);
        init(&mut home, &InitOptions::default()).expect("init");
        fs::write(mint_db_path(&home), b"this is not a sqlite database").expect("write junk");
        assert!(read_counters(&mint_db_path(&home)).is_err());
        assert!(counters_advertisement(&home).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// NEGATIVE 4 of 4 (§6): the URL is absent from `accepted_mints` ⇒ no tag, because
    /// `IssuerMintAd::from_tags` would read it as UNSTATED anyway.
    #[test]
    fn an_issuer_url_outside_accepted_mints_advertises_nothing() {
        let root = temp_root("unlisted");
        let mut home = home_at(&root);
        init(&mut home, &InitOptions::default()).expect("init");
        let url = home.config.issuer_mint().expect("set by init").to_owned();
        home::save_config(&mut home, |config| {
            config.accepted_mints.retain(|m| m != &url);
        })
        .expect("save");
        assert!(counters_advertisement(&home).is_none());
        // ...and the mirror: a reader would have dropped it too.
        let ad = IssuerMintAd {
            mint_url: url.clone(),
            outstanding_sats: 7,
            retired_sats: 0,
            last_seen: 1,
        };
        assert_eq!(
            IssuerMintAd::from_tags(&[ad.to_tag()], &home.config.accepted_mints),
            None
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A ledger line that will not parse is an ERROR to `status` and SILENCE to the beat. Both
    /// halves matter: the operator asking directly gets the truth; the wire gets no wrong number.
    #[test]
    fn a_corrupt_retirement_ledger_is_reported_and_silences_the_tag() {
        let root = temp_root("ledger");
        let mut home = home_at(&root);
        init(&mut home, &InitOptions::default()).expect("init");
        let ledger = retired_ledger_path(&home);
        assert_eq!(retired_total(&ledger).expect("absent is zero"), 0);

        append_retirement(
            &ledger,
            &RetirementRecord {
                at: 1,
                sats: 40,
                mint_url: "http://127.0.0.1:3338/".into(),
                quote_id: "q1".into(),
            },
        )
        .expect("append");
        append_retirement(
            &ledger,
            &RetirementRecord {
                at: 2,
                sats: 2,
                mint_url: "http://127.0.0.1:3338/".into(),
                quote_id: "q2".into(),
            },
        )
        .expect("append");
        assert_eq!(retired_total(&ledger).expect("sums"), 42);

        let mut raw = fs::read_to_string(&ledger).expect("read");
        raw.push_str("{not json}\n");
        fs::write(&ledger, raw).expect("corrupt");
        assert!(matches!(retired_total(&ledger), Err(IssuerError::Ledger(_))));
        assert!(counters_advertisement(&home).is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
