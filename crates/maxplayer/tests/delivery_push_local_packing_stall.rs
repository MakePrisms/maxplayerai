//! T1: a delivery held inside REAL LOCAL libgit2 packing — not a shell, not the wire.
//!
//! Every held-phase gate in this workspace before this file held something that is not local
//! packing. `delivery_push_production_child.rs` holds a `/bin/sh` fixture that traps `SIGTERM`.
//! `delivery_push_shipped_child.rs` holds the HTTP legs: the fixture parks `GET /info/refs` or
//! `POST /git-receive-pack` and the child sits in `reqwest`. Both were credited for what they are,
//! and both were named for what they are not: **an HTTP hold is not local packing, and a shell that
//! sleeps is not libgit2.** The phase this product actually fears — libgit2 walking and packing
//! objects, where the cancellation answer is discarded (`pack-objects.c:979`) — had no gate at all.
//!
//! # The stall, and why it is deterministic
//!
//! A commit's tree is read by libgit2's packbuilder during `queue_objects` — the object walk that
//! runs AFTER the advertisement and `push_negotiation`, and BEFORE a single pack byte is produced.
//! Replacing that one loose tree object with a **FIFO** makes the walk's `open(2)` block until a
//! writer appears, and nothing in this test ever opens the write end.
//!
//! No sleep, no size heuristic, no "make the repo big enough and hope": the child is parked on a
//! kernel primitive, indefinitely, at a point in the real packing path. Measured first-hand before
//! this gate was written — `PackBuilder::insert_commit` does not return, while `find_commit` on the
//! gated oid still succeeds, because a commit lookup does not read the tree.
//!
//! # What separates this from a POST stall, as an assertion rather than a claim
//!
//! libgit2 does not stream the pack to the socket. `HttpStream::write`
//! (`git_transport.rs:1184-1203`) buffers every chunk into memory and the POST is not issued until
//! the first `read` (`1171-1183`). So a delivery stopped during the object walk has, necessarily,
//! issued its advertisement GET and **no POST at all** — and the fixture is asked, at the end,
//! exactly that. A POST in the recorded requests would mean this gate was measuring the wire after
//! all, and it fails.
//!
//! # Platform
//!
//! darwin-arm64 and Linux both provide `mkfifo(2)` and both block an `open(2)` for reading until a
//! writer arrives (POSIX). Nothing here was measured on Linux; see the reports for what was.

#![cfg(all(unix, feature = "wallet"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::REAP_BOUND;
use maxplayer_core::delivery_turn::delivery_turn;
use maxplayer_core::git_transport::{self, AuthMinter};
use maxplayer_core::seller_git::{SellerGitError, neutralize_then_push_in_child_off_runtime};

#[path = "../../maxplayer-core/tests/git_http_fixture/mod.rs"]
mod git_http_fixture;

use git_http_fixture::GitHttpAuthServer;

/// Dropped when the seat's turn is handed back, so custody is observed and not inferred.
struct Token(Arc<AtomicBool>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "maxplayer-local-pack-{label}-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn shipped_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_maxplayer"))
}

/// A seller workdir holding one commit on the delivery ref. Returns the workdir, the gated oid and
/// the oid of the commit's TREE — the object this gate turns into a FIFO.
fn job_workdir(root: &Path, branch: &str) -> (PathBuf, String, git2::Oid) {
    let workdir = root.join("workdir");
    let repo = git2::Repository::init(&workdir).expect("init workdir");
    std::fs::write(workdir.join("deliverable.txt"), "local packing stall\n").expect("write");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("deliverable.txt")).expect("add");
    index.write().expect("write index");
    let tree_oid = index.write_tree().expect("tree");
    let sig = git2::Signature::new("s", "s@example.invalid", &git2::Time::new(1_700_000_000, 0))
        .expect("sig");
    let oid = {
        let tree = repo.find_tree(tree_oid).expect("find tree");
        repo.commit(
            Some(&git_transport::delivery_ref(branch)),
            &sig,
            &sig,
            "delivery",
            &tree,
            &[],
        )
        .expect("commit")
    };
    (workdir, oid.to_string(), tree_oid)
}

