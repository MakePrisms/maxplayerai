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

// Gated like its siblings: everything below needs `seller_node` and the delivery executor, which
// exist only with the `wallet` feature (and `libc`, which comes with it).
#![cfg(feature = "wallet")]

mod wedge;

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::{CANCELLATION_POLL, REAP_BOUND};
use maxplayer_core::git_transport::AuthorityCheck;
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

/// What the CHILD is allowed for coming up, kept OUT of the budget whose bound is being measured —
/// the same allowance, for the same reason, as `delivery_push_production_child::CHILD_STARTUP`.
///
/// Every deadline in this file is fixed BEFORE its child exists, because that is the executor's
/// arming contract. So a shell that is slow to reach its first line spends the delivery's budget,
/// and the watch below — 1.2 s from the moment the child is seen running — used to have only the
/// budget's remainder to fit in. Once startup ate 0.8 s of a 2 s budget, the watch outlived the
/// deadline it was watching and read the deadline's own kill as the second delivery going Ready
/// early (gate run 7 at 53d1322, on an otherwise idle host). Every bound below is therefore stated
/// relative to the DEADLINE, and the deadline carries this allowance in front of the budget.
const CHILD_STARTUP: Duration = Duration::from_secs(10);

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
    let hold = wedge::wedge(&dir);
    let pidfile = dir.join("child.pid");
    let program = fixture(
        &dir,
        &format!(
            "trap '' TERM\necho $$ > {}\n{HELLO}\n{hold}",
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

    // The deadline as an INSTANT, fixed here before the child exists, with the startup allowance in
    // front of the budget. Every bound on delivery one below is measured against it.
    let deadline = Instant::now() + CHILD_STARTUP + budget;

    let first = {
        let lock = Arc::clone(&lock);
        let program = program.clone();
        let workdir = dir.join("workdir-one");
        tokio::spawn(async move {
            let started = Instant::now();
            let outcome = serialized_bounded_push(
                &lock,
                generous,
                deadline,
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
    //
    // WAITED FOR, not slept at. A fixed sleep here reads as a setup convenience and is really an
    // assumption about how fast this machine forks under load: the whole workspace suite runs these
    // binaries in parallel, and a 300ms sleep failed there while passing alone. The condition is
    // unchanged - the first child is RUNNING before a second delivery is queued - it is just now
    // established by observing it rather than by guessing a duration. The bound still fails the
    // test if the child never starts.
    let wedged_pid = wait_for_running_child(&pidfile, Duration::from_secs(20))
        .await
        .expect(
            "the first delivery's child must have started and recorded its pid before a second \
             delivery is queued behind it",
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
    // THE BOUND, AGAINST THE DEADLINE. Not before it — the kill is the deadline's doing, not an
    // early give-up — and within the executor's two windows after it: the reap, and end of file on
    // the child's stdout. Startup happens before the deadline and is deliberately not bounded here.
    assert!(
        returned >= deadline,
        "delivery one returned {:?} before the deadline it was given",
        deadline.saturating_duration_since(returned)
    );
    assert!(
        returned.saturating_duration_since(deadline) < 2 * REAP_BOUND,
        "delivery one held the seat for {:?} past its deadline, beyond the reap and end-of-file \
         windows this executor states",
        returned.saturating_duration_since(deadline)
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
        "MEASURED budget={:?} startup_allowance={:?} held={:?} past_deadline={:?} handover={:?} \
         samples_pending={} reap_bound={:?}",
        budget,
        CHILD_STARTUP,
        held,
        returned.saturating_duration_since(deadline),
        acquired_at.saturating_duration_since(returned),
        samples,
        REAP_BOUND,
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


/// Wait until `pidfile` names a process that is actually alive, or give up at `bound`.
///
/// Used where a test needs a child to be RUNNING before it does the next thing. Polling the real
/// condition keeps the gate honest under load — a machine that forks slowly makes this take longer,
/// not make it pass early — while a timeout keeps a child that never starts a failure rather than a
/// hang.
async fn wait_for_running_child(pidfile: &std::path::Path, bound: Duration) -> Option<i32> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(pid) = std::fs::read_to_string(pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            if alive(pid) {
                return Some(pid);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Holds the executor inside this delivery's authority check, on the executor's own thread, until
/// the test lets it go.
///
/// The composed gate in `seller_git` asks the delivery's authority FIRST and the turn's lifetime
/// SECOND, so an ask that has not returned is an executor that has not yet consulted the turn and
/// cannot yet kill. That is the executor's documented slow-owner case — the `S` term in its bounds
/// — and this test uses it as a synchronization point rather than a fault: while the ask is held,
/// the child is alive and cannot die, and a `Poll::Pending` taken then is a fact about a live child.
///
/// The hold has a safety bound of its own, so a failed assertion in the test cannot leave the
/// executor's blocking thread parked for the life of the runtime; a hold that ran out is reported
/// and fails the test rather than passing it.
#[derive(Default)]
struct AskHold {
    state: Mutex<AskHoldState>,
    released: Condvar,
}

#[derive(Default)]
struct AskHoldState {
    armed: bool,
    open: bool,
    held_at: Option<Instant>,
    asks: Vec<Instant>,
    expired: bool,
}

impl AskHold {
    /// Called by the authority closure, on the executor's thread, at every ask.
    fn enter(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.asks.push(Instant::now());
        if !state.armed || state.open {
            return;
        }
        state.held_at.get_or_insert(Instant::now());
        let bound = Instant::now() + Duration::from_secs(30);
        while !state.open {
            let left = bound.saturating_duration_since(Instant::now());
            if left.is_zero() {
                state.expired = true;
                return;
            }
            state = self
                .released
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// From now on the next ask is held open. Returns the instant of arming.
    fn arm(&self) -> Instant {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .armed = true;
        Instant::now()
    }

    fn held_at(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .held_at
    }

    /// Let the held ask return. Returns the instant of release.
    fn release(&self) -> Instant {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.open = true;
        }
        self.released.notify_all();
        Instant::now()
    }

    fn asks(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .asks
            .len()
    }

    fn expired(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .expired
    }
}

/// Wait until the executor is inside the held ask, or give up at `bound`.
async fn wait_until_held(hold: &AskHold, bound: Duration) -> Instant {
    let give_up = Instant::now() + bound;
    loop {
        if let Some(held_at) = hold.held_at() {
            return held_at;
        }
        assert!(
            Instant::now() < give_up,
            "the executor never asked about this delivery's authority again after the hold was armed"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// C: A REAL TASK-ABORT, AND A SECOND DELIVERY OBSERVED PENDING THROUGH IT.
///
/// **This is not a revocation.** No authority ends, nothing tells the delivery to stop, and the
/// difference is the point: revocation is the OWNER withdrawing and the executor acting on it, and
/// the two must not be argued for one another. Here the delivery's own tokio task is simply
/// destroyed mid-flight — the shape of a cancelled request, a dropped select branch, a shutting-down
/// supervisor — while its child is running and holding the seat.
///
/// The seat's exclusion is an `OwnedMutexGuard` moved into the turn, which is moved into the
/// blocking call that runs the delivery. So an abort takes the awaiting task and leaves the work:
/// the guard is not the aborter's to drop. The property that must hold, and had no gate, is that
/// this is FAIL-CLOSED — a second delivery must not be let onto a seat whose previous child is
/// still alive, however the first delivery's task died.
///
/// What was MEASURED here, and it is better than the fail-closed minimum: the abort drops the
/// delivery's turn control, the executor sees that at its next check, and the child is killed and
/// reaped promptly — the seat comes back in a fraction of the remaining budget rather than at the
/// deadline. So the gate asserts both halves, and the second one is what stops this from being a
/// test that would pass on a seat that simply leaks: the handover must happen AFTER the child is
/// gone, and the cleanup must run long BEFORE the deadline that would otherwise have ended it.
///
/// # Observed pending is established by synchronization, not by racing the cleanup
///
/// The executor asks this delivery's authority on every poll and consults the turn only after that
/// ask has returned (`seller_git` composes the gate as authority first, lifetime second). This test
/// passes its own authority closure — always yes, as production's `PushAuthority` is during an
/// abort — and, once the abort is issued, HOLDS the executor's next ask open ([`AskHold`]). While
/// the ask is held the child is alive and cannot be killed, so every `Poll::Pending` the second
/// delivery returns in that window is a fact about a seat held by a live child. An earlier shape of
/// this test observed the child alive and then polled, assuming the reap could not complete in
/// between; a `try_wait` that succeeds at once and a scheduler gap both defeat that assumption, and
/// the same gap sat in its sampling loop. Nothing here assumes a minimum cleanup duration.
///
/// Then the ask is released, and everything that follows is the executor's: the turn check
/// refuses, the child is killed, its exit is confirmed, and the seat is handed on. Deadline cleanup
/// cannot issue its kill before the deadline, so the discriminator is what happens after the
/// release, measured against what the delivery still had at that instant.
///
/// # What each measured interval is, stated as narrowly as it is true
///
/// - `armed_to_ask`: from arming the hold to the executor's next authority ask. The executor's poll
///   cadence, and only that. It is an authority CHECK, not a kill: the signal is issued after the
///   check returns and the turn refuses, under the executor's child guard.
/// - `released_to_child_gone`: from the release of the ask to the sampler first seeing the child
///   gone. The refused check, the kill, the reap that confirms the exit (within [`REAP_BOUND`]) and
///   the sampler's own lag, together. The child disappearing is what is observed; no signal instant
///   is claimed.
/// - `released_to_handover`: from the release to the second delivery entering its push body. The
///   above plus end of file on the child's stdout (a second window the executor states as up to
///   [`REAP_BOUND`]) and the scheduling of the threads that carry those facts.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn an_aborted_delivery_task_does_not_hand_the_seat_on_while_its_child_still_runs() {
    let dir = scratch("aborted");
    let hold = wedge::wedge(&dir);
    let pidfile = dir.join("child.pid");
    // Ignores TERM and never speaks again: only the executor's kill-and-reap ends this. ONE process,
    // so the group kill has exactly one member to reach.
    let program = fixture(
        &dir,
        &format!("trap '' TERM\necho $$ > {}\n{HELLO}\n{hold}", pidfile.display()),
    );

    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let budget = Duration::from_millis(3_000);
    let generous = Duration::from_secs(30);
    // THE ORIGINAL DEADLINE AS AN INSTANT, taken out here rather than inside the task. What
    // discriminates prompt abort cleanup from deadline cleanup is the time that was still LEFT on
    // this deadline when the executor was let act, and that needs the deadline itself. The startup
    // allowance sits in front of the budget, so a slow shell cannot spend the remainder the
    // discrimination is measured against.
    let deadline = Instant::now() + CHILD_STARTUP + budget;

    // THE TEST'S OWN AUTHORITY: never refuses — this is an abort, not a revocation, and only the
    // turn may end the work — but once armed it holds the executor's next ask open.
    let ask_hold = Arc::new(AskHold::default());
    let authority: AuthorityCheck = {
        let ask_hold = Arc::clone(&ask_hold);
        Arc::new(move || {
            ask_hold.enter();
            Ok(())
        })
    };

    let first = {
        let lock = Arc::clone(&lock);
        let program = program.clone();
        let workdir = dir.join("workdir-one");
        tokio::spawn(async move {
            serialized_bounded_push(
                &lock,
                generous,
                deadline,
                move |turn| async move {
                    neutralize_then_push_in_child_off_runtime(
                        program,
                        workdir,
                        "https://relay.example.invalid/seller.git".to_owned(),
                        "delivery/one".to_owned(),
                        "0123456789012345678901234567890123456789".to_owned(),
                        None,
                        Some(authority),
                        turn,
                    )
                    .await
                },
            )
            .await
        })
    };

    let wedged_pid = wait_for_running_child(&pidfile, Duration::from_secs(20))
        .await
        .expect("the first delivery's child must be running before its task is aborted");

    // ARM, THEN ABORT. Armed first, so that no ask can slip between the abort and the hold: from
    // here the executor's next ask blocks before it can consult the turn, whether it arrives
    // before or after the abort lands.
    let armed_at = ask_hold.arm();
    first.abort();
    let joined = first.await;
    assert!(
        joined.as_ref().err().is_some_and(|error| error.is_cancelled()),
        "this gate is only meaningful if the first delivery's task was really cancelled: \
         {joined:?}"
    );

    // THE HOLD IS TAKEN. The executor is inside this delivery's authority check and cannot kill
    // until it is let go. How long it took to get here is the executor's poll cadence.
    let held_at = wait_until_held(&ask_hold, Duration::from_secs(20)).await;
    let armed_to_ask = held_at.saturating_duration_since(armed_at);
    assert!(
        armed_to_ask < CANCELLATION_POLL + Duration::from_secs(1),
        "the executor's next authority ask came {armed_to_ask:?} after the hold was armed, past the \
         {CANCELLATION_POLL:?} poll it states plus a second for its thread to be scheduled"
    );
    assert!(
        alive(wedged_pid),
        "the child was gone while the executor was still inside its authority check; nothing but \
         the executor may end it"
    );

    let second_state = Arc::new(AtomicU8::new(NOT_STARTED));
    let second_acquired_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let second = serialized_bounded_push(&lock, generous, Instant::now() + Duration::from_secs(20), {
        let state = Arc::clone(&second_state);
        let at = Arc::clone(&second_acquired_at);
        move |turn| async move {
            at.lock().expect("clock").replace(Instant::now());
            state.store(ACQUIRED, Ordering::SeqCst);
            drop(turn);
            Ok::<_, SellerGitError>("second-delivery-oid".to_owned())
        }
    });
    tokio::pin!(second);

    // OBSERVED PENDING WHILE THE CHILD IS ALIVE — BY CONSTRUCTION. Every poll in this loop is
    // taken while the executor is held inside the authority check, so the child is alive at the
    // instant of the poll and no cleanup can be racing it. Twenty polls, one millisecond apart,
    // each the serializer's own answer to "may this delivery have the seat".
    const HELD_SAMPLES: usize = 20;
    for sample in 0..HELD_SAMPLES {
        assert!(
            alive(wedged_pid),
            "the child died while the executor's authority check was held (sample {sample})"
        );
        assert!(
            poll_once(second.as_mut()).await.is_pending(),
            "a second delivery was admitted to a seat whose aborted predecessor's child is alive \
             and cannot yet be killed (sample {sample})"
        );
        second_state.store(PENDING, Ordering::SeqCst);
        assert_eq!(
            second_state.load(Ordering::SeqCst),
            PENDING,
            "the second delivery's push body ran while the aborted delivery's child was alive"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // RELEASE. From here every instant belongs to the executor: the ask returns, the turn check
    // refuses, and the abort is acted on — or, on an executor that ignored the dropped control,
    // nothing happens until the deadline.
    let released_at = ask_hold.release();
    let remaining_at_release = deadline.saturating_duration_since(released_at);

    // WATCH THE EXECUTOR ACT. No Pending is asserted here: after the release the reap and the
    // handover can land between any two observations, and a sample that raced them would be a
    // statement about the sampler. What is recorded is when the child was last seen alive, when it
    // was first seen gone, and when the seat moved; the ordering facts below are what those support.
    let mut last_alive_at = Instant::now();
    let mut child_gone_at: Option<Instant> = None;
    let mut pending_after_release = 0usize;
    let give_up = deadline + 2 * REAP_BOUND + Duration::from_secs(5);
    let second_outcome = loop {
        let now = Instant::now();
        if alive(wedged_pid) {
            last_alive_at = now;
        } else if child_gone_at.is_none() {
            child_gone_at = Some(now);
        }
        match poll_once(second.as_mut()).await {
            Poll::Ready(outcome) => break outcome,
            Poll::Pending => pending_after_release += 1,
        }
        assert!(
            now < give_up,
            "the seat never came back: the second delivery was still pending {:?} after the \
             aborted delivery's deadline",
            now.saturating_duration_since(deadline)
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    };
    assert_eq!(
        second_outcome.expect("the seat must come back once the abandoned child is reaped"),
        "second-delivery-oid"
    );
    assert_eq!(second_state.load(Ordering::SeqCst), ACQUIRED);
    let acquired = second_acquired_at.lock().expect("clock").expect("acquired");
    // The child may have gone between the last observation and the handover; it is gone NOW.
    assert!(
        !alive(wedged_pid),
        "the seat was handed on while the aborted delivery's child was still running"
    );
    let child_gone_at = child_gone_at.unwrap_or_else(Instant::now);
    // ORDERED, NOT MERELY EVENTUAL. An `alive` observation can only precede the reap, the release
    // follows the reap, and the push body runs after the release: the seat moved after the child
    // was last seen alive, whatever the sampler's cadence.
    assert!(
        acquired >= last_alive_at,
        "the second delivery entered its push body at {acquired:?}, before the aborted delivery's \
         child was last seen alive at {last_alive_at:?}"
    );
    assert!(
        !ask_hold.expired(),
        "the hold ran out on its own safety bound; the release above is what must have let the \
         executor go"
    );

    let released_to_child_gone = child_gone_at.saturating_duration_since(released_at);
    let released_to_handover = acquired.saturating_duration_since(released_at);
    // THE NUMBERS, PRINTED. `cargo test ... -- --nocapture` reproduces the measurement rather than
    // the claim.
    eprintln!(
        "MEASURED armed_to_ask={armed_to_ask:?} released_to_child_gone={released_to_child_gone:?} \
         released_to_handover={released_to_handover:?} remaining_at_release={remaining_at_release:?} \
         budget={budget:?} held_samples={HELD_SAMPLES} pending_after_release={pending_after_release} \
         asks={}",
        ask_hold.asks()
    );

    // THE ABORT WAS ACTED ON AT THE CHECK, NOT AT THE DEADLINE. On an executor that ignored the
    // dropped turn control, the released check returns yes, nothing is killed, and the child lives
    // until the watchdog ends it at the deadline; this is the assertion that goes red then.
    assert!(
        child_gone_at < deadline,
        "the child was still alive at the delivery's ORIGINAL DEADLINE; the released check did not \
         end it, so deadline cleanup, not the abort, is what ended this child"
    );
    // THE EXIT WAS CONFIRMED INSIDE THE REAP BOUND, counted from the release: the refused check,
    // the kill and the reap all run on the executor's thread after this instant, and the reap is
    // budgeted at [`REAP_BOUND`]. The second is for the threads involved to be scheduled and for
    // the sampler's own cadence.
    assert!(
        released_to_child_gone < REAP_BOUND + Duration::from_secs(1),
        "the child was seen gone only {released_to_child_gone:?} after the check was released, \
         past the {REAP_BOUND:?} reap bound this executor states"
    );
    // THE SEAT CAME BACK INSIDE THE EXECUTOR'S FULL STATEMENT: one reap window for the exit, one
    // for end of file on the child's stdout, and slack for the threads that carry those facts.
    assert!(
        released_to_handover < 2 * REAP_BOUND + Duration::from_secs(2),
        "the seat took {released_to_handover:?} to come back after the check was released, past \
         the reap and end-of-file bounds this executor states"
    );
    // AND BEFORE THE DEADLINE, BY A MARGIN. Deadline cleanup cannot begin before `remaining_at_release`
    // has elapsed; abort cleanup must finish well inside it. Half is not arbitrary: anything near
    // the remainder is indistinguishable from the deadline's own work, and this gate refuses to call
    // it. The fixture is one process, so its stdout closes with its exit and the end-of-file window
    // closes with the reap, which is what makes the margin derivable rather than hoped for.
    assert!(
        acquired < deadline,
        "the seat came back at or after this delivery's ORIGINAL DEADLINE, which is what ordinary \
         deadline cleanup does; an abort must release it earlier. released_to_handover=\
         {released_to_handover:?} remaining_at_release={remaining_at_release:?}"
    );
    assert!(
        released_to_handover * 2 < remaining_at_release,
        "the handover ({released_to_handover:?}) is not clearly shorter than the \
         {remaining_at_release:?} the delivery still had when the executor was let act; at that \
         margin this gate cannot tell abort cleanup from deadline cleanup"
    );
}
