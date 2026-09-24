//! Relay-operator reviewer service entrypoint, separate from client review recovery.
use std::io::Write;
const USAGE: &str = "Usage:\n  maxplayer reviewer serve <config.json>\n";

#[cfg(feature = "wallet")]
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if crate::cli::is_help_request(args) {
        let _ = write!(out, "{USAGE}");
        return 0;
    }
    let result = (|| -> Result<(), String> {
        let [action, config] = args else {
            return Err(USAGE.into());
        };
        if action != "serve" {
            return Err(USAGE.into());
        }
        let config: maxplayer_core::reviewer::ServiceConfig = serde_json::from_slice(
            &std::fs::read(config).map_err(|_| "cannot read reviewer config")?,
        )
        .map_err(|_| "invalid reviewer config")?;
        let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        runtime.block_on(maxplayer_core::reviewer::run(config))
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
    let _ = writeln!(err, "reviewer requires the wallet feature\n{USAGE}");
    2
}

#[cfg(all(test, feature = "wallet"))]
mod tests {
    #[test]
    fn reviewer_cli_dispatch_is_separate_from_client_review_commands() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args: Vec<String> = vec!["maxplayer".into(), "reviewer".into(), "--help".into()];
        assert_eq!(crate::cli::run(&args, &mut out, &mut err), 0);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("maxplayer reviewer serve")
        );
        assert!(err.is_empty());
        let mut out = Vec::new();
        let args: Vec<String> = vec![
            "maxplayer".into(),
            "review".into(),
            "serve".into(),
            "unused.json".into(),
        ];
        assert_ne!(crate::cli::run(&args, &mut out, &mut err), 0);
        assert!(String::from_utf8(err).unwrap().contains("review status"));
    }
}
