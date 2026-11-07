use anyhow::{Context, Result, ensure};
use cdk::{
    cdk_database::WalletDatabase,
    nuts::CurrencyUnit,
    wallet::{Wallet, WalletBuilder},
};

use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
async fn info(mint: &str) -> Result<serde_json::Value> {
    crate::Asset::new(mint)?.fence()?;
    let info = crate::transport::get(mint, "info").await?;
    // The mint's clock decides claim deadlines (backoff claim rounds).
    if let Some(time) = info["time"].as_u64() {
        crate::backoff::observe_time(mint, time);
    }
    Ok(info)
}
pub async fn action_time(mint: &str) -> Result<u64> {
    let info = info(mint).await?;
    Ok(cdk::util::unix_time().max(info["time"].as_u64().context("missing mint time")?))
}
pub async fn refund_time(mint: &str) -> Result<u64> {
    // This clock is used for refund eligibility; the mint enforces locktime.
    // Missing info must not strand an otherwise working swap endpoint.
    let local = cdk::util::unix_time();
    Ok(match info(mint).await {
        Ok(info) => local.max(info["time"].as_u64().unwrap_or(local)),
        Err(_) => local,
    })
}
/// Bound one CDK wallet call against `mint`: 15 s for HTTP, the connector window plus its outer
/// margin for `nostr://` (see `transport`). The only CDK wallet bound: there is deliberately no
/// mint-agnostic variant, so a new call site cannot silently get the 15 s HTTP bound on nostr.
/// An enclosing budget (advance, message, recovery item) can still drop a call mid-window; that
/// is ambiguous, never a failure: swaps are journaled first and restored before any replay.
/// Gated by [`crate::backoff`]: while `mint` backs off the call is not started ([`Bound::Deferred`]);
/// a timeout or a 429/503/connection failure starts or extends the backoff.
pub async fn bounded_for<T>(
    mint: &str,
    f: impl std::future::Future<Output = Result<T, cdk::Error>>,
) -> Result<Result<T, cdk::Error>, Bound> {
    // Display only: a timed-out or unanswered wallet call shows in the swap's progress lines.
    crate::progress::call(
        mint,
        "wallet",
        bounded_inner(mint, f),
        crate::progress::of_wallet,
    )
    .await
}
async fn bounded_inner<T>(
    mint: &str,
    f: impl std::future::Future<Output = Result<T, cdk::Error>>,
) -> Result<Result<T, cdk::Error>, Bound> {
    crate::backoff::admit(mint, false).map_err(Bound::Deferred)?;
    match tokio::time::timeout(crate::transport::wallet_bound(mint), f).await {
        Err(_) => {
            crate::backoff::record(
                mint,
                crate::backoff::Outcome::Failed(crate::backoff::Reason::Timeout, None),
            );
            Err(Bound::Elapsed)
        }
        Ok(r) => {
            if let Err(e) = &r {
                crate::backoff::record_cdk(mint, e);
            }
            Ok(r)
        }
    }
}
/// Why [`bounded_for`] produced no CDK result. Both are ambiguous (no answer), never refusals.
#[derive(Debug)]
pub enum Bound {
    Elapsed,
    Deferred(crate::backoff::Deferred),
}
impl std::fmt::Display for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bound::Elapsed => write!(f, "no answer within the CDK wallet bound"),
            Bound::Deferred(d) => d.fmt(f),
        }
    }
}
impl std::error::Error for Bound {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Bound::Elapsed => None,
            Bound::Deferred(d) => Some(d),
        }
    }
}
pub async fn preflight(mint: &str) -> Result<()> {
    preflight_for(mint, None).await
}
pub async fn preflight_for(mint: &str, operation: Option<&str>) -> Result<()> {
    preflight_impl(mint, operation, false).await.map(|_| ())
}
/// New list/take admission only. Recovery uses the original preflight without this gate.
pub async fn preflight_admission(mint: &str) -> Result<serde_json::Value> {
    preflight_impl(mint, None, true).await
}
async fn preflight_impl(
    mint: &str,
    operation: Option<&str>,
    admission: bool,
) -> Result<serde_json::Value> {
    let started = std::time::Instant::now();
    let mut observation = None;
    let result = if admission {
        crate::Asset::new(mint)?.fence()?;
        crate::transport::get_observed(mint, "info", &mut observation).await
    } else {
        info(mint).await
    };
    let info = match result {
        Ok(info) => info,
        Err(error) => {
            if admission {
                crate::diagnostics::emit(
                    serde_json::json!({"mint":mint,"op":"info","reachability":if crate::backoff::deferred(&error) {"not_checked_backoff"} else {"unsuccessful"},"elapsed_ms":started.elapsed().as_millis(),"compatibility":"not_checked"}),
                );
            }
            return Err(error);
        }
    };
    if let Some(time) = info["time"].as_u64() {
        crate::backoff::observe_time(mint, time);
    }
    // A slow warning describes the matched reply, not bounded disconnect cleanup afterward.
    let elapsed = observation
        .as_ref()
        .and_then(|o| o.reply_ms)
        .map(u128::from)
        .unwrap_or_else(|| started.elapsed().as_millis());
    let compatibility = crate::compatibility::assess(&info);
    if admission {
        crate::diagnostics::emit(
            serde_json::json!({"mint":mint,"op":"info","reachability":"answered","elapsed_ms":elapsed,"latency":if elapsed > 5000 {"slow (warning threshold 5s)"} else {"normal"},"compatibility":compatibility}),
        );
        ensure!(
            compatibility["status"] != "incompatible",
            "{}; missing {}",
            compatibility["message"]
                .as_str()
                .unwrap_or("upgrade the mint sidecar"),
            compatibility["missing"]
        );
    }
    ensure!(
        info["nuts"]["9"]["supported"] == true && info["nuts"]["12"]["supported"] == true,
        "mint does not advertise NUT-09 and NUT-12"
    );
    ensure!(
        info["nuts"]["7"]["supported"] == true && info["nuts"]["14"]["supported"] == true,
        "mint does not advertise NUT-07 and NUT-14"
    );
    ensure!(
        info["nuts"]["11"]["supported"] == true,
        "mint does not advertise NUT-11"
    );
    if let Some(operation) = operation {
        let nut = if operation == "fund" { "4" } else { "5" };
        ensure!(
            info["nuts"][nut]["disabled"] == false
                && info["nuts"][nut]["methods"]
                    .as_array()
                    .is_some_and(|methods| methods
                        .iter()
                        .any(|m| m["method"] == "bolt11" && m["unit"] == "sat")),
            "mint does not enable required bolt11/sat NUT-{nut}"
        );
        if operation == "fund" {
            ensure!(
                info["nuts"]["20"]["supported"] == true,
                "mint does not advertise NUT-20"
            );
        }
    }
    let now = cdk::util::unix_time();
    let time = info["time"]
        .as_u64()
        .context("mint clock uncertainty: missing info time")?;
    ensure!(
        now.abs_diff(time) <= 60,
        "mint clock skew exceeds 60 seconds"
    );
    let keysets = crate::transport::get(mint, "keysets").await?;
    ensure!(
        keysets["keysets"].as_array().is_some_and(|sets| sets
            .iter()
            .any(|k| k["unit"] == "sat" && k["active"] == true)),
        "mint has no active sat keyset"
    );
    eprintln!(
        "{}",
        serde_json::json!({"mint":mint,"version":compatibility["version"],"nut07":true,"nut07_witnesses":"unverified: requires a known spent HTLC; advertisement is not emission evidence","nut14":true,"clock_skew_seconds":now.abs_diff(time)})
    );
    Ok(keysets)
}
/// serve runs units concurrently (`sched`): creating `wallet.seed` and opening (migrating) a
/// wallet database happen one at a time, so two first uses of a mint never race.
static OPEN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub async fn wallet(home: &std::path::Path, mint: &str) -> Result<Wallet> {
    crate::Asset::new(mint)?.fence()?;
    if let Layout::Shared { root } = layout(home) {
        // No mint policy here: recovery of an in-flight item must always be able to open its
        // wallet. New work is admitted through `check_mint` (CLI, list/take, serve).
        let mh = maxplayer_home(&root)?;
        let _opening = OPEN.lock().await;
        let wallet_dir = root.join("wallet");
        if !wallet_dir.exists() {
            fs::create_dir_all(&wallet_dir)?;
            fs::set_permissions(&wallet_dir, fs::Permissions::from_mode(0o700))?;
        }
        let connector = if crate::transport::is_nostr(mint) {
            Some(crate::transport::connector(mint)?)
        } else {
            None
        };
        return maxplayer_core::shared_wallet::open_shared_wallet(&mh, mint, connector)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"));
    }
    let opening = OPEN.lock().await;
    let path = home.join("wallet.seed");
    if !path.exists() {
        let a = cashu::nuts::SecretKey::generate();
        let b = cashu::nuts::SecretKey::generate();
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(&a.to_secret_bytes())?;
        f.write_all(&b.to_secret_bytes())?;
        f.sync_all()?;
        std::fs::File::open(home)?.sync_all()?;
    }
    let seed: [u8; 64] = fs::read(path)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid seed"))?;
    let asset = hex::encode(Sha256::digest(format!("{mint}|sat")));
    let db_path = home.join(format!("{asset}.sqlite"));
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&db_path)?;
    let db = cdk_sqlite::WalletSqliteDatabase::new(db_path).await?;
    drop(opening);
    let builder = WalletBuilder::new()
        .mint_url(mint.parse()?)
        .unit(CurrencyUnit::Sat)
        .localstore(Arc::new(db))
        .seed(seed);
    if crate::transport::is_nostr(mint) {
        // Same construction as maxplayer_core::nostr_mint::build_wallet_with_relays, over the
        // configured mint relays. cdk's WebSocket subscriptions cannot address a nostr:// URL.
        return Ok(builder
            .client(crate::transport::connector(mint)?)
            .use_http_subscription()
            .build()?);
    }
    Ok(builder.build()?)
}

