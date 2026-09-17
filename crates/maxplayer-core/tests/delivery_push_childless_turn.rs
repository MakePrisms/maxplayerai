//! A delivery refused BETWEEN taking the turn and spawning the child must hand that turn back —
//! and the proof is a SECOND REAL DELIVERY taking it.
//!
//! The window is small and it was permanent. `neutralize_then_push_in_child_off_runtime` calls
//! `turn.begin()`, installs the custody guard, and only then asks the two questions that can refuse
//! the delivery: is this still authorized, and is there time left. Both refuse with `?`, through the
//! guard's `Drop` — and that `Drop` retained unconditionally. So a delivery cancelled in a window
//! where NO CHILD EXISTS closed this seat's delivery lane for the life of the process, over a
//! process that was never created. Retention is what an unknown child costs; there was no child, and
//! nothing to be unknown about.
//!
//! These gates therefore do not assert on a flag or a log line. They run the seat's real serializer
//! twice on one lock, and the second delivery either gets the seat or it does not.

#![cfg(feature = "git-delivery")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maxplayer_core::delivery_turn::delivery_turn;
use maxplayer_core::git_transport::AuthorityCheck;
use maxplayer_core::seller_git::{neutralize_then_push_in_child_off_runtime, SellerGitError};
use maxplayer_core::seller_node::run::{serialized_bounded_push, DeliveryPushErr};

const OID: &str = "0123456789012345678901234567890123456789";
const REMOTE: &str = "https://relay.example.invalid/seller.git";

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mp-childless-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn fixture(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("create fixture");
    write!(file, "#!/bin/sh\n{body}").expect("write fixture");
    drop(file);
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

const HELLO: &str = r#"printf '{"t":"Hello","version":1,"argv":[],"env":{}}\n'"#;

/// A child that touches a file, finishes the protocol properly and exits. Its existence is the
/// evidence a child ran; its `Done` is the evidence the second delivery completed.
fn cooperating_child(dir: &Path, ran: &Path) -> PathBuf {
    fixture(
        dir,
        "cooperating.sh",
        &format!(
            "{HELLO}\nIFS= read -r _request\n: > {}\nprintf '{{\"t\":\"Done\",\"oid\":\"{OID}\",\"error\":null}}\\n'\n",
            ran.display()
        ),
    )
}

/// A child that records that it started. Nothing in this file may ever see this file appear.
fn recording_child(dir: &Path, ran: &Path) -> PathBuf {
    fixture(
        dir,
        "recording.sh",
        &format!("echo $$ > {}\nsleep 30\n", ran.display()),
    )
}

/// The exclusion token the turn carries — the stand-in for the delivery lock's owned guard. Its
/// `Drop` is the seat becoming free.
struct Token(Arc<AtomicBool>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// An authority that is live until the test says otherwise.
fn authority_of(live: &Arc<AtomicBool>) -> AuthorityCheck {
    let live = Arc::clone(live);
    Arc::new(move || {
        if live.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("the owner of this delivery went away".to_owned())
        }
    })
}

/// The window itself, at the finest grain available: the turn was BEGUN (the control says so), no
/// child was spawned (nothing recorded), and the exclusion token was dropped (the seat is free).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_after_begin_and_before_spawn_releases_a_childless_turn() {
    let dir = scratch("window");
    let ran = dir.join("child.pid");
    let program = recording_child(&dir, &ran);

    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(
        Token(Arc::clone(&released)),
        Instant::now() + Duration::from_secs(30),
    );
    // Live at dispatch — `begin()` must succeed — and refusing by the time the pre-spawn gate asks.
    // This is the whole point: the refusal is INSIDE the guarded window, not before it.
    let live = Arc::new(AtomicBool::new(false));

    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        REMOTE.to_owned(),
        "delivery/job".to_owned(),
        OID.to_owned(),
        None,
        Some(authority_of(&live)),
        turn,
    )
    .await;

    assert!(
        matches!(outcome, Err(SellerGitError::Cancelled(_))),
        "a revoked delivery must be refused: {outcome:?}"
    );
    assert!(
        control.work_started(),
        "this gate is about the window AFTER begin; if the turn was never begun it proves nothing"
    );
    assert!(
        !ran.exists(),
        "a delivery refused before the spawn started a child anyway"
    );
    // THE DISCRIMINATOR. The work side publishes "ended" by DROPPING its `RunningWork`; the guard
    // that retains does so by never dropping it. So a turn that was begun and is not ended is a
    // turn this process has decided to keep forever — whatever anything else reports.
    assert!(
        control.work_ended(),
        "the turn was begun and never ended: a childless refusal retained this seat for the life \
         of the process, over a child that was never created"
    );
    // And the supervising side finishing is then enough to free the exclusion token, which is what
    // the next delivery actually needs. While the work half is retained this can never happen.
    control.end();
    assert!(
        released.load(Ordering::SeqCst),
        "the exclusion token was not handed back after a childless refusal"
    );
    assert!(
        !control.holds_ownership(),
        "the turn's ownership token is still held after a childless refusal"
    );
}

