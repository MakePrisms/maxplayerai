//! Trade's `serve`, hosted inside a long-running maxplayer daemon (the buyer daemon `maxplayer
//! buyer serve` and the seller `maxplayer seller serve`), so nobody runs `maxplayer trade serve` by hand.
//!
//! It is the same serve as the standalone command: it holds `<home>/trade/owner.lock` for the whole
//! session, binds `<home>/trade/serve.sock` (protocol [`crate::serve::PROTOCOL_V`]) before its
//! startup recovery, runs [`crate::coordinator::run_with`], and prints the same lifecycle lines
//! ([`crate::shutdown`]), the startup one naming its `host`. What hosting adds:
//!
//! - **Nothing is created on a home that does not trade.** A session starts only once
//!   `<home>/trade/trade.sqlite` exists (the first trading command creates it); until then the host
//!   only looks for it every [`POLL`]. A legacy standalone home or a mixed one is never served.
//! - **One serve per home.** `owner.lock` decides: whoever holds it (a standalone serve, the other
//!   daemon on this home, a running trade command) keeps it, and this host stands by, checking
//!   again every [`POLL`], and takes over once it is free. Trade commands hand off to whichever
//!   serve answers on the socket.
//! - **Errors never reach the daemon.** A session that ends with an error (a closed market inbox,
//!   an unreadable journal row) prints its final line, releases the home and is restarted after a
//!   backoff ([`RESTART_MIN`] doubling to [`RESTART_MAX`]); the daemon keeps running.
//! - **Safe stop.** [`Hosted::shutdown`] (the daemon is exiting) and TERM/INT once a session has
//!   started stop serve exactly as a signal stops the standalone one: nothing new is admitted, the
//!   step in progress gets [`crate::shutdown::GRACE`], a second signal (or the grace) cancels it at
//!   its next wait with its journal intact, and the final line says `exit` 5. The daemon waits for
//!   that before it exits.
use crate::{coordinator, journal::Journal, market::Market, shutdown, wallet};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use futures_util::FutureExt;
use std::{
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::SeqCst},
    },
    time::Duration,
};

/// Test builds only: the next session panics once started (proves a panic is contained).
#[cfg(feature = "local-test-mints")]
#[doc(hidden)]
pub static PANIC_NEXT_SESSION: AtomicBool = AtomicBool::new(false);

/// How often a host that is not serving checks again (the home starts trading, `owner.lock` frees).
pub const POLL: Duration = Duration::from_secs(5);
/// First restart delay after a session ended with an error.
pub const RESTART_MIN: Duration = Duration::from_secs(2);
/// Longest restart delay.
pub const RESTART_MAX: Duration = Duration::from_secs(60);

/// How a daemon hosts serve.
#[derive(Clone, Debug)]
pub struct Options {
    /// Who hosts it, for the startup line (`buyer daemon`, `seller`).
    pub host: &'static str,
    /// Market relays; empty means [`crate::market::DEFAULT_RELAYS`] (what `maxplayer trade` uses).
    pub relays: Vec<String>,
    /// `nostr://` mint relays; empty means the defaults.
    pub mint_relays: Vec<String>,
    pub poll: Duration,
    pub restart_min: Duration,
    pub restart_max: Duration,
    /// Install TERM/INT handling once a session starts (production). Tests stop through
    /// [`Hosted::shutdown`] and [`crate::shutdown::request`] instead.
    pub signals: bool,
    /// The runtime the TERM/INT listener runs on; `None`: serve's own. A daemon with no other
    /// listener (the buyer) passes its own runtime, which lives as long as the process: tokio never
    /// restores the default disposition, so a listener that died with serve's runtime would leave
    /// TERM/INT swallowed.
    pub signal_runtime: Option<tokio::runtime::Handle>,
}
impl Options {
    pub fn new(host: &'static str, relays: Vec<String>, mint_relays: Vec<String>) -> Self {
        Self {
            host,
            relays,
            mint_relays,
            poll: POLL,
            restart_min: RESTART_MIN,
            restart_max: RESTART_MAX,
            signals: true,
            signal_runtime: None,
        }
    }
}

