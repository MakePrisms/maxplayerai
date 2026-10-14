//! `serve` lifecycle: a timestamped startup line, SIGTERM/SIGINT handling with a bounded safe
//! stop, and one final line with the exit reason and what is left unfinished.
//!
//! Field report 2026-10-12 §4e: a serve stopped around noon with no reason and no time; the next
//! start only logged `stale_removed`. Now:
//!
//! - start: `{"at":…,"serve":"started","version":…,"commit":…,"pid":…,"home":…,"relays":[…],…}`;
//! - first SIGTERM or SIGINT: serve stops admitting work (no new inbox message, hand-off request
//!   or recovery tick is started; queued hand-offs are refused as not started) and lets the step
//!   in progress finish, so its journal writes and mint call complete normally;
//! - that step gets at most [`GRACE`]. A second signal, or the grace running out, cancels it at
//!   its next wait, exactly as a step budget already does: every money step is journaled before
//!   its effect and a single journal write is never split, so the journal stays intact and the
//!   step is replayed from it on the next start (ambiguous until then, never released or refused);
//! - exit: `{"at":…,"serve":"exited","reason":"signal"|"error"|"inbox_closed",…,"exit":N,
//!   "unfinished":{…}}`, then exit code [`EXIT_STOPPED`] after a signal. Errors keep their codes
//!   (1, 3, 4). The socket is removed, so a later `stale_removed` means no final line was written. SIGKILL, OOM kills and power loss cannot be logged from inside the process: run
//!   serve under a supervisor that records exit status and signal.
//!
//! Lifecycle output never carries keys, tokens, proofs or preimages: public ids, states and
//! redacted error text only.
use std::{
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, OnceLock},
    time::Duration,
};

/// Exit code of a serve stopped by SIGTERM/SIGINT (graceful or forced). Distinct from 0 (serve
/// never ends on its own), 1 (error), 2 (usage), 3 (unresolved work) and 4 (manual recovery).
pub const EXIT_STOPPED: u8 = 5;
/// Longest the step in progress may run after the first signal before it is cancelled.
pub const GRACE: Duration = Duration::from_secs(30);

/// Serve stopped on a signal. `forced`: the step in progress was cancelled (second signal or
/// grace); its journal is intact and the next start replays it.
#[derive(Debug, Clone)]
pub struct Stopped {
    pub signal: &'static str,
    pub forced: Option<&'static str>,
}
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "serve stopped on {}; journal retained, restart serve to resume",
            self.signal
        )
    }
}
impl std::error::Error for Stopped {}

/// The market inbox closed: no trade message can arrive any more. Exit 1.
#[derive(Debug)]
pub struct InboxClosed;
impl std::fmt::Display for InboxClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "market inbox closed (relay clients stopped); serve cannot receive trade messages. Journal retained: restart serve"
        )
    }
}
impl std::error::Error for InboxClosed {}

/// 0 running, 1 stop requested, 2 stop forced.
static PHASE: LazyLock<tokio::sync::watch::Sender<u8>> =
    LazyLock::new(|| tokio::sync::watch::channel(0).0);
static SIGNALS: Mutex<(Option<&'static str>, Option<&'static str>)> = Mutex::new((None, None));
static HOME: OnceLock<PathBuf> = OnceLock::new();

fn signals() -> (Option<&'static str>, Option<&'static str>) {
    *SIGNALS.lock().unwrap_or_else(|e| e.into_inner())
}
/// The stop requested so far, if any.
pub fn stop() -> Option<Stopped> {
    let (signal, forced) = signals();
    signal.map(|signal| Stopped { signal, forced })
}
/// Resolves once a stop is requested (immediately if it already was).
pub async fn requested() -> Stopped {
    let mut rx = PHASE.subscribe();
    let _ = rx.wait_for(|p| *p >= 1).await;
    stop().unwrap_or(Stopped {
        signal: "stop",
        forced: None,
    })
}
async fn forced() -> Stopped {
    let mut rx = PHASE.subscribe();
    let _ = rx.wait_for(|p| *p >= 2).await;
    stop().unwrap_or(Stopped {
        signal: "stop",
        forced: Some("stop"),
    })
}
/// Run `f` unless the stop is forced first: then `f` is dropped at its current wait (see the
/// module docs for why that keeps the journal intact) and [`Stopped`] is returned.
/// Not an `async fn`: `f` is boxed before the returned future exists (see `progress::scoped`).
pub fn bounded<T>(
    f: impl std::future::Future<Output = anyhow::Result<T>>,
) -> impl std::future::Future<Output = anyhow::Result<T>> {
    let f = Box::pin(f);
    async move {
        tokio::select! {
            r = f => r,
            s = forced() => Err(s.into()),
        }
    }
}

/// Request a stop as signal `name` would (first call), or force it (second call).
pub fn signal(name: &'static str) {
    let phase = {
        let mut s = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        if s.0.is_none() {
            s.0 = Some(name);
            1
        } else {
            s.1.get_or_insert(name);
            2
        }
    };
    // Never hold a `borrow()` guard across `send_*`: the watch lock would deadlock.
    PHASE.send_if_modified(|p| {
        let changed = phase > *p;
        *p = (*p).max(phase);
        changed
    });
    let now = crate::clock::now();
    let line = if phase == 1 {
        serde_json::json!({"at":crate::clock::iso(now),"serve":"stopping","signal":name,
            "grace_seconds":GRACE.as_secs(),
            "note":"no new messages, requests or recovery ticks; the step in progress may finish; a second signal cancels it now"})
    } else {
        serde_json::json!({"at":crate::clock::iso(now),"serve":"stop_forced","by":name,
            "note":"the step in progress is cancelled at its next wait; its journal is intact and the next start resumes it"})
    };
    eprintln!("{line}");
}

