//! Where does the seller's push token go when the workdir's own `.git/config` rewrites the relay URL?
//!
//! The job agent writes the workdir, `.git/config` included, and exits. A planted
//! `url.<evil>.insteadOf = <relay>` makes libgit2 resolve the relay URL to `<evil>` when the push
//! creates its remote. The seller's NIP-98 header was minted for the relay. Without a destination
//! binding the transport attaches that header to whatever URL libgit2 resolved, and `<evil>`
//! collects the token and the pack.
//!
//! This test is the observation. An "evil" HTTPS endpoint (the rustls fixture) records every
//! request it receives, with the `Authorization` header it carried. The push runs through the
//! production transport (`git_transport::push_branch_with_header`) with the config rewrite in
//! place and WITHOUT the config scrub, so it exercises the transport binding and nothing else.
//! Expected: the push fails with the binding refusal, and the endpoint sees no request at all.
//!
//! Red-on-revert: remove the URL check in `git_transport::bound_remote` and the destination check
//! in `NostrHttp::action`. The endpoint then records
//! `GET /git/evil/r.git/info/refs?service=git-receive-pack` with an `Authorization: Nostr …`
//! header, and the push lands in the evil repository.
//!
//! The evil endpoint has a self-signed certificate, so this binary sets `GIT_SSL_NO_VERIFY`, as
//! the other fixture tests do. A real attacker's endpoint has a valid certificate for its own
//! hostname, and TLS verification does not defend against a URL rewrite; the flag models the
//! attacker faithfully.
#![cfg(all(unix, feature = "git-delivery"))]

mod git_http_fixture;

use std::path::{Path, PathBuf};
use std::sync::Once;

use git_http_fixture::GitHttpAuthServer;
use maxplayer_core::git_transport::{
    TransportError, delivery_ref, nip98_authorization_header_with_keys, push_branch_with_header,
};

static ENV_INIT: Once = Once::new();

/// Stage the process env once. This integration test binary is its own process, so the transport's
/// HTTP client (which reads `GIT_SSL_NO_VERIFY` once, at first use) is built after this.
fn init_test_env() {
    ENV_INIT.call_once(|| {
        // SAFETY (edition 2024 set_var): every test funnels through this Once before it touches the
        // transport; racing test threads block in call_once until the env is fully staged.
        unsafe {
            std::env::set_var("GIT_SSL_NO_VERIFY", "1");
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
            std::env::set_var("no_proxy", "127.0.0.1,localhost");
        }
    });
}

fn temp(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "maxplayer-push-binding-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir root");
    root
}

/// A committed workdir: one commit on `refs/heads/<branch>`. Returns (workdir, commit oid).
fn committed_workdir(root: &Path, branch: &str) -> (PathBuf, String) {
    let workdir = root.join("workdir");
    let repo = git2::Repository::init(&workdir).expect("init workdir");
    std::fs::write(workdir.join("deliverable.txt"), "work\n").expect("write");
    let mut index = repo.index().expect("index");
    index
        .add_path(Path::new("deliverable.txt"))
        .expect("add");
    index.write().expect("write index");
    let tree = repo
        .find_tree(index.write_tree().expect("tree"))
        .expect("find tree");
    let sig = git2::Signature::new(
        "s",
        "s@example.invalid",
        &git2::Time::new(1_700_000_000, 0),
    )
    .expect("sig");
    let oid = repo
        .commit(Some(&delivery_ref(branch)), &sig, &sig, "delivery", &tree, &[])
        .expect("commit");
    (workdir, oid.to_string())
}

/// The attack: the job writes a rewrite rule into the workdir's own `.git/config`.
fn plant_insteadof(workdir: &Path, attacker_url: &str, intended_url: &str) {
    let repo = git2::Repository::open(workdir).expect("open workdir");
    let mut cfg = repo.config().expect("repo config");
    cfg.set_str(&format!("url.{attacker_url}.insteadOf"), intended_url)
        .expect("plant insteadOf");
}

