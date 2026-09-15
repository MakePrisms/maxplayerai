//! **A STALLED SUPERVISOR MAY NOT HOLD THE SEAT.**
//!
//! `delivery_turn` hands the exclusion token back only when BOTH halves are finished: the work has
//! stopped, AND the supervising side has published `supervisor_done`. The second half is published
//! by `TurnControl::end` — and by nothing else. So a supervisor that never reaches its `end` (a
//! task starved, parked, or blocked in a call that never answers) held the seat for as long as it
//! stalled, whatever the child did. That term had no number: it is the `S` the executor's module
//! documentation names, and it sat between "this process confirmed the child's exit" and "the next
//! delivery may begin".
//!
//! These tests pin the replacement: a CUSTODY BAILIFF that runs on neither side. Given a confirmed
//! exit — never a signal, never a passed deadline — it fences the supervisor out of shared state
//! and completes the handoff itself, so B's acquisition is bounded by the executor's own numbers
//! plus one custody tick.
//!
//! What is deliberately NOT relaxed:
//! - **no overlap** — a supervisor that is INSIDE a declared shared-state section is not fenced out
//!   from under itself; custody is retained until it leaves (`a_supervisor_inside_a_shared_state_...`);
//! - **unknown-exit retention** — an exit this process did not observe never hands the seat on
//!   (`a_signal_without_a_confirmed_exit_...`), which is the same rule `ChildCustody` already
//!   applies to `RunningWork`.
//!
//! Each test states its own bound as a literal and MEASURES against it, so a lost bound is a red
//! test rather than a hang. Platform: POSIX — these drive a real child through `SIGKILL`/`waitpid`,
//! the same contract `delivery_executor` documents. Measured on whatever host runs them.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::{
    Exclusion, PushRequest, REAP_BOUND, run_push_in_child,
};
use maxplayer_core::delivery_turn::{
    CUSTODY_TICK, CustodyHandoff, TurnRelease, delivery_turn,
};
use maxplayer_core::git_transport::{AuthMinter, AuthorityCheck};
use maxplayer_core::seller_git::turn_after_child_push;