pub async fn database(
    home: &std::path::Path,
    mint: &str,
) -> Result<Arc<cdk_sqlite::WalletSqliteDatabase>> {
    let _ = wallet(home, mint).await?;
    let _opening = OPEN.lock().await;
    Ok(Arc::new(
        cdk_sqlite::WalletSqliteDatabase::new(db_path(home, mint)).await?,
    ))
}
/// The wallet database holding `mint`'s proofs: the shared `<home>/wallet/cdk-wallet.sqlite` of a
/// maxplayer home, or the legacy per-mint `<home>/<sha256(mint|sat)>.sqlite`.
pub fn db_path(home: &std::path::Path, mint: &str) -> std::path::PathBuf {
    match layout(home) {
        Layout::Shared { root } => {
            maxplayer_core::shared_wallet::wallet_db_path(&root.join("wallet"))
        }
        Layout::Legacy => {
            let asset = hex::encode(Sha256::digest(format!("{mint}|sat")));
            home.join(format!("{asset}.sqlite"))
        }
    }
}
pub fn identity(home: &std::path::Path) -> Result<nostr_sdk::Keys> {
    if let Layout::Shared { root } = layout(home) {
        // The home's own key (linked with `maxplayer account link`); never created here.
        return shared_identity(&root);
    }
    let p = home.join("trade.key");
    if !p.exists() {
        let k = nostr_sdk::Keys::generate();
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&p)?;
        f.write_all(k.secret_key().to_secret_hex().as_bytes())?;
        f.sync_all()?;
        std::fs::File::open(home)?.sync_all()?;
    }
    Ok(nostr_sdk::Keys::parse(&fs::read_to_string(p)?)?)
}
/// This home's `trade.key` when it already exists. Never creates the key or the home: for
/// read-only callers such as `doctor`.
pub fn existing_identity(home: &std::path::Path) -> Result<Option<nostr_sdk::Keys>> {
    if let Layout::Shared { root } = layout(home) {
        return shared_identity(&root).map(Some);
    }
    let p = home.join("trade.key");
    match fs::symlink_metadata(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(_) => Ok(Some(nostr_sdk::Keys::parse(
            fs::read_to_string(&p)?.trim(),
        )?)),
    }
}

