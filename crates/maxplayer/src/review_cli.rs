//! Explicit local recovery and relay-owner service entrypoints; no wallet/payment actions.
use std::io::Write;
const USAGE: &str = "Usage:\n  maxplayer review status <subject-id> [--home <path>]\n  maxplayer review retry <offer-id> [--home <path>]\n  maxplayer review serve <config.json>\nBuyer retries: repeat the same collect or accept operation. Seller retries are picked up by the running daemon; no restart is needed.\n";
#[cfg(feature = "wallet")]
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    use maxplayer_core::{home, review};
    if crate::cli::is_help_request(args) {
        let _ = write!(out, "{USAGE}");
        return 0;
    }
    let result = (|| -> Result<(), String> {
        match args {
            [action, config] if action == "serve" => {
                let config: review::service::ServiceConfig = serde_json::from_slice(
                    &std::fs::read(config).map_err(|_| "cannot read reviewer config")?,
                )
                .map_err(|_| "invalid reviewer config")?;
                let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
                runtime.block_on(review::service::run(config))
            }
            [action, id, rest @ ..] if matches!(action.as_str(), "status" | "retry") => {
                let root = match rest {
                    [] => home::default_home_dir().map_err(|e| e.to_string())?,
                    [flag, path] if flag == "--home" => path.into(),
                    _ => return Err(USAGE.into()),
                };
                if action == "status" {
                    let status = review::state::read(&root, id)?;
                    writeln!(
                        out,
                        "{}",
                        serde_json::to_string_pretty(&status).map_err(|e| e.to_string())?
                    )
                    .map_err(|e| e.to_string())?;
                } else {
                    review::state::retry(&root, id)?;
                    writeln!(out,"Review retry queued for {id}. The seller daemon must be running and the offer must still be eligible.").map_err(|e|e.to_string())?;
                }
                Ok(())
            }
            _ => Err(USAGE.into()),
        }
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            2
        }
    }
}
#[cfg(not(feature = "wallet"))]
pub fn run(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "review requires the wallet feature\n{USAGE}");
    2
}

#[cfg(all(test, feature = "wallet"))]
mod tests {
    use super::*;
    #[test]
    fn review_help_is_available_without_setup() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(run(&["--help".into()], &mut out, &mut err), 0);
        assert!(String::from_utf8(out).unwrap().contains("review retry"));
        assert!(err.is_empty());
    }
    #[test]
    fn review_status_and_retry_commands_operate_on_the_selected_home() {
        use maxplayer_core::review::{self, Subject};
        let root = std::env::temp_dir().join(format!(
            "review-cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let subject = Subject {
            offer: "a".repeat(64),
            event: "a".repeat(64),
            kind: 3401,
            commit: None,
        };
        review::state::write(&root, &subject, "error", "review timed out").unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut args = vec![
            "status".into(),
            subject.event.clone(),
            "--home".into(),
            root.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args, &mut out, &mut err), 0);
        assert!(
            String::from_utf8(out.clone())
                .unwrap()
                .contains("review timed out")
        );
        args[0] = "retry".into();
        assert_eq!(run(&args, &mut out, &mut err), 0);
        assert!(review::state::take_retry(&root, &subject.event));
        assert!(err.is_empty());
    }
}
