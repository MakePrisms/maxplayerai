//! Exercise the real startup/config loader, not only TOML deserialization.
#![cfg(feature = "wallet")]
use std::{fs, process::Command};

const SELLER: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

#[test]
fn blocked_sellers_on_disk_config_and_environment_fail_closed() {
    let root =
        std::env::temp_dir().join(format!("maxplayer-blocked-sellers-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let run = |override_value: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
        for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("MAXPLAYER_")) {
            cmd.env_remove(key);
        }
        cmd.env("MAXPLAYER_HOME", &root).arg("whoami");
        if let Some(value) = override_value {
            cmd.env("MAXPLAYER_BUYER__BLOCKED_SELLERS", value);
        }
        cmd.output().unwrap()
    };
    // Fresh default initializes and reloads without a buyer section.
    assert!(run(None).status.success());
    assert!(run(None).status.success());
    for sellers in ["[]".to_owned(), format!("[\"{SELLER}\", \"{SELLER}\"]")] {
        let raw = format!("[buyer]\nblocked_sellers = {sellers}\n");
        fs::write(root.join("config.toml"), &raw).unwrap();
        let output = run(None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read_to_string(root.join("config.toml")).unwrap(), raw);
        assert!(run(Some(SELLER)).status.success());
    }
    for seller in [
        "".into(),
        "seller-name".into(),
        "npub1invalid".into(),
        "nsec1invalid".into(),
        "a".repeat(63),
        "g".repeat(64),
        "0".repeat(64),
        SELLER.to_uppercase(),
        format!(" {SELLER}"),
    ] {
        fs::write(
            root.join("config.toml"),
            format!("[buyer]\nblocked_sellers = [\"{seller}\"]\n"),
        )
        .unwrap();
        let output = run(None);
        assert!(!output.status.success(), "invalid identity was accepted");
        assert!(String::from_utf8_lossy(&output.stderr).contains("buyer.blocked_sellers"));
    }
    fs::write(root.join("config.toml"), "[buyer]\nblocked_sellers = []\n").unwrap();
    let output = run(Some("invalid"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("buyer.blocked_sellers"));
    fs::remove_dir_all(root).unwrap();
}
