//! `maxplayer delivery show|extract` — read a settled git delivery from the buyer store, or extract
//! it on purpose into a throwaway folder (spec `docs/specs/settle-without-unpacking.md` §2.3–2.4).
//!
//! Thin adapter over the buyer daemon's `read_delivery` / `extract_delivery` RPCs.

use std::io::Write;
use std::path::PathBuf;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

#[derive(Debug, PartialEq, Eq)]
struct Opts {
    action: Action,
    job_id: String,
    turn: Option<u32>,
    home: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Show {
        path: Option<String>,
        offset: Option<usize>,
        max_bytes: Option<usize>,
    },
    Extract {
        to: Option<String>,
    },
}

fn usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n\
         \x20 maxplayer delivery show <job_id> [<path>] [--offset <n>] [--max-bytes <n>] [--turn <n>] [--home <path>]\n\
         \x20 maxplayer delivery extract <job_id> [--to <dir>] [--turn <n>] [--home <path>]\n\
         \n\
         A settled git delivery stays in the buyer store; nothing is written to disk when it settles.\n\
         show lists its files, or prints one file's text, without writing anything.\n\
         extract writes it into a new folder under the system temp dir (or --to: a new or empty\n\
         folder outside the maxplayer home). The files keep the seller's names, including agent\n\
         instruction and config files: do not start an agent session or run git in that folder\n\
         until it is reviewed. extract refuses a delivery that lays out a git repository.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut iter = args.iter();
    let action = iter
        .next()
        .ok_or("delivery requires show or extract")?
        .clone();
    let mut job_id = None;
    let mut path = None;
    let (mut offset, mut max_bytes, mut to, mut turn, mut home) = (None, None, None, None, None);
    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--offset" => {
                offset = Some(
                    value("--offset")?
                        .parse()
                        .map_err(|_| "--offset is a number")?,
                )
            }
            "--max-bytes" => {
                max_bytes = Some(
                    value("--max-bytes")?
                        .parse()
                        .map_err(|_| "--max-bytes is a number")?,
                )
            }
            "--to" if action == "extract" => to = Some(value("--to")?),
            "--turn" => turn = Some(value("--turn")?.parse().map_err(|_| "--turn is a number")?),
            "--home" => home = Some(PathBuf::from(value("--home")?)),
            other if other.starts_with('-') => return Err(format!("unknown flag {other}")),
            other if job_id.is_none() => job_id = Some(other.to_owned()),
            other if action == "show" && path.is_none() => path = Some(other.to_owned()),
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let action = match action.as_str() {
        "show" => Action::Show {
            path,
            offset,
            max_bytes,
        },
        "extract" if offset.is_none() && max_bytes.is_none() => Action::Extract { to },
        "extract" => return Err("--offset/--max-bytes apply to show".into()),
        other => return Err(format!("unknown delivery action {other} (show or extract)")),
    };
    Ok(Opts {
        action,
        job_id: job_id.ok_or("delivery requires <job_id>")?,
        turn,
        home,
    })
}

#[cfg(feature = "wallet")]
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::home;
    if crate::cli::is_help_request(args) {
        usage(out);
        return SUCCESS;
    }
    let opts = match parse(args) {
        Ok(opts) => opts,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            usage(err);
            return USAGE_ERROR;
        }
    };
    let root = match opts.home {
        Some(path) => path,
        None => match home::default_home_dir() {
            Ok(path) => path,
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return RUNTIME_ERROR;
            }
        },
    };
    let home = match home::bootstrap(&root) {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let (method, params) = match opts.action {
        Action::Show {
            path,
            offset,
            max_bytes,
        } => (
            "read_delivery",
            serde_json::json!({ "job_id": opts.job_id, "turn": opts.turn, "path": path, "offset": offset, "max_bytes": max_bytes }),
        ),
        Action::Extract { to } => {
            // Resolved here: the daemon's working directory is not the caller's.
            let to = match to.map(PathBuf::from) {
                Some(to) if to.is_relative() => match std::env::current_dir() {
                    Ok(cwd) => Some(cwd.join(to).display().to_string()),
                    Err(error) => {
                        let _ = writeln!(err, "current dir: {error}");
                        return RUNTIME_ERROR;
                    }
                },
                other => other.map(|p| p.display().to_string()),
            };
            (
                "extract_delivery",
                serde_json::json!({ "job_id": opts.job_id, "turn": opts.turn, "to": to }),
            )
        }
    };
    match crate::daemon::ensure_then_call(&home, method, params) {
        Ok(body) => {
            // `show <path>` prints the text itself; everything else prints the JSON body.
            match body.get("content").and_then(|c| c.as_str()) {
                Some(content) => {
                    let _ = write!(out, "{content}");
                    if let Some(next) = body.get("next_offset") {
                        let _ = writeln!(err, "\n[truncated; continue with --offset {next}]");
                    }
                }
                None => {
                    let _ = writeln!(out, "{body}");
                }
            }
            SUCCESS
        }
        Err(message) => {
            let _ = writeln!(err, "{message}");
            RUNTIME_ERROR
        }
    }
}

#[cfg(not(feature = "wallet"))]
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if crate::cli::is_help_request(args) {
        usage(out);
        return SUCCESS;
    }
    if parse(args).is_err() {
        usage(err);
        return USAGE_ERROR;
    }
    let _ = writeln!(err, "maxplayer delivery requires the wallet feature");
    USAGE_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_show_and_extract() {
        let show = parse(&args(&[
            "show", "j", "src/a.rs", "--offset", "5", "--turn", "2",
        ]))
        .unwrap();
        assert_eq!(
            show.action,
            Action::Show {
                path: Some("src/a.rs".into()),
                offset: Some(5),
                max_bytes: None
            }
        );
        assert_eq!(show.turn, Some(2));
        let extract = parse(&args(&["extract", "j", "--to", "/tmp/x"])).unwrap();
        assert_eq!(
            extract.action,
            Action::Extract {
                to: Some("/tmp/x".into())
            }
        );
        for bad in [
            &["extract", "j", "extra"][..],
            &["show"],
            &["unpack", "j"],
            &["extract", "j", "--offset", "1"],
            &["show", "j", "--to", "/x"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
