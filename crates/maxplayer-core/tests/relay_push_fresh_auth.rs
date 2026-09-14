//! When is the seller's delivery push token signed, and where can it go?
//!
//! A delivery push is not one request. libgit2 asks for the receive-pack advertisement, then POSTs
//! the pack, and it opens a fresh stream for each — with this seat's delivery lock, a slow relay,
//! and a large pack in between. A token minted once, before all of that, is oldest exactly when the
//! relay finally checks its `created_at`; the honest fix is to sign each request as it is made,
//! which is what [`maxplayer_core::git_transport::AuthMinter`] is for.
//!
//! These tests watch both ends of that. The minter records what it signed and when; the HTTPS
//! fixture records the exact `Authorization` bytes each request arrived with. Joining the two by
//! equality gives the whole chain — this token, minted at this moment, appeared on this wire
//! request — rather than an inference from either side alone.
//!
//! The fixture holds its first response for a real interval, so "each leg mints its own" is not a
//! claim about instructions but about elapsed time on the clock.
//!
//! Self-signed certificate, so this binary sets `GIT_SSL_NO_VERIFY` exactly as the other fixture
//! tests do.
#![cfg(all(unix, feature = "git-delivery"))]

mod git_http_fixture;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use git_http_fixture::{FixtureOptions, GitHttpAuthServer};
use maxplayer_core::git_transport::{
    AuthMinter, TransportError, delivery_ref, nip98_authorization_header_with_keys,
    push_branch_with_header, push_branch_with_minter,
};

/// Long enough that a re-mint is unmistakable on a one-second-granularity `created_at`, short
/// enough to keep the suite quick.
const HELD_LEG: Duration = Duration::from_millis(2_500);

static ENV_INIT: Once = Once::new();

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
        "maxplayer-push-fresh-auth-{label}-{}",
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
    index.add_path(Path::new("deliverable.txt")).expect("add");
    index.write().expect("write index");
    let tree = repo
        .find_tree(index.write_tree().expect("tree"))
        .expect("find tree");
    let sig = git2::Signature::new("s", "s@example.invalid", &git2::Time::new(1_700_000_000, 0))
        .expect("sig");
    let oid = repo
        .commit(
            Some(&delivery_ref(branch)),
            &sig,
            &sig,
            "delivery",
            &tree,
            &[],
        )
        .expect("commit");
    (workdir, oid.to_string())
}

/// Add a second commit and move the local branch onto it. Returns the new oid.
fn move_branch_forward(workdir: &Path, branch: &str) -> String {
    let repo = git2::Repository::open(workdir).expect("open workdir");
    let parent = repo
        .find_commit(
            repo.refname_to_id(&delivery_ref(branch))
                .expect("branch tip"),
        )
        .expect("parent commit");
    std::fs::write(workdir.join("deliverable.txt"), "moved on\n").expect("write");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("deliverable.txt")).expect("add");
    index.write().expect("write index");
    let tree = repo
        .find_tree(index.write_tree().expect("tree"))
        .expect("find tree");
    let sig = git2::Signature::new("s", "s@example.invalid", &git2::Time::new(1_700_000_100, 0))
        .expect("sig");
    repo.commit(
        Some(&delivery_ref(branch)),
        &sig,
        &sig,
        "later work",
        &tree,
        &[&parent],
    )
    .expect("second commit")
    .to_string()
}

/// One call the test's minter answered: when it signed, for which destination, and the exact header
/// bytes it handed back — which is what the fixture must then see on the wire.
#[derive(Clone, Debug)]
struct Minted {
    at: Instant,
    destination: String,
    header: String,
}

/// A production-shaped minter for tests: signs a real NIP-98 token scoped to `scope` for `bound`,
/// refuses any other destination, and records every call.
fn recording_minter(
    bound: &str,
    scope: &str,
    keys: nostr_sdk::Keys,
) -> (AuthMinter, Arc<Mutex<Vec<Minted>>>) {
    let log: Arc<Mutex<Vec<Minted>>> = Arc::new(Mutex::new(Vec::new()));
    let bound = bound.to_owned();
    let scope = scope.to_owned();
    let sink = Arc::clone(&log);
    let minter: AuthMinter = Arc::new(move |destination: &str| {
        if destination != bound {
            sink.lock().expect("log").push(Minted {
                at: Instant::now(),
                destination: destination.to_owned(),
                header: String::new(),
            });
            return Err(format!(
                "refusing to authorize a leg to {destination}: this delivery is bound to {bound}"
            ));
        }
        let header = nip98_authorization_header_with_keys(destination, &keys, Some(&scope), None)
            .map_err(|error| error.to_string())?;
        sink.lock().expect("log").push(Minted {
            at: Instant::now(),
            destination: destination.to_owned(),
            header: header.clone(),
        });
        Ok(header)
    });
    (minter, log)
}