#[test]
fn the_header_never_follows_a_config_rewrite_to_another_host() {
    init_test_env();
    let root = temp("rewrite");
    let branch = "maxplayer/abc12345";
    let (workdir, oid) = committed_workdir(&root, branch);

    // The evil endpoint: an HTTPS git server the job could reach. It records every request.
    let evil_repo = root.join("evil.git");
    git2::Repository::init_bare(&evil_repo).expect("evil bare");
    let evil = GitHttpAuthServer::spawn(&evil_repo, "/git/evil/r.git");
    let evil_url = evil.repo_url();

    // The relay the seller named, and a branch-scoped token minted for it.
    let intended = "https://relay.example/git/seller/r.git";
    let keys = nostr_sdk::Keys::generate();
    let header =
        nip98_authorization_header_with_keys(intended, &keys, Some(&delivery_ref(branch)), None)
            .expect("header");

    plant_insteadof(&workdir, &evil_url, intended);

    // The push through the production transport, with the rewrite in place and no scrub.
    let result = push_branch_with_header(&workdir, intended, branch, &oid, Some(header));

    // The observation comes first: nothing reached the evil endpoint — no request, so no header.
    let requests = evil.requests();
    let with_auth: Vec<_> = requests
        .iter()
        .filter(|request| request.authorization.is_some())
        .collect();
    assert!(
        with_auth.is_empty(),
        "SECURITY: the token reached the attacker: {with_auth:?}"
    );
    assert!(requests.is_empty(), "the attacker saw a request: {requests:?}");
    assert!(
        git2::Repository::open_bare(&evil_repo)
            .expect("evil bare")
            .refname_to_id(&delivery_ref(branch))
            .is_err(),
        "the pack must not land at the attacker"
    );

    // And the push failed closed with the binding refusal.
    let err = result.expect_err("the destination binding must refuse");
    assert!(
        matches!(&err, TransportError::Transport(m) if m.contains("rewrote")),
        "{err}"
    );
    drop(evil);
    let _ = std::fs::remove_dir_all(&root);
}

/// Positive control: the same transport and the same fixture, no rewrite. The push reaches the
/// endpoint the caller named, WITH the header, and the object-sourced push succeeds against a real
/// smart-HTTP server. This proves the fixture records headers, so the empty recording above is
/// evidence and not a broken fixture.
///
/// It also pins the leg COUNT. The delivery push is now exactly two authorized requests — the
/// receive-pack advertisement and the pack POST — because the remote's per-ref ACK in that POST's
/// status report is the answer, and nothing is read back afterwards. A third authorized request to
/// the delivery remote would be a regression of that.
/// Red-on-revert: restore the post-push advertisement read and the count assertions below go red.
#[test]
fn the_intended_destination_receives_the_header_and_the_gated_object() {
    init_test_env();
    let root = temp("control");
    let branch = "maxplayer/abc12345";
    let (workdir, oid) = committed_workdir(&root, branch);

    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&relay_repo, "/git/seller/r.git");
    let url = relay.repo_url();
    let keys = nostr_sdk::Keys::generate();
    let header =
        nip98_authorization_header_with_keys(&url, &keys, Some(&delivery_ref(branch)), None)
            .expect("header");

    let pushed =
        push_branch_with_header(&workdir, &url, branch, &oid, Some(header)).expect("push");
    assert_eq!(pushed, oid, "the returned oid is the gated one");

    let requests = relay.requests();
    assert!(!requests.is_empty(), "the relay saw the push");
    assert!(
        requests.iter().all(|request| {
            request
                .authorization
                .as_deref()
                .is_some_and(|value| value.starts_with("Nostr "))
        }),
        "every leg carried the NIP-98 header: {requests:?}"
    );
    let advertisement = "/git/seller/r.git/info/refs?service=git-receive-pack";
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == advertisement)
            .count(),
        1,
        "exactly one advertisement — the push's own; no read-back follows it: {requests:?}"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/git/seller/r.git/git-receive-pack")
            .count(),
        1,
        "exactly one receive-pack POST: {requests:?}"
    );
    assert_eq!(
        requests.len(),
        2,
        "the delivery push is those two requests and nothing else: {requests:?}"
    );
    assert_eq!(
        git2::Repository::open_bare(&relay_repo)
            .expect("relay bare")
            .refname_to_id(&delivery_ref(branch))
            .expect("delivered ref")
            .to_string(),
        oid,
        "the relay holds the gated object at the delivery ref"
    );
    drop(relay);
    let _ = std::fs::remove_dir_all(&root);
}
