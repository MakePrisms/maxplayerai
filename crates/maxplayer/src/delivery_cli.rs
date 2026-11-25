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
                Some(to) => match resolve_to(&to, std::env::current_dir) {
                    Ok(path) => Some(path),
                    Err(message) => {
                        let _ = writeln!(err, "{message}");
                        return USAGE_ERROR;
                    }
                },
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

/// The `--to` folder as an absolute path without `.` or `..`, as the daemon requires (#1311 fable
/// r2 N3; codex r3). Every folder the path goes THROUGH (a component followed by more) must be an
/// existing directory, and is canonicalised, so a `..` steps to its real parent and `file/..` is
/// refused. The final component is kept as written: a symlink there reaches the daemon, which
/// refuses it. After a missing component, later names are appended and `..` is refused. The
/// daemon still runs every check on the result (outside the maxplayer home, new or empty, final
/// component not a symlink). `cwd` is only consulted for a relative `to`; its components are
/// walked separately so a Windows verbatim base cannot swallow a `..` of `to`.
fn resolve_to(to: &Path, cwd: impl FnOnce() -> std::io::Result<PathBuf>) -> Result<String, String> {
    let shown = to.display();
    let base = if to.is_absolute() {
        PathBuf::new()
    } else {
        cwd().map_err(|error| format!("--to {shown}: current dir: {error}"))?
    };
    let parts: Vec<Component> = base.components().chain(to.components()).collect();
    let last_name = parts
        .iter()
        .rposition(|c| matches!(c, Component::Normal(_) | Component::ParentDir));
    let mut out = PathBuf::new();
    let mut exists = true;
    for (index, component) in parts.iter().enumerate() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !exists {
                    return Err(format!(
                        "--to {shown}: `..` follows {}, which does not exist",
                        out.display()
                    ));
                }
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if !exists || Some(index) == last_name {
                    continue;
                }
                match std::fs::canonicalize(&out) {
                    Ok(real) if real.is_dir() => out = real,
                    Ok(_) => {
                        return Err(format!("--to {shown}: {} is not a folder", out.display()));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // A missing folder (or a dangling symlink): the daemon creates the rest.
                        exists = false;
                    }
                    Err(error) => return Err(format!("--to {shown}: {}: {error}", out.display())),
                }
            }
        }
    }
    out.into_os_string().into_string().map_err(|path| {
        format!(
            "--to {shown}: {} is not valid UTF-8",
            path.to_string_lossy()
        )
    })
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
        let at = |to: &str| resolve_to(Path::new(to), || Ok(cwd.clone()));
        let s = |p: PathBuf| p.into_os_string().into_string().unwrap();
        // The N3 case: a relative `..` the daemon would refuse is resolved to a plain path.
        assert_eq!(at("../out").unwrap(), s(root_real.join("out")));
        assert_eq!(at("./out").unwrap(), s(cwd.join("out")));
        assert_eq!(at("out/new").unwrap(), s(cwd.join("out/new")));
        assert_eq!(at("out/.").unwrap(), s(cwd.join("out")));
        // An absolute `to` never asks for the current dir.
        let absolute = format!("{}/work/../abs", root_real.display());
        let resolved = resolve_to(Path::new(&absolute), || panic!("cwd not needed")).unwrap();
        assert_eq!(resolved, s(root_real.join("abs")));
        assert!(
            !resolved.contains("/../") && !resolved.contains("/./"),
            "{resolved}"
        );
        // Nothing to resolve `..` against: refused here, not guessed.
        let missing = at("missing/../out").unwrap_err();
        assert!(missing.contains("does not exist"), "{missing}");
        // A file is not a folder to go through (codex r3).
        std::fs::write(cwd.join("file"), b"x").unwrap();
        let file = at("file/../out").unwrap_err();
        assert!(file.contains("is not a folder"), "{file}");
        assert!(at("file/sub").unwrap_err().contains("is not a folder"));
        // A current dir that cannot be read is reported, not mistaken for an absent folder.
        let failing =
            resolve_to(Path::new("out"), || Err(std::io::Error::other("gone"))).unwrap_err();
        assert!(failing.contains("current dir: gone"), "{failing}");
        #[cfg(unix)]
        {
            let real = root_real.join("real");
            std::fs::create_dir(&real).unwrap();
            std::os::unix::fs::symlink(&real, cwd.join("link")).unwrap();
            // A symlinked folder on the way resolves to its target, so the daemon checks the real path.
            assert_eq!(at("link/out").unwrap(), s(real.join("out")));
            assert_eq!(at("link/../out").unwrap(), s(root_real.join("out")));
            // A symlink as the FINAL component is sent as written, so the daemon's refusal of a
            // symlinked destination still applies (codex r3 Medium).
            assert_eq!(at("link").unwrap(), s(cwd.join("link")));
            assert_eq!(at("./link/.").unwrap(), s(cwd.join("link")));
            // A dangling symlink is not a folder to step out of.
            std::os::unix::fs::symlink(root_real.join("gone"), cwd.join("dangling")).unwrap();
            assert!(at("dangling/../out").is_err());
            assert_eq!(at("dangling").unwrap(), s(cwd.join("dangling")));
            // A path that is not UTF-8 cannot be sent over JSON: refused, never replaced.
            use std::os::unix::ffi::OsStrExt;
            let odd = std::ffi::OsStr::from_bytes(b"bad\xff");
            std::fs::create_dir(cwd.join(odd)).unwrap();
            let non_utf8 = resolve_to(&Path::new(odd).join("out"), || Ok(cwd.clone())).unwrap_err();
            assert!(non_utf8.contains("not valid UTF-8"), "{non_utf8}");
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
