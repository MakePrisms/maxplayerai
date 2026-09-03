//! Buyer wallet setup for packaged `~/.maxplayer`: open the CDK wallet at a mint, derive its seed from
//! the nostr secret, and read its balance. The wallet opens at the configured mint
//! ([`crate::home::MaxplayerConfig::default_mint`]) or at an explicit realized mint
//! ([`open_wallet_at_mint_async`]). Which mints this seat may use is its configured list; the open
//! path only checks the URL is a usable mint URL.
//!
//! Funding a wallet (mint quote → surface the bolt11 invoice → wait for payment → mint) lives in
//! [`crate::wallet_ops`]: `begin_mint_async` returns the invoice up front and `complete_mint_async`
//! finishes once it is paid.

use std::path::Path;
use std::sync::Arc;

use cdk::nuts::CurrencyUnit;
use cdk::wallet::Wallet;
use cdk_sqlite::wallet::WalletSqliteDatabase;
use sha2::{Digest, Sha256};

use crate::home::{self, HomeError, MaxplayerHome};

#[derive(Debug)]
pub enum FundError {
    Home(HomeError),
    Wallet(String),
    /// The configured mint is not permitted under the real-mint fence (issue #49): a real mint with
    /// The mint URL is not a well-formed `http://` / `https://` mint URL. Fail-closed before
    /// opening/quoting the wallet. Mint POLICY is not decided here: the seat's configured list is
    /// the only gate, and it is applied by the callers that own a config.
    MintNotAllowed { mint_url: String },
}

impl std::fmt::Display for FundError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Home(error) => write!(formatter, "{error}"),
            Self::Wallet(message) => write!(formatter, "wallet fund error: {message}"),
            Self::MintNotAllowed { mint_url } => write!(
                formatter,
                "mint {mint_url} is not a usable mint URL (expected http:// or https:// with a host)"
            ),
        }
    }
}

impl std::error::Error for FundError {}

impl From<HomeError> for FundError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

