//! `maxplayer seller drain` (#1299): ask the seller running on a home to drain — stop claiming,
//! finish in-flight work, publish its terminal `accepting=n`, exit 0 — and by default wait for it.
//!
//! The running node records its pid in `$MAXPLAYER_HOME/seller.lock`, the file it holds an
//! exclusive lock on for its whole life. This command trusts that pid only while the lock is held
//! and (on Linux) only if that pid has the lock file open, so a stale or foreign pid is refused,
//! never signalled. A seller in a container is reached with `docker exec <c> maxplayer seller drain`
//! or `docker kill --signal=USR1 <c>`.
//!
//! Exit codes: 0 drained (or, with `--no-wait`, the request was delivered); 1 usage; 2 nothing was
//! sent (no seller running, no pid recorded, not the holder, signal failed); 3 `--timeout` elapsed
//! with the seller still draining (the drain continues).

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const NOT_SENT: i32 = 2;
const STILL_DRAINING: i32 = 3;

/// How often the wait re-checks the holder. Read-only, so polling never touches the lock.
const POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    home: Option<PathBuf>,
    wait: bool,
    timeout: Option<Duration>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options { home: None, wait: true, timeout: None };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--home" => {
                let value = iter.next().ok_or("missing value for --home")?;
                options.home = Some(PathBuf::from(value));
            }
            "--no-wait" => options.wait = false,
            "--timeout" => {
                let value = iter.next().ok_or("missing value for --timeout")?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("--timeout expects whole seconds, got {value:?}"))?;
                options.timeout = Some(Duration::from_secs(secs));
            }
            other => return Err(format!("unknown option for `seller drain`: {other}")),
        }
    }
    if !options.wait && options.timeout.is_some() {
        return Err("--timeout has no effect with --no-wait".into());
    }
    Ok(options)
}

fn usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n  maxplayer seller drain [--home <dir>] [--no-wait | --timeout <secs>]\n\n\
         Ask the seller running on this home to drain: advertise accepting=n, claim nothing new,\n\
         honour awards for claims it already published, finish and deliver running jobs, then exit 0.\n\
         Waits until it has exited unless --no-wait. A supervisor with Restart=always then starts\n\
         the (upgraded) binary again; Restart=on-failure does not, exit 0 is not a failure.\n\
         Stop it immediately instead (abandoning the drain) with SIGTERM / `systemctl stop`.\n\n\
         Exit codes: 0 drained (or request delivered with --no-wait); 1 usage; 2 nothing sent\n\
         (no seller running, no pid recorded, pid not the lock holder, signal failed);\n\
         3 --timeout elapsed while the seller is still draining (the drain continues)."
    );
}

/// Entry from `sell::run` for `maxplayer seller drain ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if crate::cli::is_help_request(args) {
        usage(out);
        return SUCCESS;
    }
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            usage(err);
            return USAGE_ERROR;
        }
    };
    let root = match options.home.clone() {
        Some(path) => path,
        None => match maxplayer_core::home::default_home_dir() {
            Ok(path) => path,
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return NOT_SENT;
            }
        },
    };
    let lock = root.join(maxplayer_core::seller_node::LOCK_FILE);
    let pid = match maxplayer_core::seller_node::lock::request_drain(&lock) {
        Ok(pid) => pid,
        Err(refusal) => {
            let _ = writeln!(err, "seller drain: {refusal}");
            return NOT_SENT;
        }
    };
    let _ = writeln!(
        out,
        "seller drain: drain requested (SIGUSR1 to pid {pid}, home {})",
        root.display()
    );
    if !options.wait {
        let _ = writeln!(
            out,
            "seller drain: not waiting; the seller exits once its in-flight work is delivered (watch its `DRAINING` status line)"
        );
        return SUCCESS;
    }
    let _ = writeln!(
        out,
        "seller drain: waiting for in-flight work to finish (Ctrl-C stops waiting, not the drain)"
    );
    let started = Instant::now();
    while maxplayer_core::seller_node::lock::still_holding(pid, &lock) {
        if options.timeout.is_some_and(|limit| started.elapsed() >= limit) {
            let _ = writeln!(
                err,
                "seller drain: still draining after {}s; the drain continues — re-run to wait again",
                started.elapsed().as_secs()
            );
            return STILL_DRAINING;
        }
        std::thread::sleep(POLL);
    }
    let _ = writeln!(
        out,
        "seller drain: seller pid {pid} has exited after {}s",
        started.elapsed().as_secs()
    );
    SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_defaults_and_flags() {
        assert_eq!(
            parse(&[]).unwrap(),
            Options { home: None, wait: true, timeout: None }
        );
        assert_eq!(
            parse(&args(&["--home", "/h", "--timeout", "60"])).unwrap(),
            Options { home: Some("/h".into()), wait: true, timeout: Some(Duration::from_secs(60)) }
        );
        assert!(!parse(&args(&["--no-wait"])).unwrap().wait);
        assert!(parse(&args(&["--no-wait", "--timeout", "5"])).is_err());
        assert!(parse(&args(&["--timeout", "soon"])).is_err());
        assert!(parse(&args(&["--bogus"])).is_err());
    }

    /// No seller on the home: nothing is sent and the exit says so.
    #[test]
    fn no_running_seller_exits_not_sent() {
        let root = std::env::temp_dir().join(format!("maxplayer-drain-cli-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args(&["--home", root.to_str().unwrap()]), &mut out, &mut err);
        assert_eq!(code, NOT_SENT);
        assert!(String::from_utf8_lossy(&err).contains("no seller is running"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
