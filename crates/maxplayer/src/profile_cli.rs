//! `maxplayer profile set` — set the optional buyer/seller kind-0 identity (name/about) and
//! publish it to the configured relay. Never required; absent profile leaves the identity as hex.
//! Never echoes the secret key.

use std::io::Write;
#[cfg(feature = "wallet")]
use std::path::Path;
use std::path::PathBuf;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

struct Opts {
    name: Option<String>,
    about: Option<String>,
    home: Option<PathBuf>,
    dry_run: bool,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut name = None;
    let mut about = None;
    let mut home = None;
    let mut dry_run = false;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--name" => {
                idx += 1;
                name = Some(args.get(idx).ok_or("--name requires a value")?.clone());
            }
            "--about" => {
                idx += 1;
                about = Some(args.get(idx).ok_or("--about requires a value")?.clone());
            }
            "--home" => {
                idx += 1;
                home = Some(PathBuf::from(args.get(idx).ok_or("--home requires a value")?));
            }
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown argument {other}")),
        }
        idx += 1;
    }
    Ok(Opts { name, about, home, dry_run })
}

fn usage(err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "Usage:\n\
         \x20 maxplayer profile set [--name <name>] [--about <about>] [--home <path>] [--dry-run]\n\
         \n\
         Publishes/replaces the kind-0 metadata event on the configured relay. Called with no\n\
         name/about = re-publish from existing config. --dry-run prints the resolved home and\n\
         public key without publishing. Never echoes the secret key.\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

#[cfg(feature = "wallet")]
fn require_initialized(root: &Path) -> Result<(), String> {
    if maxplayer_core::home::is_initialized(root) {
        return Ok(());
    }

    // #655 review: the recommended commands must stay REAL dispatch arms. `sell` is retired
    // (cli.rs dispatches `Some("seller")`; retired_sell_subcommand_is_rejected_no_alias pins it)
    // and doctor's parser accepts `--home` only — no `--fix`. A test below asserts both, so this
    // remediation text cannot drift back to commands the operator cannot run.
    Err(format!(
        "profile set refused: no initialized maxplayer home at {}\n\
         {} — this looks like a typo'd or new --home path.\n\
         `profile set` never creates a new identity: run `maxplayer seller --home {} ...` \
         (or check the home with `maxplayer doctor --home {}`) to initialize a home first, \
         or check the path is correct.",
        root.display(),
        if root.exists() {
            "the directory exists but has no key file"
        } else {
            "the directory does not exist"
        },
        root.display(),
        root.display(),
    ))
}

/// Entry from `cli::run` for `maxplayer profile ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` (`profile --help` or `profile set --help`) prints usage to STDOUT and
    // exits 0 before any parse, home bootstrap, or relay publish.
    if crate::cli::is_help_request(args) {
        usage(out);
        return SUCCESS;
    }
    match args.first().map(String::as_str) {
        Some("set") => cmd_set(&args[1..], out, err),
        _ => {
            usage(err);
            USAGE_ERROR
        }
    }
}

#[cfg(feature = "wallet")]
fn cmd_set(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::home;
    use maxplayer_core::profile::{self, SetProfileRequest};

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
    if let Err(message) = require_initialized(&root) {
        let _ = writeln!(err, "{message}");
        return RUNTIME_ERROR;
    }
    let mut home = match home::bootstrap(&root) {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    if opts.dry_run {
        let pubkey = match home::public_key_hex(&home) {
            Ok(pubkey) => pubkey,
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return RUNTIME_ERROR;
            }
        };
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({
                "dry_run": true,
                "home": home.root.display().to_string(),
                "pubkey": pubkey,
            })
        );
        return SUCCESS;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "profile runtime: {error}");
            return RUNTIME_ERROR;
        }
    };
    let outcome = match runtime.block_on(profile::set_profile_async(
        &mut home,
        SetProfileRequest {
            name: opts.name,
            about: opts.about,
        },
    )) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let body = serde_json::json!({
        "ok": outcome.ok,
        "pubkey": outcome.pubkey,
        "name": outcome.name,
        "about": outcome.about,
        "event_id": outcome.event_id,
        "relay_url": outcome.relay_url,
    });
    let rendered = body.to_string();
    if let Ok(secret) = home::read_secret_key_hex(&home) {
        if !secret.is_empty() && rendered.contains(&secret) {
            let _ = writeln!(err, "profile set refused: response would echo secret key");
            return RUNTIME_ERROR;
        }
    }
    let _ = writeln!(out, "{rendered}");
    SUCCESS
}