/// A running host. Keep it for the daemon's life and end it with [`Hosted::shutdown`].
///
/// Serve runs on its own thread and tokio runtime (its scheduler is not `Send`, exactly why the
/// standalone command `block_on`s it), so the daemon's runtime never polls trade work and a stuck
/// trade step cannot stall the daemon's timers.
pub struct Hosted {
    thread: Option<std::thread::JoinHandle<()>>,
    done: tokio::sync::watch::Receiver<bool>,
    serving: Arc<AtomicBool>,
    sessions: Arc<AtomicU64>,
}

/// Why a host is not serving right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Idle {
    /// Not a maxplayer home (`key` / `config.toml`): never served here.
    NotMaxplayerHome,
    /// A maxplayer home that also holds legacy standalone trading files: refused, as by every
    /// trade command.
    Mixed,
    /// No trade journal: this home has never traded. Nothing is created.
    NotTrading,
    /// `owner.lock` is held by another process.
    Busy,
}

/// Start hosting serve for `home` once the daemon owns its home.
///
/// The relay sets are checked first; an invalid one is returned and nothing runs. Creates nothing
/// in `home` until it trades.
pub fn spawn(home: &Path, options: Options) -> Result<Hosted> {
    let relays: Vec<String> = if options.relays.is_empty() {
        crate::market::DEFAULT_RELAYS
            .iter()
            .map(|r| r.to_string())
            .collect()
    } else {
        options.relays.clone()
    };
    ensure!(relays.len() <= 8, "[trade] relays: at most 8 market relays");
    for relay in &relays {
        crate::market::check_market_relay(relay)
            .with_context(|| format!("[trade] relays: {relay} is not a wss:// relay"))?;
    }
    crate::transport::configure_mint_relays(&options.mint_relays).context("[trade] mint_relays")?;
    // Mint requests sign as this home's key (a maxplayer home's key is never created here).
    crate::transport::configure_mint_identity(home, false);
    let serving = Arc::new(AtomicBool::new(false));
    let sessions = Arc::new(AtomicU64::new(0));
    let (done_tx, done) = tokio::sync::watch::channel(false);
    let (home, s, n) = (home.to_path_buf(), serving.clone(), sessions.clone());
    let (built_tx, built_rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("maxplayer-trade-serve".into())
        .spawn(move || {
            // Declared first, dropped last: `done` turns true however this thread ends (a panic
            // included), after the runtime is gone.
            let _done = Done(done_tx, s.clone());
            // Built here, not in the caller: a runtime dropped inside the daemon's runtime panics.
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("maxplayer-trade")
                .enable_all()
                .build()
            {
                Ok(runtime) => {
                    let _ = built_tx.send(Ok(()));
                    runtime
                }
                Err(e) => {
                    let _ = built_tx.send(Err(e));
                    return;
                }
            };
            runtime.block_on(supervise(home, relays, options, s, n));
            // Relay clients and the socket listener die with the runtime. Bounded: a blocking task
            // still running (none should be once serve stopped) never holds the daemon's exit.
            runtime.shutdown_timeout(Duration::from_secs(5));
        })
        .context("trade serve thread")?;
    built_rx
        .recv()
        .context("trade serve thread")?
        .context("trade serve runtime")?;
    Ok(Hosted {
        thread: Some(thread),
        done,
        serving,
        sessions,
    })
}

/// Marks the host thread finished (and not serving) when dropped.
struct Done(tokio::sync::watch::Sender<bool>, Arc<AtomicBool>);
impl Drop for Done {
    fn drop(&mut self) {
        self.1.store(false, SeqCst);
        let _ = self.0.send(true);
    }
}

/// Request the safe stop as `reason` unless one was requested already (never escalates).
fn request_stop(serving: bool, reason: &'static str) {
    if shutdown::request_first(reason) && serving {
        shutdown::announce_phase(1, reason);
    }
}
/// Force the stop (the grace ran out), unless a second signal already forced it.
fn force_stop(serving: bool) {
    if shutdown::stop().is_some_and(|s| s.forced.is_some()) {
        return;
    }
    let phase = shutdown::request("grace");
    if serving {
        shutdown::announce_phase(phase, "grace");
    }
}

/// A host is never left running unattended: dropping it (the daemon failed after starting it, or
/// a panic unwound past it) stops serve exactly as [`Hosted::shutdown`] does, blocking.
impl Drop for Hosted {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        request_stop(self.serving(), "daemon exiting");
        let end = std::time::Instant::now() + shutdown::GRACE;
        while self.running() && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.running() {
            force_stop(self.serving());
        }
        let _ = thread.join();
    }
}

