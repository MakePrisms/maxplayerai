//! Flexible ecash wallet ops for `maxplayer wallet` / MCP mirrors, over the packaged CDK wallet at
//! `home/.maxplayer/wallet`. This module owns the mint-fund path: [`begin_mint_async`] creates a mint
//! quote and returns the bolt11 invoice up front, then [`complete_mint_async`] mints once it is
//! paid. ([`crate::buyer_fund`] covers wallet open, seed derivation, and balance read.)
//!
//! **Funding assumption:** a mint invoices, and someone pays that invoice. [`begin_mint_async`]
//! returns the bolt11 and the caller pays it, then calls [`complete_mint_async`]. No mint is
//! special-cased: a mint is a mint, and the seat's configured list decides which ones it uses.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use cashu::{MintUrl, Token};
use sha2::{Digest, Sha256};
use cdk::cdk_database::WalletDatabase;
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::wallet::WalletSqliteDatabase;

use crate::buyer_fund::seed_from_secret_hex;
use crate::home::{self, HomeError, MaxplayerHome};

#[derive(Debug)]
pub enum WalletOpsError {
    Home(HomeError),
    /// The mint is not in this home's configured set (`accepted_mints`/`extra_mints`) — a
    /// MEMBERSHIP miss, cleared by `maxplayer wallet mints add`. `default_mint` carries the home's
    /// ACTUAL default (`config.default_mint()`) so the Display names the mint this home really
    /// pins rather than a constant (#506). This is the ONLY mint policy gate there is.
    MintNotAllowed { mint_url: String, default_mint: String },
    /// `remove_mint` refuses to remove the home's pinned default mint. `mint_url` carries that
    /// actual default (`config.default_mint()`) so the message names the real pinned mint rather
    /// than a hardcoded constant — on a real-minibits home the constant would be a false-default
    /// lie (#579).
    MintPinnedDefault { mint_url: String },
    Wallet(String),
}

impl std::fmt::Display for WalletOpsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Home(error) => write!(formatter, "{error}"),
            Self::MintNotAllowed { mint_url, default_mint } => write!(
                formatter,
                "mint {mint_url} is not configured; add it with `maxplayer wallet mints add` (default stays {default_mint})"
            ),
            Self::MintPinnedDefault { mint_url } => write!(
                formatter,
                "cannot remove the default mint ({mint_url}); only extra_mints are removable"
            ),
            Self::Wallet(message) => write!(formatter, "wallet error: {message}"),
        }
    }
}

impl std::error::Error for WalletOpsError {}

impl From<HomeError> for WalletOpsError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintBalance {
    pub mint_url: String,
    pub balance_sats: u64,
    pub is_default: bool,
    /// Whether the mint is in this home's configured set (default + `extra_mints`). Rows with
    /// `configured == false` are DISCOVERED — the shared wallet DB holds proofs or a registration
    /// for a mint the config no longer (or never) names. Display surfaces them (#266); accept-time
    /// source selection deliberately ignores them (see `crossmint::holds_at_least`).
    pub configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintOutcome {
    pub mint_url: String,
    pub invoice: String,
    pub quote_id: String,
    pub funded_sats: u64,
    pub balance_sats: u64,
}

/// Bolt11 mint quote ready for payment (invoice is available before any wait).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintQuote {
    pub mint_url: String,
    pub invoice: String,
    pub quote_id: String,
    pub amount_sats: u64,
}

/// Result of a mint attempt: auto-paid fund, or invoice awaiting external pay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintFlow {
    Funded(MintOutcome),
    /// Non-autopay mint: bolt11 surfaced; pay then [`complete_mint_async`].
    NeedsPayment(MintQuote),
}

#[derive(PartialEq, Eq)]
pub struct SendOutcome {
    pub mint_url: String,
    pub sent_sats: u64,
    pub balance_sats: u64,
    /// Bearer cashu token — spendable ecash. Never emitted by [`Debug`] (redacted below); read the
    /// field directly to hand the token to the payee.
    pub token: String,
}

// Manual Debug: the `token` field is a BEARER cashu token (spendable ecash). A derived Debug would
// print it verbatim, so any debug log of a `SendOutcome` would leak spendable funds. Redact it to a
// SHA-256 hash prefix (identifies the token for correlation without exposing spendable material).
// `Clone` is intentionally NOT derived: nothing needs to duplicate a bearer token, and each extra
// copy is another place it can leak.
impl std::fmt::Debug for SendOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SendOutcome")
            .field("mint_url", &self.mint_url)
            .field("sent_sats", &self.sent_sats)
            .field("balance_sats", &self.balance_sats)
            .field("token", &redact_secret(&self.token))
            .finish()
    }
}

/// Render a secret as `<redacted:sha256:HEX12>` — a stable 12-hex-char digest prefix that lets two
/// log lines be correlated to the same secret without exposing any spendable material. An empty
/// secret renders `<redacted:empty>` (no digest of nothing).
fn redact_secret(secret: &str) -> String {
    if secret.is_empty() {
        return "<redacted:empty>".to_string();
    }
    let digest = Sha256::digest(secret.as_bytes());
    format!("<redacted:sha256:{}>", &hex::encode(digest)[..12])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveOutcome {
    pub mint_url: String,
    pub received_sats: u64,
    pub balance_sats: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeltOutcome {
    pub mint_url: String,
    pub paid_sats: u64,
    pub fee_sats: u64,
    pub balance_sats: u64,
}

fn sqlite_path(wallet_dir: &Path) -> std::path::PathBuf {
    wallet_dir.join("cdk-wallet.sqlite")
}

/// Normalize a mint URL (trim, strip trailing `/`, parse as [`MintUrl`]).
pub fn normalize_mint_url(raw: &str) -> Result<String, WalletOpsError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(WalletOpsError::Wallet("mint URL is empty".into()));
    }
    let parsed = MintUrl::from_str(trimmed)
        .map_err(|error| WalletOpsError::Wallet(format!("invalid mint URL: {error}")))?;
    Ok(parsed.to_string())
}


