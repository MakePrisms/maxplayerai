//! `maxplayer buyer` — the persistent per-home daemon and its thin client.
//!
//! - `maxplayer buyer` (or `maxplayer buyer serve`) runs the daemon: it takes the exclusive
//!   home lock, opens the wallet + identity behind serialized actors and the
//!   durable state DB, and serves the local unix socket until terminated. A second
//!   daemon on the same home fails closed.
//! - `maxplayer buyer status` is the thin client: it connects to the running daemon's
//!   socket and prints its status. It holds no wallet, key, or state — proving the
//!   thin-client boundary.

use std::io::Write;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` — `buyer --help`, `buyer serve --help`, or `buyer status --help` —
    // prints usage to STDOUT and exits 0 BEFORE any dispatch. In particular `buyer status --help`
    // must NOT reach `status()` and open the daemon socket: a help request never touches the network.
    if crate::cli::is_help_request(args) {
        write_usage(out);
        return SUCCESS;
    }
    let (dispatch, rest): (fn(&mut dyn Write, &mut dyn Write) -> i32, &[String]) =
        match args.first().map(String::as_str) {
            None => (serve, args),
            Some("serve") => (serve, &args[1..]),
            Some("status") => (status, &args[1..]),
            Some("--home") => (serve, args),
            _ => return usage(err),
        };
    // #438: `maxplayer buyer` takes its home ONLY from $MAXPLAYER_HOME (or ~/.maxplayer) —
    // never from --home, in either its space-separated or `--home=<dir>` form. Honoring it
    // would require the daemon to plumb an operator flag through its exclusive-lock/socket-path
    // bootstrap; refusing loudly is the safer, smaller fix. A silently-ignored --home was the
    // actual bug: the daemon bound $MAXPLAYER_HOME while the operator believed they'd selected
    // --home's wallet. serve/status take no arguments at all, so ANY unrecognized trailing arg
    // is refused rather than swallowed (the wallet CLI's catch-all property) — an exact-string
    // match here previously let `--home=<dir>` through to the same silent divergence.
    if let Some(arg) = rest.first() {
        if arg == "--home" || arg.starts_with("--home=") {
            let _ = writeln!(
                err,
                "error: 'maxplayer buyer' takes its home from $MAXPLAYER_HOME (or ~/.maxplayer); \
                 --home is not supported here. Use: MAXPLAYER_HOME=<dir> maxplayer buyer"
            );
        } else {
            let _ = writeln!(err, "error: unknown argument: {arg}");
            write_usage(err);
        }
        return USAGE_ERROR;
    }
    dispatch(out, err)
}

/// Help that was asked for goes to stdout and succeeds; the same text on stderr means the
/// invocation was wrong. Only the destination and exit code differ, so both render here.
fn write_usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n  maxplayer buyer          # run the persistent per-home daemon (exclusive lock)\n  maxplayer buyer serve    # alias for `maxplayer buyer`\n  maxplayer buyer status   # thin client: query the running daemon over its socket"
    );
}

fn usage(err: &mut dyn Write) -> i32 {
    write_usage(err);
    USAGE_ERROR
}

#[cfg(feature = "wallet")]
fn serve(out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::home;

    let root = match home::default_home_dir() {
        Ok(root) => root,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    // First line of the daemon's log: which build is serving (before anything that can fail).
    let _ = writeln!(
        err,
        "{}",
        crate::build_stamp::start_line("buyer", &format!("home {}", root.display()))
    );
    let home = match home::bootstrap(&root) {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("maxplayer-buyer")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "buyer runtime: {error}");
            return RUNTIME_ERROR;
        }
    };

    let _ = writeln!(
        err,
        "maxplayer buyer online (home={}, socket={})",
        home.root.display(),
        home.root.join(maxplayer_core::buyer::SOCKET_FILE).display()
    );
    let _ = out.flush();

    let result = runtime.block_on(run_hosting_trade(home));
    // The daemon only returns on an error or after trade's safe stop; whatever is still parked on
    // the runtime (relay sessions, blocking store calls) must not hold the exit.
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    match result {
        Ok(None) => SUCCESS,
        Ok(Some(code)) => code,
        Err(error) => {
            let _ = writeln!(err, "maxplayer buyer: {error}");
            RUNTIME_ERROR
        }
    }
}

/// The buyer daemon plus trade's serve for this home ([`crate::trade_host`]), started once the
/// daemon holds its home lock (a second daemon that fails at the lock never hosts it).
///
/// While a serve session runs, TERM/INT stop the daemon through trade's safe stop: nothing new is
/// admitted, the step in progress finishes (or is cancelled at its next wait after the grace, its
/// journal intact), the session's final line is printed, and the daemon exits
/// `maxplayer_trade::shutdown::EXIT_STOPPED` (5). A daemon that never started a session keeps the
/// default disposition (killed by the signal, as before).
#[cfg(feature = "wallet")]
async fn run_hosting_trade(
    home: maxplayer_core::home::MaxplayerHome,
) -> Result<Option<i32>, maxplayer_core::buyer::BuyerError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let for_trade = home.clone();
    let daemon = maxplayer_core::buyer::run_with_ready(home, move || {
        if let Some(hosted) = crate::trade_host::start(&for_trade, "buyer daemon") {
            let _ = tx.send(hosted);
        }
    });
    tokio::pin!(daemon);
    let mut rx = rx;
    let hosted = tokio::select! {
        result = &mut daemon => {
            // The daemon may fail in the same poll that started the host (review of #1334, M1):
            // never leave it running unattended.
            if let Ok(hosted) = rx.try_recv() {
                hosted.shutdown("buyer daemon exiting").await;
            }
            return result.map(|()| None);
        }
        hosted = &mut rx => hosted.ok(),
    };
    let Some(hosted) = hosted else {
        return daemon.await.map(|()| None);
    };
    tokio::select! {
        result = &mut daemon => {
            hosted.shutdown("buyer daemon exiting").await;
            result.map(|()| None)
        }
        () = hosted.stop_requested() => {
            // Keep answering clients while trade stops safely (review of #1334, L2).
            let stop = hosted.shutdown("buyer daemon exiting");
            tokio::pin!(stop);
            tokio::select! {
                () = &mut stop => {}
                result = &mut daemon => {
                    stop.await;
                    result?;
                }
            }
            Ok(Some(i32::from(maxplayer_trade::shutdown::EXIT_STOPPED)))
        }
    }
}

#[cfg(feature = "wallet")]
fn status(out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::home;
    use maxplayer_core::buyer::{SOCKET_FILE, client};

    let root = match home::default_home_dir() {
        Ok(root) => root,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let socket = root.join(SOCKET_FILE);
    match client::status(&socket) {
        Ok(response) => {
            // Print exactly what the daemon returned (result or structured error).
            let body = serde_json::to_string(&response).unwrap_or_else(|error| {
                format!("{{\"error\":\"encode status: {error}\"}}")
            });
            let _ = writeln!(out, "{body}");
            if response.error.is_some() {
                RUNTIME_ERROR
            } else {
                SUCCESS
            }
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

#[cfg(not(feature = "wallet"))]
fn serve(_out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let _ = writeln!(
        err,
        "maxplayer buyer requires the wallet feature: rebuild with `--features wallet` (on by default)"
    );
    USAGE_ERROR
}

#[cfg(not(feature = "wallet"))]
fn status(_out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let _ = writeln!(
        err,
        "maxplayer buyer requires the wallet feature: rebuild with `--features wallet` (on by default)"
    );
    USAGE_ERROR
}
