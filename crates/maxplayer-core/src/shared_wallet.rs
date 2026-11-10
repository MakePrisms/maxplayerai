//! The buyer wallet shared with other local processes on the same home (`maxplayer-trade`).
//!
//! One home, one CDK wallet database (`<home>/wallet/cdk-wallet.sqlite`), one seed (derived from the
//! home `key`). Two processes may write it: the buyer daemon / `maxplayer wallet` CLI and
//! `maxplayer-trade`. SQLite serializes the writes (cdk-sqlite opens every connection with WAL,
//! `busy_timeout = 10000` and `BEGIN IMMEDIATE` transactions); what SQLite cannot serialize is a
//! READ-then-MARK step: cdk selects UNSPENT proofs, then reserves them one row at a time
//! (`reserve_proofs` is `UPDATE … WHERE state = 'UNSPENT'` per proof, not one transaction). Two
//! processes selecting at once could each reserve part of the other's selection, leaving a cdk saga
//! that failed half-way with proofs RESERVED under an operation nothing will ever release.
//!
//! [`SelectLock`] closes that window: an exclusive OS advisory lock (`flock`) on
//! `<home>/wallet/select.lock`, held ONLY across LOCAL database work: "pick unspent proofs + reserve
//! them" (cdk's `prepare_send` / `prepare_melt` via [`prepare_send_locked`] / [`prepare_melt_locked`],
//! trade's verify-and-reserve), trade's credit of new proofs, and the local apply of a recovered swap
//! saga. No mint request runs while it is held: the mint metadata cdk's prepare re-reads (info,
//! keysets) is fetched BEFORE the lock ([`warm_metadata`]) and replayed inside it ([`local_only`]),
//! which refuses every other mint call. Acquisition is bounded ([`SELECT_LOCK_TIMEOUT`]); a timeout
//! is a retryable refusal that has reserved nothing. The kernel drops a `flock` when its descriptor
//! closes, including when the holder is killed (`kill -9`), so a crash never leaves the lock held.
//!
//! Ownership: every reservation names its owner in `used_by_operation` (a cdk saga id for the buyer,
//! a journal operation id for trade). Recovery only ever writes rows it owns or inserts rows that do
//! not exist yet. cdk's full-wallet NUT-13 `Wallet::restore` upserts every re-derived seed proof with
//! NO owner, one batch per commit, so it would publish other owners' reservations as spendable; it is
//! never called (a crate-wide test enforces that), and a spent swap saga recovers only its own outputs
//! (`payment_wallet::complete_spent_swap_saga`).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cdk::nuts::{
    BatchCheckMintQuoteRequest, BatchMintRequest, CheckStateRequest, CheckStateResponse,
    CurrencyUnit, Id, KeySet, KeysetResponse, MeltRequest, MintInfo, MintRequest, MintResponse,
    PaymentMethod, RestoreRequest, RestoreResponse, SwapRequest, SwapResponse,
};
use cdk::wallet::{AuthWallet, LnurlPayInvoiceResponse, LnurlPayResponse, MintConnector, Wallet};
use cdk::{
    Error as CdkError, MeltQuoteCreateResponse, MeltQuoteRequest, MeltQuoteResponse,
    MintQuoteRequest, MintQuoteResponse,
};
use cdk_sqlite::wallet::WalletSqliteDatabase;

use crate::buyer_fund::{FundError, seed_from_secret_hex};
use crate::home::{self, MaxplayerHome};

/// The lock file inside the home's `wallet/` directory.
pub const SELECT_LOCK_FILE: &str = "select.lock";

/// How long a caller waits for the selection lock before refusing (retryable, nothing reserved).
pub const SELECT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

const POLL: Duration = Duration::from_millis(20);

/// The shared wallet database inside a home's `wallet/` directory.
pub fn wallet_db_path(wallet_dir: &Path) -> PathBuf {
    wallet_dir.join("cdk-wallet.sqlite")
}

/// A held selection lock. Dropping it (or the process exiting, however it exits) releases it.
#[derive(Debug)]
pub struct SelectLock {
    _file: File,
}

/// Failure to take the selection lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectLockError {
    /// Another operation held the lock for the whole wait. Nothing was selected or reserved;
    /// retrying is safe.
    Busy { path: PathBuf, waited: Duration },
    /// The lock file could not be opened, or `flock` failed for another reason.
    Io(String),
}

impl std::fmt::Display for SelectLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { path, waited } => write!(
                formatter,
                "the wallet is busy: another process is selecting proofs ({} held for {}s); \
                 nothing was reserved, retry",
                path.display(),
                waited.as_secs()
            ),
            Self::Io(message) => write!(formatter, "wallet selection lock: {message}"),
        }
    }
}

impl std::error::Error for SelectLockError {}

fn open_lock_file(wallet_dir: &Path) -> Result<(PathBuf, File), SelectLockError> {
    open_named_lock_file(wallet_dir, SELECT_LOCK_FILE)
}

fn open_named_lock_file(wallet_dir: &Path, name: &str) -> Result<(PathBuf, File), SelectLockError> {
    let path = wallet_dir.join(name);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| SelectLockError::Io(format!("{}: {error}", path.display())))?;
    Ok((path, file))
}

fn try_flock(file: &File) -> Result<bool, SelectLockError> {
    try_flock_mode(file, libc::LOCK_EX)
}

fn try_flock_mode(file: &File, mode: libc::c_int) -> Result<bool, SelectLockError> {
    // Safety: plain libc call on a valid descriptor owned by `file`, which outlives the call.
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK => Ok(false),
        Some(code) if code == libc::EINTR => Ok(false),
        _ => Err(SelectLockError::Io(error.to_string())),
    }
}

