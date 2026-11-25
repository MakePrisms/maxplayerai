//! `maxplayer delivery show|extract` — read a settled git delivery from the buyer store, or extract
//! it on purpose into a throwaway folder (spec `docs/specs/settle-without-unpacking.md` §2.3–2.4).
//!
//! Thin adapter over the buyer daemon's `read_delivery` / `extract_delivery` RPCs.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

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
                Some(to) => {
                    let cwd = match std::env::current_dir() {
                        Ok(cwd) => cwd,
                        Err(error) => {
                            let _ = writeln!(err, "current dir: {error}");
                            return RUNTIME_ERROR;
                        }
                    };
                    match resolve_to(&cwd, &to) {
                        Ok(path) => Some(path.display().to_string()),
                        Err(message) => {
                            let _ = writeln!(err, "{message}");
                            return USAGE_ERROR;
                        }
                    }
                }
                None => None,
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
                    let _ = write!(out, "{}", printable(content));
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

/// The `--to` folder as an absolute path without `.` or `..` (#1311 fable r2 N3). The daemon
/// refuses any `..` component, so `--to ../out` is resolved here: each existing prefix is
/// canonicalised, so a `..` steps to its real parent. A `..` after a component that does not
/// exist (or is a dangling symlink) cannot be resolved and is refused. The daemon still runs every
/// check on the result (outside the maxplayer home, new or empty, no symlinked ancestor).
fn resolve_to(cwd: &Path, to: &Path) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    let mut exists = true;
    for component in cwd.join(to).components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !exists {
                    return Err(format!(
                        "--to {}: `..` follows {}, which does not exist",
                        to.display(),
                        out.display()
                    ));
                }
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if exists {
                    match std::fs::canonicalize(&out) {
                        Ok(real) => out = real,
                        Err(_) => exists = false,
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Seller text with control characters escaped (`\x1b` → `\u{1b}`), so a delivered file cannot
/// drive the terminal it is printed to (#1311 fable L7). Newlines and tabs are kept.
fn printable(content: &str) -> String {
    content
        .chars()
        .map(|c| match c {
            '\n' | '\t' => c.to_string(),
            c if c.is_control() => c.escape_unicode().to_string(),
            c => c.to_string(),
        })
        .collect()
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
    fn printable_escapes_terminal_control_but_keeps_lines() {
        assert_eq!(printable("a\tb\nc"), "a\tb\nc");
        assert_eq!(printable("x\u{1b}[2Jy\r"), "x\\u{1b}[2Jy\\u{d}");
    }

    #[test]
    fn to_resolves_dot_dot_through_existing_folders_only() {
        let root = tempfile::tempdir().unwrap();
        let root_real = std::fs::canonicalize(root.path()).unwrap();
        let cwd = root_real.join("work");
        std::fs::create_dir(&cwd).unwrap();
        // The N3 case: a relative `..` the daemon would refuse is resolved to a plain path.
        assert_eq!(
            resolve_to(&cwd, Path::new("../out")).unwrap(),
            root_real.join("out")
        );
        assert_eq!(
            resolve_to(&cwd, Path::new("./out")).unwrap(),
            cwd.join("out")
        );
        assert_eq!(
            resolve_to(&cwd, Path::new("out/new")).unwrap(),
            cwd.join("out/new")
        );
        let absolute = root_real.join("work/../abs");
        assert_eq!(resolve_to(&cwd, &absolute).unwrap(), root_real.join("abs"));
        for path in [
            resolve_to(&cwd, Path::new("../out")).unwrap(),
            resolve_to(&cwd, &absolute).unwrap(),
        ] {
            assert!(
                !path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
                "{path:?}"
            );
        }
        // Nothing to resolve `..` against: refused here, not guessed.
        let missing = resolve_to(&cwd, Path::new("missing/../out")).unwrap_err();
        assert!(missing.contains("does not exist"), "{missing}");
        #[cfg(unix)]
        {
            // A symlinked folder resolves to its target, so the daemon checks the real path.
            let real = root_real.join("real");
            std::fs::create_dir(&real).unwrap();
            std::os::unix::fs::symlink(&real, cwd.join("link")).unwrap();
            assert_eq!(
                resolve_to(&cwd, Path::new("link/out")).unwrap(),
                real.join("out")
            );
            // A dangling symlink is not a folder to step out of.
            std::os::unix::fs::symlink(root_real.join("gone"), cwd.join("dangling")).unwrap();
            assert!(resolve_to(&cwd, Path::new("dangling/../out")).is_err());
        }
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