/// The regression as the seat actually experiences it: one lock, the real serializer, a first
/// delivery revoked in the pre-spawn window, and a SECOND REAL DELIVERY that has to get the seat and
/// run its child. Not a simulated acquisition — the second push spawns a process and returns an oid.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_delivery_takes_the_seat_after_a_revoked_childless_turn() {
    let dir = scratch("next-after-revoke");
    let never = dir.join("never.pid");
    let refused_program = recording_child(&dir, &never);
    let ran = dir.join("second.ran");
    let second_program = cooperating_child(&dir, &ran);

    // THE SEAT'S ONE DELIVERY LOCK, and the seat's own serializer around it.
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let live = Arc::new(AtomicBool::new(false));
    let workdir_one = dir.join("workdir-one");
    let workdir_two = dir.join("workdir-two");

    let first = serialized_bounded_push(
        &lock,
        Duration::from_secs(20),
        Instant::now() + Duration::from_secs(30),
        move |turn| async move {
            neutralize_then_push_in_child_off_runtime(
                refused_program,
                workdir_one,
                REMOTE.to_owned(),
                "delivery/one".to_owned(),
                OID.to_owned(),
                None,
                Some(authority_of(&live)),
                turn,
            )
            .await
        },
    )
    .await;

    assert!(
        matches!(
            first,
            Err(DeliveryPushErr::Push(SellerGitError::Cancelled(_)))
        ),
        "the first delivery must be refused in the pre-spawn window: {first:?}"
    );
    assert!(!never.exists(), "the refused delivery spawned a child");

    // THE REAL NEXT ACQUISITION. Same lock, same serializer, a child that actually runs. If the
    // refused delivery kept the seat, this waits out the serializer's timeout and comes back
    // `TimedOut` instead of an oid.
    let started = Instant::now();
    let second = serialized_bounded_push(
        &lock,
        Duration::from_secs(20),
        Instant::now() + Duration::from_secs(30),
        move |turn| async move {
            neutralize_then_push_in_child_off_runtime(
                second_program,
                workdir_two,
                REMOTE.to_owned(),
                "delivery/two".to_owned(),
                OID.to_owned(),
                None,
                None,
                turn,
            )
            .await
        },
    )
    .await;

    assert_eq!(
        second.expect("the second delivery must get the seat and report its oid"),
        OID,
        "the second delivery did not complete on a seat that should have been free"
    );
    assert!(
        ran.exists(),
        "the second delivery returned without its child ever running"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the second delivery waited on a seat nobody was using: {:?}",
        started.elapsed()
    );
}

/// The same window, reached by EXPIRY rather than revocation. The lifetime check is the second of
/// the two pre-spawn gates and it returns through the same guard; a delivery whose clock runs out
/// while it is queued must also leave the seat usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_delivery_takes_the_seat_after_an_expired_childless_turn() {
    let dir = scratch("next-after-expiry");
    let never = dir.join("never.pid");
    let expired_program = recording_child(&dir, &never);
    let ran = dir.join("second.ran");
    let second_program = cooperating_child(&dir, &ran);

    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let workdir_one = dir.join("workdir-one");
    let workdir_two = dir.join("workdir-two");

    // A delivery with 400ms to live, and an authority call that takes longer than that. `begin()`
    // succeeds; by the time the lifetime gate is asked, this delivery's time is gone.
    let slow_authority: AuthorityCheck = Arc::new(|| {
        std::thread::sleep(Duration::from_millis(800));
        Ok(())
    });

    let first = serialized_bounded_push(
        &lock,
        Duration::from_secs(20),
        Instant::now() + Duration::from_millis(400),
        move |turn| async move {
            neutralize_then_push_in_child_off_runtime(
                expired_program,
                workdir_one,
                REMOTE.to_owned(),
                "delivery/one".to_owned(),
                OID.to_owned(),
                None,
                Some(slow_authority),
                turn,
            )
            .await
        },
    )
    .await;

    assert!(
        matches!(
            first,
            Err(DeliveryPushErr::Push(SellerGitError::Cancelled(_)))
        ),
        "a delivery whose clock ran out before the spawn must be cancelled: {first:?}"
    );
    assert!(!never.exists(), "the expired delivery spawned a child");

    let started = Instant::now();
    let second = serialized_bounded_push(
        &lock,
        Duration::from_secs(20),
        Instant::now() + Duration::from_secs(30),
        move |turn| async move {
            neutralize_then_push_in_child_off_runtime(
                second_program,
                workdir_two,
                REMOTE.to_owned(),
                "delivery/two".to_owned(),
                OID.to_owned(),
                None,
                None,
                turn,
            )
            .await
        },
    )
    .await;

    assert_eq!(
        second.expect("the second delivery must get the seat and report its oid"),
        OID,
        "an expired childless delivery kept this seat"
    );
    assert!(
        ran.exists(),
        "the second delivery returned without its child ever running"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the second delivery waited on a seat nobody was using: {:?}",
        started.elapsed()
    );
}
