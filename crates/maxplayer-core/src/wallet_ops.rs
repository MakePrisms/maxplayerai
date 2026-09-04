//! Flexible ecash wallet ops for `maxplayer wallet` / MCP mirrors, over the packaged CDK wallet at
//! `home/.maxplayer/wallet`. This module owns the mint-fund path: [`begin_mint_async`] creates a mint
//! quote and returns the bolt11 invoice up front, then [`complete_mint_async`] mints once it is
//! paid. ([`crate::buyer_fund`] covers wallet open, seed derivation, and balance read.)
//!
//! **Funding assumption:** only the pinned testnut host ([`DEFAULT_MINT_URL`])
//! FakeWallet-auto-pays mint quotes. For other configured mints, [`begin_mint_async`]
//! returns the bolt11 and callers must pay it, then [`complete_mint_async`].

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
use crate::home::{self, HomeError, MaxplayerHome, DEFAULT_MINT_URL};

#[derive(Debug)]
pub enum WalletOpsError {
    Home(HomeError),
    /// The mint is not in this home's configured set (`accepted_mints`/`extra_mints`) — a
    /// MEMBERSHIP miss, cleared by `maxplayer wallet mints add`. `default_mint` carries the home's
    /// ACTUAL default (`config.default_mint()`) so the Display names it rather than the pinned
    /// testnut constant — on a real-minibits home the latter is a money-relevant lie (#506).
    MintNotAllowed { mint_url: String, default_mint: String },
    /// The mint IS configured but is a real mint refused by the real-mint fence (issue #49):
    /// `allow_real_mints` is off. A POLICY block — `mints add` cannot clear it, so it must NOT
    /// borrow [`Self::MintNotAllowed`]'s remedy; the control is `MAXPLAYER_ALLOW_REAL_MINTS` (#465).
    RealMintDisallowed { mint_url: String },
    /// `remove_mint` refuses to remove the home's pinned default mint. `mint_url` carries that
    /// actual default (`config.default_mint()`) so the message names the real pinned mint rather
    /// than a hardcoded constant — on a real-minibits home the constant would be a false-default
    /// lie (#579).
    MintPinnedDefault { mint_url: String },
    /// A Lightning operation (`wallet fund`, `wallet melt`) aimed at an ISSUER mint — a mint whose
    /// info lists no bolt11 method (§4.2 "Issuer mint"). There is no Lightning there to mint from or
    /// melt to; the message carries the plain reason. Issuance at a seat's own mint is a different
    /// path (stage 3), never this one.
    IssuerMint(String),
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
            Self::RealMintDisallowed { mint_url } => write!(
                formatter,
                "mint {mint_url} not allowed: allow_real_mints is off (only {DEFAULT_MINT_URL} is permitted). \
                 Set MAXPLAYER_ALLOW_REAL_MINTS=true (or allow_real_mints in config.toml) to opt in, \
                 or use --mint {DEFAULT_MINT_URL} for dev/play-money"
            ),
            Self::IssuerMint(reason) => write!(formatter, "{reason}"),
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

fn is_autopay_mint(mint_url: &str) -> bool {
    normalize_mint_url(mint_url)
        .ok()
        .as_deref()
        == Some(DEFAULT_MINT_URL)
}

/// Money class a mint moves, derived purely from the mint URL. The pinned testnut host
/// ([`DEFAULT_MINT_URL`]) FakeWallet-auto-pays its own invoices — play money — while every other
/// mint invoices for real sats. Internal: it gates the #445 fail-closed refusal of silently
/// auto-funding play money and drives a play-money marker on dev rows. Ordinary mints carry no
/// money-class label in user output — a mint is a mint, identified by its URL (#577).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoneyType {
    /// A real mint — its invoices move real sats.
    Real,
    /// The testnut dev/play mint — auto-pays its own invoices with fake sats.
    Play,
}

