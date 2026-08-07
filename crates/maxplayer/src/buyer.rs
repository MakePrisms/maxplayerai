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
    match args.first().map(String::as_str) {
        None | Some("serve") => serve(out, err),
        Some("status") => status(out, err),
        _ => usage(err),
    }
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

    match runtime.block_on(maxplayer_core::buyer::run(home)) {
        Ok(()) => SUCCESS,
        Err(error) => {
            let _ = writeln!(err, "maxplayer buyer: {error}");
            RUNTIME_ERROR
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