/// The seat's exclusion token, in the shape the production turn carries one: something whose DROP
/// is the moment the next delivery may begin. `released` flips exactly then and never back.
struct Token(Arc<AtomicBool>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A minter these tests never reach: the child never answers its hello, so no leg is ever
/// authorized. Reaching it would mean the test measured something other than the stop.
fn no_mint() -> AuthMinter {
    Arc::new(|_destination: &str| panic!("a child that never answers cannot ask for a token"))
}

fn still_ours() -> AuthorityCheck {
    Arc::new(|| Ok(()))
}

/// A child that reads nothing and answers nothing, and will not stop until it is killed.
fn deaf_child() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mp-stalled-sup-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let path = dir.join("child.sh");
    std::fs::write(&path, "#!/bin/sh\nwhile true; do sleep 1; done\n").expect("fixture script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn unix_ms_from_now(budget_ms: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
        .saturating_add(budget_ms)
}

fn request(budget_ms: u64) -> PushRequest {
    PushRequest {
        workdir: std::env::temp_dir(),
        remote_url: "https://relay.invalid/repo.git".to_owned(),
        branch: "delivery".to_owned(),
        gated_oid: "0".repeat(40),
        authenticated: false,
        budget_ms,
        deadline_unix_ms: unix_ms_from_now(budget_ms),
    }
}

/// How long the bailiff is asked to keep trying. It is NOT the bound being measured — the bound is
/// asserted separately, below — it is the window inside which the bailiff will report an answer at
/// all. Three reap windows: the executor's own worst case for reaching a confirmed exit is two, and
/// the third is slack that keeps a slow host from turning a bound test into a hang.
const CUSTODY_PATIENCE: Duration = Duration::from_secs(15);

/// **THE BOUND, STATED AS A LITERAL, AND THE WHOLE POINT OF THIS FILE.**
///
/// From the delivery's absolute deadline, the seat is handed on within
/// `WATCHDOG_TICK + w + 2 * REAP_BOUND + CUSTODY_TICK`, where `w` is scheduler latency. Those are
/// exactly the executor's published terms for reaching a confirmed exit, plus ONE custody tick for
/// the handoff — and no `S`. A supervisor that never returns does not appear in it.
///
/// `w` is not a number this module can promise, so it is carried here as a generous scheduling
/// allowance rather than hidden: if the machine is so loaded that waking two sleeping threads costs
/// more than this, the test is measuring the host, not the lane.
const SCHEDULING_ALLOWANCE: Duration = Duration::from_millis(750);

fn seat_handoff_bound() -> Duration {
    REAP_BOUND * 2 + CUSTODY_TICK + SCHEDULING_ALLOWANCE
}

/// **T-S1. The defect, end to end: A stops, the supervisor never does, B still gets the seat.**
///
/// The supervising side of this turn never calls `end` while the assertions run — it is parked in
/// the wait below, which is what a stalled supervisor looks like from the seat's point of view. The
/// work runs a REAL child through the REAL executor: the child answers nothing, so the delivery
/// ends at its deadline, by kill, with an exit this process reaped.
///
/// Before the bailiff, the token could not come back here: the work's half was published and the
/// supervisor's half never was. What is asserted is not just that it comes back, but WHEN — within
/// the bound stated above, measured from the deadline.
#[test]
fn a_stalled_supervisor_does_not_hold_the_seat_past_the_custody_bound() {
    let released = Arc::new(AtomicBool::new(false));
    let budget_ms = 400;
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), deadline);

    // Armed by the side that OWNS the turn, before the work starts. It holds no part of the
    // supervisor's state and runs on its own thread.
    let watch = control.custody_bailiff().arm(deadline, CUSTODY_PATIENCE);

    let program = deaf_child();
    let worker = std::thread::spawn(move || {
        let running = turn.begin().expect("the turn is ours");
        let outcome = run_push_in_child(
            &program,
            &request(budget_ms),
            deadline,
            no_mint(),
            still_ours(),
        );
        // The same rule the production release site applies, asserted here so that a run in which
        // the child was NOT confirmed dead cannot be read as a passing handoff test.
        assert_eq!(
            turn_after_child_push(&outcome),
            Exclusion::Release,
            "this test only says something if the exit was confirmed; it was not: {outcome:?}"
        );
        running.confirm_exit();
        drop(running);
    });

    // THE SUPERVISOR IS HERE, AND IT IS STALLED: no `end`, no drop, for the whole of the wait.
    let handoff = watch
        .wait(CUSTODY_PATIENCE)
        .expect("the bailiff must answer within its patience");
    let took = Instant::now().saturating_duration_since(deadline);

    assert_eq!(
        handoff,
        CustodyHandoff::HandedOn,
        "a confirmed exit and a supervisor outside shared state is a safe handoff"
    );
    assert!(
        released.load(Ordering::SeqCst),
        "the seat's exclusion token must be back before the supervisor is"
    );
    assert!(
        !control.holds_ownership(),
        "the turn still holds the token, so the next delivery is still waiting on a stalled supervisor"
    );
    assert!(
        took <= seat_handoff_bound(),
        "the seat came back {took:?} after the deadline; the stated bound is {:?}",
        seat_handoff_bound()
    );

    worker.join().expect("the work thread must not panic");
    // Only NOW does the supervisor finish. It finds the turn already handed on, and says so.
    assert_eq!(
        control.end(),
        TurnRelease::AlreadyEnded,
        "a supervisor that arrives after the handoff must not pretend it still owns anything"
    );
}

