//! A REAL libgit2 push over the real HTTPS fixture, with certificate verification ON.
//!
//! Every other fixture test in this crate reaches the smart-HTTP server by setting
//! `GIT_SSL_NO_VERIFY=1`. That is a reasonable trade when the claim under test is "the right
//! requests were made in the right order" — and it is worthless for the claim this file makes,
//! which is about the transport itself. A push accepted by a peer nobody authenticated is not
//! evidence of a verified push.
//!
//! It also matters for the delivery-push CHILD specifically. `GIT_SSL_NO_VERIFY` is deliberately
//! ABSENT from `delivery_executor::CHILD_ENV_ALLOWLIST`, and `SSL_CERT_FILE` is ON it — the
//! allowlist's own doc comment says why: a host with a non-default trust store (every musl
//! container image this product ships into) would otherwise fail TLS in the child while succeeding
//! in the parent. So the only trust input a delivery child can be given is the one this file uses,
//! and whether that input is honoured end-to-end by the transport's reqwest-backed libgit2
//! subtransport is a question about production, not about a fixture.
//!
//! **This file is its own test binary on purpose.** The transport's HTTP client is a process-wide
//! `OnceLock`, and its root store is decided when that client is first built. A test that set
//! `SSL_CERT_FILE` after some other test in the same binary had already pushed would be asserting
//! nothing. Its negative control is `delivery_push_untrusted_tls.rs`, a second binary that differs
//! in exactly one thing: it never sets the variable.
//!
//! What this does NOT certify: verification against a public CA, revocation, pinning, or any
//! behaviour on a platform other than the one it ran on. A per-run self-signed certificate handed
//! to a client as its only anchor is still a fixture.

#![cfg(all(unix, feature = "wallet"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use maxplayer_core::git_transport;

#[path = "git_http_fixture/mod.rs"]
mod git_http_fixture;

use git_http_fixture::GitHttpAuthServer;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "maxplayer-verified-tls-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A workdir holding one commit on the delivery ref, ready to push.
fn job_workdir(root: &Path, name: &str, branch: &str) -> (PathBuf, String) {
    let workdir = root.join(name);
    let repo = git2::Repository::init(&workdir).expect("init workdir");
    std::fs::write(
        workdir.join("deliverable.txt"),
        format!("work from {name}\n"),
    )
    .expect("write");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("deliverable.txt")).expect("add");
    index.write().expect("write index");
    let tree = repo
        .find_tree(index.write_tree().expect("tree"))
        .expect("find tree");
    let sig = git2::Signature::new("s", "s@example.invalid", &git2::Time::new(1_700_000_000, 0))
        .expect("sig");
    let oid = repo
        .commit(
            Some(&git_transport::delivery_ref(branch)),
            &sig,
            &sig,
            "delivery",
            &tree,
            &[],
        )
        .expect("commit");
    (workdir, oid.to_string())
}

/// What the REMOTE holds at `refs/heads/<branch>` — the only honest answer to "did the push land".
/// Read out of the bare repo directly, not from the pushing client's own report.
fn remote_head(bare: &Path, branch: &str) -> Option<String> {
    let repo = git2::Repository::open_bare(bare).expect("open bare");
    repo.find_reference(&format!("refs/heads/{branch}"))
        .ok()
        .and_then(|reference| reference.target())
        .map(|oid| oid.to_string())
}

/// The push succeeds with verification ON, and the remote actually moved.
///
/// Red-on-revert: drop the `SSL_CERT_FILE` line and this fails at the handshake — which is the
/// whole point, and is what the sibling binary asserts deliberately.
#[test]
fn a_push_verified_against_the_fixture_certificate_lands_on_the_remote() {
    let root = temp("verified");
    let branch = "maxplayer/aaaa1111";
    let (workdir, oid) = job_workdir(&root, "job", branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&bare, "/git/seller/r.git");
    let ca = relay.ca_file(&root);

    // BEFORE the transport's client exists. Nothing in this binary has pushed yet.
    //
    // SAFETY (edition 2024 set_var): this is the first test statement to touch the environment in a
    // single-test binary; no other thread is running.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &ca);
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    }
    assert!(
        std::env::var_os("GIT_SSL_NO_VERIFY").is_none(),
        "this gate is void if anything disabled verification: the bypass is what it exists to avoid"
    );

    let url = relay.repo_url();
    let minter: git_transport::AuthMinter = Arc::new(|_| Ok("Nostr fixture-token".to_owned()));

    let pushed = git_transport::push_branch_with_minter(
        &workdir,
        &url,
        branch,
        &oid,
        Some(minter),
        None,
        None,
    )
    .expect("a push whose peer certificate verified");

    assert_eq!(
        pushed, oid,
        "the transport reported pushing a different object"
    );
    assert_eq!(
        remote_head(&bare, branch).as_deref(),
        Some(oid.as_str()),
        "the remote ref did not move: the client's own success report is not delivery"
    );

    // The server saw a real smart-HTTP push, not merely a handshake.
    let seen: Vec<String> = relay
        .requests()
        .iter()
        .map(|request| format!("{} {}", request.method, request.target))
        .collect();
    assert!(
        seen.iter().any(|line| line.contains("/info/refs")),
        "no advertisement leg reached the server: {seen:?}"
    );
    assert!(
        seen.iter().any(|line| line.contains("git-receive-pack")),
        "no pack upload reached the server: {seen:?}"
    );
}