impl Hosted {
    /// Whether a session holds the home and serves it right now.
    pub fn serving(&self) -> bool {
        self.serving.load(SeqCst)
    }
    /// How many serve sessions started so far (a restart after an error starts another).
    pub fn sessions(&self) -> u64 {
        self.sessions.load(SeqCst)
    }
    /// Whether the host task is still running (it ends only after a stop).
    pub fn running(&self) -> bool {
        !*self.done.borrow()
    }
    /// Resolves once a stop of the hosted serve was requested (TERM/INT after a session started, or
    /// [`crate::shutdown::request`]). A daemon then calls [`Hosted::shutdown`] and exits.
    pub async fn stop_requested(&self) {
        let _ = shutdown::requested().await;
    }

    /// Stop hosting and wait until serve has stopped safely: a session in progress admits nothing
    /// new, its running step gets [`shutdown::GRACE`] and is then cancelled at its next wait
    /// (journal intact). `reason` names the stop when nothing requested one yet.
    pub async fn shutdown(mut self, reason: &'static str) {
        request_stop(self.serving(), reason);
        if tokio::time::timeout(shutdown::GRACE, self.done.wait_for(|d| *d))
            .await
            .is_err()
        {
            force_stop(self.serving());
            let _ = self.done.wait_for(|d| *d).await;
        }
        if let Some(thread) = self.thread.take() {
            // `done` is set as the thread's last act, so this join returns at once.
            let _ = thread.join();
        }
    }
}

/// Sleep `d`, or less when a stop is requested.
async fn pause(d: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = shutdown::requested() => {}
    }
}

async fn supervise(
    home: PathBuf,
    relays: Vec<String>,
    options: Options,
    serving: Arc<AtomicBool>,
    sessions: Arc<AtomicU64>,
) {
    let mut said: Option<Idle> = None;
    let mut delay = options.restart_min;
    let mut handlers = false;
    loop {
        if shutdown::stop().is_some() {
            return;
        }
        match session(&home, &relays, &options, &serving, &sessions, &mut handlers).await {
            Ok(idle) => {
                if said != Some(idle) {
                    if let Some(line) = idle_line(&home, idle) {
                        shutdown::emit(&line);
                    }
                    said = Some(idle);
                }
                pause(options.poll).await;
            }
            Err((error, ran)) => {
                said = None;
                if shutdown::stop().is_some() {
                    return;
                }
                // A session that ran for a while starts the backoff over.
                if ran >= options.restart_max {
                    delay = options.restart_min;
                }
                shutdown::emit(
                    &serde_json::json!({"at":crate::clock::iso(crate::clock::now()),
                        "serve":"restarting","host":options.host,
                        "in_seconds":delay.as_secs_f64(),
                        "after":crate::redact_reason(&format!("{error:#}"))})
                    .to_string(),
                );
                pause(delay).await;
                delay = (delay * 2).min(options.restart_max);
            }
        }
    }
}

/// What a host prints once when it starts waiting for `idle` (nothing for a home that never
/// traded: a non-trading daemon stays quiet).
fn idle_line(home: &Path, idle: Idle) -> Option<String> {
    let state = wallet::state_dir(home);
    match idle {
        Idle::NotTrading | Idle::NotMaxplayerHome => None,
        Idle::Mixed => Some(format!(
            "trade serve not hosted: {}",
            wallet::mixed_refusal(home)
        )),
        Idle::Busy => Some(format!(
            "trade serve on {} is held by another process ({} is locked: a standalone `maxplayer \
             trade serve`, the other daemon on this home or a running trade command); standing by \
             and taking over when it is free",
            home.display(),
            state.join("owner.lock").display()
        )),
    }
}

