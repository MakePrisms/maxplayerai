//! `doctor --home` against a relay that admits only relay members (relay.maxplayer.ai since
//! #1223). Its own binary: it serves a real credits sidecar and runs the CLI for ~10 s, which
//! starved the timing-sensitive open-relay inbox test when they shared `auth_gated`.

#[path = "../../maxplayer-core/tests/support/auth_gated_relay.rs"]
mod auth_gated_relay;

use auth_gated_relay::{AuthRelay, AuthRelayOptions};
use nostr_sdk::prelude::Keys;

/// The real credits sidecar served on `relay` with `keys` (it signs in as that key).
async fn start_mint(
    relay: &str,
    keys: &Keys,
) -> (String, tokio::task::JoinHandle<()>, tempfile::TempDir) {
    use maxplayer_mint::{backend, home::mint_url, server::Server};
    let dir = tempfile::tempdir().unwrap();
    let url = mint_url(keys).unwrap();
    let mut seed = [0u8; 64];
    seed[..32].copy_from_slice(&Keys::generate().secret_key().to_secret_bytes());
    seed[32..].copy_from_slice(&Keys::generate().secret_key().to_secret_bytes());
    let mint = backend::open(&dir.path().join("mint.sqlite"), &seed, &url)
        .await
        .unwrap();
    let server = Server::connect(mint, keys.clone(), &[relay.to_owned()], 100)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    (url, task, dir)
}

fn doctor(home: Option<&std::path::Path>, mint: &str, relay: &str) -> (i32, serde_json::Value) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"));
    if let Some(home) = home {
        cmd.arg("--home").arg(home);
    }
    let out = cmd
        .args([
            "doctor",
            mint,
            "--mint-relay",
            relay,
            "--json",
            "--timeout-seconds",
            "8",
        ])
        .output()
        .unwrap();
    let report = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code().unwrap_or(-1), report)
}

/// relay.maxplayer.ai admits only relay members: `doctor --home` signs as that home's (linked)
/// `trade.key` and reaches the mint there; without `--home` its throwaway key is refused. A home
/// with no `trade.key` falls back to the throwaway key and gets no key created.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn doctor_signs_as_the_homes_trade_key_on_a_members_only_relay() {
    use nostr_sdk::prelude::ToBech32;
    let trader = Keys::generate();
    let mint_keys = Keys::generate();
    let relay = AuthRelay::start(AuthRelayOptions {
        members: Some([trader.public_key(), mint_keys.public_key()]),
        buzz_refusal: true,
        ..Default::default()
    })
    .await;
    let (mint, task, _dir) = start_mint(&relay.url, &mint_keys).await;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("trading");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join("trade.key"), trader.secret_key().to_secret_hex()).unwrap();

    let (code, report) = doctor(Some(&home), &mint, &relay.url);
    assert_eq!(code, 0, "{report}");
    assert_eq!(
        report["observation"]["reachability"], "answered",
        "{report}"
    );
    assert_eq!(report["signer"]["source"], "home trade.key");
    assert_eq!(
        report["signer"]["npub"],
        trader.public_key().to_bech32().unwrap()
    );

    let (code, report) = doctor(None, &mint, &relay.url);
    assert_eq!(code, 2, "{report}");
    // buzz closes the socket after refusing the sign-in, so the label is `auth_failed` or
    // `relay_refused`; either way the refusal is the throwaway key's sign-in.
    assert_ne!(
        report["observation"]["reachability"], "answered",
        "{report}"
    );
    assert_eq!(
        report["observation"]["relays"][&relay.url]["refusal_category"], "auth",
        "{report}"
    );
    assert_eq!(report["signer"]["source"], "ephemeral");

    let empty = root.path().join("no-key-yet");
    std::fs::create_dir(&empty).unwrap();
    let (code, report) = doctor(Some(&empty), &mint, &relay.url);
    assert_eq!(code, 2, "{report}");
    assert_eq!(report["signer"]["source"], "ephemeral");
    assert!(
        !empty.join("trade.key").exists(),
        "doctor never creates a trade key"
    );
    task.abort();
}