/// Every wire request of a delivery push carries a token minted for THAT request, after whatever the
/// push waited on — proven by holding the first response for [`HELD_LEG`] and showing the second
/// token was signed on the far side of that hold.
///
/// Red-on-revert: make `HttpStream::send` mint once and cache it (or pass a `static_auth` minter
/// from the push entry point) and the two legs arrive with identical bytes signed at the same
/// instant, failing both the distinctness and the elapsed-time assertions.
#[test]
fn every_wire_leg_mints_its_own_token_after_whatever_the_push_waited_on() {
    init_test_env();
    let root = temp("fresh");
    let branch = "maxplayer/abc12345";
    let (workdir, oid) = committed_workdir(&root, branch);

    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let relay = GitHttpAuthServer::spawn_with(
        &relay_repo,
        "/git/seller/r.git",
        FixtureOptions {
            first_response_delay: Some(HELD_LEG),
            ..FixtureOptions::default()
        },
    );
    let url = relay.repo_url();
    let (minter, minted) =
        recording_minter(&url, &delivery_ref(branch), nostr_sdk::Keys::generate());

    let pushed = push_branch_with_minter(&workdir, &url, branch, &oid, Some(minter), None).expect("push");
    assert_eq!(pushed, oid, "the returned oid is the gated one");

    // What the minter was asked for.
    let minted = minted.lock().expect("log").clone();
    assert_eq!(
        minted.len(),
        2,
        "one mint per wire request, no more and no fewer: {minted:?}"
    );
    assert!(
        minted.iter().all(|entry| entry.destination == url),
        "every mint was asked for the destination this push named: {minted:?}"
    );
    assert_ne!(
        minted[0].header, minted[1].header,
        "the second leg must not re-present the first leg's token"
    );
    let gap = minted[1].at.duration_since(minted[0].at);
    assert!(
        gap >= HELD_LEG,
        "the second token was signed AFTER the held first leg, not before it: {gap:?}"
    );

    // What actually arrived on the wire, joined to the mints by exact bytes.
    let requests = relay.requests();
    assert_eq!(
        requests.len(),
        2,
        "the advertisement and the pack POST: {requests:?}"
    );
    assert_eq!(
        requests[0].target, "/git/seller/r.git/info/refs?service=git-receive-pack",
        "{requests:?}"
    );
    assert_eq!(requests[1].target, "/git/seller/r.git/git-receive-pack");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some(minted[0].header.as_str()),
        "the advertisement carried the token minted for it"
    );
    assert_eq!(
        requests[1].authorization.as_deref(),
        Some(minted[1].header.as_str()),
        "the pack POST carried its OWN token, minted after the hold"
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

/// The contrast, on the same fixture and the same hold: a token minted ONCE up front is the same
/// ageing bytes on every leg. This is what the read legs still do (`static_auth`, one short
/// exchange) and what the delivery push no longer does — and it is why the test above is about
/// elapsed time rather than about call counts.
#[test]
fn a_token_minted_once_up_front_is_the_same_ageing_bytes_on_every_leg() {
    init_test_env();
    let root = temp("static");
    let branch = "maxplayer/abc12345";
    let (workdir, oid) = committed_workdir(&root, branch);

    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let relay = GitHttpAuthServer::spawn_with(
        &relay_repo,
        "/git/seller/r.git",
        FixtureOptions {
            first_response_delay: Some(HELD_LEG),
            ..FixtureOptions::default()
        },
    );
    let url = relay.repo_url();
    let keys = nostr_sdk::Keys::generate();
    let header =
        nip98_authorization_header_with_keys(&url, &keys, Some(&delivery_ref(branch)), None)
            .expect("header");

    push_branch_with_header(&workdir, &url, branch, &oid, Some(header.clone())).expect("push");

    let requests = relay.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some(header.as_str())),
        "the once-minted header rode every leg unchanged: {requests:?}"
    );
    drop(relay);
    let _ = std::fs::remove_dir_all(&root);
}