/// One serve session: `Ok(idle)` when it did not start (nothing created), `Err` with how long it
/// ran when it ended.
async fn session(
    home: &Path,
    relays: &[String],
    options: &Options,
    serving: &Arc<AtomicBool>,
    sessions: &Arc<AtomicU64>,
    handlers: &mut bool,
) -> std::result::Result<Idle, (anyhow::Error, Duration)> {
    if !wallet::is_maxplayer_home(home) {
        return Ok(Idle::NotMaxplayerHome);
    }
    if wallet::is_mixed_dir(home) {
        return Ok(Idle::Mixed);
    }
    let state = wallet::state_dir(home);
    // The bootstrap rule: only a home that already trades is served (no journal, lock or socket
    // is created on any other).
    if std::fs::symlink_metadata(state.join("trade.sqlite")).is_err() {
        return Ok(Idle::NotTrading);
    }
    // A trading home: lifecycle lines also go to <home>/trade/serve.log (the daemon may have no
    // stderr).
    shutdown::log_to(Some(state.join("serve.log")));
    let started = tokio::time::Instant::now();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(state.join("owner.lock"))
        .context("open trade owner.lock")
        .map_err(|e| (e, started.elapsed()))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(Idle::Busy);
    }
    if options.signals && !*handlers {
        install_signal_handlers(serving.clone(), options.signal_runtime.clone())
            .map_err(|e| (e, started.elapsed()))?;
        *handlers = true;
    }
    shutdown::announce(home, relays, Some(options.host));
    sessions.fetch_add(1, SeqCst);
    serving.store(true, SeqCst);
    // A panic in serve ends this session like an error (restarted after the backoff), with the
    // lock still held so its final line and socket removal stay ours.
    let run = std::panic::AssertUnwindSafe(async {
        #[cfg(feature = "local-test-mints")]
        if PANIC_NEXT_SESSION.swap(false, SeqCst) {
            panic!("test panic in hosted serve");
        }
        // Bound before the startup recovery, so hand-offs queue instead of failing meanwhile.
        let mut jobs = crate::serve::bind(home, relays.to_vec())?;
        let opened = shutdown::bounded(async {
            let j = Journal::open(home).await?;
            let m = Market::connect(wallet::identity(home)?, relays).await?;
            anyhow::Ok((j, m))
        })
        .await;
        let (j, mut m) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                // Hand-offs queued meanwhile never started: say so (not a dropped connection).
                jobs.close();
                while let Ok(job) = jobs.try_recv() {
                    crate::serve::refuse(job, "serve could not start; nothing was applied");
                }
                return Err(e);
            }
        };
        let r = coordinator::run_with(home, &j, &mut m, None, Some(jobs)).await;
        for c in &m.clients {
            let _ = tokio::time::timeout(Duration::from_secs(5), c.shutdown()).await;
        }
        r
    })
    .catch_unwind()
    .await;
    let result: Result<()> = run.unwrap_or_else(|panic| {
        let text = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        Err(anyhow::anyhow!("serve panicked: {text}"))
    });
    let error = match result {
        Ok(()) => anyhow::anyhow!("serve returned"),
        Err(e) => e,
    };
    let code = if error.is::<shutdown::Stopped>() {
        shutdown::EXIT_STOPPED
    } else {
        crate::serve::exit_code(&error)
    };
    // Still holding owner.lock: the socket is ours to remove.
    shutdown::finished(code, Some(&error));
    serving.store(false, SeqCst);
    drop(lock);
    Err((error, started.elapsed()))
}

/// TERM/INT once a session has started (the daemon no longer dies on them outright): the first
/// requests the safe stop, a second one, or [`shutdown::GRACE`], forces it.
fn install_signal_handlers(
    serving: Arc<AtomicBool>,
    runtime: Option<tokio::runtime::Handle>,
) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let runtime = runtime.unwrap_or_else(tokio::runtime::Handle::current);
    let _enter = runtime.enter();
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    runtime.spawn(async move {
        let first = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        };
        let phase = shutdown::request(first);
        if serving.load(SeqCst) {
            shutdown::announce_phase(phase, first);
        }
        let second = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
            _ = tokio::time::sleep(shutdown::GRACE) => "grace",
        };
        let phase = shutdown::request(second);
        if serving.load(SeqCst) {
            shutdown::announce_phase(phase, second);
        }
    });
    Ok(())
}