/// Take the selection lock for the wallet in `wallet_dir`, waiting at most `timeout`.
///
/// `flock` belongs to the open file description, so two acquisitions in ONE process (two serve
/// units, two daemon threads) exclude each other exactly like two processes do.
pub async fn acquire(wallet_dir: &Path, timeout: Duration) -> Result<SelectLock, SelectLockError> {
    let (path, file) = open_lock_file(wallet_dir)?;
    let started = tokio::time::Instant::now();
    loop {
        if try_flock(&file)? {
            return Ok(SelectLock { _file: file });
        }
        let waited = started.elapsed();
        if waited >= timeout {
            return Err(SelectLockError::Busy { path, waited });
        }
        tokio::time::sleep(POLL.min(timeout - waited)).await;
    }
}

/// Non-blocking variant: `Ok(None)` when another holder has it.
pub fn try_acquire(wallet_dir: &Path) -> Result<Option<SelectLock>, SelectLockError> {
    let (_, file) = open_lock_file(wallet_dir)?;
    Ok(try_flock(&file)?.then_some(SelectLock { _file: file }))
}

/// The selection guard for a wallet: `Some(dir)` takes `dir`'s lock (bounded wait), `None` takes
/// nothing (only for wallets no other process shares: in-memory test wallets). Hold the returned
/// guard across cdk's `prepare_send` / `prepare_melt` (selection + reservation) run under
/// [`local_only`], and drop it as soon as they return: the chosen proofs are then RESERVED under
/// their operation and no other selector can take them. Take it OUTSIDE any mint timeout, so a busy
/// wallet reads as busy, not as a dead mint, and AFTER [`warm_metadata`], so no mint request runs
/// while it is held.
pub async fn select_guard(
    wallet_dir: Option<&Path>,
) -> Result<Option<SelectLock>, SelectLockError> {
    match wallet_dir {
        Some(dir) => acquire(dir, SELECT_LOCK_TIMEOUT).await.map(Some),
        None => Ok(None),
    }
}

// ---- Operation lock: evidence that a send or melt is still running ----------------------------
//
// cdk 0.17.2 sagas say nothing about whether the process that wrote them is alive. A send's or
// melt's `confirm` swaps with `ProofReservation::Skip`, so for the whole time its swap request is
// out, the parent saga owns the inputs and the child swap saga owns nothing but its stored blinded
// messages. To another process on this home, a live operation and a crashed one look identical in
// the database. The operation lock is the evidence the code can check instead: every core send and
// melt takes it SHARED before cdk's `prepare_*` creates the parent saga and holds it until
// `confirm`/`cancel` returns (or its future is dropped), and recovery asks for it EXCLUSIVE without
// waiting ([`operation_in_flight`]). The kernel drops a `flock` when its descriptor closes, so a
// killed process (even `kill -9`) never leaves an operation looking alive.

/// The operation lock file inside the home's `wallet/` directory.
pub const OPERATION_LOCK_FILE: &str = "operation.lock";

/// A held (shared) operation lock: one cdk send or melt of this process is between `prepare` and
/// the end of its `confirm`/`cancel`. Dropping it ends that.
#[derive(Debug)]
pub struct OperationGuard {
    _file: File,
}

/// Take the operation lock SHARED for one send or melt on the wallet in `wallet_dir`. Take it BEFORE
/// cdk's `prepare_send`/`prepare_melt` (so the parent saga never exists without it) and keep it until
/// `confirm`/`cancel` has returned. Any number of operations hold it at once; it only waits while a
/// recovery pass is probing ([`operation_in_flight`] holds it exclusive for one system call), and
/// that wait is bounded ([`SELECT_LOCK_TIMEOUT`], a retryable refusal before anything is prepared).
/// `None` takes nothing (wallets no other process opens: in-memory tests).
pub async fn operation_guard(
    wallet_dir: Option<&Path>,
) -> Result<Option<OperationGuard>, SelectLockError> {
    let Some(dir) = wallet_dir else {
        return Ok(None);
    };
    let (path, file) = open_named_lock_file(dir, OPERATION_LOCK_FILE)?;
    let started = tokio::time::Instant::now();
    loop {
        if try_flock_mode(&file, libc::LOCK_SH)? {
            return Ok(Some(OperationGuard { _file: file }));
        }
        let waited = started.elapsed();
        if waited >= SELECT_LOCK_TIMEOUT {
            return Err(SelectLockError::Busy { path, waited });
        }
        tokio::time::sleep(POLL.min(SELECT_LOCK_TIMEOUT - waited)).await;
    }
}

/// Whether any process (this one included) holds an [`OperationGuard`] on the wallet in
/// `wallet_dir` right now: a send or melt there is between `prepare` and the end of `confirm`.
/// Never waits. `false` means every operation whose saga the caller read BEFORE this call has ended
/// in its process (returned, failed, was dropped, or the process died) and will write nothing more.
/// `None` (a wallet no other process opens) is never in flight.
pub fn operation_in_flight(wallet_dir: Option<&Path>) -> Result<bool, SelectLockError> {
    let Some(dir) = wallet_dir else {
        return Ok(false);
    };
    let (_, file) = open_named_lock_file(dir, OPERATION_LOCK_FILE)?;
    // Acquired ⇒ nobody holds it; closing `file` releases it at once.
    Ok(!try_flock_mode(&file, libc::LOCK_EX)?)
}