/// **T-S2. Custody is CONTINUOUS: the seat is never free between the two.**
///
/// A bound on when the seat comes back is worth nothing if the seat was briefly free earlier. This
/// samples ownership from before the work starts until after the handoff and pins the ORDER: the
/// first instant the token was observed free is at or after the instant this process confirmed the
/// exit. No sample in between, ever.
#[test]
fn custody_is_continuously_pending_until_the_handoff_is_safe() {
    let released = Arc::new(AtomicBool::new(false));
    let budget_ms = 400;
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), deadline);
    let watch = control.custody_bailiff().arm(deadline, CUSTODY_PATIENCE);

    let confirmed_at: Arc<std::sync::Mutex<Option<Instant>>> = Arc::new(std::sync::Mutex::new(None));
    let stamp = Arc::clone(&confirmed_at);

    let program = deaf_child();
    let worker = std::thread::spawn(move || {
        let running = turn.begin().expect("the turn is ours");
        let outcome = run_push_in_child(
            &program,
            &request(budget_ms),
            deadline,
            no_mint(),
            still_ours(),
        );
        assert_eq!(
            turn_after_child_push(&outcome),
            Exclusion::Release,
            "this test only says something if the exit was confirmed; it was not: {outcome:?}"
        );
        *stamp.lock().expect("stamp") = Some(Instant::now());
        running.confirm_exit();
        drop(running);
    });

    // Sampled on THIS thread — the stalled supervisor's thread — so the samples come from the side
    // that must never see a free seat early.
    let mut first_free: Option<Instant> = None;
    let sampling_until = Instant::now() + CUSTODY_PATIENCE;
    while Instant::now() < sampling_until {
        if !control.holds_ownership() {
            first_free = Some(Instant::now());
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let first_free = first_free.expect("the seat must come back inside the bailiff's patience");
    worker.join().expect("the work thread must not panic");
    let confirmed_at = confirmed_at
        .lock()
        .expect("stamp")
        .expect("the work must have confirmed an exit");

    assert!(
        first_free >= confirmed_at,
        "the seat was observed free {:?} BEFORE this process confirmed the child's exit — that is \
         an overlap, not a handoff",
        confirmed_at.saturating_duration_since(first_free)
    );
    assert_eq!(
        watch.wait(CUSTODY_PATIENCE),
        Some(CustodyHandoff::HandedOn),
        "the release observed above must be the bailiff's handoff, not some other path"
    );
    assert!(released.load(Ordering::SeqCst));
}

/// **T-S3. MUTATION CONTROL — premature handoff. A SIGNAL SENT IS NOT AN EXIT CONFIRMED.**
///
/// The work here stops without ever confirming an exit — exactly what the executor reports when it
/// killed a child and could not reap it. The deadline is long past and the work is over, so every
/// term except the confirmation is satisfied; the bailiff must still refuse, for the whole of its
/// patience, and the seat must stay held.
///
/// Mutate `attempt_handoff` to skip the confirmation check — release on "the work ended", or on
/// "the signal was issued" — and this test goes RED at the first tick, not at some later timing
/// coincidence.
#[test]
fn a_signal_without_a_confirmed_exit_does_not_hand_custody_on() {
    let released = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_millis(50);
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), deadline);
    let bailiff = control.custody_bailiff();

    let running = turn.begin().expect("the turn is ours");
    // The work stops. `confirm_exit` is NOT called: this is the unconfirmed-exit path, where a kill
    // was issued and the exit was never observed.
    drop(running);
    std::thread::sleep(Duration::from_millis(100));

    let refusing_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < refusing_until {
        assert_eq!(
            bailiff.attempt_handoff(),
            CustodyHandoff::ExitUnconfirmed,
            "an exit this process never observed must not release the seat"
        );
        assert!(
            control.holds_ownership(),
            "the seat was handed on over an unconfirmed exit"
        );
        std::thread::sleep(CUSTODY_TICK);
    }
    assert!(
        !released.load(Ordering::SeqCst),
        "the exclusion token was dropped while a child may still be running"
    );
}