/// Where a loose object lives inside a workdir's object database.
fn loose_object_path(workdir: &Path, oid: git2::Oid) -> PathBuf {
    let hex = oid.to_string();
    workdir
        .join(".git")
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..])
}

/// Replace one loose object with a FIFO nothing will ever write to.
fn park_object_on_a_fifo(workdir: &Path, oid: git2::Oid) -> PathBuf {
    let path = loose_object_path(workdir, oid);
    assert!(
        path.is_file(),
        "object {oid} must be loose before it can be parked: {path:?}"
    );
    std::fs::remove_file(&path).expect("remove the loose object");
    let raw = std::ffi::CString::new(path.as_os_str().to_str().expect("utf-8 path"))
        .expect("path without NUL");
    let rc = unsafe { libc::mkfifo(raw.as_ptr(), 0o644) };
    assert_eq!(
        rc,
        0,
        "mkfifo({path:?}) failed: {}",
        std::io::Error::last_os_error()
    );
    path
}

/// Is some process still parked on `fifo` as a READER?
///
/// This is the liveness probe, and it is exact rather than approximate. POSIX: an `open(2)` for
/// writing with `O_NONBLOCK` fails with `ENXIO` when no process has the FIFO open for reading, and
/// succeeds when one does — a reader blocked in `open(O_RDONLY)` counts, because that blocking open
/// IS the rendezvous the write side completes. Measured first-hand on this host before it was relied
/// on: no reader -> `ENXIO`; a child blocked in `open(O_RDONLY)` -> the write open succeeds; the
/// same child killed -> `ENXIO` again.
///
/// So this answers, about the one process this gate parked and about no other: is it still in the
/// object walk? A pidfile cannot be used here — the child is the shipped binary, which does not
/// write one — and scanning the process table would catch a neighbouring test's child. This cannot:
/// only a process holding THIS delivery's parked object can make it say yes.
fn a_reader_is_still_parked_on(fifo: &Path) -> bool {
    let raw = std::ffi::CString::new(fifo.as_os_str().to_str().expect("utf-8 path"))
        .expect("path without NUL");
    let fd = unsafe { libc::open(raw.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
    if fd >= 0 {
        // Succeeded: a reader is there. Close immediately — this is a probe, not a release.
        unsafe { libc::close(fd) };
        return true;
    }
    let error = std::io::Error::last_os_error();
    assert_eq!(
        error.raw_os_error(),
        Some(libc::ENXIO),
        "the parked-object probe failed for a reason that is not 'nobody is reading': {error}"
    );
    false
}

fn stage_env(ca: &Path) {
    // SAFETY (edition 2024 `set_var`): called at the top of the test body, before any task is
    // spawned, and this binary's tests stage the same values under one lock.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", ca);
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
        std::env::remove_var("GIT_SSL_NO_VERIFY");
    }
}

/// `SSL_CERT_FILE` is per-process, so fixture-backed deliveries in this binary run one at a time.
static TRUST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exclusive_trust() -> std::sync::MutexGuard<'static, ()> {
    TRUST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// T1. The shipped binary, a real HTTPS remote, a real libgit2 push — parked in the object walk.
///
/// Red-on-revert: this gate is about the STOP, so the control that makes it red is the stop itself.
/// Remove the deadline kill from `drive` and this test hangs to its harness timeout instead of
/// returning inside `budget + REAP_BOUND`; the mutation receipts recorded for this round do exactly
/// that, on the shipped enforcement path rather than on a lookalike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_parked_in_real_local_packing_is_stopped_at_its_deadline_before_any_pack_upload()
{
    let _trust = exclusive_trust();
    let root = scratch("parked-walk");
    let branch = "maxplayer/eeee1111";
    let (workdir, oid, tree) = job_workdir(&root, branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&bare, "/git/seller/r.git");
    stage_env(&relay.ca_file(&root));

    // The commit still resolves — a gated-object lookup never reads the tree — but the packbuilder's
    // walk will block on this FIFO forever.
    let fifo = park_object_on_a_fifo(&workdir, tree);

    // The parent mints; the key never crosses the pipe. Counting the asks is how this test knows the
    // child got as far as the authenticated advertisement.
    let mints = Arc::new(AtomicUsize::new(0));
    let asked = Arc::clone(&mints);
    let minter: AuthMinter = Arc::new(move |_| {
        asked.fetch_add(1, Ordering::SeqCst);
        Ok("Nostr fixture-token".to_owned())
    });

    // The production number is DELIVERY_PUSH_TIMEOUT (150s); this is the same arithmetic at a scale
    // a gate can run. What is asserted is the SHAPE — deadline, then at most one reap window.
    let budget = Duration::from_millis(2_500);
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), Instant::now() + budget);

    let unconfirmed_before = maxplayer_core::delivery_executor::unconfirmed_children();
    let started = Instant::now();
    let outcome = neutralize_then_push_in_child_off_runtime(
        shipped_binary(),
        workdir.clone(),
        relay.repo_url(),
        branch.to_owned(),
        oid.clone(),
        Some(minter),
        None,
        turn,
    )
    .await;
    let elapsed = started.elapsed();

    let error = match outcome {
        Err(SellerGitError::Cancelled(error)) => error,
        other => panic!(
            "a child parked in libgit2's object walk must be killed, not awaited: {other:?}"
        ),
    };
    assert!(
        error.contains("was killed") && error.contains("confirmed the exit"),
        "the refusal must say the child was killed AND that its exit was confirmed: {error}"
    );

    // THE BOUND, MEASURED. It waited its whole budget — so the stop is the deadline's doing and not
    // an early give-up — and returned inside one reap window after it.
    assert!(
        elapsed >= budget,
        "returned before the deadline it was given: {elapsed:?} < {budget:?}"
    );
    assert!(
        elapsed < budget + REAP_BOUND,
        "the seat's turn was held {elapsed:?}, past its bound of {:?}",
        budget + REAP_BOUND
    );

    // THE PHASE. The child authenticated and read the advertisement, so it was inside the push; and
    // it never issued the upload, because it never finished walking the objects. This is the whole
    // difference between this gate and the wire holds: a POST here would mean the stall was HTTP.
    let seen: Vec<String> = relay
        .requests()
        .iter()
        .map(|request| format!("{} {}", request.method, request.target))
        .collect();
    assert!(
        seen.iter().any(|line| line.contains("/info/refs")),
        "the child never reached the advertisement, so it was never in the packing phase: {seen:?}"
    );
    assert!(
        !seen.iter().any(|line| line.contains("git-receive-pack")
            && line.starts_with("POST")),
        "a pack upload was issued: this delivery stalled on the WIRE, not in local packing: {seen:?}"
    );
    assert!(
        mints.load(Ordering::SeqCst) >= 1,
        "the child never asked the parent to authorize a leg, so it never got into the push"
    );

    // AND NOTHING WAS DELIVERED.
    let remote = git2::Repository::open_bare(&bare).expect("open bare");
    assert!(
        remote
            .find_reference(&format!("refs/heads/{branch}"))
            .is_err(),
        "the remote ref moved for a delivery that never finished packing"
    );

    // The turn comes back, and only because the work stopped.
    assert!(control.work_ended(), "the work must be recorded as ended");
    control.end();
    assert!(
        !control.holds_ownership() && released.load(Ordering::SeqCst),
        "the exclusion token was not handed back after a confirmed exit"
    );
    assert_eq!(
        maxplayer_core::delivery_executor::unconfirmed_children(),
        unconfirmed_before,
        "a child this gate saw confirmed dead was counted as unconfirmed"
    );

    // The FIFO is still a FIFO with no writer: nothing in this test released the stall, so the stop
    // was the executor's and not the phase finishing on its own.
    let meta = std::fs::metadata(&fifo).expect("stat the parked object");
    assert!(
        std::os::unix::fs::FileTypeExt::is_fifo(&meta.file_type()),
        "the parked object stopped being a FIFO during the run: the stall was not what ended"
    );

    // AND THE PARKED PROCESS IS ACTUALLY GONE — the difference between a bound on the parent's
    // patience and a bound on the work. A `kill_and_reap` that reported success without signalling
    // would satisfy every assertion above and fail this one, because its child would still be
    // holding this object open inside libgit2's walk.
    assert!(
        !a_reader_is_still_parked_on(&fifo),
        "a process is STILL parked on this delivery's object after the executor reported a \
         confirmed exit: the local phase outlived its turn"
    );
}