/// Open this home's shared wallet at `mint_url` for another local process (`maxplayer-trade`):
/// the same database ([`wallet_db_path`]) and the same seed ([`seed_from_secret_hex`] of the home
/// key) the buyer uses, never a second database under the same seed (its derivation counters and
/// secrets would collide with this one's).
///
/// Policy (configured mints, real-mint fence) is the caller's. A `nostr://` mint needs the caller's `connector` (built over the caller's mint relays, signing
/// as the home key); an `https://` mint is exactly `Wallet::new`.
pub async fn open_shared_wallet(
    home: &MaxplayerHome,
    mint_url: &str,
    connector: Option<crate::nostr_mint::NostrMintConnector>,
) -> Result<Wallet, FundError> {
    let secret = home::read_secret_key_hex(home)?;
    let seed = seed_from_secret_hex(&secret)?;
    let store = WalletSqliteDatabase::new(wallet_db_path(&home.wallet_dir))
        .await
        .map_err(|error| FundError::Wallet(error.to_string()))?;
    if crate::mint_wire::is_nostr_scheme(mint_url) {
        let connector = connector.ok_or_else(|| {
            FundError::Wallet(format!("nostr:// mint {mint_url} needs a relay connector"))
        })?;
        return crate::nostr_mint::build_wallet_with_connector(
            mint_url,
            CurrencyUnit::Sat,
            Arc::new(store),
            seed,
            None,
            connector,
        )
        .map(scoped)
        .map_err(|error| FundError::Wallet(error.to_string()));
    }
    Wallet::new(mint_url, CurrencyUnit::Sat, Arc::new(store), seed, None)
        .map(scoped)
        .map_err(|error| FundError::Wallet(error.to_string()))
}

/// The wallet seed of the home whose `wallet/` directory is `wallet_dir`: `seed_from_secret_hex` of
/// `<home>/key` (every home keeps `wallet/` next to `key`). Read only to re-derive a recovering
/// saga's own outputs; the caller checks the re-derived blinded messages against the ones cdk
/// stored, so a wrong key can never credit anything.
pub fn home_seed(wallet_dir: &Path) -> Result<[u8; 64], String> {
    let root = wallet_dir
        .parent()
        .ok_or_else(|| format!("{} has no parent home", wallet_dir.display()))?;
    let secret = home::read_key_file_hex(&root.join("key")).map_err(|error| error.to_string())?;
    seed_from_secret_hex(&secret).map_err(|error| error.to_string())
}

// ---- Mint metadata outside the selection lock -------------------------------------------------
//
// cdk's `prepare_send` (online send kinds) calls `refresh_keysets`, which ALWAYS fetches mint info
// and keysets from the mint, and `prepare_melt` may fetch them on a cold cache. Both run while the
// selection lock is held. Every wallet this crate builds for a home gets a [`LockScopedConnector`]:
// outside any scope it is the plain connector; inside [`warm_metadata`] it records the info and
// keyset answers; inside [`local_only`] it replays them and refuses every other mint call, so the
// locked section is local database work only (milliseconds), whatever the mint's latency.

#[derive(Debug, Default)]
struct Recorded {
    info: Option<MintInfo>,
    keysets: Option<KeysetResponse>,
}

/// Mint info and keysets fetched by [`warm_metadata`], replayed by [`local_only`].
#[derive(Debug, Default)]
pub struct MetadataSnapshot {
    recorded: std::sync::Mutex<Recorded>,
}

#[derive(Debug)]
enum ScopeMode {
    Record,
    Replay,
}

#[derive(Debug)]
struct MetadataScope {
    mode: ScopeMode,
    snapshot: Arc<MetadataSnapshot>,
}

tokio::task_local! {
    static METADATA_SCOPE: Arc<MetadataScope>;
}

/// The refusal a mint call gets inside [`local_only`].
pub const LOCAL_ONLY_REFUSAL: &str =
    "mint request refused: the wallet selection lock is held (local-only section)";

fn scope() -> Option<Arc<MetadataScope>> {
    METADATA_SCOPE.try_with(Arc::clone).ok()
}

fn refused() -> CdkError {
    CdkError::Custom(LOCAL_ONLY_REFUSAL.to_owned())
}

/// Fetch (outside the selection lock) the mint info, keysets and any missing keyset keys that cdk's
/// selection will re-read, and return them for [`local_only`]. Never fails: a mint that does not
/// answer leaves the snapshot empty, and the locked section then runs on the stored keysets exactly
/// as cdk does when its own refresh fails. Callers bound it with their mint timeout.
pub async fn warm_metadata(wallet: &Wallet) -> Arc<MetadataSnapshot> {
    let snapshot = Arc::new(MetadataSnapshot::default());
    let scope = Arc::new(MetadataScope {
        mode: ScopeMode::Record,
        snapshot: snapshot.clone(),
    });
    let _ = METADATA_SCOPE.scope(scope, wallet.refresh_keysets()).await;
    snapshot
}

/// Run `future` (cdk selection + reservation, under the selection lock) with NO mint I/O: mint info
/// and keysets come from `snapshot`, every other mint call is refused with [`LOCAL_ONLY_REFUSAL`].
/// Only effective on a wallet whose connector is a [`LockScopedConnector`] (every home wallet).
pub async fn local_only<F: std::future::Future>(
    snapshot: Arc<MetadataSnapshot>,
    future: F,
) -> F::Output {
    let scope = Arc::new(MetadataScope {
        mode: ScopeMode::Replay,
        snapshot,
    });
    METADATA_SCOPE.scope(scope, future).await
}

/// The connector every home wallet uses (see [`warm_metadata`] / [`local_only`]).
#[derive(Debug, Clone)]
pub struct LockScopedConnector(pub Arc<dyn MintConnector + Send + Sync>);