/// Print the startup line and install the TERM/INT handlers (replacing their default, which
/// kills the process without a word). Call once, before binding the socket.
pub fn start(home: &Path, relays: &[String]) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal as unix_signal};
    let mut term = unix_signal(SignalKind::terminate())?;
    let mut int = unix_signal(SignalKind::interrupt())?;
    let _ = HOME.set(home.to_path_buf());
    let now = crate::clock::now();
    eprintln!(
        "{}",
        serde_json::json!({"at":crate::clock::iso(now),"serve":"started",
            "version":env!("CARGO_PKG_VERSION"),"commit":BUILD_COMMIT,"pid":std::process::id(),
            "home":home,"relays":relays,"mint_relays":crate::transport::mint_relays(),
            "stop":format!("SIGTERM/SIGINT: safe stop within {}s, exit {EXIT_STOPPED}", GRACE.as_secs())})
    );
    tokio::spawn(async move {
        let first = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        };
        signal(first);
        let second = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
            _ = tokio::time::sleep(GRACE) => "grace",
        };
        signal(second);
    });
    Ok(())
}

/// The final line: why serve ended, its exit code and what is unfinished. A no-op unless
/// [`start`] ran in this process. `error` is the returned error, if any.
pub fn finished(code: u8, error: Option<&anyhow::Error>) {
    let Some(home) = HOME.get() else {
        return;
    };
    let now = crate::clock::now();
    let mut v = serde_json::json!({"at":crate::clock::iso(now),"serve":"exited","exit":code,
        "pid":std::process::id(),"journal":"retained"});
    match error {
        Some(e) if e.is::<Stopped>() => {
            let s = e.downcast_ref::<Stopped>().cloned().or_else(stop);
            v["reason"] = "signal".into();
            v["signal"] = s.as_ref().map(|s| s.signal).into();
            v["forced"] = s.and_then(|s| s.forced).into();
        }
        Some(e) if e.is::<InboxClosed>() => v["reason"] = "inbox_closed".into(),
        Some(e) => {
            v["reason"] = "error".into();
            v["error"] = crate::redact_reason(&format!("{e:#}")).into();
            if let Some(s) = stop() {
                v["signal"] = s.signal.into();
            }
        }
        None => v["reason"] = "returned".into(),
    }
    v["unfinished"] = unfinished(home, now);
    // This process still holds owner.lock, so the socket is ours: remove it, so a later
    // `stale_removed` at startup means the previous serve ended without this line (SIGKILL, OOM,
    // power loss). Removing it says nothing about settlement; `unfinished` does.
    let socket = home.join(crate::serve::SOCKET);
    if std::fs::symlink_metadata(&socket).is_ok_and(|m| {
        use std::os::unix::fs::FileTypeExt;
        m.file_type().is_socket()
    }) {
        v["socket_removed"] = std::fs::remove_file(&socket).is_ok().into();
    }
    eprintln!("{v}");
}

/// Public ids and states of everything still owed work (read-only snapshot, no lock).
pub fn unfinished(home: &Path, now: u64) -> serde_json::Value {
    use crate::coordinator::{Listing, Swap, terminal};
    let rows = |ns: &str| crate::observe::journal_rows(home, ns).unwrap_or_default();
    let mut swaps = vec![];
    for (_, v, _) in rows("swap") {
        let Ok(s) = serde_json::from_slice::<Swap>(&v) else {
            swaps.push(serde_json::json!({"swap_id":null,"state":"unreadable"}));
            continue;
        };
        if terminal(&s) && !s.state.ends_with("_quarantined") {
            continue;
        }
        let mut line = serde_json::json!({"swap_id":s.id,"role":s.role,"state":s.state});
        if s.state.ends_with("_quarantined") {
            line["manual_recovery"] = true.into();
        }
        if let Some(at) = crate::observe::refund_available_at(&s) {
            line["refund_earliest"] = crate::clock::iso(at).into();
            line["refund_earliest_in"] = crate::clock::remaining(at, now).into();
        }
        swaps.push(line);
    }
    let mut lots = vec![];
    for (id, v, _) in rows("listing") {
        if let Ok(l) = serde_json::from_slice::<Listing>(&v) {
            if !l.cancelled
                && l.active.is_none()
                && crate::lifecycle(&l.event, &l.statuses)
                    .is_ok_and(|s| s == crate::Status::Available)
            {
                lots.push(id);
            }
        }
    }
    let ids = |ns: &str, open: &dyn Fn(&[u8]) -> Option<String>| -> Vec<String> {
        rows(ns).iter().filter_map(|(_, v, _)| open(v)).collect()
    };
    let funding = ids("funding", &|v| {
        let f: crate::money::Funding = serde_json::from_slice(v).ok()?;
        (!f.done && !f.expired_unpaid).then_some(f.id)
    });
    let withdrawals = ids("withdrawal", &|v| {
        let a: crate::money::Withdrawal = serde_json::from_slice(v).ok()?;
        (!a.terminal()).then_some(a.id)
    });
    let receives = ids("receive", &|v| {
        let r: crate::receive::Receipt = serde_json::from_slice(v).ok()?;
        r.unresolved().then_some(r.id)
    });
    let sends = ids("send", &|v| {
        let a: crate::send::SendAttempt = serde_json::from_slice(v).ok()?;
        (!a.terminal() || a.manual_recovery()).then_some(a.id)
    });
    serde_json::json!({"swaps":swaps,"lots_unserved":lots,"funding":funding,
        "withdrawals":withdrawals,"receives":receives,"sends":sends})
}

/// The commit this binary was built from (`build.rs`), or `unknown`.
pub const BUILD_COMMIT: &str = include_str!(concat!(env!("OUT_DIR"), "/build_commit"));
