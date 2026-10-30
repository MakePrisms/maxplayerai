//! `maxplayer account …` driven as the shipped binary against a fake registration service that
//! applies the real service's NIP-98 rules and error codes (`tests/support/fake_registration.rs`).
//! Nothing here reaches the network: the service is on 127.0.0.1 and no email is sent.
#![cfg(feature = "wallet")]

#[path = "support/fake_registration.rs"]
mod fake_registration;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use fake_registration::FakeRegistration;
use nostr_sdk::nostr::hashes::Hash as _;
use nostr_sdk::nostr::hashes::sha256;
use serde_json::Value;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Home {
    root: PathBuf,
    pubkey: String,
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The binary with no inherited `MAXPLAYER_*` variables: the config layer refuses unknown names,
/// and a developer's shell must not change what these tests prove.
fn maxplayer() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MAXPLAYER_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn base_command(home: &Path, registration_url: &str) -> Command {
    let mut cmd = maxplayer();
    cmd.env("MAXPLAYER_HOME", home)
        .env("MAXPLAYER_REGISTRATION_URL", registration_url)
        .stdin(Stdio::null());
    cmd
}

fn new_home() -> Home {
    let root = std::env::temp_dir().join(format!(
        "maxplayer-account-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let out = maxplayer()
        .args(["whoami", "--home", root.to_str().unwrap()])
        .output()
        .expect("whoami");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let pubkey = stdout
        .lines()
        .find_map(|l| l.strip_prefix("pubkey: "))
        .expect("pubkey line")
        .trim()
        .to_owned();
    Home { root, pubkey }
}

fn run(home: &Home, url: &str, args: &[&str]) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = base_command(&home.root, url)
        .arg("account")
        .args(args)
        .output()
        .expect("run maxplayer account");
    (
        status.code().unwrap_or(-1),
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

fn request_code(fake: &FakeRegistration, home: &Home) {
    let (code, out, err) = run(
        home,
        &fake.base_url(),
        &["link", "--email", "You@Example.com", "--label", "my-laptop"],
    );
    assert_eq!(code, 0, "{out}\n{err}");
}

#[test]
fn two_step_link_then_status_and_marketing() {
    let fake = FakeRegistration::start();
    let home = new_home();
    fake.set_next_code("482913");

    let (code, out, err) = run(
        &home,
        &fake.base_url(),
        &["link", "--email", "you@example.com", "--label", "my-laptop"],
    );
    assert_eq!(code, 0, "{out}\n{err}");
    // Spec §3 steps 1, 2 and 6: privacy link, terms notice, marketing notice with the off switch.
    assert!(
        out.contains("Privacy Policy: https://www.maxplayer.ai/privacy/20261029"),
        "{out}"
    );
    assert!(out.contains("maxplayer account marketing off"), "{out}");
    assert!(
        out.contains(
            "Entering it accepts the Maxplayer Terms of Service, effective 20261029: \
             https://www.maxplayer.ai/terms/20261029"
        ),
        "{out}"
    );
    assert!(
        out.contains("maxplayer account link --code <6 digits>"),
        "{out}"
    );
    assert!(
        fake.registration(&home.pubkey).is_none(),
        "nothing linked before the code"
    );
    let pending = fake.pending(&home.pubkey).expect("code requested");
    assert_eq!(pending.email, "you@example.com");
    assert_eq!(pending.label, "my-laptop");

    let (code, out, err) = run(
        &home,
        &fake.base_url(),
        &["link", "--code", "482 913", "--entered-by", "human"],
    );
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("Done, linked to you@example.com"), "{out}");
    assert!(
        out.contains("The relay now accepts writes from this key."),
        "{out}"
    );
    assert!(out.contains("Product news: on."), "{out}");
    let reg = fake.registration(&home.pubkey).expect("linked");
    assert_eq!(reg.entered_by, "human");
    assert_eq!(
        (
            reg.terms_version.as_str(),
            reg.privacy_version.as_str(),
            reg.notice_version.as_str()
        ),
        ("20261029", "20261029", "v1")
    );

    let (code, out, _) = run(&home, &fake.base_url(), &["status"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("Linked:") && out.contains("you@example.com"),
        "{out}"
    );
    assert!(out.contains("member (writes accepted)"), "{out}");
    let (code, out, _) = run(&home, &fake.base_url(), &["status", "--json"]);
    assert_eq!(code, 0);
    let json: Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(json["registered"], true);
    assert_eq!(json["pubkey"], home.pubkey.as_str());
    assert_eq!(json["code_entered_by"], "human");

    let (code, out, _) = run(&home, &fake.base_url(), &["marketing", "off"]);
    assert_eq!(code, 0);
    assert!(out.contains("Product news: off."), "{out}");
    assert!(out.contains("Account emails"), "{out}");
    assert_eq!(fake.marketing("you@example.com"), Some(false));
    let (code, out, _) = run(&home, &fake.base_url(), &["marketing", "on"]);
    assert_eq!(code, 0);
    assert!(out.contains("Product news: on."), "{out}");
    assert_eq!(fake.marketing("you@example.com"), Some(true));
}

/// Every request is NIP-98 signed by the home key over `<registration_url><path>` (the PUBLIC URL),
/// with the body's SHA-256 as the payload tag, and a fresh event each time.
#[test]
fn requests_are_signed_over_the_public_url_with_the_payload_hash() {
    let fake = FakeRegistration::start();
    let home = new_home();
    request_code(&fake, &home);
    run(&home, &fake.base_url(), &["link", "--code", "123456"]);
    run(&home, &fake.base_url(), &["status"]);
    run(&home, &fake.base_url(), &["status"]);

    let requests = fake.requests();
    assert_eq!(requests.len(), 4, "{requests:?}");
    let mut ids = std::collections::HashSet::new();
    for r in &requests {
        assert_eq!(r.error, None, "{r:?}");
        let auth = r.auth.as_ref().expect("signed");
        assert_eq!(auth.pubkey.to_hex(), home.pubkey);
        let tag = |name: &str| -> Vec<String> {
            auth.tags
                .iter()
                .filter(|t| t.kind().as_str() == name)
                .filter_map(|t| t.content().map(str::to_owned))
                .collect()
        };
        let route = r.path.strip_prefix("/account").unwrap();
        assert_eq!(tag("u"), [format!("{}{route}", fake.base_url())]);
        assert_eq!(tag("method"), std::slice::from_ref(&r.method));
        if r.body.is_empty() {
            assert!(tag("payload").is_empty());
        } else {
            assert_eq!(tag("payload"), [sha256::Hash::hash(&r.body).to_string()]);
        }
        assert!(ids.insert(auth.id), "each request signs a new event");
    }
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["terms_version"], "20261029");
    assert_eq!(body["privacy_version"], "20261029");
    assert_eq!(body["notice_version"], "v1");
    let confirm: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        confirm["entered_by"], "agent",
        "--code without --entered-by reports agent"
    );
}

/// A service whose public URL differs from the configured one refuses the signature, and the CLI
/// points at the setting.
#[test]
fn a_signature_over_the_wrong_url_is_refused_with_a_plain_message() {
    let fake = FakeRegistration::start();
    fake.set_public_base("https://relay.maxplayer.ai/account");
    let home = new_home();
    let (code, _, err) = run(&home, &fake.base_url(), &["status"]);
    assert_eq!(code, 2);
    assert!(err.contains("registration_url"), "{err}");
    assert_eq!(fake.requests()[0].error.as_deref(), Some("url_mismatch"));
}

#[test]
fn wrong_code_says_how_many_tries_are_left() {
    let fake = FakeRegistration::start();
    let home = new_home();
    request_code(&fake, &home);
    let (code, _, err) = run(&home, &fake.base_url(), &["link", "--code", "000000"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("That code is not right. 4 tries left."),
        "{err}"
    );
    // A malformed code never reaches the service, so it costs no try.
    let (code, _, err) = run(&home, &fake.base_url(), &["link", "--code", "12"]);
    assert_eq!(code, 2);
    assert!(err.contains("6 digits"), "{err}");
    assert_eq!(fake.confirm_calls(), 1);
    assert_eq!(fake.pending(&home.pubkey).unwrap().attempts, 1);
}

#[test]
fn an_expired_code_says_to_ask_again() {
    let fake = FakeRegistration::start();
    let home = new_home();
    request_code(&fake, &home);
    fake.expire_codes();
    let (code, _, err) = run(&home, &fake.base_url(), &["link", "--code", "123456"]);
    assert_eq!(code, 2);
    assert!(err.contains("That code has expired"), "{err}");
    assert!(err.contains("maxplayer account link"), "{err}");
    assert!(fake.registration(&home.pubkey).is_none());
}

#[test]
fn no_waiting_code_says_to_request_one() {
    let fake = FakeRegistration::start();
    let home = new_home();
    let (code, _, err) = run(&home, &fake.base_url(), &["link", "--code", "123456"]);
    assert_eq!(code, 2);
    assert!(err.contains("No code is waiting"), "{err}");
}

#[test]
fn a_key_linked_to_another_email_gets_the_409_message() {
    let fake = FakeRegistration::start();
    let home = new_home();
    fake.link_existing(&home.pubkey, "first@example.com");
    let (code, _, err) = run(
        &home,
        &fake.base_url(),
        &["link", "--email", "second@example.com", "--label", "x"],
    );
    assert_eq!(code, 2);
    assert!(err.contains("already linked to a different email"), "{err}");
    assert!(
        !err.contains("first@example.com"),
        "never echoes the other email: {err}"
    );
    assert!(fake.pending(&home.pubkey).is_none());
    assert_eq!(
        fake.requests()[0].error.as_deref(),
        Some("key_linked_to_other_email")
    );
}

#[test]
fn resend_cooldown_and_rate_limit_say_when_to_try_again() {
    let fake = FakeRegistration::start();
    let home = new_home();
    request_code(&fake, &home);
    let (code, _, err) = run(
        &home,
        &fake.base_url(),
        &["link", "--email", "you@example.com", "--label", "my-laptop"],
    );
    assert_eq!(code, 2);
    assert!(
        err.contains("A code was sent to this key moments ago"),
        "{err}"
    );
    // It names the command that enters the code already sent.
    assert!(
        err.contains("`maxplayer account link --code <6 digits>`"),
        "{err}"
    );
    assert!(err.contains("seconds"), "{err}");

    fake.set_cooldown_secs(0);
    fake.set_send_limit(1);
    let (code, _, err) = run(
        &home,
        &fake.base_url(),
        &["link", "--email", "you@example.com", "--label", "my-laptop"],
    );
    assert_eq!(code, 2);
    assert!(
        err.contains("Too many requests for now. Try again in about an hour."),
        "{err}"
    );
}

#[test]
fn an_unreachable_service_is_a_plain_network_error() {
    let home = new_home();
    // Bind and drop: a loopback port with nothing listening.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/account");
    let (code, _, err) = run(&home, &url, &["status"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("Could not reach the registration service"),
        "{err}"
    );
    assert!(err.contains(&url), "{err}");
}

#[test]
fn without_a_terminal_link_needs_email_and_label() {
    let fake = FakeRegistration::start();
    let home = new_home();
    let (code, _, err) = run(&home, &fake.base_url(), &["link"]);
    assert_eq!(code, 1);
    assert!(err.contains("--email and --label"), "{err}");
    assert!(fake.requests().is_empty(), "nothing is sent");
}

#[test]
fn not_linked_status_and_marketing_point_at_link() {
    let fake = FakeRegistration::start();
    let home = new_home();
    let (code, out, _) = run(&home, &fake.base_url(), &["status"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("Not linked") && out.contains("maxplayer account link"),
        "{out}"
    );
    let (code, _, err) = run(&home, &fake.base_url(), &["marketing", "off"]);
    assert_eq!(code, 2);
    assert!(err.contains("not linked to an email yet"), "{err}");
}

#[test]
fn an_explicit_home_without_a_key_is_refused_before_any_request() {
    let fake = FakeRegistration::start();
    let missing =
        std::env::temp_dir().join(format!("maxplayer-account-missing-{}", std::process::id()));
    let out = maxplayer()
        .args(["account", "status", "--home", missing.to_str().unwrap()])
        .env("MAXPLAYER_REGISTRATION_URL", fake.base_url())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!missing.exists(), "no new identity is created");
    assert!(fake.requests().is_empty());
}

#[test]
fn mint_flag_links_the_mint_servers_own_key() {
    let fake = FakeRegistration::start();
    let home = new_home();
    let mint_keys = nostr_sdk::prelude::Keys::generate();
    let dir = home.root.join("mint");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("nostr.key"),
        mint_keys.secret_key().to_secret_hex(),
    )
    .unwrap();
    let (code, out, err) = run(
        &home,
        &fake.base_url(),
        &[
            "link",
            "--mint",
            "--email",
            "ops@example.com",
            "--label",
            "mint-1",
        ],
    );
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("this mint's key"), "{out}");
    assert!(fake.pending(&mint_keys.public_key().to_hex()).is_some());
    assert!(fake.pending(&home.pubkey).is_none());
}

#[test]
fn top_level_help_lists_the_account_command() {
    let out = maxplayer().arg("--help").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("maxplayer account <link|status|marketing on|off>"),
        "{text}"
    );
    let out = maxplayer().args(["account", "--help"]).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    for needle in [
        "account link [--email <email>] [--label <name>]",
        "account link --code <6 digits>",
        "account status [--json]",
        "account marketing on|off",
        "MAXPLAYER_REGISTRATION_URL",
    ] {
        assert!(text.contains(needle), "{needle}\n{text}");
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Review M1: a home that holds only a mint server (`maxplayer-mint init` creates `<home>/mint/`
/// and nothing else). `--mint` links and reports that key, creates no agent key, `config.toml` or
/// `wallet/`, and every "run link" hint keeps `--mint --home <h>`. Both ways of naming the home.
#[test]
fn mint_only_home_links_the_mint_key_and_creates_nothing_else() {
    use std::os::unix::fs::PermissionsExt;
    let fake = FakeRegistration::start();
    let root = std::env::temp_dir().join(format!(
        "maxplayer-account-mint-only-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let mint_dir = root.join("mint");
    std::fs::create_dir_all(&mint_dir).unwrap();
    let mint_keys = nostr_sdk::prelude::Keys::generate();
    let key_path = mint_dir.join("nostr.key");
    std::fs::write(&key_path, mint_keys.secret_key().to_secret_hex()).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mint_hex = mint_keys.public_key().to_hex();
    let home_arg = root.to_str().unwrap().to_owned();
    let flags = format!("--mint --home {home_arg}");
    let run_mint = |args: &[&str], via_env: bool| {
        let mut cmd = maxplayer();
        cmd.env("MAXPLAYER_REGISTRATION_URL", fake.base_url())
            .stdin(Stdio::null())
            .arg("account")
            .args(args)
            .arg("--mint");
        if via_env {
            cmd.env("MAXPLAYER_HOME", &root);
        } else {
            cmd.args(["--home", &home_arg]);
        }
        let out = cmd.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    };

    // Not linked yet: status names this key and the exact command, through --home ...
    let (code, out, err) = run_mint(&["status"], false);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("this mint's key"), "{out}");
    assert!(
        out.contains(&format!("`maxplayer account link {flags}`")),
        "{out}"
    );
    assert!(!err.contains("Created a new agent key"), "{err}");
    // ... and through MAXPLAYER_HOME (it used to bootstrap an agent key there).
    let (code, out, err) = run_mint(&["status"], true);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains("Run `maxplayer account link --mint` to link it."),
        "{out}"
    );
    assert_eq!(names_in(&root), ["mint"], "status created nothing");

    // A service refusal names the flags too.
    let (code, _, err) = run_mint(&["link", "--code", "123456"], false);
    assert_eq!(code, 2);
    assert!(
        err.contains(&format!("`maxplayer account link {flags} --email")),
        "{err}"
    );

    fake.set_next_code("482913");
    let (code, out, err) = run_mint(
        &["link", "--email", "ops@example.com", "--label", "mint-1"],
        false,
    );
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("this mint's key"), "{out}");
    assert!(
        out.contains(&format!("maxplayer account link {flags} --code <6 digits>")),
        "{out}"
    );
    assert!(fake.pending(&mint_hex).is_some());
    let (code, out, err) = run_mint(&["link", "--code", "482913"], true);
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(
        fake.registration(&mint_hex).unwrap().email,
        "ops@example.com"
    );
    let (code, out, err) = run_mint(&["status", "--json"], false);
    assert_eq!(code, 0, "{out}\n{err}");
    let status: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(status["registered"], true);
    assert_eq!(status["pubkey"], mint_hex);

    assert_eq!(
        names_in(&root),
        ["mint"],
        "no key, config.toml or wallet/ was created"
    );
    assert_eq!(names_in(&mint_dir), ["nostr.key"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// `--mint` on a home with no mint key is refused before any request, and creates nothing.
#[test]
fn mint_flag_without_a_mint_key_is_refused_and_creates_nothing() {
    let fake = FakeRegistration::start();
    let root = std::env::temp_dir().join(format!(
        "maxplayer-account-no-mint-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let out = maxplayer()
        .env("MAXPLAYER_HOME", &root)
        .env("MAXPLAYER_REGISTRATION_URL", fake.base_url())
        .args(["account", "status", "--mint"])
        .output()
        .unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("no mint key at"), "{err}");
    assert!(names_in(&root).is_empty(), "nothing created");
    assert!(fake.requests().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// Review L6: `status --json` failures are JSON on stdout (`{"error", "message"}`), not stderr text.
#[test]
fn status_json_errors_are_json_on_stdout() {
    let home = new_home();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/account");
    let (code, out, err) = run(&home, &url, &["status", "--json"]);
    assert_eq!(code, 2, "{err}");
    let value: Value = serde_json::from_str(out.trim()).expect("stdout is JSON");
    assert_eq!(value["error"], "network");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("Could not reach the registration service"),
        "{value}"
    );

    let fake = FakeRegistration::start();
    fake.set_public_base("https://relay.maxplayer.ai/account");
    let (code, out, _) = run(&home, &fake.base_url(), &["status", "--json"]);
    assert_eq!(code, 2);
    let value: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(value["error"], "url_mismatch");

    let missing = std::env::temp_dir().join(format!(
        "maxplayer-account-json-missing-{}",
        std::process::id()
    ));
    let out = maxplayer()
        .args([
            "account",
            "status",
            "--json",
            "--home",
            missing.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let value: Value = serde_json::from_str(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    assert_eq!(value["error"], "home");
    assert!(!missing.exists());
}