/// Wrap `client` so [`local_only`] can keep mint I/O out of the selection lock.
pub fn lock_scoped(
    client: Arc<dyn MintConnector + Send + Sync>,
) -> Arc<dyn MintConnector + Send + Sync> {
    Arc::new(LockScopedConnector(client))
}

macro_rules! network_only {
    ($call:expr) => {{
        match scope() {
            Some(scope) if matches!(scope.mode, ScopeMode::Replay) => Err(refused()),
            _ => $call.await,
        }
    }};
}

#[async_trait]
impl MintConnector for LockScopedConnector {
    async fn fetch_lnurl_pay_request(&self, url: &str) -> Result<LnurlPayResponse, CdkError> {
        network_only!(self.0.fetch_lnurl_pay_request(url))
    }
    async fn fetch_lnurl_invoice(&self, url: &str) -> Result<LnurlPayInvoiceResponse, CdkError> {
        network_only!(self.0.fetch_lnurl_invoice(url))
    }
    async fn get_mint_keys(&self) -> Result<Vec<KeySet>, CdkError> {
        network_only!(self.0.get_mint_keys())
    }
    async fn get_mint_keyset(&self, keyset_id: Id) -> Result<KeySet, CdkError> {
        network_only!(self.0.get_mint_keyset(keyset_id))
    }
    async fn get_mint_keysets(&self) -> Result<KeysetResponse, CdkError> {
        match scope() {
            Some(scope) if matches!(scope.mode, ScopeMode::Replay) => scope
                .snapshot
                .recorded
                .lock()
                .map_err(|_| refused())?
                .keysets
                .clone()
                .ok_or_else(refused),
            Some(scope) => {
                let answer = self.0.get_mint_keysets().await;
                if let (Ok(keysets), Ok(mut recorded)) = (&answer, scope.snapshot.recorded.lock()) {
                    recorded.keysets = Some(keysets.clone());
                }
                answer
            }
            None => self.0.get_mint_keysets().await,
        }
    }
    async fn post_mint_quote(
        &self,
        request: MintQuoteRequest,
    ) -> Result<MintQuoteResponse<String>, CdkError> {
        network_only!(self.0.post_mint_quote(request))
    }
    async fn post_mint(
        &self,
        method: &PaymentMethod,
        request: MintRequest<String>,
    ) -> Result<MintResponse, CdkError> {
        network_only!(self.0.post_mint(method, request))
    }
    async fn post_batch_check_mint_quote_status(
        &self,
        method: &PaymentMethod,
        request: BatchCheckMintQuoteRequest<String>,
    ) -> Result<Vec<MintQuoteResponse<String>>, CdkError> {
        network_only!(self.0.post_batch_check_mint_quote_status(method, request))
    }
    async fn post_batch_mint(
        &self,
        method: &PaymentMethod,
        request: BatchMintRequest<String>,
    ) -> Result<MintResponse, CdkError> {
        network_only!(self.0.post_batch_mint(method, request))
    }
    async fn post_melt_quote(
        &self,
        request: MeltQuoteRequest,
    ) -> Result<MeltQuoteCreateResponse<String>, CdkError> {
        network_only!(self.0.post_melt_quote(request))
    }
    async fn get_mint_quote_status(
        &self,
        method: PaymentMethod,
        quote_id: &str,
    ) -> Result<MintQuoteResponse<String>, CdkError> {
        network_only!(self.0.get_mint_quote_status(method, quote_id))
    }
    async fn get_melt_quote_status(
        &self,
        method: PaymentMethod,
        quote_id: &str,
    ) -> Result<MeltQuoteResponse<String>, CdkError> {
        network_only!(self.0.get_melt_quote_status(method, quote_id))
    }
    async fn post_melt(
        &self,
        method: &PaymentMethod,
        request: MeltRequest<String>,
    ) -> Result<MeltQuoteResponse<String>, CdkError> {
        network_only!(self.0.post_melt(method, request))
    }
    async fn post_swap(&self, request: SwapRequest) -> Result<SwapResponse, CdkError> {
        network_only!(self.0.post_swap(request))
    }
    async fn get_mint_info(&self) -> Result<MintInfo, CdkError> {
        match scope() {
            Some(scope) if matches!(scope.mode, ScopeMode::Replay) => scope
                .snapshot
                .recorded
                .lock()
                .map_err(|_| refused())?
                .info
                .clone()
                .ok_or_else(refused),
            Some(scope) => {
                let answer = self.0.get_mint_info().await;
                if let (Ok(info), Ok(mut recorded)) = (&answer, scope.snapshot.recorded.lock()) {
                    recorded.info = Some(info.clone());
                }
                answer
            }
            None => self.0.get_mint_info().await,
        }
    }
    async fn post_check_state(
        &self,
        request: CheckStateRequest,
    ) -> Result<CheckStateResponse, CdkError> {
        network_only!(self.0.post_check_state(request))
    }
    async fn post_restore(&self, request: RestoreRequest) -> Result<RestoreResponse, CdkError> {
        network_only!(self.0.post_restore(request))
    }
    async fn get_auth_wallet(&self) -> Option<AuthWallet> {
        self.0.get_auth_wallet().await
    }
    async fn set_auth_wallet(&self, wallet: Option<AuthWallet>) {
        self.0.set_auth_wallet(wallet).await
    }
}

/// `wallet` with its connector wrapped in a [`LockScopedConnector`]. Every wallet opened on a home's
/// shared database goes through this (`open_shared_wallet`, `buyer_fund::open_wallet_at_mint_async`,
/// `wallet_ops::open_wallet_async`), so [`local_only`] can keep its selection free of mint I/O.
pub fn scoped(mut wallet: Wallet) -> Wallet {
    let client = wallet.mint_connector();
    wallet.set_client(lock_scoped(client));
    wallet
}