#[cfg(not(feature = "wallet"))]
fn cmd_set(args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if parse(args).is_err() {
        usage(err);
        return USAGE_ERROR;
    }
    let _ = writeln!(err, "maxplayer profile requires the wallet feature");
    USAGE_ERROR
}

#[cfg(all(test, feature = "wallet"))]
mod tests {
    use super::*;
    use maxplayer_core::home;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            let id = NEXT.fetch_add(1, Ordering::SeqCst);
            Self(std::env::temp_dir().join(format!(
                "maxplayer-profile-{label}-{}-{id}",
                std::process::id()
            )))
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            if self.0.exists() {
                fs::remove_dir_all(&self.0).expect("remove temp home");
            }
        }
    }

    fn args(root: &Path, dry_run: bool) -> Vec<String> {
        let mut args = vec!["--home".to_owned(), root.display().to_string()];
        if dry_run {
            args.push("--dry-run".to_owned());
        }
        args
    }

    #[test]
    fn profile_set_refuses_nonexistent_home_without_creating_it() {
        let temp = TempHome::new("missing");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = cmd_set(&args(&temp.0, false), &mut out, &mut err);

        assert_eq!(result, RUNTIME_ERROR);
        assert!(!temp.0.exists());
        assert!(!temp.0.join("key").exists());
        assert!(String::from_utf8(err).expect("utf8").contains("the directory does not exist"));
        assert!(out.is_empty());
    }

    #[test]
    fn profile_set_refuses_empty_home_without_creating_key() {
        let temp = TempHome::new("empty");
        fs::create_dir(&temp.0).expect("create empty home");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = cmd_set(&args(&temp.0, false), &mut out, &mut err);

        assert_eq!(result, RUNTIME_ERROR);
        assert!(temp.0.exists());
        assert!(!temp.0.join("key").exists());
        assert!(String::from_utf8(err).expect("utf8").contains("has no key file"));
        assert!(out.is_empty());
    }

    #[test]
    fn refusal_recommends_only_commands_that_exist() {
        // #655 review: the remediation text once recommended the retired `maxplayer sell` and a
        // nonexistent `doctor --fix` — the confused operator this refusal protects followed it
        // verbatim into two more errors. The recommended commands must stay real dispatch arms:
        // cli.rs dispatches Some("seller") (retired_sell_subcommand_is_rejected_no_alias pins
        // that `sell` stays rejected), and parse_doctor_args accepts --home only.
        let message = require_initialized(Path::new("/nonexistent/home/for-655"))
            .expect_err("uninitialized home must refuse");
        assert!(
            message.contains("maxplayer seller --home"),
            "must recommend the real seller subcommand: {message}"
        );
        assert!(
            message.contains("maxplayer doctor --home"),
            "must recommend the real doctor form: {message}"
        );
        assert!(
            !message.contains("--fix"),
            "doctor has no --fix flag: {message}"
        );
        assert!(
            !message.contains("maxplayer sell "),
            "`sell` is retired (dispatch arm is `seller`): {message}"
        );
    }

    #[test]
    fn initialized_home_passes_check_without_minting_another_key() {
        let temp = TempHome::new("initialized");
        let first = home::bootstrap(&temp.0).expect("first bootstrap");
        assert!(first.key_created);
        let key_before = fs::read(&first.key_path).expect("read key");

        require_initialized(&temp.0).expect("initialized home accepted");
        let second = home::bootstrap(&temp.0).expect("second bootstrap");

        assert!(!second.key_created);
        assert_eq!(fs::read(&second.key_path).expect("read key"), key_before);
    }

    #[test]
    fn profile_set_dry_run_prints_home_and_pubkey_without_publishing() {
        let temp = TempHome::new("dry-run");
        let initialized = home::bootstrap(&temp.0).expect("bootstrap");
        let expected_pubkey = home::public_key_hex(&initialized).expect("pubkey");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = cmd_set(&args(&temp.0, true), &mut out, &mut err);

        assert_eq!(result, SUCCESS);
        assert!(err.is_empty());
        let body: serde_json::Value = serde_json::from_slice(&out).expect("dry-run json");
        assert_eq!(body["dry_run"], true);
        assert_eq!(body["home"], temp.0.display().to_string());
        assert_eq!(body["pubkey"], expected_pubkey);
    }

    #[test]
    fn profile_set_dry_run_refuses_nonexistent_home_without_creating_it() {
        let temp = TempHome::new("dry-run-missing");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = cmd_set(&args(&temp.0, true), &mut out, &mut err);

        assert_eq!(result, RUNTIME_ERROR);
        assert!(!temp.0.exists());
        assert!(!temp.0.join("key").exists());
        assert!(String::from_utf8(err).expect("utf8").contains("the directory does not exist"));
        assert!(out.is_empty());
    }
}