/// **T-S4. MUTATION CONTROL — deadline-only cleanup. A PASSED DEADLINE IS NOT A STOPPED DELIVERY.**
///
/// The deadline is long gone and the work is STILL RUNNING: it holds `RunningWork` and has not
/// dropped it. A bailiff that fences on the clock alone — the obvious simplification, since it is
/// already a thread that wakes at a deadline — hands the seat to B while A is still on the wire.
///
/// Mutate `attempt_handoff` to fence once `Instant::now() >= deadline`, and this test goes RED.
#[test]
fn a_passed_deadline_alone_does_not_hand_custody_on() {
    let released = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_millis(50);
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), deadline);
    let bailiff = control.custody_bailiff();

    let running = turn.begin().expect("the turn is ours");
    // Work that is past its deadline and has NOT stopped. Its own checks will refuse further
    // phases; what it has not done is RETURN, and custody is about the second thing.
    std::thread::sleep(Duration::from_millis(100));
    assert!(running.check().is_err(), "the deadline must really be past");

    let refusing_until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < refusing_until {
        assert_eq!(
            bailiff.attempt_handoff(),
            CustodyHandoff::WorkStillRunning,
            "the clock is not a report that the work stopped"
        );
        assert!(control.holds_ownership(), "the seat was handed on under running work");
        std::thread::sleep(CUSTODY_TICK);
    }

    // AND THE DANGEROUS CASE, WHICH IS REAL: the child has been reaped and the work has NOT
    // returned. That is the executor between its reap and its bounded cleanup drain — the exit is
    // confirmed, and the work thread is still holding the workdir. Every term a clock-driven fence
    // looks at is now satisfied, and the seat must still not move.
    running.confirm_exit();
    let refusing_until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < refusing_until {
        assert_eq!(
            bailiff.attempt_handoff(),
            CustodyHandoff::WorkStillRunning,
            "a confirmed exit under work that has not returned is still not a handoff"
        );
        assert!(
            control.holds_ownership(),
            "the seat was handed on while the work thread was still running"
        );
        std::thread::sleep(CUSTODY_TICK);
    }
    assert!(!released.load(Ordering::SeqCst));

    // And once the work really does stop, the same bailiff hands it on.
    drop(running);
    assert_eq!(bailiff.attempt_handoff(), CustodyHandoff::HandedOn);
    assert!(released.load(Ordering::SeqCst));
}

/// **T-S5. NO OVERLAP IS NOT RELAXED: the supervisor is not fenced out from under itself.**
///
/// The fence exists to EXCLUDE a supervisor that is stalled somewhere harmless, never to cut one
/// that is mid-way through touching the seat. While a declared shared-state section is open the
/// bailiff refuses, however confirmed the exit is; when the section closes it proceeds; and once
/// fenced, the supervisor can no longer open a new section at all — it fails closed rather than
/// entering a seat that now belongs to B.
#[test]
fn a_supervisor_inside_a_shared_state_section_is_not_fenced_out_from_under_itself() {
    let released = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_millis(50);
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), deadline);
    let bailiff = control.custody_bailiff();

    let running = turn.begin().expect("the turn is ours");
    running.confirm_exit();
    drop(running);

    let section = control
        .enter_shared_state()
        .expect("an unfenced supervisor may enter");
    let refusing_until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < refusing_until {
        assert_eq!(
            bailiff.attempt_handoff(),
            CustodyHandoff::SupervisorInSharedState,
            "custody may not move while the supervisor is inside the section it excludes"
        );
        assert!(control.holds_ownership());
        std::thread::sleep(CUSTODY_TICK);
    }
    assert!(!released.load(Ordering::SeqCst));

    drop(section);
    assert_eq!(bailiff.attempt_handoff(), CustodyHandoff::HandedOn);
    assert!(released.load(Ordering::SeqCst));
    assert!(
        control.enter_shared_state().is_err(),
        "a fenced supervisor must be refused the section, not allowed into a seat that is now B's"
    );
    assert!(control.is_fenced());
}