/// Why [`prepare_send_locked`] / [`prepare_melt_locked`] did not prepare.
#[derive(Debug)]
pub enum LockedSelectError {
    /// The selection lock was not obtained; nothing was selected or reserved (retryable).
    Lock(SelectLockError),
    /// cdk refused the selection or reservation.
    Wallet(cdk::Error),
}

impl std::fmt::Display for LockedSelectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock(error) => error.fmt(formatter),
            Self::Wallet(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LockedSelectError {}

/// cdk `prepare_send` (select UNSPENT proofs, reserve them under a new saga) under the selection
/// lock of `wallet_dir`. Every buyer-side send that selects proofs goes through this (or takes
/// [`select_guard`] itself), so it can never interleave with `maxplayer-trade`'s reservation. The
/// mint metadata is fetched first, outside the lock; inside it the prepare is [`local_only`].
pub async fn prepare_send_locked<'a>(
    wallet: &'a Wallet,
    wallet_dir: Option<&Path>,
    amount: cdk::Amount,
    options: cdk::wallet::SendOptions,
) -> Result<cdk::wallet::PreparedSend<'a>, LockedSelectError> {
    let snapshot = warm_metadata(wallet).await;
    let _guard = select_guard(wallet_dir)
        .await
        .map_err(LockedSelectError::Lock)?;
    local_only(snapshot, wallet.prepare_send(amount, options))
        .await
        .map_err(LockedSelectError::Wallet)
}

