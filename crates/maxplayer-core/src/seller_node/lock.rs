//! Exclusive per-home process lock for the seller node.
//!
//! The node holds an OS advisory lock (`flock(LOCK_EX | LOCK_NB)`) on
//! `$MAXPLAYER_HOME/seller.lock` for its whole lifetime. This is the money-safety keystone: it
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
                "another maxplayer seller already owns this home (lock held: {}); refusing to start a second owner",
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

        // #1299: record the holder's pid so `maxplayer seller drain` can find this node. Written only
        // once the lock is ours (never before the flock, which would clobber a live holder's pid),
        // truncating first so a shorter pid leaves no stale tail. Best effort: a failed write costs
        // the drain command its target (it then reports "no pid recorded"), never the node its home.
        {
            use std::io::Write;
            let _ = file.set_len(0).and_then(|()| {
                (&file).write_all(format!("{}\n", std::process::id()).as_bytes())
            });
        }

        Ok(Self { path, _file: file })
    }

    /// The lock file path (diagnostic).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// #1299: who holds a seller home lock, as seen by another process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockHolder {
    /// No lock file, or nothing holds it: no seller node is running on this home.
    Free,
    /// A process holds it and recorded this pid.
    Pid(u32),
    /// Held, but no pid recorded: a seller from before #1299 (which does not handle SIGUSR1 either),
    /// or one in the instant between taking the lock and writing its pid.
    Unrecorded,
}

/// #1299: probe a seller home lock without taking it for longer than the probe.
///
/// Opens the file read-only (never creates it) and tries a NON-blocking shared lock: success means
/// nobody holds it (released again at once); `EWOULDBLOCK` means a node holds it, and the file's
/// content is the pid that node recorded. The brief shared lock can make a node starting in that
/// same instant fail its own acquire, which it reports as an ordinary "already owned" refusal.
pub fn holder(path: impl AsRef<Path>) -> Result<LockHolder, LockError> {
    use std::io::Read;
    let mut file = match OpenOptions::new().read(true).open(path.as_ref()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(LockHolder::Free),
        Err(error) => return Err(LockError::Open(error.to_string())),
    };
    // Safety: plain libc call on a valid descriptor owned by `file` for the whole call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(LockHolder::Free); // our shared lock is released when `file` drops
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
        return Err(LockError::Flock(error.to_string()));
    }
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|error| LockError::Open(error.to_string()))?;
    Ok(match content.trim().parse::<u32>() {
        Ok(pid) if pid > 0 => LockHolder::Pid(pid),
        _ => LockHolder::Unrecorded,
    })
}

/// #1299: why a drain request was not sent.
#[derive(Debug)]
pub enum DrainRefusal {
    /// Nothing holds the lock: no seller is running on this home.
    NotRunning,
    /// Held, but no pid recorded.
    Unrecorded,
    /// The recorded pid does not have this lock file open: the node runs in another pid namespace
    /// (a container) or the pid is not the holder. Signalling it could hit an unrelated process.
    NotHolder { pid: u32 },
    /// The lock or the holder could not be inspected.
    Inspect(String),
    /// `kill(2)` failed.
    Signal { pid: u32, error: String },
}

impl std::fmt::Display for DrainRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning => write!(f, "no seller is running on this home (its lock is free)"),
            Self::Unrecorded => write!(
                f,
                "a seller holds this home but recorded no pid: it is older than the drain feature \
                 (SIGUSR1 would kill it outright, so nothing was sent) or still starting — retry \
                 in a moment, or wait until it is idle and stop it normally"
            ),
            Self::NotHolder { pid } => write!(
                f,
                "the recorded seller pid {pid} does not hold this home's lock from here: the seller \
                 probably runs in a container (use `docker exec <container> maxplayer seller drain` \
                 or `docker kill --signal=USR1 <container>`); nothing was sent"
            ),
            Self::Inspect(error) => write!(f, "could not inspect the running seller: {error}"),
            Self::Signal { pid, error } => write!(f, "could not signal seller pid {pid}: {error}"),
        }
    }
}

impl std::error::Error for DrainRefusal {}

/// #1299: ask the seller node holding the lock at `path` to drain (SIGUSR1). Returns its pid.
///
/// Signals only a pid that (on Linux) is proven to have THIS lock file open, so a recorded pid that
/// means something else here — a container's pid namespace, a reused pid — is refused, not hit.
pub fn request_drain(path: impl AsRef<Path>) -> Result<u32, DrainRefusal> {
    let path = path.as_ref();
    let pid = match holder(path).map_err(|error| DrainRefusal::Inspect(error.to_string()))? {
        LockHolder::Free => return Err(DrainRefusal::NotRunning),
        LockHolder::Unrecorded => return Err(DrainRefusal::Unrecorded),
        LockHolder::Pid(pid) => pid,
    };
    if !holds_file_open(pid, path).map_err(DrainRefusal::Inspect)? {
        return Err(DrainRefusal::NotHolder { pid });
    }
    let raw = libc::pid_t::try_from(pid).map_err(|_| DrainRefusal::NotHolder { pid })?;
    // Safety: plain libc call; `raw` is a positive pid verified above.
    if unsafe { libc::kill(raw, libc::SIGUSR1) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Err(DrainRefusal::NotRunning); // exited between the check and the signal
        }
        return Err(DrainRefusal::Signal { pid, error: error.to_string() });
    }
    Ok(pid)
}