/// Expand the 32-byte nostr secret into a 64-byte cdk wallet seed (deterministic).
pub fn seed_from_secret_hex(secret_hex: &str) -> Result<[u8; 64], FundError> {
    let bytes = hex::decode(secret_hex.trim())
        .map_err(|error| FundError::Wallet(format!("secret key hex decode: {error}")))?;
    if bytes.len() != 32 {
        return Err(FundError::Wallet(format!(
            "secret key must decode to 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut seed = [0u8; 64];
    seed[..32].copy_from_slice(&bytes);
    let digest = Sha256::digest(&bytes);
    seed[32..].copy_from_slice(&digest);
    Ok(seed)
}

fn sqlite_path(wallet_dir: &Path) -> std::path::PathBuf {
    wallet_dir.join("cdk-wallet.sqlite")
}

/// Open the wallet at the configured default mint (async). Prefer this inside an existing
/// runtime — [`open_wallet_blocking`] fails fast if a Tokio runtime is
/// already current (no nested `block_on` panic).
pub async fn open_wallet_async(home: &MaxplayerHome) -> Result<Wallet, FundError> {
    // The wallet opens at the CONFIGURED mint (`MaxplayerConfig::default_mint`), the same
    // source of truth the pay path resolves the realized mint from — no compile-time pin.
    open_wallet_at_mint_async(home, home.config.default_mint()).await
}

/// Open the shared wallet store at an EXPLICIT mint (async). Multi-mint redemption: a buyer may
/// pay at any mint in the seller's accepted set, so the seller must open the wallet at the
/// buyer's REALIZED mint (the NUT-18 payload mint), not the seller default — otherwise
/// `require_wallet_matches` refuses every non-default-mint payment. The store (sqlite) is shared
/// across mints; only the `Wallet`'s bound mint differs.
pub async fn open_wallet_at_mint_async(
    home: &MaxplayerHome,
    mint_url: &str,
) -> Result<Wallet, FundError> {
    // Shape check only: refuse a string that could never be a mint before opening/quoting a
    // wallet at it. WHICH mints this seat may use is decided by its configured list, in the callers
    // that own the config (`wallet_ops::mint_is_allowed`, the creq accepted set) — there is no
    // second policy gate here.
    if !home::mint_url_supported(mint_url) {
        return Err(FundError::MintNotAllowed {
            mint_url: mint_url.to_owned(),
        });
    }
    let secret = home::read_secret_key_hex(home)?;
    let seed = seed_from_secret_hex(&secret)?;
    let path = sqlite_path(&home.wallet_dir);
    let store = WalletSqliteDatabase::new(path)
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    Wallet::new(mint_url, CurrencyUnit::Sat, Arc::new(store), seed, None)
        .map_err(|error| FundError::Wallet(error.to_string()))
}

/// Thin sync wrapper for non-async callers (CLI / tests).
/// Do **not** call from inside an existing tokio runtime — use
/// [`open_wallet_async`] instead. Nested call fails fast (no panic).
pub fn open_wallet_blocking(home: &MaxplayerHome) -> Result<Wallet, FundError> {
    crate::runtime_guard::refuse_nested_block_on("open_wallet_blocking")
        .map_err(FundError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    runtime.block_on(open_wallet_async(home))
}

/// Read current wallet balance against the configured default mint.
pub async fn wallet_balance_sats(home: &MaxplayerHome) -> Result<u64, FundError> {
    let wallet = open_wallet_async(home).await?;
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    Ok(balance.to_u64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::{bootstrap, MaxplayerConfig};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-fund-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn seed_is_deterministic_64_bytes() {
        let secret = "11".repeat(32);
        let a = seed_from_secret_hex(&secret).expect("a");
        let b = seed_from_secret_hex(&secret).expect("b");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    // The wallet opens at the CONFIGURED mint, not a compile-time pin — a buyer configured at a
    // non-default mint spends from that mint (no `MintPinned` refusal). A mint is just a mint: what
    // decides is the seat's configured list, which this test sets.
    #[test]
    fn wallet_opens_at_the_configured_mint() {
        let root = temp_home("cfg-mint");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec!["https://minibits.example".into()];
        let wallet = open_wallet_blocking(&home).expect("open at configured mint");
        assert_eq!(wallet.mint_url.to_string(), "https://minibits.example");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Z2 (multi-mint redeem): the seller opens the shared store at the buyer's REALIZED mint, which
    // may be a NON-default member of accepted_mints (default_mint == accepted_mints[0]). The opened
    // wallet binds to the requested realized mint, not the config default — the fix that lets
    // `require_wallet_matches` pass for a non-default-mint payment.
    #[tokio::test(flavor = "current_thread")]
    async fn open_wallet_at_mint_binds_to_realized_non_default_mint() {
        let root = temp_home("realized-mint");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        // accepted[0] (== default) is the seller default; accepted[1] is the realized mint.
        home.config.accepted_mints = vec![
            "https://default-mint.example".into(),
            "https://realized-mint.example".into(),
        ];
        assert_eq!(home.config.default_mint(), "https://default-mint.example");
        let realized = "https://realized-mint.example";
        let wallet = open_wallet_at_mint_async(&home, realized)
            .await
            .expect("open at realized mint");
        assert_eq!(wallet.mint_url.to_string(), realized);
        assert_ne!(wallet.mint_url.to_string(), home.config.default_mint());
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding T(2), one-mint form: the wallet open fails closed BEFORE any network open/quote on a
    // string that is not a usable mint URL, and opens on either scheme when it is. There is no
    // real/test mint distinction left to fence on — `http://` is admitted deliberately, because a
    // seat's own sidecar mint runs on loopback.
    #[test]
    fn open_wallet_refuses_a_url_that_is_not_a_mint_and_admits_both_schemes() {
        let root = temp_home("open-mint-url-shape");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        for bad in ["not-a-url", "ftp://mint.example", "https://", ""] {
            home.config.accepted_mints = vec![bad.to_owned()];
            let err = open_wallet_blocking(&home)
                .expect_err("a string that is not a mint URL must refuse before any network touch");
            assert!(
                matches!(&err, FundError::MintNotAllowed { mint_url } if mint_url == bad),
                "expected MintNotAllowed for {bad:?}, got {err:?}"
            );
        }

        for good in ["https://real-mint.example/", "http://127.0.0.1:3338"] {
            home.config.accepted_mints = vec![good.to_owned()];
            let wallet = open_wallet_blocking(&home)
                .unwrap_or_else(|error| panic!("a configured {good} must open: {error:?}"));
            assert_eq!(wallet.mint_url.to_string(), good.trim_end_matches('/'));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_open_refuses_inside_runtime() {
        let root = temp_home("nested-open-refuse");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = open_wallet_blocking(&home).expect_err("must refuse nested block_on");
        let message = err.to_string();
        assert!(
            message.contains("nested block_on refused"),
            "unexpected: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_config_mint_is_the_shipped_minibits_default() {
        // Issue #378 flipped the shipped default mint to a minibits mint. A fresh config's default
        // mint is that minibits mint.
        assert_eq!(
            MaxplayerConfig::default().default_mint(),
            crate::home::DEFAULT_MINIBITS_MINT_URL
        );
    }
}
