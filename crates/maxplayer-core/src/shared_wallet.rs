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
//! `<home>/wallet/select.lock`, held ONLY across "pick unspent proofs + reserve them" (cdk's
//! `prepare_send` / `prepare_melt` via [`select_guard`], trade's verify-and-reserve) and across a NUT-13 restore, which
//! rewrites proof rows wholesale. Acquisition is bounded ([`SELECT_LOCK_TIMEOUT`]); a timeout is a
//! retryable refusal that has reserved nothing. The kernel drops a `flock` when its descriptor
//! closes, including when the holder is killed (`kill -9`), so a crash never leaves the lock held.
//!
//! Ownership: every reservation names its owner in `used_by_operation` (a cdk saga id for the buyer,
//! a journal operation id for trade). Recovery only ever releases rows it owns; see
//! `payment_wallet::restore_keeping_other_reservations` for the one cdk call (`Wallet::restore`)
//! that would otherwise clear rows it does not own.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cdk::nuts::CurrencyUnit;
use cdk::wallet::Wallet;
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
    let path = wallet_dir.join(SELECT_LOCK_FILE);
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
    // Safety: plain libc call on a valid descriptor owned by `file`, which outlives the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
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
/// guard across cdk's `prepare_send` / `prepare_melt` (selection + reservation) and drop it as soon
/// as they return: the chosen proofs are then RESERVED under their operation and no other selector
/// can take them. Take it OUTSIDE any mint timeout, so a busy wallet reads as busy, not as a dead
/// mint.
pub async fn select_guard(wallet_dir: Option<&Path>) -> Result<Option<SelectLock>, SelectLockError> {
    match wallet_dir {
        Some(dir) => acquire(dir, SELECT_LOCK_TIMEOUT).await.map(Some),
        None => Ok(None),
    }
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
        .map_err(|error| FundError::Wallet(error.to_string()));
    }
    Wallet::new(mint_url, CurrencyUnit::Sat, Arc::new(store), seed, None)
        .map_err(|error| FundError::Wallet(error.to_string()))
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
/// [`select_guard`] itself), so it can never interleave with `maxplayer-trade`'s reservation.
pub async fn prepare_send_locked<'a>(
    wallet: &'a Wallet,
    wallet_dir: Option<&Path>,
    amount: cdk::Amount,
    options: cdk::wallet::SendOptions,
) -> Result<cdk::wallet::PreparedSend<'a>, LockedSelectError> {
    let _guard = select_guard(wallet_dir)
        .await
        .map_err(LockedSelectError::Lock)?;
    wallet
        .prepare_send(amount, options)
        .await
        .map_err(LockedSelectError::Wallet)
}

/// cdk `prepare_melt` (select and reserve the melt inputs) under the selection lock.
pub async fn prepare_melt_locked<'a>(
    wallet: &'a Wallet,
    wallet_dir: Option<&Path>,
    quote_id: &str,
) -> Result<cdk::wallet::PreparedMelt<'a>, LockedSelectError> {
    let _guard = select_guard(wallet_dir)
        .await
        .map_err(LockedSelectError::Lock)?;
    wallet
        .prepare_melt(quote_id, std::collections::HashMap::new())
        .await
        .map_err(LockedSelectError::Wallet)
}