/// A redirect is a destination nothing authorized: the allowlist never saw it, the remote binding
/// never compared it, and the minter was never shown it. So the client must not follow one — and
/// the proof is that the place it pointed at recorded no request at all.
///
/// Red-on-revert: drop `no_redirects()` from the transport's clients. reqwest then follows the hop,
/// the target records `GET …/info/refs?service=git-receive-pack` carrying the seller's token, and
/// the emptiness assertions below go red.
#[test]
fn a_redirect_is_refused_and_the_token_never_follows_it() {
    init_test_env();
    let root = temp("redirect");
    let branch = "maxplayer/abc12345";
    let (workdir, oid) = committed_workdir(&root, branch);

    // Where the redirect points: a second, perfectly functional HTTPS git endpoint.
    let elsewhere_repo = root.join("elsewhere.git");
    git2::Repository::init_bare(&elsewhere_repo).expect("elsewhere bare");
    let elsewhere = GitHttpAuthServer::spawn(&elsewhere_repo, "/git/elsewhere/r.git");
    let elsewhere_url = elsewhere.repo_url();

    // The relay the seller named — which answers every request with a 302 to that endpoint.
    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let relay = GitHttpAuthServer::spawn_with(
        &relay_repo,
        "/git/seller/r.git",
        FixtureOptions {
            redirect_to: Some(format!(
                "{elsewhere_url}/info/refs?service=git-receive-pack"
            )),
            ..FixtureOptions::default()
        },
    );
    let url = relay.repo_url();
    let (minter, minted) =
        recording_minter(&url, &delivery_ref(branch), nostr_sdk::Keys::generate());

    let err = push_branch_with_minter(&workdir, &url, branch, &oid, Some(minter), None)
        .expect_err("a redirected leg must fail the push");

    // The observation first: nothing reached the redirect target.
    let followed = elsewhere.requests();
    assert!(
        followed.is_empty(),
        "SECURITY: the client followed the redirect: {followed:?}"
    );
    assert!(
        git2::Repository::open_bare(&elsewhere_repo)
            .expect("elsewhere bare")
            .refname_to_id(&delivery_ref(branch))
            .is_err(),
        "SECURITY: the pack landed at the redirect target"
    );
    // And only the leg that was actually attempted was ever authorized.
    let minted = minted.lock().expect("log").clone();
    assert_eq!(
        minted.len(),
        1,
        "one authorized leg: the advertisement the relay redirected: {minted:?}"
    );
    assert_eq!(minted[0].destination, url);

    assert!(
        err.to_string().contains("redirect"),
        "the failure names the redirect: {err}"
    );
    drop(relay);
    drop(elsewhere);
    let _ = std::fs::remove_dir_all(&root);
}

/// The minter is the last gate in front of the wire: a leg for a destination it did not authorize
/// gets no token, and therefore no request is made at all.
///
/// This is the belt to the transport's braces. `NostrHttp::action` already refuses a leg whose URL
/// is not the bound destination; refusing to SIGN as well means that even a leg that somehow passed
/// that check cannot obtain a credential to carry.
#[test]
fn a_leg_the_minter_refuses_is_never_put_on_the_wire() {
    init_test_env();
    let root = temp("refuse");
    let branch = "maxplayer/abc12345";
    let (workdir, oid) = committed_workdir(&root, branch);

    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&relay_repo, "/git/seller/r.git");
    let url = relay.repo_url();

    // A minter bound to a DIFFERENT delivery remote than the one this push names.
    let (minter, minted) = recording_minter(
        "https://relay.example/git/seller/r.git",
        &delivery_ref(branch),
        nostr_sdk::Keys::generate(),
    );

    let err = push_branch_with_minter(&workdir, &url, branch, &oid, Some(minter), None)
        .expect_err("the minter must refuse this destination");

    let requests = relay.requests();
    assert!(
        requests.is_empty(),
        "no request is made when the leg cannot be authorized: {requests:?}"
    );
    let minted = minted.lock().expect("log").clone();
    assert_eq!(minted.len(), 1, "the refusal happened once: {minted:?}");
    assert!(
        minted[0].header.is_empty(),
        "nothing was signed for the refused destination"
    );
    assert!(
        err.to_string().contains("authorize") || err.to_string().contains("bound to"),
        "the failure names the refusal: {err}"
    );
    assert!(
        git2::Repository::open_bare(&relay_repo)
            .expect("relay bare")
            .refname_to_id(&delivery_ref(branch))
            .is_err(),
        "nothing was pushed"
    );
    drop(relay);
    let _ = std::fs::remove_dir_all(&root);
}