/// Configured mints: default `mint_url` first, then opt-in `extra_mints` (deduped).
pub fn configured_mints(home: &MaxplayerHome) -> Result<Vec<String>, WalletOpsError> {
    let mut out = Vec::new();
    let default = normalize_mint_url(home.config.default_mint())?;
    out.push(default.clone());
    for extra in &home.config.extra_mints {
        let normalized = normalize_mint_url(extra)?;
        if !out.iter().any(|existing| existing == &normalized) {
            out.push(normalized);
        }
    }
    Ok(out)
}

fn mint_is_allowed(home: &MaxplayerHome, mint_url: &str) -> Result<String, WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let allowed = configured_mints(home)?;
    if allowed.iter().any(|entry| entry == &normalized) {
        Ok(normalized)
    } else {
        Err(WalletOpsError::MintNotAllowed {
            mint_url: normalized,
            default_mint: home.config.default_mint().to_string(),
        })
    }
}

/// Resolve the reported post-confirm balance from a balance-read result (finding U). A cashu
/// `confirm` is the effect boundary — the ecash has already moved — so a read failure, or a
/// stale/equal balance, must NEVER make the caller discard the confirmed token/outcome: report the
/// read balance when available, otherwise a best-effort `before - spent` estimate (the authoritative
/// record is the returned token / paid+fee). `op` is `"send"`/`"melt"` for the diagnostic. Pure so
/// "a read failure still yields the outcome" is unit-testable without a mint.
fn post_confirm_balance(read: Result<u64, String>, before: u64, spent_sats: u64, op: &str) -> u64 {
    match read {
        Ok(balance) => {
            if balance >= before {
                eprintln!(
                    "wallet {op} WARN: post-confirm balance did not decrease (before={before} \
                     after={balance}); returning the confirmed outcome anyway ({op} already happened)"
                );
            }
            balance
        }
        Err(error) => {
            eprintln!(
                "wallet {op} WARN: post-confirm balance read failed (returning the confirmed outcome \
                 anyway; {op} already happened): {error}"
            );
            before.saturating_sub(spent_sats)
        }
    }
}

/// Resolve the reported post-receive balance from a balance-read result (finding X, sibling of
/// finding U). A successful `receive` is the effect boundary — the token's proofs are already
/// redeemed into the wallet — so a read failure, or a stale/non-increasing balance, must NEVER make
/// the caller discard the credited outcome (a discarded outcome retries into an already-spent
/// token): report the read balance when available, otherwise a best-effort `before + received`
/// estimate. Pure so "a read failure still yields the outcome" is unit-testable without a mint.
fn post_receive_balance(read: Result<u64, String>, before: u64, received_sats: u64) -> u64 {
    match read {
        Ok(balance) => {
            if balance <= before {
                eprintln!(
                    "wallet receive WARN: post-receive balance did not increase (before={before} \
                     after={balance}); returning the credited outcome anyway (receive already happened)"
                );
            }
            balance
        }
        Err(error) => {
            eprintln!(
                "wallet receive WARN: post-receive balance read failed (returning the credited \
                 outcome anyway; receive already happened): {error}"
            );
            before.saturating_add(received_sats)
        }
    }
}

fn resolve_mint(home: &MaxplayerHome, mint_override: Option<&str>) -> Result<String, WalletOpsError> {
    match mint_override {
        Some(url) => mint_is_allowed(home, url),
        None => normalize_mint_url(home.config.default_mint()),
    }
}

/// Open the packaged CDK wallet for one allowed mint (shared sqlite + seed).
pub async fn open_wallet_async(
    home: &MaxplayerHome,
    mint_url: &str,
) -> Result<Wallet, WalletOpsError> {
    let mint_url = mint_is_allowed(home, mint_url)?;
    let secret = home::read_secret_key_hex(home)?;
    let seed = seed_from_secret_hex(&secret).map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let path = sqlite_path(&home.wallet_dir);
    let store = WalletSqliteDatabase::new(path)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    Wallet::new(
        mint_url.as_str(),
        CurrencyUnit::Sat,
        Arc::new(store),
        seed,
        None,
    )
    .map_err(|error| WalletOpsError::Wallet(error.to_string()))
}

