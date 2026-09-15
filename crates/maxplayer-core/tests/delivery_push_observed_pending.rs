//! A REAL held local phase, driven through the production serializer, with a SECOND REAL DELIVERY
//! OBSERVED PENDING while it is held.
//!
//! Round 1 was failed here for a reason worth restating: a `try_lock` that comes back `Err`, or a
//! "Requested" marker set by the test itself, proves that the lock is held. It does not prove that a
//! second DELIVERY is waiting on it, because no second delivery ever ran. These gates run the real
//! [`serialized_bounded_push`] — the seat's own serializer, on the seat's own lock type — twice,
//! concurrently, and SAMPLE the second delivery's state repeatedly while the first is held. Pending
//! is observed, not inferred, and the handover instant is compared against the first delivery's
//! return instant rather than assumed to follow it.
//!
//! What the first delivery is doing while it holds the turn is the shape libgit2's delta search puts
//! the seat in: a child that ignores `SIGTERM`, never speaks again, and would hold this seat's one
//! delivery remote forever if the deadline could not reach it.
//!
//! **Tokens cross the pipe; the private key does not.** The parent mints and writes a scoped header
//! to the child, so a token DOES traverse this IPC channel — that is the point of the round trip.
//! The signing key stays in the actor the parent calls. Both halves of that sentence are load
//! bearing and neither is weakened by the other.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use maxplayer_core::seller_git::{neutralize_then_push_in_child_off_runtime, SellerGitError};
use maxplayer_core::seller_node::run::{serialized_bounded_push, DeliveryPushErr};

/// Where the second delivery has got to. Written only by the second delivery's own task, and only
/// forward, so a sample that reads PENDING is a fact about that task and not about the sampler.
const NOT_STARTED: u8 = 0;
const PENDING: u8 = 1;
const ACQUIRED: u8 = 2;

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mp-obs-{}-{}-{}",
        label,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn fixture(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("child.sh");
    let mut file = std::fs::File::create(&path).expect("create fixture");
    write!(file, "#!/bin/sh\n{body}").expect("write fixture");
    drop(file);
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

const HELLO: &str = r#"printf '{"t":"Hello","version":1,"argv":[],"env":{}}\n'"#;

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// ONE real poll of a real future, and the `Poll` it returned handed straight back.
///
/// This is the difference the verdict asked for. A marker the test stores before awaiting proves
/// the test reached a line; `Future::poll` returning [`Poll::Pending`] is the serializer's own
/// answer to "may this delivery have the seat", taken from the future under test rather than
/// inferred around it. The waker is the real one the surrounding runtime supplies, so a future that
/// was ready would say so here.
async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(move |cx| Poll::Ready(future.as_mut().poll(cx))).await
}

