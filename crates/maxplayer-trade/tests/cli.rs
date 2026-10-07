use std::process::Command;
fn home() -> tempfile::TempDir {
    tempfile::tempdir_in(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target")).unwrap()
}
#[test]
fn fresh_home_balance_is_zero_and_private() {
    use std::os::unix::fs::PermissionsExt;
    let h = home();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "balance",
            "https://testnut.cashu.space/",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let balance: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(balance["balance"], 0);
    assert_eq!(balance["mint"], "https://testnut.cashu.space");
    assert_eq!(
        std::fs::metadata(h.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(h.path().join("wallet.seed"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let second = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "balance",
            "https://testnut.cashu.space",
        ])
        .output()
        .unwrap();
    assert!(second.status.success());
    assert_eq!(out.stdout, second.stdout);
}
#[test]
fn cli_fence_refuses_real_mint_before_wallet_initialization() {
    let h = home();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "fund",
            "https://mint.minibits.cash/Bitcoin",
            "--amount",
            "1",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("mint fence"));
    assert!(!h.path().join("wallet.seed").exists());
}
#[test]
fn concurrent_home_is_refused() {
    use fs2::FileExt;
    let h = home();
    let f = std::fs::File::create(h.path().join("owner.lock")).unwrap();
    f.lock_exclusive().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "balance",
            "https://testnut.cashu.space",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("home is already in use"));
}
#[test]
fn no_real_money_override_exists() {
    let h = home();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "--allow-real-mint",
            "balance",
            "https://mint.minibits.cash/Bitcoin",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unexpected argument"));
    assert!(!h.path().join("wallet.seed").exists());
}
