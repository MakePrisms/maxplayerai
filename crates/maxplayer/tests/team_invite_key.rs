//! `MAXPLAYER_TEAM_INVITE_PUBKEY` through the shipped binary: a malformed value, or any value on a
//! home that uses relay.maxplayer.ai, stops `maxplayer team` with an error naming the variable.
#![cfg(feature = "wallet")]

use std::process::Command;

fn team(home: &std::path::Path, key: &str, args: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("MAXPLAYER_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd
        .env("MAXPLAYER_TEAM_INVITE_PUBKEY", key)
        .arg("team")
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_invite_key_override_is_refused_on_production_and_when_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let key = "ab".repeat(32);
    let test = ["--relay", "ws://127.0.0.1:9"];
    // A test deployment (its own relay and registration service) may use it.
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "registration_url = \"http://127.0.0.1:9/account\"\n",
    )
    .unwrap();
    let (code, err) = team(&home, &key, &[&test[..], &["list"]].concat());
    assert_eq!(code, 0, "{err}");
    // Malformed: refused, not ignored.
    let (code, err) = team(&home, &key.to_uppercase(), &[&test[..], &["list"]].concat());
    assert!(
        code == 2 && err.contains("MAXPLAYER_TEAM_INVITE_PUBKEY"),
        "{code} {err}"
    );
    // The production relay (the default) or the production registration service: refused.
    let (code, err) = team(&home, &key, &["list"]);
    assert!(
        code == 2 && err.contains("MAXPLAYER_TEAM_INVITE_PUBKEY"),
        "{code} {err}"
    );
    std::fs::write(home.join("config.toml"), "").unwrap();
    let (code, err) = team(&home, &key, &[&test[..], &["list"]].concat());
    assert!(
        code == 2 && err.contains("MAXPLAYER_TEAM_INVITE_PUBKEY"),
        "{code} {err}"
    );
}
