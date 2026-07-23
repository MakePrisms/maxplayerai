//! Relay-git auth over real smart HTTP — integration tests (closes the in-process gap).
//!
//! Until now the seller relay-git auth wiring (`seller_git.rs` fetch/push) had only
//! construction-level coverage: we inspected the built `git` Command, but never ran an
//! authenticated transfer end-to-end. These tests drive the REAL contribution fork path
//! (`init_contribution_workdir` → scrubbed `fetch-base` → checkout fork tip) against a
//! local git-over-HTTPS fixture that, like mobee relay-git, refuses upload-pack /
//! receive-pack without an `Authorization` header (NIP-98-style: 401 + challenge).
//!
//! Auth is presented the same way production does it: the scrubbed fetch child gets
//! `credential.helper=git-credential-nostr` wired by `build_scrubbed_command`; here the
//! helper binary is a stub injected through the existing `MOBEE_GIT_CREDENTIAL_NOSTR`
//! override, so the whole 401 → credential fill → authorized retry dance is real git.
#![cfg(unix)]

mod git_http_fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

use git_http_fixture::GitHttpAuthServer;
use mobee_core::seller_git::{
    init_contribution_workdir, DeliveryAgentIdentity, PushAuth, SellerGitError,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "mobee-relay-git-auth-{label}-{}-{id}",
        std::process::id()
    ))
}

static ENV_INIT: Once = Once::new();

/// Stage process env every test needs, exactly once:
/// - a stub `git-credential-nostr` reachable via the `MOBEE_GIT_CREDENTIAL_NOSTR`
///   override that `resolve_git_credential_nostr` checks first;
/// - `GIT_SSL_NO_VERIFY` so git accepts the fixture's self-signed cert (the scrub
///   keeps it — it only strips SSH/insteadOf/config vectors);
/// - proxy bypass for loopback so an ambient `https_proxy` cannot swallow the fixture.
fn init_test_env() {
    ENV_INIT.call_once(|| {
        let helper_dir = temp("cred-helper");
        fs::create_dir_all(&helper_dir).expect("helper dir");
        let helper = helper_dir.join("git-credential-nostr-stub");
        fs::write(
            &helper,
            "#!/bin/sh\n\
             # NIP-98 stand-in: any `get` yields a static credential; the fixture only\n\
             # checks that an Authorization header is presented at all.\n\
             if [ \"$1\" = \"get\" ]; then\n\
             \tprintf 'username=nostr\\npassword=nip98-test-token\\n'\n\
             fi\n",
        )
        .expect("write helper stub");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&helper).expect("helper meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&helper, perms).expect("helper exec bit");

        // SAFETY (edition 2024 set_var): every test funnels through this Once before
        // spawning any git child; racing test threads block in call_once until the env
        // is fully staged, so no reader observes a partial update.
        unsafe {
            std::env::set_var("MOBEE_GIT_CREDENTIAL_NOSTR", &helper);
            std::env::set_var("GIT_SSL_NO_VERIFY", "1");
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
            std::env::set_var("no_proxy", "127.0.0.1,localhost");
        }
    });
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed in {}", dir.display());
    String::from_utf8(out.stdout).expect("utf8").trim().to_owned()
}

/// Upstream repo the fixture serves: one commit on `main`. Returns (dir, head oid).
fn make_upstream(label: &str) -> (PathBuf, String) {
    let dir = temp(label);
    fs::create_dir_all(&dir).expect("upstream dir");
    git_in(&dir, &["init", "--initial-branch=main"]);
    git_in(&dir, &["config", "user.name", "Upstream Author"]);
    git_in(&dir, &["config", "user.email", "upstream@example.invalid"]);
    fs::write(dir.join("README.md"), "upstream base\n").expect("write");
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-m", "base commit"]);
    let oid = git_stdout(&dir, &["rev-parse", "HEAD"]);
    (dir, oid)
}