/// Read-only SQLite snapshot: no seed creation, migrations, recovery or owner lock.
pub fn read_balance(home: &std::path::Path, mint: &str) -> Result<u64> {
    let path = db_path(home, mint);
    if !path.exists() {
        return Ok(0);
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(db.query_row("SELECT COALESCE(SUM(amount),0) FROM proof WHERE mint_url=?1 AND unit='sat' AND state='UNSPENT' AND used_by_operation IS NULL AND spending_condition IS NULL", [mint], |r| r.get(0))?)
}
pub fn read_status(home: &std::path::Path) -> Result<serde_json::Value> {
    let path = state_dir(home).join("trade.sqlite");
    if !path.exists() {
        return Ok(serde_json::json!({"records":[]}));
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    let mut stmt = db.prepare("SELECT secondary_namespace, value FROM kv_store WHERE primary_namespace='trade-v1' AND secondary_namespace IN ('swap','funding','withdrawal','receive','send')")?;
    let mut records = vec![];
    for row in stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
    })? {
        let (kind, bytes) = row?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        let public = if kind == "withdrawal" {
            serde_json::from_slice::<crate::money::Withdrawal>(&bytes)?.summary()
        } else if kind == "send" {
            crate::send::public(&bytes)?
        } else if kind == "receive" {
            // Same public view as `recover`; the token is never part of it.
            let mut s = serde_json::from_slice::<crate::receive::Receipt>(&bytes)?.summary();
            s["kind"] = kind.clone().into();
            s
        } else {
            serde_json::json!({"kind":kind,"id":v["id"],"state":v["state"],"done":v["done"],"expired_unpaid":v["expired_unpaid"]})
        };
        records.push(public);
    }
    Ok(serde_json::json!({"records":records}))
}