/// cdk `prepare_melt` (select and reserve the melt inputs) under the selection lock, local-only
/// like [`prepare_send_locked`].
pub async fn prepare_melt_locked<'a>(
    wallet: &'a Wallet,
    wallet_dir: Option<&Path>,
    quote_id: &str,
) -> Result<cdk::wallet::PreparedMelt<'a>, LockedSelectError> {
    let snapshot = warm_metadata(wallet).await;
    let _guard = select_guard(wallet_dir)
        .await
        .map_err(LockedSelectError::Lock)?;
    local_only(
        snapshot,
        wallet.prepare_melt(quote_id, std::collections::HashMap::new()),
    )
    .await
    .map_err(LockedSelectError::Wallet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "maxplayer-select-lock-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn a_second_holder_waits_then_refuses_and_proceeds_after_release() {
        let wallet = dir("excl");
        let first = acquire(&wallet, SELECT_LOCK_TIMEOUT).await.unwrap();
        let busy = acquire(&wallet, Duration::from_millis(150)).await;
        assert!(
            matches!(busy, Err(SelectLockError::Busy { .. })),
            "the lock must exclude a second holder in the same process: {busy:?}"
        );
        assert!(try_acquire(&wallet).unwrap().is_none());
        drop(first);
        let again = acquire(&wallet, Duration::from_millis(150)).await;
        assert!(again.is_ok(), "released on drop: {again:?}");
    }

    #[tokio::test]
    async fn select_guard_waits_a_bounded_time_then_refuses() {
        let wallet = dir("refuse");
        let _held = acquire(&wallet, SELECT_LOCK_TIMEOUT).await.unwrap();
        let started = std::time::Instant::now();
        let outcome = select_guard(Some(&wallet)).await;
        assert!(
            matches!(outcome, Err(SelectLockError::Busy { .. })),
            "{outcome:?}"
        );
        let waited = started.elapsed();
        assert!(
            waited >= SELECT_LOCK_TIMEOUT && waited < SELECT_LOCK_TIMEOUT * 3,
            "bounded wait, never forever: {waited:?}"
        );
        assert!(select_guard(None).await.unwrap().is_none());
    }

    /// M2: the selection lock never spans a mint request. Every production `select_guard(` call
    /// fetches the mint metadata first (`warm_metadata(` within the 15 lines before it) and runs
    /// cdk's selection `local_only(` within the 15 lines after it, or is a marked local apply.
    #[test]
    fn every_select_guard_warms_metadata_first_and_selects_local_only() {
        let call = format!("{}(", "select_guard");
        let mut seen = 0;
        for (name, source) in [
            ("shared_wallet.rs", include_str!("shared_wallet.rs")),
            ("payment_wallet.rs", include_str!("payment_wallet.rs")),
            ("crossmint_hop.rs", include_str!("crossmint_hop.rs")),
            ("wallet_ops.rs", include_str!("wallet_ops.rs")),
            ("buyer_fund.rs", include_str!("buyer_fund.rs")),
            ("authorize_pay.rs", include_str!("authorize_pay.rs")),
        ] {
            let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
            let lines: Vec<&str> = production.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let code = line.trim_start();
                if !code.contains(&call) || code.starts_with("//") || code.contains("fn ") {
                    continue;
                }
                seen += 1;
                let before = lines[i.saturating_sub(15)..i].join("\n");
                let after = lines[i..(i + 15).min(lines.len())].join("\n");
                if before.contains(&format!("LOCAL {} ONLY", "APPLY")) {
                    continue;
                }
                assert!(
                    before.contains(&format!("{}(", "warm_metadata")),
                    "{name}:{}: select_guard without warm_metadata before it",
                    i + 1
                );
                assert!(
                    after.contains(&format!("{}(", "local_only")),
                    "{name}:{}: select_guard without local_only selection after it",
                    i + 1
                );
            }
        }
        assert_eq!(
            seen, 5,
            "select_guard call sites (prepare_send/melt_locked, pay, hop, swap apply)"
        );
    }

    /// Review r3 H1: every production cdk send/melt (`prepare_send`, `prepare_melt`, their locked
    /// wrappers, `revoke_send`) takes the operation lock (`operation_guard(`) first, so recovery in
    /// any process can tell a running send from a crashed one. `shared_wallet.rs` defines the
    /// wrappers; their callers are what is checked.
    #[test]
    fn every_cdk_send_and_melt_holds_the_operation_lock() {
        let mut seen = 0;
        for (name, source) in [
            ("payment_wallet.rs", include_str!("payment_wallet.rs")),
            ("crossmint_hop.rs", include_str!("crossmint_hop.rs")),
            ("wallet_ops.rs", include_str!("wallet_ops.rs")),
            ("buyer_fund.rs", include_str!("buyer_fund.rs")),
            // fee_remit.rs only calls its `RemitEffects` trait; the live melt is
            // `wallet_ops::prepare_melt_payment_blocking`'s thread, checked in wallet_ops.rs.
            ("authorize_pay.rs", include_str!("authorize_pay.rs")),
        ] {
            let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
            let lines: Vec<&str> = production.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.contains("fn ") {
                    continue;
                }
                let calls = [
                    "prepare_send_locked(",
                    "prepare_melt_locked(",
                    ".prepare_send(",
                    ".prepare_melt(",
                    ".revoke_send(",
                ];
                if !calls.iter().any(|call| code.contains(call)) {
                    continue;
                }
                seen += 1;
                let before = lines[i.saturating_sub(60)..i].join("\n");
                assert!(
                    before.contains(&format!("{}(", "operation_guard")),
                    "{name}:{}: a cdk send/melt without the operation lock taken before it",
                    i + 1
                );
            }
        }
        assert_eq!(
            seen, 6,
            "pay send, revoke, wallet send, two melt paths, hop melt"
        );
    }

    /// Review r6 H1: every core spend path runs the one mint payment gate
    /// (`payment_wallet::payment_gate`) and takes its operation lock only AFTER it (the caller's own
    /// lock would hide a dead parent from the gate).
    #[test]
    fn every_core_spend_runs_the_payment_gate_before_its_operation_lock() {
        for (name, source, expected) in [
            ("payment_wallet.rs", include_str!("payment_wallet.rs"), 1),
            ("wallet_ops.rs", include_str!("wallet_ops.rs"), 3),
            ("crossmint_hop.rs", include_str!("crossmint_hop.rs"), 1),
        ] {
            let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
            let lines: Vec<&str> = production.lines().collect();
            let mut seen = 0;
            for (i, line) in lines.iter().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.contains("fn ") || !code.contains("payment_gate(")
                {
                    continue;
                }
                seen += 1;
                let before = lines[i.saturating_sub(40)..i].join("\n");
                let after = lines[i..(i + 40).min(lines.len())].join("\n");
                assert!(
                    !before.contains("operation_guard("),
                    "{name}:{}: the operation lock is taken before the payment gate",
                    i + 1
                );
                assert!(
                    after.contains("operation_guard("),
                    "{name}:{}: payment gate not followed by the operation lock",
                    i + 1
                );
            }
            assert_eq!(seen, expected, "{name}: payment_gate call sites");
        }
    }

    #[test]
    fn operation_lock_is_shared_between_operations_and_seen_by_recovery() {
        let wallet = dir("operation");
        assert!(!operation_in_flight(Some(&wallet)).unwrap());
        assert!(!operation_in_flight(None).unwrap());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let first = runtime.block_on(operation_guard(Some(&wallet))).unwrap();
        let second = runtime.block_on(operation_guard(Some(&wallet))).unwrap();
        assert!(first.is_some() && second.is_some(), "operations share it");
        assert!(operation_in_flight(Some(&wallet)).unwrap());
        drop(first);
        assert!(
            operation_in_flight(Some(&wallet)).unwrap(),
            "one still runs"
        );
        drop(second);
        assert!(
            !operation_in_flight(Some(&wallet)).unwrap(),
            "released on drop"
        );
        // A killed holder (another process) never looks alive: the kernel releases the flock. The
        // holder is `flock` plus the `sleep` it forks (which inherits the descriptor), in their own
        // process group, killed together.
        use std::os::unix::process::CommandExt;
        let mut holder = std::process::Command::new("flock")
            .arg("-s")
            .arg(wallet.join(OPERATION_LOCK_FILE))
            .args(["sleep", "600"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = holder.id() as libc::pid_t;
        let started = std::time::Instant::now();
        while !operation_in_flight(Some(&wallet)).unwrap() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(20));
        }
        // Safety: signals only the process group this test created.
        unsafe { libc::kill(-group, libc::SIGKILL) };
        holder.wait().unwrap();
        let started = std::time::Instant::now();
        while operation_in_flight(Some(&wallet)).unwrap() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "a killed holder must release the operation lock"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Rust sources under `dir`, recursively (relative path, contents).
    fn rust_sources(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_sources(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let name = path.strip_prefix(root).unwrap().display().to_string();
                out.push((name, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }

    /// No full-wallet NUT-13 restore anywhere in `maxplayer-core` or `maxplayer` (src, tests,
    /// examples, benches): cdk 0.17.2's `Wallet::restore` / `restore_with_opts` upserts every
    /// re-derived seed proof with NO owner, one committed batch at a time, so on the shared home
    /// wallet it would publish trade's (and in-flight cdk sagas') reservations as spendable, and a
    /// failure, dropped future or kill between batches leaves them that way. A spent swap saga
    /// recovers only its own outputs (`payment_wallet::complete_spent_swap_saga`).
    ///
    /// Also walks `maxplayer-trade` (src and tests; review r2 L4): trade opens this same wallet.
    ///
    /// `ALLOWED` is the explicit allowlist (`(path, reason)`), exact paths only. The patterns are
    /// built at run time so this test's own source never matches them.
    #[test]
    fn no_full_wallet_restore_anywhere_in_core_or_the_cli() {
        const ALLOWED: &[(&str, &str)] = &[(
            "maxplayer-trade/tests/shared_home.rs",
            "buyer_restore_and_saga_retire_leave_trade_reservations_alone calls cdk's restore on a \
             throwaway home to show the hazard this test forbids",
        )];
        let calls = [
            format!(".{}()", "restore"),
            format!(".{}(", "restore_with_opts"),
            format!("Wallet::{}(", "restore"),
            format!("::{}(", "restore_with_opts"),
        ];
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0;
        let mut allowed_seen = 0;
        for krate in [
            manifest.to_path_buf(),
            manifest.join("../maxplayer"),
            manifest.join("../maxplayer-trade"),
        ] {
            let mut sources = Vec::new();
            for sub in ["src", "tests", "examples", "benches"] {
                if krate.join(sub).is_dir() {
                    rust_sources(&krate.join(sub), &krate, &mut sources);
                }
            }
            assert!(!sources.is_empty(), "no sources under {}", krate.display());
            for (name, source) in sources {
                checked += 1;
                let shown = format!("{}/{name}", krate.file_name().unwrap().to_string_lossy());
                if ALLOWED.iter().any(|(path, _)| *path == shown) {
                    allowed_seen += 1;
                    continue;
                }
                for call in &calls {
                    assert!(
                        !source.contains(call.as_str()),
                        "{shown} calls {call}: a full-wallet NUT-13 restore clears other owners' \
                         reservations on the shared wallet; recover one saga's outputs instead"
                    );
                }
            }
        }
        assert!(checked > 100, "walked only {checked} files");
        assert_eq!(
            allowed_seen,
            ALLOWED.len(),
            "an allowlisted path no longer exists"
        );
    }

    /// Review r2 H1: the only NUT-09 restore this workspace builds is a spent swap saga's, and it
    /// names only that saga's STORED messages' keyset. Production `post_restore(` / `restore_batch(`
    /// in `maxplayer-core`, `maxplayer` and `maxplayer-trade` occur only in the connector
    /// pass-throughs and in `payment_wallet::{recover_swap_saga_outputs, seed_derived_outputs}`;
    /// that pair never reads the mint's keyset list, derives on `stored[0].keyset_id` only, and
    /// refuses a request naming any other keyset before it is sent. cdk's own restores are its
    /// full-wallet `restore` (forbidden above) and saga recovery, which restores
    /// `blinded_messages[0].keyset_id` only and is reached only through
    /// `recover_incomplete_sagas_guarded`. Trade's own `/restore` RPCs send its journaled outputs
    /// (random secrets), never a seed derivation.
    #[test]
    fn the_only_restore_names_the_stored_messages_keyset() {
        let post = format!(".{}(", "post_restore");
        let batch = format!("{}(", "restore_batch");
        let cdk_recovery = format!(".{}()", "recover_incomplete_sagas");
        let all = format!("{}::All", "KeysetFilter");
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (mut posts, mut batches, mut recoveries) = (Vec::new(), Vec::new(), Vec::new());
        for krate in [
            manifest.to_path_buf(),
            manifest.join("../maxplayer"),
            manifest.join("../maxplayer-trade"),
        ] {
            let mut sources = Vec::new();
            for sub in ["src", "tests", "examples", "benches"] {
                if krate.join(sub).is_dir() {
                    rust_sources(&krate.join(sub), &krate, &mut sources);
                }
            }
            for (name, source) in sources {
                // `#[cfg(test)] mod tests` blocks are scanned too, except for derivations (below).
                let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
                for (i, line) in source.lines().enumerate() {
                    let code = line.trim_start();
                    if code.starts_with("//") {
                        continue;
                    }
                    let site = format!("{name}:{}: {code}", i + 1);
                    if code.contains(&post) {
                        posts.push(site.clone());
                    }
                    if code.contains(&cdk_recovery) {
                        recoveries.push(site.clone());
                    }
                    // A derivation sends nothing; test fixtures may derive their expected outputs.
                    if code.contains(&batch) && production.contains(line) {
                        batches.push(site);
                    }
                }
            }
        }
        // Connector pass-throughs, and the transport test's empty request (names no keyset).
        let passthrough = |site: &&String| {
            site.contains("self.0.post_restore(request)")
                || site.contains("post_restore(RestoreRequest { outputs: vec![] })")
        };
        let requests: Vec<_> = posts.iter().filter(|s| !passthrough(s)).collect();
        assert_eq!(requests.len(), 1, "exactly one restore request: {posts:#?}");
        assert!(requests[0].contains("payment_wallet.rs"), "{requests:?}");
        assert_eq!(
            batches.len(),
            1,
            "exactly one restore derivation: {batches:#?}"
        );
        assert!(batches[0].contains("payment_wallet.rs"), "{batches:?}");
        // cdk-wallet path: cdk's saga recovery restores `blinded_messages[0].keyset_id` only, and
        // is reached only through the guarded wrapper.
        assert_eq!(recoveries.len(), 1, "{recoveries:#?}");
        assert!(
            recoveries[0].contains("payment_wallet.rs"),
            "{recoveries:?}"
        );
        let pw = include_str!("payment_wallet.rs");
        let guarded = &pw[pw
            .find("async fn recover_incomplete_sagas_guarded(")
            .unwrap()..];
        assert!(
            guarded[..guarded.find("\n}\n").unwrap()].contains(&cdk_recovery),
            "the one cdk recovery call is inside recover_incomplete_sagas_guarded"
        );
        let body = |name: &str| {
            let start = pw.find(&format!("fn {name}(")).unwrap();
            let rest = &pw[start..];
            &rest[..rest.find("\n}\n").unwrap()]
        };
        let recover = body("recover_swap_saga_outputs");
        let derive = body("seed_derived_outputs");
        assert_eq!(
            recover.matches(&post).count(),
            1,
            "the restore request is here"
        );
        assert_eq!(derive.matches(&batch).count(), 1, "the derivation is here");
        assert!(
            !recover.contains(&batch),
            "derive only through seed_derived_outputs"
        );
        for (fun, text) in [("recover", recover), ("derive", derive)] {
            assert!(
                !text.contains(&all) && !text.contains("get_mint_keysets"),
                "{fun}: never restores over the mint's keyset list"
            );
        }
        let guard = recover
            .find("refusing a restore outside the stored messages' keyset")
            .expect("keyset guard");
        assert!(
            guard < recover.find(&post).unwrap(),
            "guard before the request"
        );
        assert!(recover.contains("let keyset = stored[0].keyset_id;"));
        assert!(recover.contains("seed_derived_outputs(seed, keyset,"));
        assert!(
            !recover.contains("seed_derived_outputs(seed, keyset_")
                && !recover.contains("for keyset in"),
            "one keyset only"
        );
    }

    /// A seller is paid into this same wallet and `maxplayer-trade` reserves in it, so every seller
    /// path that selects, reserves, releases or restores proofs must go through this module's locked
    /// primitives. The only seller-side selection is the platform-fee remittance melt:
    /// `fee_remit::LiveEffects::prepare_melt` -> `wallet_ops::prepare_melt_payment_blocking` ->
    /// [`prepare_melt_locked`]. The seller node itself only receives (cdk `receive` adds new proofs
    /// and selects none) and reads balances.
    #[test]
    fn seller_paths_select_proofs_only_under_the_select_lock() {
        let seller = [
            ("seller.rs", include_str!("seller.rs")),
            ("fee_remit.rs", include_str!("fee_remit.rs")),
            ("seller_node/mod.rs", include_str!("seller_node/mod.rs")),
            ("seller_node/run.rs", include_str!("seller_node/run.rs")),
            (
                "seller_node/session_run.rs",
                include_str!("seller_node/session_run.rs"),
            ),
            (
                "seller_node/session_gc.rs",
                include_str!("seller_node/session_gc.rs"),
            ),
            (
                "seller_node/wallet_actor.rs",
                include_str!("seller_node/wallet_actor.rs"),
            ),
            ("seller_node/store.rs", include_str!("seller_node/store.rs")),
        ];
        for (name, source) in seller {
            for call in [
                "wallet.prepare_melt(",
                ".prepare_send(",
                ".reserve_proofs(",
                ".release_proofs(",
                ".recover_incomplete_sagas(",
                ".check_all_pending_proofs(",
                "get_unspent_proofs(",
            ] {
                assert!(
                    !source.contains(call),
                    "{name} calls {call} outside the shared-wallet lock primitives"
                );
            }
        }
        let fee_remit = include_str!("fee_remit.rs");
        assert!(
            fee_remit.contains("wallet_ops::prepare_melt_payment_blocking(&self.home, quote_id"),
            "the live remittance melt must go through wallet_ops' locked prepare"
        );
        let wallet_ops = include_str!("wallet_ops.rs");
        assert!(
            wallet_ops.contains(
                "crate::shared_wallet::prepare_melt_locked(&wallet, Some(&home.wallet_dir), &quote.id)"
            ),
            "prepare_melt_payment_blocking must select under the home's select lock"
        );
        for call in ["wallet.prepare_melt(", ".prepare_send("] {
            assert!(
                !wallet_ops.contains(call),
                "wallet_ops.rs calls {call} without the select lock"
            );
        }
    }

    /// Child half of [`a_holder_killed_with_sigkill_releases_the_lock`]: run only when re-executed
    /// with the env var set; holds the lock, signals readiness, then sleeps until killed.
    #[test]
    fn select_lock_kill_child() {
        let Some(wallet) = std::env::var_os("MAXPLAYER_TEST_SELECT_LOCK_CHILD_DIR") else {
            return;
        };
        let wallet = PathBuf::from(wallet);
        let (_, file) = open_lock_file(&wallet).unwrap();
        assert!(try_flock(&file).unwrap(), "child must get the lock first");
        std::fs::write(wallet.join("child.ready"), b"1").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    /// "The lock must be released after a crash": a process holding the lock is killed with
    /// SIGKILL (no destructor, no cleanup); the kernel closes its descriptor and the other side
    /// proceeds at once. No stale lock file needs reaping.
    #[tokio::test]
    async fn a_holder_killed_with_sigkill_releases_the_lock() {
        let wallet = dir("kill9");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shared_wallet::tests::select_lock_kill_child",
                "--nocapture",
            ])
            .env("MAXPLAYER_TEST_SELECT_LOCK_CHILD_DIR", &wallet)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ready = wallet.join("child.ready");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "child never took the lock"
            );
            assert!(child.try_wait().unwrap().is_none(), "child exited early");
            std::thread::sleep(Duration::from_millis(20));
        }
        let busy = acquire(&wallet, Duration::from_millis(200)).await;
        assert!(
            matches!(busy, Err(SelectLockError::Busy { .. })),
            "{busy:?}"
        );
        // Safety: plain kill(2) on our own child's pid.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
        let status = child.wait().unwrap();
        assert!(!status.success(), "killed, not exited: {status:?}");
        let after = acquire(&wallet, Duration::from_secs(2)).await;
        assert!(
            after.is_ok(),
            "the kernel must release a killed holder's flock: {after:?}"
        );
    }
}