/// #1299: whether the node a drain was sent to is still up, for a caller waiting on the drain.
///
/// Read-only on purpose: it never touches the lock, so polling it cannot make a supervisor's
/// immediate restart of the drained node fail its own acquire. On Linux "up" means the pid still has
/// the lock file open (a reused pid does not); elsewhere, that the pid still exists.
pub fn still_holding(pid: u32, path: impl AsRef<Path>) -> bool {
    #[cfg(target_os = "linux")]
    {
        holds_file_open(pid, path.as_ref()).unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        let Ok(raw) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // Safety: signal 0 only probes for existence.
        unsafe { libc::kill(raw, 0) == 0 }
    }
}

/// Whether `pid` has the file at `path` open, compared by device and inode through `/proc`.
#[cfg(target_os = "linux")]
fn holds_file_open(pid: u32, path: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let lock = std::fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let fds = std::path::PathBuf::from(format!("/proc/{pid}/fd"));
    let entries = match std::fs::read_dir(&fds) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("{}: {error}", fds.display())),
    };
    Ok(entries.flatten().any(|entry| {
        std::fs::metadata(entry.path())
            .is_ok_and(|open| open.dev() == lock.dev() && open.ino() == lock.ino())
    }))
}

/// Elsewhere there is no `/proc`: the held lock plus the pid its holder wrote is the evidence.
#[cfg(not(target_os = "linux"))]
fn holds_file_open(_pid: u32, _path: &Path) -> Result<bool, String> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_lock(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-seller-lock-{label}-{}-{id}.lock",
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

    /// #1299: the holder records its pid, a probe reads it without disturbing the lock, and a
    /// released (or never created) lock reads as free.
    #[test]
    fn the_holder_records_its_pid_and_a_probe_reads_it() {
        let path = temp_lock("holder");
        let _ = std::fs::remove_file(&path);
        assert_eq!(holder(&path).unwrap(), LockHolder::Free, "no file: free");
        let held = HomeLock::acquire(&path).expect("acquire");
        assert_eq!(holder(&path).unwrap(), LockHolder::Pid(std::process::id()));
        assert_eq!(holder(&path).unwrap(), LockHolder::Pid(std::process::id()), "probing twice");
        drop(held);
        assert_eq!(holder(&path).unwrap(), LockHolder::Free, "released: free");
        // A pid file left behind is not a holder.
        assert!(matches!(request_drain(&path), Err(DrainRefusal::NotRunning)));
        let _ = std::fs::remove_file(&path);
    }

    /// #1299: a recorded pid that does not have the lock open (another pid namespace, a reused
    /// pid) is refused, never signalled. RED ON REVERT: drop the `/proc` check and the `sleep`
    /// child below is sent SIGUSR1, which kills it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_recorded_pid_that_does_not_hold_the_lock_is_not_signalled() {
        let path = temp_lock("not-holder");
        let _ = std::fs::remove_file(&path);
        let mut stranger = std::process::Command::new("sleep").arg("30").spawn().expect("sleep");
        let held = HomeLock::acquire(&path).expect("acquire");
        std::fs::write(&path, format!("{}\n", stranger.id())).unwrap();
        let refused = request_drain(&path);
        assert!(
            matches!(refused, Err(DrainRefusal::NotHolder { pid }) if pid == stranger.id()),
            "{refused:?}"
        );
        assert!(stranger.try_wait().unwrap().is_none(), "the stranger must not have been signalled");
        let _ = stranger.kill();
        let _ = stranger.wait();
        drop(held);
        let _ = std::fs::remove_file(&path);
    }

    const DRAIN_CHILD_ENV: &str = "MAXPLAYER_TEST_DRAIN_CHILD_LOCK";

    /// Not a test on its own: the child half of the next test, re-executed in a separate process.
    /// Without the env var it returns at once.
    #[test]
    fn drain_child_process() {
        let Some(path) = std::env::var_os(DRAIN_CHILD_ENV) else {
            return;
        };
        extern "C" fn on_usr1(_: libc::c_int) {
            // Safety: `_exit` is async-signal-safe.
            unsafe { libc::_exit(0) }
        }
        // Handler first, THEN the lock (which publishes the pid): the parent can only find us once
        // a signal can no longer kill us by default.
        let handler: extern "C" fn(libc::c_int) = on_usr1;
        // Safety: installs a handler that only calls an async-signal-safe function.
        unsafe { libc::signal(libc::SIGUSR1, handler as libc::sighandler_t) };
        let _held = HomeLock::acquire(PathBuf::from(path)).expect("child acquire");
        std::thread::sleep(std::time::Duration::from_secs(30));
        std::process::exit(7); // never signalled
    }

    /// #1299: `request_drain` finds the real holder in another process through the lock alone and
    /// delivers SIGUSR1 to it.
    #[cfg(target_os = "linux")]
    #[test]
    fn request_drain_signals_the_process_holding_the_lock() {
        let path = temp_lock("drain-child");
        let _ = std::fs::remove_file(&path);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "seller_node::lock::tests::drain_child_process", "--test-threads=1"])
            .env(DRAIN_CHILD_ENV, &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while holder(&path).unwrap() != LockHolder::Pid(child.id()) {
            assert!(std::time::Instant::now() < deadline, "the child never took the lock");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(request_drain(&path).expect("drain sent"), child.id());
        let status = child.wait().unwrap();
        assert!(!still_holding(child.id(), &path), "an exited holder is not still holding");
        assert_eq!(status.code(), Some(0), "the child must exit through its SIGUSR1 handler");
        assert_eq!(holder(&path).unwrap(), LockHolder::Free);
        let _ = std::fs::remove_file(&path);
    }
}