/// The remote's per-ref answer is the whole answer, so a rejection has to fail the push — loudly,
/// naming the ref and the remote's reason. With the post-push read-back gone this is the only thing
/// standing between "the push call returned" and "the delivery landed".
///
/// A real `pre-receive` hook declines, so this is the remote's own report-status on the wire, not a
/// synthesized status tuple.
#[test]
fn a_ref_the_remote_declines_fails_the_push() {
    init_test_env();
    let root = temp("declined");
    let branch = "maxplayer/abc12345";
    let (workdir, oid) = committed_workdir(&root, branch);

    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let hook = relay_repo.join("hooks").join("pre-receive");
    std::fs::create_dir_all(relay_repo.join("hooks")).expect("hooks dir");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'delivery policy declined this ref' >&2\nexit 1\n",
    )
    .expect("write hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("hook mode");
    }

    let relay = GitHttpAuthServer::spawn(&relay_repo, "/git/seller/r.git");
    let url = relay.repo_url();
    let (minter, _minted) =
        recording_minter(&url, &delivery_ref(branch), nostr_sdk::Keys::generate());

    let err = push_branch_with_minter(&workdir, &url, branch, &oid, Some(minter), None)
        .expect_err("a declined ref must fail the push");
    assert!(
        matches!(&err, TransportError::Rejected(message) if message.contains(&delivery_ref(branch))),
        "the failure is a rejection naming our ref: {err:?}"
    );
    assert!(
        git2::Repository::open_bare(&relay_repo)
            .expect("relay bare")
            .refname_to_id(&delivery_ref(branch))
            .is_err(),
        "the declined ref was not created"
    );
    drop(relay);
    let _ = std::fs::remove_dir_all(&root);
}

/// Through the production delivery wrapper (`neutralize_then_push_off_runtime`): the object the gate
/// approved is what ships, even though the local branch has moved on since. The push sends the OID
/// by value and never re-resolves the branch, so the move is invisible to it.
///
/// Red-on-revert: have the wrapper re-resolve `refs/heads/<branch>` instead of pushing the approved
/// oid and the relay ends up holding the later commit.
#[tokio::test]
async fn the_wrapper_delivers_the_approved_object_after_the_local_branch_moved() {
    init_test_env();
    let root = temp("gated-oid");
    let branch = "maxplayer/abc12345";
    let (workdir, approved) = committed_workdir(&root, branch);

    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&relay_repo, "/git/seller/r.git");
    let url = relay.repo_url();
    let (minter, minted) =
        recording_minter(&url, &delivery_ref(branch), nostr_sdk::Keys::generate());

    // The gate approved `approved`; afterwards the local branch moves to a later commit.
    let moved_on = move_branch_forward(&workdir, branch);
    assert_ne!(moved_on, approved);

    let pushed = maxplayer_core::seller_git::neutralize_then_push_off_runtime(
        workdir.clone(),
        url.clone(),
        branch.to_owned(),
        approved.clone(),
        Some(minter),
        None,
    )
    .await
    .expect("push through the production wrapper");

    assert_eq!(pushed, approved, "the wrapper reports the approved object");
    assert_eq!(
        git2::Repository::open_bare(&relay_repo)
            .expect("relay bare")
            .refname_to_id(&delivery_ref(branch))
            .expect("delivered ref")
            .to_string(),
        approved,
        "the relay holds the APPROVED object, not the moved-on branch tip"
    );
    assert_eq!(
        git2::Repository::open(&workdir)
            .expect("workdir")
            .refname_to_id(&delivery_ref(branch))
            .expect("local ref")
            .to_string(),
        moved_on,
        "the local branch was not touched"
    );
    // Every leg the wrapper made was scoped to this job's ref and this job's remote.
    let minted = minted.lock().expect("log").clone();
    assert_eq!(minted.len(), 2, "{minted:?}");
    assert!(minted.iter().all(|entry| entry.destination == url));
    drop(relay);
    let _ = std::fs::remove_dir_all(&root);
}
