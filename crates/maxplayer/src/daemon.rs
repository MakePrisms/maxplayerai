//! Connect-or-spawn client to the per-home buyer daemon.
//!
//! The MCP and the CLI money commands never own the wallet, key, or budget ledger — the buyer
//! daemon does, guarded by the exclusive home lock. A caller that needs the daemon calls
//! [`ensure`], which connects to `$MAXPLAYER_HOME/buyer.sock` if a daemon is already serving, or spawns
//! one (this same binary, `maxplayer buyer serve`, detached) and waits for it to come up. A concurrent
//! double-spawn is safe: the loser fails closed at the exclusive home lock and exits, the winner
//! binds the socket, and both callers connect to the winner. No manual `maxplayer buyer` command is
//! ever needed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use maxplayer_core::buyer::{client, SOCKET_FILE};
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
    if client::status(&sock).is_ok() {
        return Ok(sock); // a daemon is already serving this home.
    }
    spawn_detached(&home.root)?;
    // Poll until our daemon — or a racing winner's — answers, or time out.
    let deadline = Instant::now() + SPAWN_READY_TIMEOUT;
    loop {
        if client::status(&sock).is_ok() {
            return Ok(sock);
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
        .map(|_child| ())
        .map_err(|error| format!("failed to spawn the buyer daemon (`maxplayer buyer serve`): {error}"))
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
