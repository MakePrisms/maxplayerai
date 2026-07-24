//! Exclusive per-home process lock for the seller node.
//!
//! The node holds an OS advisory lock (`flock(LOCK_EX | LOCK_NB)`) on
//! `$MOBEE_HOME/seller.lock` for its whole lifetime. This is the money-safety keystone: it
//! guarantees a single owner of the receiving wallet, the seller identity, and the lifecycle DB
//! per home. A second node on the same home fails closed here — before it ever opens the wallet —
//! so two processes can never sign or redeem proofs concurrently.
//!
//! `flock` locks the open file description, so a second independent `open()` + `flock(LOCK_NB)` on
//! the same path is refused even from within the same process. The lock releases automatically when
//! the held descriptor closes (process exit or `Drop`), including on crash — no stale lock to reap.
//!
//! This mirrors the buyer's home lock; the two are kept separate on purpose — a shared node core is
//! deferred until both consumers are proven (issue #131).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// A held exclusive lock. Keep it alive for as long as the node owns the home; dropping it (or
/// exiting) releases the OS lock.
#[derive(Debug)]
pub struct HomeLock {
    path: PathBuf,
    // Held for the RAII lifetime: closing the descriptor releases the flock.
    _file: File,
}

/// Failure to acquire the home lock.
#[derive(Debug)]
pub enum LockError {
    /// The lock file could not be opened/created.
    Open(String),
    /// Another live node already holds the lock for this home (fail closed).
    Held { path: PathBuf },
    /// The lock syscall itself failed for an unexpected reason.
    Flock(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(message) => write!(formatter, "seller lock open error: {message}"),
            Self::Held { path } => write!(
                formatter,
                "another mobee seller already owns this home (lock held: {}); refusing to start a second owner",
                path.display()
            ),
            Self::Flock(message) => write!(formatter, "seller lock flock error: {message}"),
        }
    }
}

impl std::error::Error for LockError {}

impl HomeLock {
    /// Take the exclusive lock at `path`, or fail closed if it is already held.
    ///
    /// Never blocks: uses the non-blocking `LOCK_NB` variant so a busy home is reported immediately
    /// as [`LockError::Held`] rather than hanging.
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, LockError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| LockError::Open(error.to_string()))?;

        // Safety: `flock` is a plain libc call on a valid, owned descriptor. The descriptor
        // outlives the call (held by `file`).
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(code) if code == libc::EWOULDBLOCK => Err(LockError::Held { path }),
                _ => Err(LockError::Flock(error.to_string())),
            };
        }

        Ok(Self { path, _file: file })
    }

    /// The lock file path (diagnostic).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_lock(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-lock-{label}-{}-{id}.lock",
            std::process::id()
        ))
    }

    #[test]
    fn second_acquire_fails_closed_while_first_is_held() {
        let path = temp_lock("exclusive");
        let _ = std::fs::remove_file(&path);

        let first = HomeLock::acquire(&path).expect("first acquire");
        let second = HomeLock::acquire(&path);
        assert!(
            matches!(second, Err(LockError::Held { .. })),
            "second acquire must fail closed while first is held, got {second:?}"
        );

        // Releasing the first lets a fresh acquire succeed — the lock is not stale.
        drop(first);
        let third = HomeLock::acquire(&path).expect("acquire after release");
        drop(third);
        let _ = std::fs::remove_file(&path);
    }
}