#[test]
fn authenticated_fetch_base_succeeds_via_fork_path() {
    init_test_env();
    let (upstream, base_oid) = make_upstream("authed-upstream");
    // Relay-git URL shape (`…/git/<owner>/<repo>.git`) so `fetch_base_auth` applies the
    // seller NIP-98 credential — the exact gating the daemon fork path relies on.
    let mount = format!("/git/{}/base.git", "ab".repeat(32));
    let server = GitHttpAuthServer::spawn(&upstream, &mount);
    let url = server.repo_url();
    assert!(
        mobee_core::delivery_transport::is_relay_git_locator(&url),
        "fixture URL must look like relay-git to trigger auth: {url}"
    );

    let workdir = temp("authed-workdir");
    let identity = DeliveryAgentIdentity::for_seller(&"11".repeat(32));
    let auth = PushAuth {
        secret_key_hex: "22".repeat(32),
    };
    let branch = "mobee/contribution/relay-auth-itest";

    init_contribution_workdir(
        &workdir, &identity, &url, "main", &base_oid, branch,
        Some(&auth),
    )
    .expect("authenticated fetch-base through the fork path must succeed");

    // Fork tip: on `branch` at base_oid with the upstream tree checked out.
    assert_eq!(git_stdout(&workdir, &["rev-parse", "HEAD"]), base_oid);
    assert_eq!(
        git_stdout(&workdir, &["symbolic-ref", "--short", "HEAD"]),
        branch
    );
    assert!(
        workdir.join("README.md").is_file(),
        "upstream tree must be checked out at the fork tip"
    );

    // In-process libgit2 injects the NIP-98 header on EVERY request up front — there is no
    // git-credential-protocol unauthenticated probe. So every request (the info/refs advertisement
    // AND the upload-pack POST) must carry Authorization, and pack data must only move authorized.
    let requests = server.requests();
    assert!(!requests.is_empty(), "client must have reached the fixture");
    assert!(
        requests
            .iter()
            .all(|r| r.authorization.as_deref().is_some_and(|v| !v.is_empty())),
        "in-process path must carry Authorization on every request: {requests:?}"
    );
    let posts: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert!(!posts.is_empty(), "smart fetch must POST git-upload-pack");
    assert!(
        posts.iter().all(|r| r.target == format!("{mount}/git-upload-pack")),
        "fetch must only ever hit the upload-pack rpc: {requests:?}"
    );

    drop(server);
    let _ = fs::remove_dir_all(&upstream);
    let _ = fs::remove_dir_all(&workdir);
}

#[test]
fn unauthenticated_fetch_of_protected_repo_fails_closed() {
    init_test_env();
    let (upstream, base_oid) = make_upstream("unauth-upstream");
    let mount = format!("/git/{}/base.git", "cd".repeat(32));
    let server = GitHttpAuthServer::spawn(&upstream, &mount);
    let url = server.repo_url();

    let workdir = temp("unauth-workdir");
    let identity = DeliveryAgentIdentity::for_seller(&"33".repeat(32));

    // No seller auth → the in-process path presents NO NIP-98 header → the relay's 401 on the
    // info/refs advertisement is terminal (no credential fallback exists). Fail-closed as an auth
    // failure, never a success or a pack transfer.
    let err = init_contribution_workdir(
        &workdir, &identity, &url, "main", &base_oid,
        "mobee/contribution/unauth-itest",
        None,
    )
    .expect_err("fetch of an auth-required repo without credentials must fail closed");
    assert!(
        matches!(err, SellerGitError::AuthFailed(_) | SellerGitError::Io(_)),
        "unauthenticated fetch must fail closed (auth/transport error), got: {err:?}"
    );

    // Fail-closed on the wire too: only unauthenticated probes, no pack data served.
    let requests = server.requests();
    assert!(!requests.is_empty(), "client must have reached the fixture");
    assert!(
        requests.iter().all(|r| r.authorization.is_none()),
        "no request may have carried credentials: {requests:?}"
    );
    assert!(
        requests.iter().all(|r| r.method != "POST"),
        "unauthenticated client must never reach the upload-pack rpc: {requests:?}"
    );
    // And nothing landed locally.
    let probe = Command::new("git")
        .args(["rev-parse", "--verify", "refs/mobee/base"])
        .current_dir(&workdir)
        .output()
        .expect("probe base ref");
    assert!(
        !probe.status.success(),
        "refused fetch must leave no refs/mobee/base"
    );

    drop(server);
    let _ = fs::remove_dir_all(&upstream);
    let _ = fs::remove_dir_all(&workdir);
}
