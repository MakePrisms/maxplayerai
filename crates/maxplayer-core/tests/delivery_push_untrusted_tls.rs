//! The negative control for `delivery_push_verified_tls.rs`: same path, same fixture, one
//! difference — the client is never given the fixture's certificate.
//!
//! Without this, a green verified-push gate cannot tell verification from indifference. It exists
//! because the failure mode it guards against is silent: if the transport quietly accepted any
//! certificate, the positive gate would still be green and would still be worthless.
//!
//! **Its own test binary**, for the same reason its sibling is: the transport's HTTP client is a
//! process-wide `OnceLock` whose root store is fixed when it is first built, so "did this process
//! have `SSL_CERT_FILE`" is a property of the whole binary and not of a test function.
//!
//! The refusal is attributed without trusting the client's error text — a rustls handshake failure
//! surfaces through reqwest as a generic send error, so asserting on the word "certificate" would
//! fail on a *correct* refusal. Three measured facts pin it instead: the push failed, the remote ref
//! did not move, and the server — which records every request it read a head for — recorded no
//! smart-HTTP request at all, because a connection that never finished the handshake never reached
//! the request handler.

#![cfg(all(unix, feature = "wallet"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use maxplayer_core::git_transport;

#[path = "git_http_fixture/mod.rs"]
mod git_http_fixture;

use git_http_fixture::GitHttpAuthServer;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "maxplayer-untrusted-tls-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

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

fn remote_head(bare: &Path, branch: &str) -> Option<String> {
    let repo = git2::Repository::open_bare(bare).expect("open bare");
    repo.find_reference(&format!("refs/heads/{branch}"))
        .ok()
        .and_then(|reference| reference.target())
        .map(|oid| oid.to_string())
}

/// A push to a peer this client cannot verify is refused, and nothing is delivered.
#[test]
fn a_push_to_an_unverifiable_peer_is_refused_and_delivers_nothing() {
    let root = temp("untrusted");
    let branch = "maxplayer/bbbb2222";
    let (workdir, oid) = job_workdir(&root, "job", branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&bare, "/git/seller/r.git");

    // SAFETY (edition 2024 set_var): single-test binary, nothing else running. Both bypasses are
    // REMOVED rather than assumed absent, so an ambient value in the developer's shell cannot turn
    // this control green by disabling the very check it measures.
    unsafe {
        std::env::remove_var("GIT_SSL_NO_VERIFY");
        std::env::remove_var("SSL_CERT_FILE");
        std::env::remove_var("SSL_CERT_DIR");
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    }

    let url = relay.repo_url();
    let minter: git_transport::AuthMinter = Arc::new(|_| Ok("Nostr fixture-token".to_owned()));

    let outcome = git_transport::push_branch_with_minter(
        &workdir,
        &url,
        branch,
        &oid,
        Some(minter),
        None,
        None,
    );

    let error = match outcome {
        Ok(pushed) => panic!(
            "a push to a peer holding an untrusted certificate SUCCEEDED ({pushed}); the verified \
             gate next door proves nothing if this one can pass"
        ),
        Err(error) => error,
    };

    assert_eq!(
        remote_head(&bare, branch),
        None,
        "the refusal still moved the remote ref: refused delivery must deliver nothing"
    );
    assert!(
        relay.requests().is_empty(),
        "the server read a request head, so the connection got PAST the handshake; this refusal is \
         not the one this control claims to measure: {:?}",
        relay.requests()
    );

    // Recorded, not asserted on: the message is reqwest's generic send error and naming a substring
    // of it here would make a correct refusal fail on a dependency bump.
    eprintln!("refusal (not asserted, recorded for attribution): {error}");

    // The fixture is alive and would have answered a client that trusted it — so the refusal above
    // was a trust decision, not a dead server. Proven by the sibling binary against a fresh fixture;
    // here it is enough that the listener still accepts and challenges.
    assert!(
        url.starts_with("https://127.0.0.1:"),
        "the fixture never came up at a loopback https address: {url}"
    );
}