/// Which kind of directory `--home` (or a library caller's `home`) names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// A maxplayer home (`key`, `config.toml`, `wallet/`): trading signs with the home `key` and
    /// uses the buyer's wallet; trade-only files live in `<home>/trade/`.
    Shared { root: PathBuf },
    /// A standalone trading home (`wallet.seed`, `trade.key`, per-mint databases). Deprecated:
    /// kept so its money can be drained and its in-flight trades finished.
    Legacy,
}
/// A directory holding `wallet.seed`, `trade.key` or a root `trade.sqlite` / `owner.lock` is a
/// legacy standalone trading home (a maxplayer home keeps those under `trade/`).
pub fn is_legacy_dir(home: &Path) -> bool {
    ["wallet.seed", "trade.key", "trade.sqlite", "owner.lock"]
        .iter()
        .any(|f| fs::symlink_metadata(home.join(f)).is_ok())
}
/// A directory with a `key` or `config.toml` (and no legacy marker) is a maxplayer home.
pub fn is_maxplayer_home(home: &Path) -> bool {
    !is_legacy_dir(home)
        && ["key", "config.toml"]
            .iter()
            .any(|f| fs::symlink_metadata(home.join(f)).is_ok())
}
/// The layout of `home`. A directory that is neither a maxplayer home nor anything else is a
/// legacy home for the LIBRARY (it creates `wallet.seed` on first use, as before); the CLI never
/// lets a new command reach such a directory (see the binary's home resolution).
pub fn layout(home: &Path) -> Layout {
    if is_maxplayer_home(home) {
        Layout::Shared {
            root: home.to_path_buf(),
        }
    } else {
        Layout::Legacy
    }
}
/// Where trade's own files live (journal `trade.sqlite`, `owner.lock`, `serve.sock`):
/// `<home>/trade` for a maxplayer home, the home itself for a legacy one.
pub fn state_dir(home: &Path) -> PathBuf {
    match layout(home) {
        Layout::Shared { root } => root.join("trade"),
        Layout::Legacy => home.to_path_buf(),
    }
}
/// Create [`state_dir`] (owner-only) and return it.
pub fn ensure_state_dir(home: &Path) -> Result<PathBuf> {
    let dir = state_dir(home);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}