/// THE SENSITIVITY CONTROL for the gate above, and it is not optional.
///
/// Everything identical — same shipped binary, same fixture, same budget, same minter, same workdir
/// construction — except that the tree object is left alone. If this delivery did not SUCCEED, and
/// quickly, the gate above would be proving only that some unrelated misconfiguration stops a push,
/// and the FIFO would be decoration. It delivers, it POSTs, and the remote ref moves: so the stall
/// above is the parked object and nothing else in the setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_delivery_without_the_parked_object_packs_and_lands_well_inside_the_same_budget() {
    let _trust = exclusive_trust();
    let root = scratch("unparked-walk");
    let branch = "maxplayer/eeee2222";
    let (workdir, oid, tree) = job_workdir(&root, branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&bare, "/git/seller/r.git");
    stage_env(&relay.ca_file(&root));

    // The one difference: the tree stays a regular loose object.
    assert!(
        loose_object_path(&workdir, tree).is_file(),
        "the control must run against an intact object database"
    );

    let mints = Arc::new(AtomicUsize::new(0));
    let asked = Arc::clone(&mints);
    let minter: AuthMinter = Arc::new(move |_| {
        asked.fetch_add(1, Ordering::SeqCst);
        Ok("Nostr fixture-token".to_owned())
    });

    let budget = Duration::from_millis(2_500);
    let released = Arc::new(AtomicBool::new(false));
    let (_control, turn) = delivery_turn(Token(Arc::clone(&released)), Instant::now() + budget);

    let started = Instant::now();
    let pushed = neutralize_then_push_in_child_off_runtime(
        shipped_binary(),
        workdir,
        relay.repo_url(),
        branch.to_owned(),
        oid.clone(),
        Some(minter),
        None,
        turn,
    )
    .await
    .expect("the same delivery, with an intact object database, must land");
    let elapsed = started.elapsed();

    assert_eq!(pushed, oid, "the child delivered a different object");
    assert!(
        elapsed < budget,
        "the control delivery took {elapsed:?}, which is not 'well inside' a {budget:?} budget: \
         the gate above cannot then attribute its stop to the parked object"
    );

    // It PACKED and it UPLOADED — the two things the parked run must not be able to do.
    let seen: Vec<String> = relay
        .requests()
        .iter()
        .map(|request| format!("{} {}", request.method, request.target))
        .collect();
    assert!(
        seen.iter()
            .any(|line| line.starts_with("POST") && line.contains("git-receive-pack")),
        "the control never issued a pack upload, so 'no POST' above proves nothing: {seen:?}"
    );
    assert_eq!(
        git2::Repository::open_bare(&bare)
            .expect("open bare")
            .find_reference(&format!("refs/heads/{branch}"))
            .expect("the control must move the remote ref")
            .target()
            .map(|oid| oid.to_string())
            .as_deref(),
        Some(oid.as_str()),
        "the remote ref did not move in the control delivery"
    );
}
