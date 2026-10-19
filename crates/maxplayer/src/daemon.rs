//! Connect-or-spawn client to the per-home buyer daemon.
//!
//! The MCP and the CLI money commands never own the wallet, key, or budget ledger — the buyer
//! daemon does, guarded by the exclusive home lock. A caller that needs the daemon calls
//! [`ensure`], which connects to `$MAXPLAYER_HOME/buyer.sock` if a daemon is already serving, or spawns
//! one (this same binary, `maxplayer buyer serve`, detached) and waits for it to come up. A concurrent
//! double-spawn is safe: the loser fails closed at the exclusive home lock and exits, the winner
//! binds the socket, and both callers connect to the winner. No manual `maxplayer buyer` command is
//! ever needed.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use maxplayer_core::buyer::{LOCK_FILE, SOCKET_FILE, client};
use maxplayer_core::home::MaxplayerHome;
use serde_json::Value;

/// How long to wait for a freshly spawned daemon to bind its socket before giving up. Kept under the
/// MCP tool deadline so a cold start surfaces a clear error rather than a deadline timeout.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll cadence while waiting for the socket to answer.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The daemon socket path for a home.
pub fn socket_path(home: &MaxplayerHome) -> PathBuf {
    home.root.join(SOCKET_FILE)
}

/// Connect to the home's buyer daemon, spawning it if none is serving. Returns the socket path a
/// thin client can call. Never runs a money op itself — it only guarantees a daemon owns the home.
pub fn ensure(home: &MaxplayerHome) -> Result<PathBuf, String> {
    let sock = socket_path(home);
    if serving(&sock, &home.root.join(LOCK_FILE)) {
        return Ok(sock); // a daemon is already serving this home.
    }
    spawn_detached(&home.root)?;
    // Poll until our daemon — or a racing winner's — answers, or time out.
    let deadline = Instant::now() + SPAWN_READY_TIMEOUT;
    loop {
        if let Ok(response) = client::status(&sock) {
            if let Some(pid) = response.result.as_ref().and_then(|result| result["pid"].as_u64()) {
                // Announce only on the path where this invocation spawned, after a daemon is ready.
                // In a double-spawn race this reports the winner's pid, not necessarily our child.
                let display_home = home.root.canonicalize().unwrap_or_else(|_| home.root.clone());
                eprintln!("spawned buyer daemon pid={pid} home={}", display_home.display());
                return Ok(sock);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "buyer daemon did not come up on {} within {}s",
                sock.display(),
                SPAWN_READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(SPAWN_POLL_INTERVAL);
    }
}

/// Whether a buyer daemon is serving: something accepts on `sock` AND the home's exclusive daemon
/// lock is held. Both checks are local syscalls (a connect and a non-blocking `flock`), never an RPC.
///
/// This replaced a full `status` RPC that ran on EVERY tool call before the real request (wallet
/// balances through the wallet actor — a fresh wallet-DB open — plus ledger and award queries). On a
/// loaded buyer that probe alone could eat most of the MCP tool deadline (R4-2).
///
/// What the two checks prove, and what they do not:
/// - A dead daemon leaves a stale socket file that refuses the connect, and its lock is released by
///   the OS on exit, so it takes the spawn path.
/// - A listener on `buyer.sock` that does NOT hold `buyer.lock` is not a buyer daemon (the daemon
///   takes the lock before it binds and keeps it for life). The status probe used to reject such a
///   listener because it answered no JSON; the lock check rejects it now, and the spawned daemon
///   replaces the socket once it holds the lock (`buyer::bind_socket`).
/// - Neither check proves the lock holder speaks the protocol or is responsive. The status probe did
///   not prove that either: a JSON error reply counted as serving, and a wedged daemon hangs the
///   real call exactly as it hung the probe. A lock-holding process is never replaced here, as before.
fn serving(sock: &Path, lock: &Path) -> bool {
    if UnixStream::connect(sock).is_err() {
        return false;
    }
    // Opened read-only and never created: a home whose daemon never started has no lock file. If the
    // non-blocking lock SUCCEEDS nobody owns the home; it is dropped (released) at once, before any
    // spawn, so the spawned daemon can take it.
    match std::fs::File::open(lock) {
        Ok(file) => matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        Err(_) => false,
    }
}

/// Spawn this binary as a detached `maxplayer buyer serve`. Detached into its own process group so it
/// outlives the spawning session (a later session connects to the same daemon). A double-spawn is
/// safe — the loser fails closed at the exclusive home lock and exits.
fn spawn_detached(home_root: &Path) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot resolve the maxplayer binary to spawn the buyer daemon: {error}"))?;
    let mut command = Command::new(exe);
    command
        .arg("buyer")
        .arg("serve")
        // Pin the daemon to exactly this home so it serves the caller's home (default, MAXPLAYER_HOME,
        // or a --home path) rather than whatever the ambient env resolves to.
        .env("MAXPLAYER_HOME", home_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group: the daemon is not killed when the spawning session exits.
        command.process_group(0);
    }
    command
        .spawn()
        .map_err(|error| format!("failed to spawn the buyer daemon (`maxplayer buyer serve`): {error}"))?;
    Ok(())
}

/// Call a daemon method over the socket, returning its `result` value or a flattened error message.
/// The daemon owns the money authority; this is a pure transport.
pub fn call(sock: &Path, method: &str, params: Value) -> Result<Value, String> {
    let response = client::call(sock, method, params).map_err(|error| error.to_string())?;
    if let Some(error) = response.error {
        return Err(error.message);
    }
    response
        .result
        .ok_or_else(|| format!("buyer daemon returned neither result nor error for {method}"))
}

/// Ensure a daemon is serving `home`, then call `method` — the one-shot the MCP and CLI use.
pub fn ensure_then_call(home: &MaxplayerHome, method: &str, params: Value) -> Result<Value, String> {
    let sock = ensure(home)?;
    call(&sock, method, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maxplayer_core::buyer::lock::HomeLock;
    use std::os::unix::net::UnixListener;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp_dir(label: &str) -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "maxplayer-serving-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    #[test]
    fn serving_requires_a_listener_and_the_held_home_lock() {
        let dir = temp_dir("probe");
        let (sock, lock) = (dir.0.join(SOCKET_FILE), dir.0.join(LOCK_FILE));
        assert!(!serving(&sock, &lock), "nothing there: spawn");

        // A stale socket file from a dead daemon refuses the connect, even with the lock held.
        let held = HomeLock::acquire(&lock).unwrap();
        drop(UnixListener::bind(&sock).unwrap());
        assert!(!serving(&sock, &lock), "stale socket file: spawn");
        drop(held);
        std::fs::remove_file(&sock).unwrap();

        // A listener that does not hold the home lock is not a buyer daemon (finding 1, advisor r1).
        let _listener = UnixListener::bind(&sock).unwrap();
        assert!(!serving(&sock, &lock), "stray listener, lock free: spawn");
        // The probe released the lock it took; a daemon can still acquire it.
        let held = HomeLock::acquire(&lock).expect("probe must not keep the home lock");
        assert!(serving(&sock, &lock), "listener + held home lock: serving");
        drop(held);
        assert!(!serving(&sock, &lock), "lock released again: spawn");
    }
}
