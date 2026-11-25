//! `maxplayer-registration serve` logs `maxplayer-registration <version> — registration starting,
//! config <path>` as its FIRST log line (tracing, the service's log stream), before the config is
//! read, so even a refused start names its build. A missing config refuses right after the line, so
//! no database is needed.

use std::process::Command;

#[test]
fn serve_logs_its_version_as_the_first_line() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("missing.toml");
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-registration"))
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .env_remove("MAXPLAYER_REGISTRATION_CONFIG")
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .output()
        .expect("run maxplayer-registration");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stdout}\n{stderr}");
    let first = stdout.lines().next().unwrap_or_default();
    assert!(
        first.ends_with(&format!(
            "maxplayer-registration {} — registration starting, config {}",
            env!("CARGO_PKG_VERSION"),
            config.display()
        )) && first.contains("INFO"),
        "first log line is not the start line:\n{stdout}\n{stderr}"
    );
}

#[test]
fn migrate_logs_no_start_line() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-registration"))
        .arg("migrate")
        .arg("--config")
        .arg(dir.path().join("missing.toml"))
        .env_remove("MAXPLAYER_REGISTRATION_CONFIG")
        .env("RUST_LOG", "info")
        .output()
        .expect("run maxplayer-registration");
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("starting"));
}