/// cdk `Wallet::restore` (NUT-13: re-derive every seed proof and upsert each one the mint still
/// reports UNSPENT/PENDING) without releasing reservations it does not own.
///
/// cdk 0.17.2 writes each restored proof with `ProofInfo::new` (state from the mint, NO owner) and
/// its upsert overwrites `state` and `used_by_operation` (cdk-sql-common `wallet/mod.rs`, `INSERT …
/// ON CONFLICT(y) DO UPDATE`). A seed proof `maxplayer-trade` (or an in-flight buyer saga) has
/// RESERVED is still UNSPENT at the mint, so a plain restore silently hands it back to the
/// spendable balance while its owner still counts on it. This snapshots every in-flight row of the
/// wallet's mint under the selection lock, restores, and writes back any snapshot row the restore
/// changed. Use it for every restore; never call `Wallet::restore` directly.
pub async fn restore_keeping_other_reservations(
    wallet: &Wallet,
    wallet_dir: Option<&Path>,
) -> Result<cdk::wallet::types::Restored, cdk::Error> {
    use cashu::State;
    let _guard = select_guard(wallet_dir)
        .await
        .map_err(|error| cdk::Error::Custom(error.to_string()))?;
    let in_flight = wallet
        .localstore
        .get_proofs(
            Some(wallet.mint_url.clone()),
            Some(wallet.unit.clone()),
            Some(vec![State::Reserved, State::PendingSpent, State::Pending]),
            None,
        )
        .await?;
    // Classified BEFORE the restore: a row owned by an operation that is not a cdk saga of this
    // wallet (`maxplayer-trade`'s journal operation id) is restored to exactly the state and owner it
    // had. Releasing it is its owner's decision, never a restore's; that owner reserves and releases
    // under this same lock, so the row cannot legitimately change while we hold it. Rows owned by a
    // cdk saga are left to cdk as before, and rows the restore deleted (the mint reports them SPENT)
    // stay deleted: that is true, and their owner reconciles it.
    let mut foreign = Vec::new();
    for row in in_flight {
        if let Some(op) = row.used_by_operation {
            if wallet.localstore.get_saga(&op).await?.is_none() {
                foreign.push(row);
            }
        }
    }
    let restored = wallet.restore().await?;
    if foreign.is_empty() {
        return Ok(restored);
    }
    let now = wallet
        .localstore
        .get_proofs_by_ys(foreign.iter().map(|row| row.y).collect())
        .await?;
    let changed = foreign
        .into_iter()
        .filter(|before| {
            now.iter().any(|after| {
                after.y == before.y
                    && (after.state != before.state
                        || after.used_by_operation != before.used_by_operation)
            })
        })
        .collect::<Vec<_>>();
    if !changed.is_empty() {
        wallet.localstore.update_proofs(changed, vec![]).await?;
    }
    Ok(restored)
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
        assert!(matches!(outcome, Err(SelectLockError::Busy { .. })), "{outcome:?}");
        let waited = started.elapsed();
        assert!(
            waited >= SELECT_LOCK_TIMEOUT && waited < SELECT_LOCK_TIMEOUT * 3,
            "bounded wait, never forever: {waited:?}"
        );
        assert!(select_guard(None).await.unwrap().is_none());
    }

    /// Every NUT-13 restore in core goes through [`restore_keeping_other_reservations`]: a direct
    /// `Wallet::restore` would hand trade-reserved seed proofs back to the spendable balance.
    #[test]
    fn no_direct_wallet_restore_outside_this_module() {
        for (name, source) in [
            ("payment_wallet.rs", include_str!("payment_wallet.rs")),
            ("wallet_ops.rs", include_str!("wallet_ops.rs")),
            ("crossmint_hop.rs", include_str!("crossmint_hop.rs")),
            ("buyer_fund.rs", include_str!("buyer_fund.rs")),
        ] {
            assert!(
                !source.contains(".restore().await"),
                "{name} calls Wallet::restore directly; use restore_keeping_other_reservations"
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
            .args(["--exact", "shared_wallet::tests::select_lock_kill_child", "--nocapture"])
            .env("MAXPLAYER_TEST_SELECT_LOCK_CHILD_DIR", &wallet)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ready = wallet.join("child.ready");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !ready.exists() {
            assert!(std::time::Instant::now() < deadline, "child never took the lock");
            assert!(child.try_wait().unwrap().is_none(), "child exited early");
            std::thread::sleep(Duration::from_millis(20));
        }
        let busy = acquire(&wallet, Duration::from_millis(200)).await;
        assert!(matches!(busy, Err(SelectLockError::Busy { .. })), "{busy:?}");
        // Safety: plain kill(2) on our own child's pid.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
        let status = child.wait().unwrap();
        assert!(!status.success(), "killed, not exited: {status:?}");
        let after = acquire(&wallet, Duration::from_secs(2)).await;
        assert!(after.is_ok(), "the kernel must release a killed holder's flock: {after:?}");
    }
}