impl MoneyType {
    /// Classify a mint URL. A URL that does not normalize is treated as [`Self::Real`] — the
    /// fail-safe direction, so an unrecognized mint is never mislabeled play money.
    pub fn of_mint(mint_url: &str) -> Self {
        if is_autopay_mint(mint_url) {
            Self::Play
        } else {
            Self::Real
        }
    }
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

/// Refuse a Lightning operation at an ISSUER mint (§4.2 "Issuer mint") BEFORE the wallet is opened
/// or any quote is raised.
///
/// The class is DECLARED, never read off the mint: an operator `wallet fund`/`wallet melt` runs
/// with no accept-bind in hand, so the one declaration this seat can stand behind is its own
/// config (`issuer_mint`). A mint the config does not name is Lightning class and proceeds as it
/// always did; nothing here touches the network. `Ok(())` for a Lightning mint.
fn refuse_lightning_op_at_issuer(
    home: &MaxplayerHome,
    op: &str,
    mint_url: &str,
) -> Result<(), WalletOpsError> {
    let issuers = crate::mint_class::IssuerMints::none().with_own(home.config.issuer_mint());
    crate::mint_class::refuse_lightning_at_issuer(op, mint_url, issuers.class_of(mint_url))
        .map_err(WalletOpsError::IssuerMint)
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
    refuse_lightning_op_at_issuer(home, "wallet fund", &mint_url)?;
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
    refuse_lightning_op_at_issuer(home, "wallet fund", &mint_url)?;
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
/// Testnut ([`DEFAULT_MINT_URL`]) FakeWallet-auto-pays: begin → complete.
/// Other configured mints return [`MintFlow::NeedsPayment`] with bolt11 already
/// surfaced (caller pays, then [`complete_mint_async`]).
pub async fn mint_async(
    home: &MaxplayerHome,
    amount_sats: u64,
    mint_override: Option<&str>,
) -> Result<MintFlow, WalletOpsError> {
    let quote = begin_mint_async(home, amount_sats, mint_override).await?;
    if is_autopay_mint(&quote.mint_url) {
        Ok(MintFlow::Funded(complete_mint_async(home, &quote).await?))
    } else {
        Ok(MintFlow::NeedsPayment(quote))
    }
}

/// Create a bolt11 invoice; on testnut, mint once FakeWallet auto-pays.
/// Non-autopay mints return [`MintFlow::NeedsPayment`] (invoice before any wait).
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
    // Fail closed against the real-mint gate before opening the wallet. Operator sends are a
    // deliberate action OUTSIDE the job-pay budget gate (BudgetGate is deliberately not wired in
    // here — owner decision pending), but they must still honor `allow_real_mints`.
    //
    // CLASS-AWARE (stage 3a): the seat's OWN issuer mint passes, because a seat that cannot send its
    // own currency cannot issue one — and an issuer mint carries no sats, so the fence this widens
    // was never guarding anything at that URL. `issuers` is built exactly as
    // `refuse_lightning_op_at_issuer` builds it, from this seat's config alone, so ONLY the
    // `Own` marker admits: a mint a counterparty merely DECLARED is `Declared`, and
    // `IssuerMints::admits` answers false for it — a seller's signed tag can never open this seat's
    // real-mint fence to a mint the seller cares to name.
    if !crate::mint_class::mint_admitted(
        &mint_url,
        home.config.allow_real_mints,
        &crate::mint_class::IssuerMints::none().with_own(home.config.issuer_mint()),
    ) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
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
    let mint_url = mint_is_allowed(home, &mint_url)?;
    // Real-mint fence (issue #49): `mint_is_allowed` only checks the mint is in the CONFIGURED list;
    // this additionally fails closed on a real mint unless the operator opted in, the same gate
    // send/melt enforce. Without it a real mint left in the configured list would redeem while
    // `allow_real_mints == false`.
    //
    // CLASS-AWARE (stage 3a), the mirror of `send_async`: the seat must be able to take its OWN
    // currency back in, or it could issue tokens it can never hold. Built from this seat's config
    // alone, so only the `Own` marker admits and a counterparty's declaration widens nothing.
    if !crate::mint_class::mint_admitted(
        &mint_url,
        home.config.allow_real_mints,
        &crate::mint_class::IssuerMints::none().with_own(home.config.issuer_mint()),
    ) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
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
    // Fail closed against the real-mint gate before opening the wallet. Operator melts are a
    // deliberate action OUTSIDE the job-pay budget gate (BudgetGate is deliberately not wired in
    // here — owner decision pending), but they must still honor `allow_real_mints`.
    if !home::mint_allowed(&mint_url, home.config.allow_real_mints) {
        return Err(WalletOpsError::RealMintDisallowed { mint_url });
    }
    refuse_lightning_op_at_issuer(home, "wallet melt", &mint_url)?;
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

    // #579: MintPinnedDefault's message hardcoded the testnut DEFAULT_MINT_URL, so on a
    // real-minibits-default home `wallet mints remove <minibits>` errored "cannot remove the default
    // mint (https://testnut.cashudevkit.org)" — naming testnut as the default when the mint actually
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
        assert_ne!(default, crate::home::DEFAULT_MINT_URL);

        let message = remove_mint(&mut home, crate::home::DEFAULT_MINIBITS_MINT_URL)
            .expect_err("removing the pinned default must error")
            .to_string();
        assert!(
            message.contains(&default),
            "message must name the real pinned default ({default}): {message}"
        );
        assert!(
            !message.contains(crate::home::DEFAULT_MINT_URL),
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

    // Finding T(3): the standalone receive path fails closed on a non-allowlisted REAL mint when
    // allow_real_mints=false — even though the mint IS in the configured list (so `mint_is_allowed`
    // passes) — the same real-mint fence send/melt enforce. Reached before any wallet open, so it
    // holds offline.
    #[tokio::test(flavor = "current_thread")]
    async fn receive_refuses_real_mint_when_disallowed() {
        use std::str::FromStr;

        use cashu::secret::Secret;
        use cashu::{Amount, CurrencyUnit, Id, MintUrl, Proof, SecretKey, Token};

        let real_mint = "https://real-mint.example/";
        let root = temp_home("receive-real-mint-fence");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        home.config.accepted_mints = vec![real_mint.into()];
        home.config.allow_real_mints = false;

        let proof = Proof::new(
            Amount::from(5),
            Id::from_str("009a1f293253e41e").expect("keyset id"),
            Secret::new("receive-fence-test-secret"),
            SecretKey::generate().public_key(),
        );
        let token = Token::new(
            MintUrl::from_str(real_mint).expect("mint url"),
            vec![proof],
            None,
            CurrencyUnit::Sat,
        );

        let err = receive_async(&home, &token.to_string())
            .await
            .expect_err("real mint must refuse under allow_real_mints=false");
        assert!(
            matches!(&err, WalletOpsError::RealMintDisallowed { mint_url } if mint_url.contains("real-mint.example")),
            "expected RealMintDisallowed (policy fence, not a membership miss), got {err:?}"
        );
        // #465: the policy refusal must name the ACTUAL control and never the membership remedy —
        // `mints add` cannot clear an allow_real_mints=false fence.
        let message = err.to_string();
        assert!(
            message.contains("MAXPLAYER_ALLOW_REAL_MINTS"),
            "policy refusal must name the real control (MAXPLAYER_ALLOW_REAL_MINTS), got: {message}"
        );
        assert!(
            !message.contains("mints add"),
            "policy refusal must NOT borrow the membership `mints add` remedy, got: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // #500: a funding op must persist ONLY its own change, never the in-memory, env-widened real-mint
    // fence. `save_config` writes the FILE-only view (re-reads config.toml, edits that), so an
    // `allow_real_mints = true` that exists only because MAXPLAYER_ALLOW_REAL_MINTS opened it in-process
    // can never leak to disk. The write-back class was fixed by #84 (fix/save-config-env-promotion);
    // this pins the FENCE field on the FUNDING path — which the scalar-only, direct-save
    // `save_does_not_persist_env_override_values` (home.rs) did not cover.
    #[test]
    fn funding_op_never_writes_back_the_env_widened_real_mint_fence() {
        let root = temp_home("500-funding-no-gate-writeback");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");

        // Durable fence CLOSED on disk — the operator's explicit opt-out.
        home::save_config(&mut home, |config| config.allow_real_mints = false)
            .expect("seed the fence closed on disk");

        // Simulate the daemon launcher's MAXPLAYER_ALLOW_REAL_MINTS=true: the env overlay opens the
        // fence IN-MEMORY only (`home.config`), while config.toml on disk stays false.
        home.config.allow_real_mints = true;

        // A funding op (adds an extra mint) — persists through `save_config`.
        let added = add_mint(&mut home, "https://real-mint.example/").expect("add an extra mint");

        let raw = std::fs::read_to_string(root.join("config.toml")).expect("read config.toml");
        let on_disk = home::parse_config_toml(&raw).expect("parse config.toml");
        // The durable fence is untouched: the env-widened in-memory value did NOT leak to disk...
        assert!(
            !on_disk.allow_real_mints,
            "a funding op must not write the env-widened real-mint fence back to disk (#500); config.toml = {raw}"
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
        let normalized =
            normalize_mint_url(" https://testnut.cashudevkit.org/ ").expect("normalize");
        assert_eq!(normalized, DEFAULT_MINT_URL);
        let err = normalize_mint_url("   ").expect_err("empty");
        assert!(matches!(err, WalletOpsError::Wallet(_)));
    }

    // #506/#577 money class: `of_mint` classifies PURELY from the mint URL — the testnut play mint is
    // Play, every other mint (including the shipped minibits default) is Real. The classification is
    // internal: it gates the #445 refusal and the play-money marker, never a surfaced money-class label.
    #[test]
    fn of_mint_classifies_testnut_play_and_others_real() {
        assert_eq!(MoneyType::of_mint(DEFAULT_MINT_URL), MoneyType::Play);
        // Trailing slash / surrounding whitespace still classify as the testnut mint (normalized).
        assert_eq!(
            MoneyType::of_mint(" https://testnut.cashudevkit.org/ "),
            MoneyType::Play
        );
        assert_eq!(
            MoneyType::of_mint(crate::home::DEFAULT_MINIBITS_MINT_URL),
            MoneyType::Real
        );
        assert_eq!(
            MoneyType::of_mint("https://real-mint.example"),
            MoneyType::Real
        );
        // Fail-safe: an unparseable URL is never classified play money.
        assert_eq!(MoneyType::of_mint("not a url"), MoneyType::Real);
    }

    // #506-A: `MintNotAllowed` must name the home's ACTUAL default (`config.default_mint()`), never
    // the pinned testnut constant. On the shipped real-minibits default home, "(default stays
    // testnut)" was a money-relevant lie (`wallet mints list` correctly shows minibits as default).
    // Red-on-revert: interpolating DEFAULT_MINT_URL again names testnut on a minibits home.
    #[test]
    fn mint_not_allowed_names_home_default_not_testnut_constant() {
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
            !message.contains(DEFAULT_MINT_URL),
            "MintNotAllowed must NOT name the testnut constant as the default on a minibits home: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A same-process mint stub: answers `GET /v1/info` with `info`, everything else with 404, and
    /// RECORDS every request path it receives. The recorder is the instrument — what the wallet
    /// asked the mint is the whole question. No mint process, no money, no network beyond loopback.
    fn recording_mint_stub(info: &cdk::nuts::MintInfo) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Write};

        let body = serde_json::to_string(info).expect("mint info serializes");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub mint");
        let address = listener.local_addr().expect("stub mint address");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(clone) => clone,
                    Err(_) => continue,
                });
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) => break,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("").to_owned();
                recorder.lock().expect("recorder").push(path.clone());
                // Match the info route however the client spells its leading slashes (a normalized
                // mint URL may carry a trailing `/`, giving `//v1/info`).
                let route = path.trim_start_matches('/');
                let ok = |json: &str| {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                         connection: close\r\n\r\n{json}",
                        json.len()
                    )
                };
                let response = if route == "v1/info" {
                    ok(&body)
                } else if route == "v1/keysets" || route == "v1/keys" {
                    // cdk refreshes keysets alongside the info load; an empty set is a valid answer
                    // and keeps the stub honest about what it is: an info document, not a mint.
                    ok(r#"{"keysets":[]}"#)
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{address}"), seen)
    }

    /// Check H (§4.2 "Issuer mint"): fund-begin and fund-completion are guarded by the DECLARED
    /// class — this seat's own `issuer_mint` config — and the guard runs before the wallet opens.
    /// NEGATIVE: at the declared mint, `begin_mint_async` and `complete_mint_async` return
    /// `WalletOpsError::IssuerMint` and the mint receives NO request at all — no quote, no mint,
    /// and not even `/v1/info`, because nothing asks a mint what it is. The stub would answer as a
    /// LIGHTNING mint (bolt11 under NUT-04) if it were asked, so a class read off the wire would
    /// have let the call through: the declaration is what refuses it.
    /// CONTROL: the identical completion against an UNDECLARED stub goes past the guard and into
    /// `poll_and_mint`, where cdk fails an unknown quote id as an ordinary Wallet error — not
    /// IssuerMint, not a refusal — so the negative above is not vacuous.
    #[tokio::test]
    async fn completing_a_mint_quote_at_an_issuer_mint_is_refused_before_any_quote_call() {
        use cdk::nuts::{MintInfo, MintMethodSettings};

        fn is_quote_or_mint_call(path: &str) -> bool {
            path.contains("/mint/quote/") || path.contains("/mint/bolt11")
        }
        fn lightning_info() -> MintInfo {
            let mut info = MintInfo::new();
            info.nuts.nut04.methods = vec![MintMethodSettings {
                method: PaymentMethod::BOLT11,
                unit: CurrencyUnit::Sat,
                min_amount: None,
                max_amount: None,
                options: None,
            }];
            info
        }

        let (issuer_url, issuer_seen) = recording_mint_stub(&lightning_info());

        let root = temp_home("complete-at-issuer");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        // The loopback stub is a CONFIGURED mint, so `mint_is_allowed`/`resolve_mint` admit it and
        // the call reaches the guard under test rather than the configured-mint check; and it is
        // this seat's DECLARED issuer mint, which is the whole of what the guard reads.
        home.config.extra_mints.push(issuer_url.clone());
        home.config.issuer_mint = Some(issuer_url.clone());

        let begin = begin_mint_async(&home, 5, Some(&issuer_url))
            .await
            .expect_err("funding at a declared issuer mint must refuse");
        assert!(
            matches!(begin, WalletOpsError::IssuerMint(_)),
            "expected IssuerMint from begin, got: {begin}"
        );
        let quote = MintQuote {
            mint_url: issuer_url.clone(),
            invoice: "lnbc1-not-a-real-invoice".to_owned(),
            quote_id: "quote-at-issuer".to_owned(),
            amount_sats: 5,
        };
        let error = complete_mint_async(&home, &quote)
            .await
            .expect_err("completing at a declared issuer mint must refuse");
        let seen = issuer_seen.lock().expect("recorder").clone();
        assert!(
            matches!(error, WalletOpsError::IssuerMint(_)),
            "expected IssuerMint, got: {error} (mint saw {seen:?})"
        );
        let message = error.to_string();
        assert!(message.contains("wallet fund refused"), "{message}");
        assert!(message.contains(&issuer_url), "{message}");
        assert!(message.contains(crate::mint_class::ISSUER_HOP_REFUSAL), "{message}");
        assert!(
            seen.is_empty(),
            "a declared issuer mint is asked NOTHING — not a quote, not its info: {seen:?}"
        );
        assert!(
            !seen.iter().any(|path| is_quote_or_mint_call(path)),
            "neither check_mint_quote nor wallet.mint may reach an issuer mint: {seen:?}"
        );

        // CONTROL: same call, same kind of stub, NOT declared. The guard passes and execution goes
        // on into `poll_and_mint`, where cdk's `check_mint_quote` resolves an id the local store has
        // never seen WITHOUT a wire call and fails as an ordinary Wallet error — not IssuerMint,
        // and not a refusal. Identical call, identical unknown quote, identical info document: the
        // only difference is the declaration.
        let (lightning_url, _lightning_seen) = recording_mint_stub(&lightning_info());
        home.config.extra_mints.push(lightning_url.clone());
        let control = complete_mint_async(
            &home,
            &MintQuote {
                mint_url: lightning_url,
                invoice: "lnbc1-not-a-real-invoice".to_owned(),
                quote_id: "quote-at-lightning".to_owned(),
                amount_sats: 5,
            },
        )
        .await
        .expect_err("an unknown quote id fails inside poll_and_mint");
        assert!(
            matches!(control, WalletOpsError::Wallet(_)),
            "control must fail past the guard, not at it: {control}"
        );
        let control_message = control.to_string();
        assert!(
            !control_message.contains("refused"),
            "control must not be a guard refusal: {control_message}"
        );
    }

    /// Stage 3a (§7): the seat's own wallet ops ADMIT the seat's OWN issuer mint, and nothing else
    /// new. Two sites only — `send_async` and `receive_async` — and each is proved with its own
    /// control, because "the fence passed" is only meaningful beside an otherwise identical mint it
    /// still refuses.
    ///
    /// `allow_real_mints = false` throughout, so `home::mint_allowed` refuses EVERY loopback
    /// `http://` URL here. The only thing that can let one through is the seat's own declaration.
    ///
    /// NEGATIVE, and it is the load-bearing half: the SAME URL, configured and reachable but NOT
    /// declared this seat's issuer mint, is still `RealMintDisallowed`. And a URL a COUNTERPARTY
    /// declared is not this seat's declaration — `IssuerMints::none().with_own(...)` is built from
    /// config alone, so a `Declared` marker never reaches this predicate at all.
    #[tokio::test]
    async fn send_and_receive_admit_this_seats_own_issuer_mint_and_nothing_else_new() {
        use cdk::nuts::{Id, MintInfo, Proof, PublicKey};
        use cdk::secret::Secret;

        fn token_at(mint_url: &str) -> String {
            let keyset = Id::from_str("009a1f293253e41e").expect("a keyset id");
            let blinded = PublicKey::from_hex(
                "02194603ffa36356f4a56b7df9371fc3192472351453ec7398b8da8117e7c3e104",
            )
            .expect("a public key");
            let proof = Proof::new(Amount::from(1), keyset, Secret::generate(), blinded);
            Token::new(
                MintUrl::from_str(mint_url).expect("a mint url"),
                vec![proof],
                None,
                CurrencyUnit::Sat,
            )
            .to_string()
        }

        let (issuer_url, _issuer_seen) = recording_mint_stub(&MintInfo::new());
        let (other_url, _other_seen) = recording_mint_stub(&MintInfo::new());

        let root = temp_home("send-receive-at-issuer");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap(&root).expect("bootstrap");
        // Both stubs are CONFIGURED, so `mint_is_allowed` admits both and the difference the test
        // measures is the class fence alone. The real-money switch is OFF, so `home::mint_allowed`
        // refuses both on its own.
        home.config.extra_mints.push(issuer_url.clone());
        home.config.extra_mints.push(other_url.clone());
        home.config.allow_real_mints = false;
        assert!(!home::mint_allowed(&issuer_url, false));
        assert!(!home::mint_allowed(&other_url, false));

        // ── CONTROL, before any declaration exists: BOTH are refused by the real-mint fence. ─────
        for url in [&issuer_url, &other_url] {
            let refused = send_async(&home, 5, Some(url))
                .await
                .expect_err("an undeclared http mint is fenced");
            assert!(
                matches!(refused, WalletOpsError::RealMintDisallowed { .. }),
                "expected RealMintDisallowed at {url}, got: {refused}"
            );
            let refused = receive_async(&home, &token_at(url))
                .await
                .expect_err("an undeclared http mint is fenced");
            assert!(
                matches!(refused, WalletOpsError::RealMintDisallowed { .. }),
                "expected RealMintDisallowed at {url}, got: {refused}"
            );
        }

        // ── Declare ONE of them this seat's own issuer mint. Nothing else changes. ───────────────
        home.config.issuer_mint = Some(issuer_url.clone());

        // send: past the fence, and on into the wallet, where an empty balance stops it as an
        // ordinary Wallet error — not a refusal.
        let sent = send_async(&home, 5, Some(&issuer_url))
            .await
            .expect_err("nothing has been issued yet, so there is nothing to send");
        assert!(
            matches!(sent, WalletOpsError::Wallet(_)),
            "the seat's own issuer mint must pass the fence, got: {sent}"
        );
        assert!(
            sent.to_string().contains("insufficient funds"),
            "it failed past the fence, on balance: {sent}"
        );

        // receive: past the fence too. It then fails at the mint (the stub is not a mint), which is
        // exactly what "past the fence" looks like from here.
        let received = receive_async(&home, &token_at(&issuer_url))
            .await
            .expect_err("the stub cannot honour a fabricated proof");
        assert!(
            matches!(received, WalletOpsError::Wallet(_)),
            "the seat's own issuer mint must pass the fence, got: {received}"
        );

        // ── NEGATIVE: the OTHER stub — same scheme, same host, same config list — is still fenced.
        let refused = send_async(&home, 5, Some(&other_url))
            .await
            .expect_err("a mint this seat did not declare stays fenced");
        assert!(
            matches!(refused, WalletOpsError::RealMintDisallowed { .. }),
            "expected RealMintDisallowed at {other_url}, got: {refused}"
        );
        let refused = receive_async(&home, &token_at(&other_url))
            .await
            .expect_err("a mint this seat did not declare stays fenced");
        assert!(
            matches!(refused, WalletOpsError::RealMintDisallowed { .. }),
            "expected RealMintDisallowed at {other_url}, got: {refused}"
        );

        // ── NEGATIVE: a COUNTERPARTY's declaration cannot reach this predicate. The fence is built
        // from `home.config.issuer_mint()` alone, so a `Declared` marker is not even constructible
        // here — and if one were, `IssuerMints::admits` answers false for it.
        let declared_by_someone_else =
            crate::mint_class::IssuerMints::none().with_declared(Some(other_url.as_str()));
        assert!(!declared_by_someone_else.admits(&other_url));
        assert!(!crate::mint_class::mint_admitted(
            &other_url,
            false,
            &declared_by_someone_else
        ));

        // ── And `wallet melt` STILL REFUSES at the seat's own issuer mint. ──────────────────────
        //
        // Stage 3a deliberately did NOT make this site class-aware: melting is paying a Lightning
        // invoice, and an issuer mint has no Lightning, so there is nothing here for the class to
        // widen. Both guards above it are still in place and the call cannot reach a mint.
        //
        // ⚠ WHICH guard answers is worth stating, because it is not the one stage 2's message
        // suggests. The class-blind real-mint fence runs FIRST, and `home::mint_allowed` refuses
        // every `http://` URL under either setting of `allow_real_mints` (it admits only `https://`,
        // or the dev allow-list). So for a LOOPBACK issuer mint — the normal case, and the only one
        // the wizard writes — melt refuses as `RealMintDisallowed` and the issuer-specific message
        // at `refuse_lightning_op_at_issuer` is never reached. The refusal stands; only its wording
        // differs. An `https://` issuer mint would get the issuer message instead.
        for allow_real_mints in [false, true] {
            home.config.allow_real_mints = allow_real_mints;
            let melt = melt_async(&home, "lnbc1-not-a-real-invoice", Some(&issuer_url))
                .await
                .expect_err("melting at an issuer mint stays refused");
            assert!(
                matches!(melt, WalletOpsError::RealMintDisallowed { .. }),
                "with allow_real_mints={allow_real_mints}, melt at a loopback issuer mint is \
                 refused by the class-blind fence first, got: {melt}"
            );
        }
        home.config.allow_real_mints = false;
        // The issuer-specific refusal is still WIRED — it is what answers once the URL gets past
        // the real-mint fence, which only an `https://` mint does.
        assert_eq!(
            refuse_lightning_op_at_issuer(&home, "wallet melt", &issuer_url)
                .expect_err("still refuses")
                .to_string()
                .contains("wallet melt refused"),
            true
        );
    }
}