/// The maxplayer home at `root`, read-only: nothing is created or rewritten (no key, config or
/// `wallet/`). Config comes through core's resolver (file layer + `MAXPLAYER_*` environment).
pub fn maxplayer_home(root: &Path) -> Result<maxplayer_core::home::MaxplayerHome> {
    let config = maxplayer_core::home::resolve_config_read_only(root)
        .map_err(|e| anyhow::anyhow!("maxplayer home {}: {e}", root.display()))?;
    Ok(maxplayer_core::home::MaxplayerHome {
        root: root.to_path_buf(),
        config,
        key_path: root.join("key"),
        wallet_dir: root.join("wallet"),
        key_created: false,
    })
}
fn shared_identity(root: &Path) -> Result<nostr_sdk::Keys> {
    let path = root.join("key");
    ensure!(
        fs::symlink_metadata(&path).is_ok(),
        "maxplayer home {} has no key: create and link it with `maxplayer account link --home {}`",
        root.display(),
        root.display()
    );
    let secret =
        maxplayer_core::home::read_key_file_hex(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(nostr_sdk::Keys::parse(&secret)?)
}
/// In a maxplayer home only one of the home's mints (`maxplayer_core::wallet_ops::tradeable_mints`:
/// the default, `extra_mints` and the seller's `accepted_mints`) that the real-mint fence
/// (`allow_real_mints`) admits may be traded: a seller is paid at every accepted mint into this same
/// wallet. Plain HTTP is refused outright (no http:// mints in a user build); only a
/// `local-test-mints` test build lets a loopback HTTP fixture mint past the real-mint fence.
pub fn check_shared_mint(root: &Path, mint: &str) -> Result<()> {
    let mh = maxplayer_home(root)?;
    let normalized =
        maxplayer_core::wallet_ops::normalize_mint_url(mint).map_err(|e| anyhow::anyhow!("{e}"))?;
    let parsed = url::Url::parse(&normalized).ok();
    let local_test = parsed
        .as_ref()
        .is_some_and(crate::transport::local_test_http_mint);
    ensure!(
        local_test || parsed.as_ref().is_none_or(|u| u.scheme() != "http"),
        "{}",
        crate::transport::HTTP_MINT_REFUSED
    );
    ensure!(
        local_test || maxplayer_core::home::mint_allowed(&normalized, mh.config.allow_real_mints),
        "mint {normalized} is refused by this home's real-mint fence (allow_real_mints is off in \
         {}); trading uses only mints this home's wallet may use",
        root.join("config.toml").display()
    );
    let tradeable =
        maxplayer_core::wallet_ops::tradeable_mints(&mh).map_err(|e| anyhow::anyhow!("{e}"))?;
    ensure!(
        tradeable.contains(&normalized),
        "mint {normalized} is not one of this home's mints (default, extra_mints or the seller's \
         accepted_mints: {}); add it to extra_mints with `maxplayer wallet mints add {normalized} \
         --home {}`, or, on a seller, to accepted_mints in {} (then restart the seller node)",
        tradeable.join(", "),
        root.display(),
        root.join("config.toml").display()
    );
    Ok(())
}
/// [`check_shared_mint`] for any home (no-op for a legacy home).
pub fn check_mint(home: &Path, mint: &str) -> Result<()> {
    match layout(home) {
        Layout::Shared { root } => check_shared_mint(&root, mint),
        Layout::Legacy => Ok(()),
    }
}

/// The wallet selection lock for `home` (shared with the buyer daemon and `maxplayer wallet`; see
/// `maxplayer_core::shared_wallet`). A legacy home has one owner (`owner.lock`) and takes none.
/// Hold it across "verify these proofs are free, journal the intent, reserve them" and across any
/// release; never across a mint request.
pub struct Select {
    home: PathBuf,
    mint: String,
    _lock: Option<maxplayer_core::shared_wallet::SelectLock>,
}
pub async fn select(home: &Path, mint: &str) -> Result<Select> {
    let lock = match layout(home) {
        Layout::Shared { root } => Some(
            maxplayer_core::shared_wallet::acquire(
                &root.join("wallet"),
                maxplayer_core::shared_wallet::SELECT_LOCK_TIMEOUT,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        ),
        Layout::Legacy => None,
    };
    Ok(Select {
        home: home.to_path_buf(),
        mint: mint.to_owned(),
        _lock: lock,
    })
}
impl Select {
    /// Every proof in `ys` is still UNSPENT, unowned and unlocked in the wallet database (another
    /// process may have taken one since it was selected). Nothing changes on a refusal.
    pub async fn verify_free(&self, ys: &[cashu::PublicKey]) -> Result<()> {
        refuse_if_taken(&self.home, &self.mint, ys).await
    }
    /// Reserve exactly `ys` under `op`. Proofs `op` already holds stay held (idempotent); `op`
    /// may hold nothing outside `ys` ("reservation conflict"). Every other proof must be UNSPENT,
    /// or `ProofNotUnspent`. cdk reserves row by row: if it fails part-way, the rows THIS call
    /// reserved under `op` (and only those) are released before the error is returned, so a
    /// retry starts from the same state and nothing is left reserved under an operation that
    /// failed.
    pub async fn reserve(&self, ys: &[cashu::PublicKey], op: &uuid::Uuid) -> Result<()> {
        use cashu::State;
        let db = database(&self.home, &self.mint).await?;
        let held = db.get_reserved_proofs(op).await?;
        ensure!(
            held.iter().all(|p| ys.contains(&p.y)),
            "reservation conflict"
        );
        // cdk's reserve_proofs takes each proof only `WHERE state = 'UNSPENT'`, one row at a time.
        let missing: Vec<_> = ys
            .iter()
            .filter(|y| !held.iter().any(|p| p.y == **y))
            .copied()
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if let Err(error) = db.reserve_proofs(missing.clone(), op).await {
            let undo = db
                .get_proofs_by_ys(missing)
                .await?
                .into_iter()
                .filter(|r| r.state == State::Reserved && r.used_by_operation == Some(*op))
                .map(|mut r| {
                    r.state = State::Unspent;
                    r.used_by_operation = None;
                    r
                })
                .collect::<Vec<_>>();
            if !undo.is_empty() {
                db.update_proofs(undo, vec![]).await?;
            }
            return Err(error.into());
        }
        Ok(())
    }
}
/// [`Select::reserve`] under a fresh selection lock (recovery re-applies a journaled reservation).
pub async fn reserve(
    home: &Path,
    mint: &str,
    ys: &[cashu::PublicKey],
    op: &uuid::Uuid,
) -> Result<()> {
    select(home, mint).await?.reserve(ys, op).await
}
/// Release every proof `op` holds (`UPDATE … WHERE used_by_operation = op`), under the selection
/// lock so it never races a buyer-side NUT-13 restore that is putting this owner's rows back.
pub async fn release(home: &Path, mint: &str, op: &uuid::Uuid) -> Result<()> {
    let _held = select(home, mint).await?;
    database(home, mint).await?.release_proofs(op).await?;
    Ok(())
}

/// Printed (stderr) by every command run against a legacy standalone trading home.
pub fn legacy_warning(home: &Path) -> String {
    format!(
        "warning: {} is a legacy standalone trading home (wallet.seed / trade.key); this mode is \
         deprecated. Only draining it and finishing in-flight trades are allowed there (balance, \
         status, recover, send, withdraw, serve, cancel, doctor). Move its funds to your maxplayer \
         home and trade from there.",
        home.display()
    )
}
/// Why a NEW list/take/fund/receive is refused in a legacy home, and how to move its money.
pub fn legacy_refusal(home: &Path) -> String {
    format!(
        "refused: {h} is a legacy standalone trading home; new trades and deposits are no longer \
         made there (finish in-flight trades with `serve`/`recover`/`cancel` first). Move its funds \
         to your maxplayer home: `maxplayer-trade --home {h} send <mint> --amount <n> --out <file>`, \
         then `maxplayer wallet receive \"$(cat <file>)\"`. Then trade from the maxplayer home \
         (`maxplayer-trade` with no --home uses $MAXPLAYER_HOME or ~/.maxplayer).",
        h = home.display()
    )
}

/// Refusal when a chosen input was taken by another operation on this home after it was chosen.
pub const INPUTS_CHANGED: &str = "the wallet's spendable proofs changed while this operation chose \
     its inputs (another process on this home spent or reserved one); nothing was journaled or \
     reserved; retry";
/// `Err(INPUTS_CHANGED)` unless every proof in `ys` is still UNSPENT, unowned and unlocked in the
/// wallet database. Proofs are chosen from a snapshot without the selection lock; on a shared home
/// the buyer may reserve or spend one before the choice is checked (NUT-07) or reserved.
pub async fn refuse_if_taken(home: &Path, mint: &str, ys: &[cashu::PublicKey]) -> Result<()> {
    let db = database(home, mint).await?;
    let rows = db.get_proofs_by_ys(ys.to_vec()).await?;
    ensure!(
        ys.iter().all(|y| rows.iter().any(|r| r.y == *y
            && r.state == cashu::State::Unspent
            && r.used_by_operation.is_none()
            && r.spending_condition.is_none())),
        "{INPUTS_CHANGED}"
    );
    Ok(())
}