/// The first delivery's local phase refuses to stop; the second delivery is watched sitting Pending
/// on the seat's real lock, and takes the turn only after the first one's child is killed and reaped.
///
/// This is F1 and the contention matrix in one run, because separating them is what let round 1
/// claim a bound nobody had watched a second delivery wait out.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_second_delivery_is_observed_pending_until_the_held_local_phase_is_killed_and_reaped() {
    let dir = scratch("pending");
    let pidfile = dir.join("child.pid");
    let program = fixture(
        &dir,
        &format!(
            "trap '' TERM\necho $$ > {}\n{HELLO}\nwhile :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    );

    // The seat's own lock, the seat's own serializer. Nothing here is a stand-in.
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let budget = Duration::from_millis(2_000);

    // The tokio timeout is set FAR past the child's deadline on purpose: if this gate passed because
    // `serialized_bounded_push` timed out, it would prove tokio can abandon a push, which is exactly
    // the non-repair the verdict refused. The only thing that can end delivery one inside this
    // timeout is the executor killing and reaping its child.
    let generous = Duration::from_secs(30);

    let second_state = Arc::new(AtomicU8::new(NOT_STARTED));
    let second_acquired_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

    let first = {
        let lock = Arc::clone(&lock);
        let program = program.clone();
        let workdir = dir.join("workdir-one");
        tokio::spawn(async move {
            let started = Instant::now();
            let outcome = serialized_bounded_push(
                &lock,
                generous,
                Instant::now() + budget,
                move |turn| async move {
                    neutralize_then_push_in_child_off_runtime(
                        program,
                        workdir,
                        "https://relay.example.invalid/seller.git".to_owned(),
                        "delivery/one".to_owned(),
                        "0123456789012345678901234567890123456789".to_owned(),
                        None,
                        None,
                        turn,
                    )
                    .await
                },
            )
            .await;
            (outcome, started, Instant::now())
        })
    };

    // Let delivery one take the lock and get its child wedged before delivery two asks for it, so
    // that "pending" means "queued behind a held turn" and not "raced and won".
    tokio::time::sleep(Duration::from_millis(300)).await;
    let wedged_pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("the first delivery's child must have started and recorded its pid")
        .trim()
        .parse()
        .expect("pid");
    assert!(
        alive(wedged_pid),
        "the first delivery's local phase must actually be running before we queue a second"
    );

    // OBSERVED PENDING — the real one, and the reason this file was rewritten. The second delivery
    // is no longer spawned onto another task and watched through a marker the test itself wrote
    // before the call. Its future is held HERE and POLLED, and what every assertion below reads is
    // the `Poll` that `serialized_bounded_push` returned. `Poll::Pending` out of the seat's own
    // serializer, while delivery one's child is demonstrably alive, is the fact that was ordered.
    let second = serialized_bounded_push(&lock, generous, Instant::now() + Duration::from_secs(20), {
        let state = Arc::clone(&second_state);
        let at = Arc::clone(&second_acquired_at);
        move |turn| async move {
            // Reached ONLY with the turn in hand: `serialized_bounded_push` builds it from the
            // acquired guard, so this line cannot run while delivery one owns the seat.
            at.lock().expect("clock").replace(Instant::now());
            state.store(ACQUIRED, Ordering::SeqCst);
            drop(turn);
            Ok::<_, SellerGitError>("second-delivery-oid".to_owned())
        }
    });
    tokio::pin!(second);

    // The FIRST poll is the ask: it is what drives the future far enough to reach for the lock.
    // There is no window here in which the second delivery has not yet asked, which is what the old
    // `await_asked` spin existed to cover.
    assert!(
        poll_once(second.as_mut()).await.is_pending(),
        "the second delivery's very first poll returned Ready while delivery one held the seat"
    );
    second_state.store(PENDING, Ordering::SeqCst);

    let mut samples = 0usize;
    let watch_until = Instant::now() + Duration::from_millis(1_200);
    while Instant::now() < watch_until {
        assert!(
            poll_once(second.as_mut()).await.is_pending(),
            "the second delivery's future returned Ready while the first one's local phase was \
             still running"
        );
        assert_eq!(
            second_state.load(Ordering::SeqCst),
            PENDING,
            "the second delivery's push body ran while delivery one held the seat"
        );
        assert!(
            alive(wedged_pid),
            "the first delivery's child must still be alive while we observe the second waiting"
        );
        samples += 1;
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        samples >= 20,
        "too few observed Poll::Pending returns to call it observed: {samples}"
    );

    let (outcome, started, returned) = first.await.expect("first delivery task");
    let held = returned.saturating_duration_since(started);

    // Delivery one ended the way F1 demands: killed at its own deadline, exit confirmed.
    match outcome {
        Err(DeliveryPushErr::Push(SellerGitError::Cancelled(why))) => {
            assert!(
                why.contains("was killed") && why.contains("confirmed the exit"),
                "delivery one must report the kill AND the confirmed exit: {why}"
            );
        }
        other => panic!("delivery one must be killed at its deadline, not awaited: {other:?}"),
    }
    assert!(
        held >= budget && held < budget + Duration::from_secs(5),
        "delivery one held the seat for {held:?}, outside its budget {budget:?} + reap bound"
    );
    assert!(
        !alive(wedged_pid),
        "the killed child must be gone before the seat is handed on"
    );

    // Same future, now driven to completion by the ordinary await.
    let second_outcome = second.await;
    assert_eq!(
        second_outcome.expect("the second delivery must get the seat once the first stops"),
        "second-delivery-oid"
    );
    assert_eq!(second_state.load(Ordering::SeqCst), ACQUIRED);

    // The handover is ORDERED, not merely eventual: the second delivery entered its push body after
    // the first delivery's call returned, which is after the reap.
    let acquired_at = second_acquired_at.lock().expect("clock").expect("acquired");

    // THE NUMBERS, PRINTED. `cargo test ... -- --nocapture` reproduces the measurement rather than
    // the claim: how long the wedged delivery actually held the seat against its stated budget, and
    // how long the handover to the delivery that was waiting for it actually took.
    eprintln!(
        "MEASURED budget={:?} held={:?} overrun={:?} handover={:?} samples_pending={} reap_bound={:?}",
        budget,
        held,
        held.saturating_sub(budget),
        acquired_at.saturating_duration_since(returned),
        samples,
        Duration::from_secs(5),
    );
    assert!(
        acquired_at >= returned,
        "the second delivery entered its push body before the first delivery returned"
    );
    assert!(
        acquired_at.saturating_duration_since(returned) < Duration::from_secs(2),
        "the second delivery waited far longer than the handover itself"
    );
}

/// The negative control for the gate above: when delivery one SUCCEEDS, delivery two is observed
/// pending exactly the same way and is let in on the same rule. Without this, the gate above could
/// be satisfied by a seat that only ever hands over after a kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_second_delivery_is_observed_pending_behind_a_first_that_succeeds() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let generous = Duration::from_secs(30);
    let state = Arc::new(AtomicU8::new(NOT_STARTED));
    let (release, held) = tokio::sync::oneshot::channel::<()>();

    let first = {
        let lock = Arc::clone(&lock);
        tokio::spawn(async move {
            serialized_bounded_push(
                &lock,
                generous,
                Instant::now() + Duration::from_secs(20),
                move |turn| async move {
                    // Held open until the test says otherwise, so the window in which the second
                    // delivery is observed pending is the test's to control.
                    let _ = held.await;
                    drop(turn);
                    Ok::<_, SellerGitError>("first-delivery-oid".to_owned())
                },
            )
            .await
        })
    };

    tokio::time::sleep(Duration::from_millis(150)).await;

    // The same real-poll oracle as the gate above: the second delivery's future is held here and
    // POLLED, so "pending" is the serializer's answer and not a marker this test set.
    let second = serialized_bounded_push(&lock, generous, Instant::now() + Duration::from_secs(20), {
        let state = Arc::clone(&state);
        move |turn| async move {
            state.store(ACQUIRED, Ordering::SeqCst);
            drop(turn);
            Ok::<_, SellerGitError>("second-delivery-oid".to_owned())
        }
    });
    tokio::pin!(second);

    assert!(
        poll_once(second.as_mut()).await.is_pending(),
        "the second delivery's first poll returned Ready while the first still held the seat"
    );
    state.store(PENDING, Ordering::SeqCst);

    let mut samples = 0usize;
    let watch_until = Instant::now() + Duration::from_millis(600);
    while Instant::now() < watch_until {
        assert!(
            poll_once(second.as_mut()).await.is_pending(),
            "the second delivery's future returned Ready while the first was still in its push body"
        );
        assert_eq!(
            state.load(Ordering::SeqCst),
            PENDING,
            "the second delivery's push body ran while the first still held the seat"
        );
        samples += 1;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        samples >= 10,
        "too few observed Poll::Pending returns to call it observed: {samples}"
    );

    release.send(()).expect("release the first delivery");
    assert_eq!(
        first.await.expect("first task").expect("first delivery"),
        "first-delivery-oid"
    );
    assert_eq!(
        second.await.expect("second delivery"),
        "second-delivery-oid"
    );
    assert_eq!(state.load(Ordering::SeqCst), ACQUIRED);
}