/// Wait for a mint quote to be paid, then issue it. Refuses a phantom credit (nothing issued) and an
/// issue that does not equal what was quoted.
pub(crate) async fn poll_and_mint(
    wallet: &Wallet,
    quote_id: &str,
    expected_sats: u64,
) -> Result<u64, WalletOpsError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let status = wallet
            .check_mint_quote(quote_id)
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
        match status.state {
            MintQuoteState::Paid | MintQuoteState::Issued => break,
            MintQuoteState::Unpaid => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(WalletOpsError::Wallet(format!(
                        "timed out waiting for mint quote {quote_id} to become paid (refusing phantom credit)"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let proofs = wallet
        .mint(quote_id, Default::default(), None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let funded = proofs
        .iter()
        .map(|proof| proof.amount.to_u64())
        .fold(0u64, |acc, value| acc.saturating_add(value));
    if funded == 0 {
        return Err(WalletOpsError::Wallet(
            "mint completed but funded amount is 0 (refusing phantom credit)".into(),
        ));
    }
    // Exact mint proofs == requested (no invented fee delta / under-over fund).
    if funded != expected_sats {
        return Err(WalletOpsError::Wallet(format!(
            "mint funded amount {funded} != requested {expected_sats} (refusing under/over fund)"
        )));
    }
    Ok(funded)
}

/// Open the shared wallet database for seedless, offline balance discovery.
async fn open_balance_store(home: &MaxplayerHome) -> Result<WalletSqliteDatabase, WalletOpsError> {
    WalletSqliteDatabase::new(sqlite_path(&home.wallet_dir))
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))
}

/// Balance per configured or wallet-database-discovered mint. The sqlite store is shared across
/// every mint, and proofs legally land at mints outside the configured set (seller redemption at
/// `accepted_mints[1..]`, cross-mint hop residue) — so the read enumerates the DB truth (proof
/// table ∪ mint registrations ∪ configured set) rather than the config filter, and tags each row
/// `configured` so callers can tell the sets apart (#266). One store open, no per-mint `Wallet`,
/// no seed, no network, and no `mint_is_allowed` fence — that fence stays load-bearing on the
/// funding paths only.
pub async fn balances_async(home: &MaxplayerHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    let default = normalize_mint_url(home.config.default_mint())?;
    let configured = configured_mints(home)?;
    let store = open_balance_store(home).await?;
    let proofs = store
        .get_proofs(
            None,
            Some(CurrencyUnit::Sat),
            Some(vec![cashu::State::Unspent]),
            None,
        )
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let registered = store
        .get_mints()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;

    let mut discovered = BTreeSet::new();
    for proof in proofs {
        discovered.insert(normalize_mint_url(&proof.mint_url.to_string())?);
    }
    for mint_url in registered.keys() {
        discovered.insert(normalize_mint_url(&mint_url.to_string())?);
    }

    let mut mint_urls = configured.clone();
    for mint_url in discovered {
        if !mint_urls.iter().any(|entry| entry == &mint_url) {
            mint_urls.push(mint_url);
        }
    }

    let mut rows = Vec::new();
    for mint_url in mint_urls {
        let balance = store
            .get_balance(
                Some(MintUrl::from_str(&mint_url).map_err(|error| {
                    WalletOpsError::Wallet(format!("invalid normalized mint URL: {error}"))
                })?),
                Some(CurrencyUnit::Sat),
                Some(vec![cashu::State::Unspent]),
            )
            .await
            .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
        rows.push(MintBalance {
            is_default: mint_url == default,
            configured: configured.iter().any(|entry| entry == &mint_url),
            mint_url,
            balance_sats: balance,
        });
    }
    Ok(rows)
}

/// Create a mint quote and return the bolt11 **before** any poll/wait.
pub async fn begin_mint_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintQuote, WalletOpsError> {
    if amount_sats == 0 {
        return Err(WalletOpsError::Wallet("amount must be > 0".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let amount = Amount::from(amount_sats);
    let quote = wallet
        .mint_quote(PaymentMethod::BOLT11, Some(amount), None, None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let invoice = quote.request.clone();
    if invoice.is_empty() {
        return Err(WalletOpsError::Wallet(
            "mint quote returned empty bolt11 (refusing silent fund path)".into(),
        ));
    }
    Ok(MintQuote {
        mint_url,
        invoice,
        quote_id: quote.id,
        amount_sats,
    })
}

/// Poll + mint a previously created quote. Refuses when proof total ≠ requested.
pub async fn complete_mint_async(
    home: &MaxplayerHome,
    quote: &MintQuote,
) -> Result<MintOutcome, WalletOpsError> {
    let mint_url = mint_is_allowed(home, &quote.mint_url)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let funded = poll_and_mint(&wallet, &quote.quote_id, quote.amount_sats).await?;
    let balance = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    Ok(MintOutcome {
        mint_url,
        invoice: quote.invoice.clone(),
        quote_id: quote.quote_id.clone(),
        funded_sats: funded,
        balance_sats: balance,
    })
}

/// Look up a mint quote persisted in the shared CDK localstore.
///
/// The wallet sqlite is shared across every configured mint, so any opened
/// wallet's localstore sees all stored quotes. Returns `None` when the quote id
/// is unknown locally, or when the stored quote has no fixed amount (e.g.
/// variable-amount methods that cannot be completed from the id alone). Lets
/// [`complete_mint_by_id_async`] recover mint/amount/invoice from the id.
pub async fn lookup_pending_quote_async(
    home: &MaxplayerHome,
    quote_id: &str,
) -> Result<Option<MintQuote>, WalletOpsError> {
    let quote_id = quote_id.trim();
    if quote_id.is_empty() {
        return Err(WalletOpsError::Wallet("quote_id is empty".into()));
    }
    let default_mint = normalize_mint_url(home.config.default_mint())?;
    let wallet = open_wallet_async(home, &default_mint).await?;
    let stored = wallet
        .localstore
        .get_mint_quote(quote_id)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let Some(amount) = stored.amount else {
        return Ok(None);
    };
    Ok(Some(MintQuote {
        mint_url: stored.mint_url.to_string(),
        invoice: stored.request,
        quote_id: stored.id,
        amount_sats: amount.to_u64(),
    }))
}

/// Complete a paid mint quote identified only by its `quote_id`.
///
/// Recovers mint/amount/invoice from the shared CDK localstore when the quote is
/// known there (so `amount_override`/`mint_override` may be omitted). Otherwise
/// the caller must supply `amount_override` (and, optionally, `mint_override`)
/// to reconstruct the quote — the underlying cdk `mint()` still requires the
/// quote (and its NUT-20 signing key) to already live in this wallet's store, so
/// a quote this wallet never created cannot be completed here.
///
/// When both a stored value and an override are present they must agree; a
/// mismatch is refused rather than guessed, keeping the funded total exactly
/// what was quoted.
pub async fn complete_mint_by_id_async(
    home: &MaxplayerHome,
    quote_id: &str,
    amount_override: Option<u64>,
    mint_override: Option<&str>,
) -> Result<MintOutcome, WalletOpsError> {
    let quote_id = quote_id.trim();
    if quote_id.is_empty() {
        return Err(WalletOpsError::Wallet("quote_id is empty".into()));
    }
    let quote = match lookup_pending_quote_async(home, quote_id).await? {
        Some(stored) => {
            if let Some(amount) = amount_override {
                if amount != stored.amount_sats {
                    return Err(WalletOpsError::Wallet(format!(
                        "amount {amount} != stored quote amount {} for quote {quote_id} (refusing mismatched completion)",
                        stored.amount_sats
                    )));
                }
            }
            if let Some(mint) = mint_override {
                let requested = normalize_mint_url(mint)?;
                let stored_mint = normalize_mint_url(&stored.mint_url)?;
                if requested != stored_mint {
                    return Err(WalletOpsError::Wallet(format!(
                        "mint {requested} != stored quote mint {stored_mint} for quote {quote_id} (refusing mismatched completion)"
                    )));
                }
            }
            stored
        }
        None => {
            let amount_sats = amount_override.ok_or_else(|| {
                WalletOpsError::Wallet(format!(
                    "quote {quote_id} has no stored amount; pass --amount to complete it"
                ))
            })?;
            if amount_sats == 0 {
                return Err(WalletOpsError::Wallet("amount must be > 0".into()));
            }
            let mint_url = resolve_mint(home, mint_override)?;
            MintQuote {
                mint_url,
                invoice: String::new(),
                quote_id: quote_id.to_owned(),
                amount_sats,
            }
        }
    };
    complete_mint_async(home, &quote).await
}

/// Flexible/repeatable mint-fund (no `already_funded` hard-block).
///
/// Always returns [`MintFlow::NeedsPayment`] with the bolt11 already surfaced: the caller pays the
/// invoice, then calls [`complete_mint_async`]. No mint is special-cased — a mint that settles its
/// own invoices does so on its own, and the caller sees the same two steps either way.
pub async fn mint_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    let quote = begin_mint_async(home, amount_sats, mint_override).await?;
    Ok(MintFlow::NeedsPayment(quote))
}

/// Create a bolt11 invoice and return it before any wait ([`MintFlow::NeedsPayment`]).
pub async fn invoice_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    mint_async(home, amount_sats, mint_override).await
}

/// Create/print an unlocked cashu token (ecash out).
pub async fn send_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<SendOutcome, WalletOpsError> {
    if amount_sats == 0 {
        return Err(WalletOpsError::Wallet("amount must be > 0".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let before = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    if before < amount_sats {
        return Err(WalletOpsError::Wallet(format!(
            "insufficient funds: balance={before} need={amount_sats}"
        )));
    }
    let prepared = wallet
        .prepare_send(Amount::from(amount_sats), SendOptions::default())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    // `confirm` is the effect boundary: it consumes the input proofs and mints the outgoing token, so
    // past this point the ecash has left the spendable balance and the caller MUST receive the token.
    // The post-confirm balance read is observational; a read failure must never discard the token
    // (finding U — see `post_confirm_balance`).
    let token = prepared
        .confirm(None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let read = wallet
        .total_balance()
        .await
        .map(|balance| balance.to_u64())
        .map_err(|error| error.to_string());
    let balance = post_confirm_balance(read, before, amount_sats, "send");
    Ok(SendOutcome {
        mint_url,
        sent_sats: amount_sats,
        balance_sats: balance,
        token: token.to_string(),
    })
}

/// Redeem a cashu token (ecash in). Mint must already be configured.
pub async fn receive_async(
    home: &MaxplayerHome,
    token: &str,
) -> Result<ReceiveOutcome, WalletOpsError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(WalletOpsError::Wallet("token is empty".into()));
    }
    let parsed = Token::from_str(token)
        .map_err(|error| WalletOpsError::Wallet(format!("invalid cashu token: {error}")))?;
    let mint_url = parsed
        .mint_url()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_string();
    // The token's own mint must be in this home's CONFIGURED list — the one and only mint gate.
    let mint_url = mint_is_allowed(home, &mint_url)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let before = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    let received = wallet
        .receive(token, ReceiveOptions::default())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let received_sats = received.to_u64();
    if received_sats == 0 {
        return Err(WalletOpsError::Wallet(
            "receive credited 0 sats (refusing phantom credit)".into(),
        ));
    }
    // `receive` is the effect boundary: the token's proofs are already redeemed, so the post-receive
    // balance read is observational and must NEVER discard the credited outcome (finding X). A read
    // failure or a stale/non-increasing balance yields a best-effort figure via `post_receive_balance`;
    // the authoritative record is `received_sats`.
    let read = wallet
        .total_balance()
        .await
        .map(|balance| balance.to_u64())
        .map_err(|error| error.to_string());
    let balance = post_receive_balance(read, before, received_sats);
    Ok(ReceiveOutcome {
        mint_url,
        received_sats,
        balance_sats: balance,
    })
}

/// Pay a lightning invoice from ecash (fail-closed on insufficient / unpaid).
/// `confirm` is the effect boundary; the post-confirm balance read is observational and never
/// discards the settled outcome (finding U — see [`post_confirm_balance`]).
pub async fn melt_async(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<MeltOutcome, WalletOpsError> {
    let bolt11 = bolt11.trim();
    if bolt11.is_empty() {
        return Err(WalletOpsError::Wallet("bolt11 invoice is empty".into()));
    }
    let mint_url = resolve_mint(home, mint_override)?;
    let wallet = open_wallet_async(home, &mint_url).await?;
    let quote = wallet
        .melt_quote(PaymentMethod::BOLT11, bolt11, None, None)
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let need = quote.amount.to_u64().saturating_add(quote.fee_reserve.to_u64());
    let before = wallet
        .total_balance()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?
        .to_u64();
    if before < need {
        return Err(WalletOpsError::Wallet(format!(
            "insufficient funds for melt: balance={before} need={need} (amount+fee_reserve)"
        )));
    }
    let prepared = wallet
        .prepare_melt(&quote.id, HashMap::new())
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    let confirmed = prepared
        .confirm()
        .await
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    // `confirm` is the effect boundary — the melt has settled and funds have left the wallet, so the
    // outcome (paid/fee, both read from `confirmed`) MUST be returned. The post-confirm balance read
    // is observational; a read failure must never discard the outcome (finding U).
    let paid_sats = confirmed.amount().to_u64();
    let fee_sats = confirmed.fee_paid().to_u64();
    let read = wallet
        .total_balance()
        .await
        .map(|balance| balance.to_u64())
        .map_err(|error| error.to_string());
    let balance_sats = post_confirm_balance(read, before, paid_sats.saturating_add(fee_sats), "melt");
    Ok(MeltOutcome {
        mint_url,
        paid_sats,
        fee_sats,
        balance_sats,
    })
}

/// List configured mints (default first).
pub fn list_mints(home: &MaxplayerHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    let default = normalize_mint_url(home.config.default_mint())?;
    Ok(configured_mints(home)?
        .into_iter()
        .map(|mint_url| MintBalance {
            is_default: mint_url == default,
            configured: true,
            mint_url,
            balance_sats: 0,
        })
        .collect())
}

/// Opt-in add of an extra mint URL (does not invent balance).
pub fn add_mint(home: &mut MaxplayerHome, mint_url: &str) -> Result<String, WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let default = normalize_mint_url(home.config.default_mint())?;
    if normalized == default {
        return Ok(normalized);
    }
    if home
        .config
        .extra_mints
        .iter()
        .any(|entry| normalize_mint_url(entry).ok().as_deref() == Some(normalized.as_str()))
    {
        return Ok(normalized);
    }
    let to_add = normalized.clone();
    home::save_config(home, |config| {
        config.extra_mints.push(to_add);
    })?;
    Ok(normalized)
}

/// Remove an opt-in extra mint. Default mint is pinned and cannot be removed.
pub fn remove_mint(home: &mut MaxplayerHome, mint_url: &str) -> Result<(), WalletOpsError> {
    let normalized = normalize_mint_url(mint_url)?;
    let default = normalize_mint_url(home.config.default_mint())?;
    if normalized == default {
        return Err(WalletOpsError::MintPinnedDefault { mint_url: default });
    }
    let present = home.config.extra_mints.iter().any(|entry| {
        normalize_mint_url(entry).ok().as_deref() == Some(normalized.as_str())
    });
    if !present {
        return Err(WalletOpsError::MintNotAllowed {
            mint_url: normalized,
            default_mint: home.config.default_mint().to_string(),
        });
    }
    let to_remove = normalized.clone();
    home::save_config(home, |config| {
        config
            .extra_mints
            .retain(|entry| normalize_mint_url(entry).ok().as_deref() != Some(to_remove.as_str()));
    })?;
    Ok(())
}

pub fn balances_blocking(home: &MaxplayerHome) -> Result<Vec<MintBalance>, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("balances_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(balances_async(home))
}

pub fn mint_blocking(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("mint_blocking").map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(mint_async(home, amount_sats, mint_override))
}

pub fn complete_mint_blocking(
    home: &MaxplayerHome,
    quote: &MintQuote,
) -> Result<MintOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("complete_mint_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(complete_mint_async(home, quote))
}

pub fn complete_mint_by_id_blocking(
    home: &MaxplayerHome,
    quote_id: &str,
    amount_override: Option<u64>,
    mint_override: Option<&str>,
) -> Result<MintOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("complete_mint_by_id_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(complete_mint_by_id_async(
        home,
        quote_id,
        amount_override,
        mint_override,
    ))
}

pub fn send_blocking(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<SendOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("send_blocking").map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(send_async(home, amount_sats, mint_override))
}

pub fn receive_blocking(
    home: &MaxplayerHome,
    token: &str,
) -> Result<ReceiveOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("receive_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(receive_async(home, token))
}

pub fn melt_blocking(
    home: &MaxplayerHome,
    bolt11: &str,
    mint_override: Option<&str>,
) -> Result<MeltOutcome, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("melt_blocking").map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(melt_async(home, bolt11, mint_override))
}

pub fn invoice_blocking(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    crate::runtime_guard::refuse_nested_block_on("invoice_blocking")
        .map_err(WalletOpsError::Wallet)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| WalletOpsError::Wallet(error.to_string()))?;
    runtime.block_on(invoice_async(home, amount_sats, mint_override))
}

#[cfg(test)]
mod tests {
    /// Test fixture mint host — NOT a default. A mint is just a mint: what makes one usable is membership of
    /// the home's configured list, which each test sets up explicitly.
    const FIXTURE_MINT_URL: &str = "https://mint.example/Bitcoin";
    use super::*;
    use crate::home::bootstrap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    // Finding DD: `SendOutcome.token` is a BEARER cashu token (spendable ecash). Its `Debug` MUST
    // redact the token — a derived Debug would print it verbatim, so any debug log of a SendOutcome
    // would leak spendable funds. Assert the debug rendering contains neither the token nor any of
    // its material, and that it carries the redaction marker + non-secret fields.
    #[test]
    fn send_outcome_debug_redacts_bearer_token() {
        let token = "cashuAeyJ0b2tlbiI6c3BlbmRhYmxlLWJlYXJlci1lY2FzaC1zZWNyZXQ";
        let outcome = SendOutcome {
            mint_url: "https://testnut.cashudevkit.org".into(),
            sent_sats: 21,
            balance_sats: 100,
            token: token.into(),
        };
        let rendered = format!("{outcome:?}");
        assert!(
            !rendered.contains(token),
            "SendOutcome Debug must not contain the bearer token: {rendered}"
        );
        // No substring of the token beyond a trivial prefix leaks (guard against partial exposure).
        assert!(
            !rendered.contains("spendable-bearer-ecash-secret")
                && !rendered.contains(&token[6..]),
            "SendOutcome Debug must not leak token material: {rendered}"
        );
        assert!(
            rendered.contains("<redacted:sha256:"),
            "redaction marker expected: {rendered}"
        );
        // Non-secret fields remain visible for diagnostics.
        assert!(rendered.contains("sent_sats: 21") && rendered.contains("balance_sats: 100"));
    }

    // An empty token renders the empty marker (no digest of nothing) — never a bare empty string
    // that could be mistaken for "no field".
    #[test]
    fn redact_secret_empty_marks_empty() {
        assert_eq!(redact_secret(""), "<redacted:empty>");
        assert!(redact_secret("x").starts_with("<redacted:sha256:"));
    }

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-wallet-ops-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn extra_mint_add_remove_keeps_default_pinned() {
        let root = temp_home("mints");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        // Issue #378: the shipped default mint is the real minibits mint (not testnut).
        assert_eq!(home.config.default_mint(), crate::home::DEFAULT_MINIBITS_MINT_URL);
        let listed = list_mints(&home).expect("list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_default);

        let added = add_mint(&mut home, "https://example.mint.test").expect("add");
        assert_eq!(added, "https://example.mint.test");
        assert_eq!(list_mints(&home).expect("list2").len(), 2);

        let err = remove_mint(&mut home, crate::home::DEFAULT_MINIBITS_MINT_URL).expect_err("pin");
        assert!(matches!(err, WalletOpsError::MintPinnedDefault { .. }));

        remove_mint(&mut home, "https://example.mint.test").expect("remove");
        assert_eq!(list_mints(&home).expect("list3").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn balances_include_unconfigured_proofs_from_the_shared_database() {
        use cashu::secret::Secret;
        use cashu::{Amount, Id, Proof, SecretKey, State};
        use cdk::wallet::types::ProofInfo;

        let root = temp_home("db-truth");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let stray = MintUrl::from_str("https://stray-mint.example/").expect("stray mint URL");
        let store = WalletSqliteDatabase::new(sqlite_path(&home.wallet_dir))
            .await
            .expect("open on-disk wallet database");
        let proof = Proof::new(
            Amount::from(37),
            Id::from_str("009a1f293253e41e").expect("keyset id"),
            Secret::new("issue-266-unconfigured-db-truth"),
            SecretKey::generate().public_key(),
        );
        let proof_info = ProofInfo::new(proof, stray.clone(), State::Unspent, CurrencyUnit::Sat)
            .expect("proof info");
        store
            .update_proofs(vec![proof_info], vec![])
            .await
            .expect("seed unconfigured proof");

        let rows = balances_async(&home).await.expect("read database truth");
        let stray_row = rows
            .iter()
            .find(|row| row.mint_url == "https://stray-mint.example")
            .expect("unconfigured proof mint appears");
        assert_eq!(stray_row.balance_sats, 37);
        assert!(!stray_row.configured);
        assert!(!stray_row.is_default);

        let row_total: u64 = rows.iter().map(|row| row.balance_sats).sum();
        let db_total = store
            .get_balance(None, Some(CurrencyUnit::Sat), Some(vec![State::Unspent]))
            .await
            .expect("whole database balance");
        assert_eq!(row_total, db_total, "per-mint rows cross-foot to DB truth");
        assert_eq!(row_total, 37);
        let _ = std::fs::remove_dir_all(&root);
    }

    // #579: MintPinnedDefault's message hardcoded one mint constant, so on a minibits-default home
    // `wallet mints remove <minibits>` errored "cannot remove the default mint (<that constant>)" —
    // naming the wrong mint as the default when the mint actually
    // pinned is minibits (config.default_mint()). Display-only lie; the guard pins correctly. This
    // pins the message to the ACTUAL default. Reverting the Display fix REDS this.
    #[test]
    fn mint_pinned_default_error_names_the_real_default_not_testnut() {
        let root = temp_home("pinned-default-message");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        let default = normalize_mint_url(home.config.default_mint()).expect("normalize default");
        // The shipped default is the real minibits mint, distinct from the testnut constant — so a
        // message naming testnut here is provably wrong, not a coincidental match.
        assert_eq!(default, crate::home::DEFAULT_MINIBITS_MINT_URL);
        assert_ne!(default, FIXTURE_MINT_URL);

        let message = remove_mint(&mut home, crate::home::DEFAULT_MINIBITS_MINT_URL)
            .expect_err("removing the pinned default must error")
            .to_string();
        assert!(
            message.contains(&default),
            "message must name the real pinned default ({default}): {message}"
        );
        assert!(
            !message.contains(FIXTURE_MINT_URL),
            "message must not name the testnut constant as the default: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_mint_refuses_inside_runtime() {
        let root = temp_home("nested");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = mint_blocking(&home, 1, None).expect_err("nested");
        assert!(err.to_string().contains("nested block_on refused"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_mint_refused_without_inventing_credit() {
        let root = temp_home("unknown");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = mint_blocking(&home, 1, Some("https://evil.example")).expect_err("deny");
        assert!(matches!(&err, WalletOpsError::MintNotAllowed { .. }));
        // #465: a genuine membership miss KEEPS the `mints add` remedy — the distinction the fix draws.
        assert!(err.to_string().contains("mints add"), "membership miss keeps the `mints add` remedy: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding T(3), one-mint form: the standalone receive path gates on the CONFIGURED LIST and on
    // nothing else — a token whose own mint this home has not configured is refused before any
    // wallet open (so it holds offline), and the same token at a configured mint gets past that gate.
    #[tokio::test(flavor = "current_thread")]
    async fn receive_refuses_a_token_from_an_unconfigured_mint() {
        use std::str::FromStr;

        use cashu::secret::Secret;
        use cashu::{Amount, CurrencyUnit, Id, MintUrl, Proof, SecretKey, Token};

        let foreign_mint = "https://not-configured.example/";
        let root = temp_home("receive-unconfigured-mint");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec!["https://configured.example/".into()];

        let proof = Proof::new(
            Amount::from(5),
            Id::from_str("009a1f293253e41e").expect("keyset id"),
            Secret::new("receive-fence-test-secret"),
            SecretKey::generate().public_key(),
        );
        let token = Token::new(
            MintUrl::from_str(foreign_mint).expect("mint url"),
            vec![proof],
            None,
            CurrencyUnit::Sat,
        );

        let err = receive_async(&home, &token.to_string())
            .await
            .expect_err("a token from an unconfigured mint must refuse");
        assert!(
            matches!(&err, WalletOpsError::MintNotAllowed { mint_url, .. } if mint_url.contains("not-configured.example")),
            "expected MintNotAllowed (the one mint gate), got {err:?}"
        );
        // The refusal names the remedy that actually clears it — there is no other control left.
        let message = err.to_string();
        assert!(
            message.contains("mints add"),
            "the membership refusal must name `mints add`, got: {message}"
        );
        assert!(
            !message.to_ascii_lowercase().contains("allow_real_mints"),
            "no real-mint switch exists any more, got: {message}"
        );

        // Configure that same mint and the membership gate no longer refuses: the call now fails
        // later, at the (unreachable) mint, which is what proves the gate was the only thing in the
        // way.
        home.config.extra_mints = vec![foreign_mint.to_owned()];
        let err = receive_async(&home, &token.to_string())
            .await
            .expect_err("the example mint is unreachable, so the receive still fails");
        assert!(
            matches!(&err, WalletOpsError::Wallet(_)),
            "a configured mint must get past the membership gate, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // #500: a funding op must persist ONLY its own change, never an in-memory value that exists
    // just because an env override set it. `save_config` writes the FILE-only view (re-reads
    // config.toml, edits that), so a `MAXPLAYER_*` value can never leak to disk. The write-back
    // class was fixed by #84 (fix/save-config-env-promotion); this pins it on the FUNDING path —
    // which the scalar-only, direct-save `save_does_not_persist_env_override_values` (home.rs) did
    // not cover.
    #[test]
    fn funding_op_never_writes_back_an_env_only_value() {
        let root = temp_home("500-funding-no-env-writeback");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        // Durable relay on disk — the operator's explicit setting.
        home::save_config(&mut home, |config| {
            config.relay_url = "wss://on-disk.example".to_owned()
        })
        .expect("seed the relay on disk");
        // Simulate an env overlay (MAXPLAYER_RELAY_URL): in-memory only, config.toml unchanged.
        home.config.relay_url = "wss://from-env.example".to_owned();

        // A funding op (adds an extra mint) — persists through `save_config`.
        let added = add_mint(&mut home, "https://extra-mint.example/").expect("add an extra mint");

        let raw = std::fs::read_to_string(root.join("config.toml")).expect("read config.toml");
        let on_disk = home::parse_config_toml(&raw).expect("parse config.toml");
        // The durable value is untouched: the env-only in-memory value did NOT leak to disk...
        assert_eq!(
            on_disk.relay_url, "wss://on-disk.example",
            "a funding op must not write an env-only value back to disk (#500); config.toml = {raw}"
        );
        // ...while the funding op's OWN change DID persist.
        assert!(
            on_disk.extra_mints.iter().any(|entry| entry == &added),
            "the funding op's own change (the added mint) must persist to disk"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding U: `confirm` is the effect boundary, so a post-confirm balance-read FAILURE must never
    // discard the confirmed token/outcome — `post_confirm_balance` returns a best-effort estimate and
    // never errors, so the caller always returns the token. A stale/equal balance also still returns.
    #[test]
    fn post_confirm_balance_read_failure_preserves_outcome() {
        // Read failed: best-effort `before - spent`, never an error → the token is still returned.
        assert_eq!(post_confirm_balance(Err("boom".into()), 100, 30, "send"), 70);
        // Underflow-safe when the estimate would go negative.
        assert_eq!(post_confirm_balance(Err("boom".into()), 10, 30, "send"), 0);
        // Read ok and balance decreased → report the read value.
        assert_eq!(post_confirm_balance(Ok(70), 100, 30, "melt"), 70);
        // Read ok but stale/equal (did-not-decrease) → still returned, WARN only (no discard).
        assert_eq!(post_confirm_balance(Ok(100), 100, 30, "send"), 100);
    }

    // Finding X: a successful `receive` is the effect boundary (proofs already redeemed), so a
    // post-receive balance-read FAILURE must never discard the credited outcome — `post_receive_balance`
    // returns a best-effort `before + received` estimate and never errors. A stale/non-increasing
    // read also still returns (WARN only), so the caller never retries into an already-spent token.
    #[test]
    fn post_receive_balance_read_failure_preserves_outcome() {
        // Read failed: best-effort `before + received`, never an error → the outcome is preserved.
        assert_eq!(post_receive_balance(Err("boom".into()), 100, 30), 130);
        // Read ok and balance increased → report the read value.
        assert_eq!(post_receive_balance(Ok(130), 100, 30), 130);
        // Read ok but stale/non-increasing → still returned, WARN only (no discard).
        assert_eq!(post_receive_balance(Ok(100), 100, 30), 100);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lookup_pending_quote_unknown_id_is_none() {
        // Pure local sqlite read — no live mint needed; an unknown id yields None
        // rather than inventing a quote.
        let root = temp_home("lookup-none");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let found = lookup_pending_quote_async(&home, "quote-does-not-exist")
            .await
            .expect("lookup");
        assert!(found.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_mint_by_id_unknown_quote_without_amount_refuses() {
        // No stored quote + no --amount => refuse rather than guess. Reached
        // before any mint round-trip, so this holds even with testnut down.
        let root = temp_home("complete-noamount");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = complete_mint_by_id_async(&home, "unknown-quote", None, None)
            .await
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("pass --amount"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_mint_by_id_empty_quote_id_refuses() {
        let root = temp_home("complete-empty");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = complete_mint_by_id_async(&home, "   ", Some(21), None)
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("quote_id is empty"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_complete_mint_by_id_refuses_inside_runtime() {
        let root = temp_home("complete-nested");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        let err = complete_mint_by_id_blocking(&home, "quote", Some(21), None)
            .expect_err("nested");
        assert!(err.to_string().contains("nested block_on refused"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_mint_url_trims_and_strips_trailing_slash() {
        let normalized = normalize_mint_url(&format!(" {FIXTURE_MINT_URL}/ ")).expect("normalize");
        assert_eq!(normalized, FIXTURE_MINT_URL);
        let err = normalize_mint_url("   ").expect_err("empty");
        assert!(matches!(err, WalletOpsError::Wallet(_)));
    }

    // #577, one-mint form: a mint is a mint. NEGATIVE — no money class survives anywhere on this
    // path: `mint_is_allowed` answers membership of the configured list and nothing else, for the
    // shipped default and for any other mint alike, over either scheme.
    #[test]
    fn membership_of_the_configured_list_is_the_only_mint_gate() {
        let root = temp_home("one-mint-gate");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        // The shipped default is admitted because it is CONFIGURED, not because of what it is.
        let default_mint = home.config.default_mint().to_owned();
        assert_eq!(default_mint, crate::home::DEFAULT_MINIBITS_MINT_URL);
        assert!(mint_is_allowed(&home, &default_mint).is_ok());

        // Not on the list ⇒ refused, whatever the mint looks like.
        for outsider in [
            "https://some-other-mint.example",
            "http://127.0.0.1:3338",
            "https://testnut.cashudevkit.org",
        ] {
            assert!(
                matches!(
                    mint_is_allowed(&home, outsider),
                    Err(WalletOpsError::MintNotAllowed { .. })
                ),
                "an unconfigured mint must refuse: {outsider}"
            );
        }

        // On the list ⇒ admitted, over EITHER scheme (http is deliberate: a seat's own sidecar mint
        // runs on loopback).
        home.config.extra_mints = vec![
            "https://some-other-mint.example".to_owned(),
            "http://127.0.0.1:3338".to_owned(),
        ];
        for insider in ["https://some-other-mint.example", "http://127.0.0.1:3338"] {
            assert!(
                mint_is_allowed(&home, insider).is_ok(),
                "a configured mint must be admitted: {insider}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // #506-A: `MintNotAllowed` must name the home's ACTUAL default (`config.default_mint()`), never
    // a compile-time constant. On the shipped minibits-default home, naming any other mint as "the
    // default" was a money-relevant lie (`wallet mints list` correctly shows minibits as default).
    // Red-on-revert: interpolating a fixture constant again names the wrong mint on a minibits home.
    #[test]
    fn mint_not_allowed_names_home_default_not_a_constant() {
        let root = temp_home("506a-default-name");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap(&root).expect("bootstrap");
        // Precondition: the fresh home's default is the real minibits mint (#378), not testnut.
        assert_eq!(
            home.config.default_mint(),
            crate::home::DEFAULT_MINIBITS_MINT_URL
        );
        let err =
            mint_is_allowed(&home, "https://evil.example").expect_err("unconfigured mint refused");
        let message = err.to_string();
        assert!(
            message.contains(crate::home::DEFAULT_MINIBITS_MINT_URL),
            "MintNotAllowed must name the home's real default: {message}"
        );
        assert!(
            !message.contains(FIXTURE_MINT_URL),
            "MintNotAllowed must NOT name the testnut constant as the default on a minibits home: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
