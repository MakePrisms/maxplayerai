//! One delivery's TURN at the seat's single delivery remote — owned by the WORK, not by the caller.
//!
//! # The bug this module exists to close
//!
//! A delivery push is serialized behind one lock because concurrent `git-receive-pack` to one repo
//! is what the relay 409s. The dangerous simplification is to tie that lock to the future that
//! started the push: the future is cancelled, or its bound elapses, the guard drops, and the next
//! delivery opens a second `git-receive-pack` while the previous upload is still on the wire. **A
//! caller that gave up is not a job that stopped.** So the turn is released on exactly two events,
//! and never on a third:
//!
//! 1. the work ACTUALLY stopped (returned, refused, panicked) — released on whichever thread got
//!    there, including a blocking thread that outlived the runtime task that spawned it; or
//! 2. the work is known to have NEVER STARTED — it was revoked while still queued for a blocking
//!    slot, so nothing was neutralized, no pack was built and nothing reached the wire.
//!
//! Case 2 is not "release because the caller timed out". It is a state transition that makes the
//! start impossible: [`DeliveryTurn::begin`] and [`TurnControl::end`] race on one atomic, exactly
//! one wins, and the loser cannot proceed. Without it a revoked push still occupies the delivery
//! turn for as long as unrelated blocking work keeps it queued — unbounded, with nothing running.
//!
//! # The bound
//!
//! The turn carries an ABSOLUTE deadline, fixed when the turn is created, and every phase of the
//! actual work checks it: queue admission ([`DeliveryTurn::begin`]), each local phase boundary, each
//! chunk of pack buffering, and each wire request before it is transmitted. Between two consecutive
//! checks the work is uninterruptible for at most one HTTP leg, which the transport client caps. The
//! seat's whole-operation drain bound is therefore `deadline + one leg cap`, stated and asserted at
//! `crate::seller_node::run::DELIVERY_DRAIN_BOUND`. That is a bound on the WORK, not on an HTTP
//! request and not on the caller's patience.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Queued for a blocking slot; nothing has run.
const PENDING: u8 = 0;
/// The actual work has begun and owns the turn until it stops.
const RUNNING: u8 = 1;
/// The work stopped, or provably never started. The turn is free.
const ENDED: u8 = 2;

/// Why the work may not proceed. Both answers are refusals to do MORE work; neither un-sends
/// anything already accepted by the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkEnded {
    /// The delivery that owns this work was cancelled, dropped, or stopped waiting.
    Cancelled,
    /// The absolute deadline for this delivery's work has passed.
    DeadlineExceeded,
}

impl WorkEnded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "this delivery's work was cancelled",
            Self::DeadlineExceeded => "this delivery's work deadline has passed",
        }
    }
}

impl std::fmt::Display for WorkEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What [`TurnControl::end`] found, and therefore whether the turn was handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRelease {
    /// The work had not begun and now never will: the turn was handed back immediately.
    NeverStarted,
    /// The work is running. The turn STAYS TAKEN; the work will refuse at its next check and hand
    /// the turn back itself when it has actually stopped.
    StillRunning,
    /// The work had already stopped; the turn was already free.
    AlreadyEnded,
}

/// The shared cell. `ownership` is whatever exclusion token the turn carries (in production the
/// delivery lock's owned guard) — held here so that whichever side legitimately ends the turn is the
/// side that drops it.
struct Turn {
    state: AtomicU8,
    cancelled: AtomicBool,
    /// The supervising side has finished with the turn: it returned, timed out, was cancelled at an
    /// await, or was dropped. It is NOT "the work stopped".
    supervisor_done: AtomicBool,
    /// How many shared-state sections the supervising side currently has OPEN, or [`FENCED`] once
    /// the supervisor has been excluded and may open no more. See [`Turn::fence_supervisor`].
    supervisor_sections: AtomicUsize,
    /// THIS PROCESS OBSERVED THE DELIVERY'S EXIT. Published by the work, at the one place that
    /// knows: [`RunningWork::confirm_exit`]. A signal issued, a deadline passed and a caller that
    /// gave up all leave it false, which is why the bailiff below cannot act on any of them.
    exit_confirmed: AtomicBool,
    deadline: Instant,
    ownership: Mutex<Option<Box<dyn Send>>>,
}

/// The value of [`Turn::supervisor_sections`] that means "fenced": the supervising side is excluded
/// from shared state permanently, and no further section may be opened. `usize::MAX` rather than a
/// second flag so that opening a section and fencing are ONE compare-and-swap on ONE word — there is
/// no instant at which a supervisor is entering while the bailiff believes it is out.
const FENCED: usize = usize::MAX;

impl std::fmt::Debug for Turn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turn")
            .field("state", &self.state)
            .field("cancelled", &self.cancelled)
            .finish_non_exhaustive()
    }
}

impl Turn {
    /// Hand the exclusion token back. Dropped OUTSIDE the lock: releasing a mutex guard can wake a
    /// waiter, and a waiter must never wake into this cell.
    /// Hand the token back only when BOTH sides are finished with the turn: the work has actually
    /// stopped (or provably never started) AND the supervising side is done with its critical
    /// section. Either condition alone has already been a bug:
    ///
    /// - supervisor alone: the caller stops waiting while the push thread is still on the wire, and
    ///   the next delivery starts against the same remote (F3/F5);
    /// - work alone: the blocking operation returns and the turn is gone while the supervising arm
    ///   is still inside the section the turn is supposed to exclude.
    ///
    /// `maybe_release` is called after each side publishes its half with `SeqCst`, so whichever
    /// side is second sees both halves; `release_ownership` takes the slot, so running it twice is
    /// harmless.
    fn maybe_release(&self) {
        if self.state.load(Ordering::SeqCst) == ENDED && self.supervisor_done.load(Ordering::SeqCst)
        {
            self.release_ownership();
        }
    }

    /// Open a shared-state section for the supervising side, unless it has been fenced.
    ///
    /// A plain `fetch_add` would be wrong: it would succeed against a fenced word and then the
    /// count would never mean anything again. The loop re-reads and refuses `FENCED` explicitly.
    fn enter_section(&self) -> Result<(), Fenced> {
        let mut current = self.supervisor_sections.load(Ordering::SeqCst);
        loop {
            if current == FENCED {
                return Err(Fenced);
            }
            match self.supervisor_sections.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(seen) => current = seen,
            }
        }
    }

    fn leave_section(&self) {
        let mut current = self.supervisor_sections.load(Ordering::SeqCst);
        loop {
            // A fenced word is never decremented: the fence is only ever taken from ZERO, so no
            // section can be open across one, and a stale guard must not turn `FENCED` into a count.
            if current == FENCED || current == 0 {
                return;
            }
            match self.supervisor_sections.compare_exchange(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(seen) => current = seen,
            }
        }
    }

    /// Exclude the supervising side from shared state, IF it is not inside a section right now.
    ///
    /// This is the whole of what makes a handoff safe without the supervisor's cooperation: after it
    /// succeeds the supervisor cannot enter shared state again ([`Turn::enter_section`] refuses),
    /// so the seat can be given to the next delivery even though the supervisor never came back.
    /// It is NOT a way to interrupt a supervisor that is already inside one — that case is reported
    /// and custody is retained.
    fn fence_supervisor(&self) -> bool {
        self.supervisor_sections
            .compare_exchange(0, FENCED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn release_ownership(&self) {
        let taken = match self.ownership.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        drop(taken);
    }

    fn check(&self) -> Result<(), WorkEnded> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(WorkEnded::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(WorkEnded::DeadlineExceeded);
        }
        Ok(())
    }

    fn reason(&self) -> WorkEnded {
        if self.cancelled.load(Ordering::SeqCst) {
            WorkEnded::Cancelled
        } else {
            WorkEnded::DeadlineExceeded
        }
    }

    fn end_now(&self) {
        if self.state.swap(ENDED, Ordering::SeqCst) != ENDED {
            self.maybe_release();
        }
    }
}

/// Create a turn: the supervisor keeps the [`TurnControl`], the work takes the [`DeliveryTurn`].
///
/// `ownership` is moved INTO the turn, which is the whole point — no copy of it stays with the
/// supervisor, so a supervisor that dies cannot take exclusion with it.
pub fn delivery_turn<O: Send + 'static>(
    ownership: O,
    deadline: Instant,
) -> (TurnControl, DeliveryTurn) {
    let turn = Arc::new(Turn {
        state: AtomicU8::new(PENDING),
        cancelled: AtomicBool::new(false),
        supervisor_done: AtomicBool::new(false),
        supervisor_sections: AtomicUsize::new(0),
        exit_confirmed: AtomicBool::new(false),
        deadline,
        ownership: Mutex::new(Some(Box::new(ownership))),
    });
    (
        TurnControl {
            turn: Arc::clone(&turn),
        },
        DeliveryTurn { turn: Some(turn) },
    )
}

/// The supervisor's end of the turn. It can REVOKE the work; it cannot take the turn back from work
/// that is running.
pub struct TurnControl {
    turn: Arc<Turn>,
}

impl TurnControl {
    /// End this delivery's entitlement to do more work, and hand the turn back IF AND ONLY IF the
    /// work never started.
    ///
    /// `cancelled` is set before the state race, so a `begin` that wins the race still sees the
    /// revocation in its own immediate check and refuses.
    ///
    /// Calling this also declares the supervising side finished with the turn, which is the truth on
    /// every path that reaches it: an explicit `end` on timeout, and the `Drop` below. The token is
    /// handed back here only if the work is finished too.
    pub fn end(&self) -> TurnRelease {
        self.turn.cancelled.store(true, Ordering::SeqCst);
        self.turn.supervisor_done.store(true, Ordering::SeqCst);
        let release = match self
            .turn
            .state
            .compare_exchange(PENDING, ENDED, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => TurnRelease::NeverStarted,
            Err(RUNNING) => TurnRelease::StillRunning,
            Err(_) => TurnRelease::AlreadyEnded,
        };
        self.turn.maybe_release();
        release
    }

    /// For assertions and operator logging: has the actual work begun?
    pub fn work_started(&self) -> bool {
        self.turn.state.load(Ordering::SeqCst) != PENDING
    }

    /// For assertions and operator logging: has the actual work stopped (or provably never begun)?
    pub fn work_ended(&self) -> bool {
        self.turn.state.load(Ordering::SeqCst) == ENDED
    }

    /// True while the turn's exclusion token is still held by this turn.
    pub fn holds_ownership(&self) -> bool {
        match self.turn.ownership.lock() {
            Ok(slot) => slot.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    /// Declare that the supervising side is about to touch state the turn EXCLUDES, and hold that
    /// declaration open until the returned guard drops.
    ///
    /// Everything the supervisor does between taking the turn and handing it back is one of two
    /// things: work that touches the seat (the workdir, the remote, the delivery's own files), or
    /// waiting. Only the first can overlap the next delivery, and only the first has to be waited
    /// for. Wrapping it makes that distinction a fact the [`CustodyBailiff`] can read instead of an
    /// assumption it has to make — and makes the SAFE default the one that costs a stall: a
    /// supervisor inside a section is never fenced.
    ///
    /// Refused once the turn has been fenced: by then the seat may already be the next delivery's,
    /// and a supervisor that discovers this must stop rather than proceed.
    pub fn enter_shared_state(&self) -> Result<SupervisorSection, Fenced> {
        self.turn.enter_section()?;
        Ok(SupervisorSection {
            turn: Arc::clone(&self.turn),
        })
    }

    /// A handle that can complete the handoff WITHOUT this supervisor — see [`CustodyBailiff`].
    pub fn custody_bailiff(&self) -> CustodyBailiff {
        CustodyBailiff {
            turn: Arc::clone(&self.turn),
        }
    }

    /// True once the supervising side has been excluded from shared state by the bailiff.
    pub fn is_fenced(&self) -> bool {
        self.turn.supervisor_sections.load(Ordering::SeqCst) == FENCED
    }
}

/// The supervising side is refused: it has been fenced out of this turn's shared state, so the seat
/// it is holding may already belong to the next delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fenced;

impl std::fmt::Display for Fenced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this delivery's supervisor has been fenced out of the seat it was holding")
    }
}

impl std::error::Error for Fenced {}

/// An OPEN declaration that the supervising side is inside shared state. Dropping it closes the
/// declaration — on return, on unwind, on cancellation — which is the only form that holds for a
/// supervisor that is about to stop being reliable.
pub struct SupervisorSection {
    turn: Arc<Turn>,
}

impl Drop for SupervisorSection {
    fn drop(&mut self) {
        self.turn.leave_section();
    }
}

/// How often [`CustodyBailiff::arm`] re-asks whether the handoff has become safe.
///
/// It is not a timeout and not a retry interval: it bounds only how long a turn that HAS become
/// safe to hand on waits for the bailiff to notice. It is the single term this lane adds to the
/// executor's own numbers.
pub const CUSTODY_TICK: Duration = Duration::from_millis(25);

/// What the bailiff found when it asked whether the seat could move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyHandoff {
    /// The supervisor was fenced out of shared state and the turn was handed on. The next delivery
    /// may begin.
    HandedOn,
    /// The turn was already free — the ordinary path completed, and there was nothing to do.
    AlreadyFree,
    /// The work has not stopped. **A passed deadline is not a stopped delivery**, so this is what a
    /// clock alone gets.
    WorkStillRunning,
    /// The work stopped, but this process never observed the delivery's exit. **A signal issued is
    /// not an exit confirmed**, so custody is RETAINED — the same answer
    /// `crate::delivery_executor::exclusion_after_reap` gives an unreaped child.
    ExitUnconfirmed,
    /// The supervisor is inside a declared shared-state section. It cannot be fenced out from under
    /// itself, so custody is retained until it leaves.
    SupervisorInSharedState,
}

/// **THE HANDOFF, INDEPENDENT OF THE SUPERVISOR.**
///
/// The turn is released when both halves are published: the work stopped, and the supervising side
/// is finished. The second half used to have exactly one publisher — [`TurnControl::end`], reached
/// from the supervisor's own stack — so a supervisor that never got there held the seat for as long
/// as it stalled. That term is the `S` in `delivery_executor`'s bounds, and it has no number: a task
/// starved by its runtime, parked on a call that never answers, or descheduled indefinitely is
/// bounded by nothing this process controls.
///
/// This is the other publisher, and it runs on neither side. It may hand the seat on ONLY when all
/// three of these hold, and it re-checks all three every time it is asked:
///
/// 1. **the work stopped** — not "its deadline passed", not "it was signalled";
/// 2. **this process observed the exit** ([`RunningWork::confirm_exit`]) — an unconfirmed exit
///    retains the seat exactly as it always did;
/// 3. **the supervisor is not inside a declared shared-state section** — and it is fenced out of
///    entering one in the same compare-and-swap that reads it, so there is no window.
///
/// What that buys: from the delivery's absolute deadline, the seat moves within the executor's own
/// published terms for reaching a confirmed exit plus one [`CUSTODY_TICK`]. No term of that sum is
/// the supervisor's latency. What it deliberately does NOT buy: a supervisor stalled INSIDE shared
/// state still holds the seat — that is the no-overlap rule, and it costs liveness on purpose.
#[derive(Clone)]
pub struct CustodyBailiff {
    turn: Arc<Turn>,
}

impl CustodyBailiff {
    /// Ask once. Cheap, lock-free apart from the ownership slot, and safe to call from any thread.
    pub fn attempt_handoff(&self) -> CustodyHandoff {
        let held = match self.turn.ownership.lock() {
            Ok(slot) => slot.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        };
        if !held {
            return CustodyHandoff::AlreadyFree;
        }
        if self.turn.state.load(Ordering::SeqCst) != ENDED {
            return CustodyHandoff::WorkStillRunning;
        }
        if !self.turn.exit_confirmed.load(Ordering::SeqCst) {
            return CustodyHandoff::ExitUnconfirmed;
        }
        if !self.turn.fence_supervisor() {
            return CustodyHandoff::SupervisorInSharedState;
        }
        // Published only now, and only here: the supervisor can no longer touch what the turn
        // excludes, which is the whole of what `supervisor_done` ever meant to the release rule.
        self.turn.supervisor_done.store(true, Ordering::SeqCst);
        self.turn.maybe_release();
        CustodyHandoff::HandedOn
    }

    /// Give this turn a thread of its own that asks until the answer is a handoff.
    ///
    /// It sleeps to `deadline` first — before it there is nothing to do, because work that has not
    /// reached its deadline is work whose supervisor is not yet late — then re-asks every
    /// [`CUSTODY_TICK`] for at most `patience`. `patience` is the window in which it will report at
    /// all; it is NOT permission to release late, and it never relaxes the three conditions.
    ///
    /// It holds no part of the supervisor's state and calls nothing the supervisor owns, so where
    /// the supervisor is does not appear in when this thread runs.
    pub fn arm(self, deadline: Instant, patience: Duration) -> CustodyWatch {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(left.min(CUSTODY_TICK));
            }
            let giving_up_at = Instant::now() + patience;
            let mut last = self.attempt_handoff();
            while !matches!(
                last,
                CustodyHandoff::HandedOn | CustodyHandoff::AlreadyFree
            ) && Instant::now() < giving_up_at
            {
                std::thread::sleep(CUSTODY_TICK);
                last = self.attempt_handoff();
            }
            // A closed receiver means the caller stopped listening, which is not this thread's
            // problem: the handoff has already happened or already been refused.
            let _ = tx.send(last);
        });
        CustodyWatch { outcome: rx }
    }
}

/// What an armed [`CustodyBailiff`] concluded. Dropping it does not stop the bailiff — custody is
/// not contingent on anyone watching.
pub struct CustodyWatch {
    outcome: std::sync::mpsc::Receiver<CustodyHandoff>,
}

impl CustodyWatch {
    /// Block for at most `within` for the bailiff's conclusion. `None` means it has not concluded,
    /// which is not the same as a refusal.
    pub fn wait(&self, within: Duration) -> Option<CustodyHandoff> {
        self.outcome.recv_timeout(within).ok()
    }
}

impl Drop for TurnControl {
    /// A supervisor that is dropped — cancelled at an await, aborted, or unwound — revokes exactly
    /// as one that returned. Work already running keeps the turn.
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// The work's end of the turn, before the work has started. Moved into the closure that does the
/// actual blocking operation, so the turn travels to the thread that will really hold it.
pub struct DeliveryTurn {
    /// `None` only after [`Self::begin`] has handed the cell to a [`RunningWork`].
    turn: Option<Arc<Turn>>,
}

impl DeliveryTurn {
    /// The first act of the actual work, on the thread that will do it.
    ///
    /// This is the queue-admission gate: a push revoked while it waited for a blocking slot finds
    /// the turn already ended and does nothing at all — no config rewrite, no pack, no wire.
    pub fn begin(mut self) -> Result<RunningWork, WorkEnded> {
        let turn = self.turn.take().expect("a turn is begun at most once");
        match turn
            .state
            .compare_exchange(PENDING, RUNNING, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {
                let running = RunningWork { turn };
                // Revoked-but-won-the-race, or already past the deadline: refuse, and let
                // `RunningWork`'s drop hand the turn straight back.
                running.check()?;
                Ok(running)
            }
            Err(_) => Err(turn.reason()),
        }
    }

    /// What is left of this delivery's absolute work budget.
    pub fn remaining(&self) -> Duration {
        match &self.turn {
            Some(turn) => turn.deadline.saturating_duration_since(Instant::now()),
            None => Duration::ZERO,
        }
    }
}

impl Drop for DeliveryTurn {
    /// Dropped before `begin`: the work will never start, so the turn is free. This covers the
    /// future being cancelled before it ever reached the blocking dispatch, and the runtime
    /// discarding a queued blocking closure at shutdown.
    fn drop(&mut self) {
        if let Some(turn) = &self.turn {
            turn.end_now();
        }
    }
}

/// The turn while the actual work runs. Dropping it — on return, on refusal, on unwind, on whatever
/// thread reaches it — is what hands the turn to the next delivery.
#[derive(Debug)]
pub struct RunningWork {
    turn: Arc<Turn>,
}

impl RunningWork {
    /// The question every phase of the work asks before doing more.
    pub fn check(&self) -> Result<(), WorkEnded> {
        self.turn.check()
    }

    /// A cheap clonable checker for the layers below (the transport's per-leg and per-chunk gates).
    pub fn lifetime(&self) -> WorkLifetime {
        WorkLifetime {
            turn: Arc::clone(&self.turn),
        }
    }

    /// What is left of this delivery's absolute work budget.
    pub fn remaining(&self) -> Duration {
        self.turn.deadline.saturating_duration_since(Instant::now())
    }

    /// The absolute deadline itself, for callers that must pass it to a bounded blocking wait (the
    /// signer call) rather than poll it.
    pub fn deadline(&self) -> Instant {
        self.turn.deadline
    }

    /// **THIS PROCESS OBSERVED THE DELIVERY'S EXIT.** Published by the work, on the thread that
    /// observed it, at the one site that can tell the difference: a kill was issued AND the kernel
    /// reported the exit (`crate::delivery_executor::exclusion_after_reap` said `Release`).
    ///
    /// Nothing else may call it. Dropping [`RunningWork`] without it is the unconfirmed-exit path,
    /// and it retains: the ordinary release still needs the supervisor, and the [`CustodyBailiff`]
    /// refuses. That is deliberate — the bailiff exists to remove a stalled SUPERVISOR from the
    /// bound, never to weaken what an unknown child costs.
    pub fn confirm_exit(&self) {
        self.turn.exit_confirmed.store(true, Ordering::SeqCst);
    }
}

impl Drop for RunningWork {
    fn drop(&mut self) {
        self.turn.end_now();
    }
}

/// A handle that can only ASK whether the work may continue. Clonable, `Send + Sync`, and holds no
/// part of the delivery's state — so it can be handed to libgit2 callbacks and to a blocking
/// transport thread that outlives the delivery arm.
#[derive(Clone)]
pub struct WorkLifetime {
    turn: Arc<Turn>,
}

impl WorkLifetime {
    pub fn check(&self) -> Result<(), WorkEnded> {
        self.turn.check()
    }

    pub fn remaining(&self) -> Duration {
        self.turn.deadline.saturating_duration_since(Instant::now())
    }

    pub fn deadline(&self) -> Instant {
        self.turn.deadline
    }

    /// The same question in the shape the transport already asks before every wire request
    /// (`git_transport::AuthorityCheck`), so the lifetime gate needs no second plumbing type.
    pub fn checker(&self) -> Arc<dyn Fn() -> Result<(), String> + Send + Sync> {
        let turn = Arc::clone(&self.turn);
        Arc::new(move || turn.check().map_err(|ended| ended.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cancellation the lock bug is about: revoked BEFORE a blocking slot ever came free. The
    /// work never starts, so the turn is handed back at once instead of sitting queued.
    #[test]
    fn revoking_work_that_never_started_hands_the_turn_back() {
        let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(60));
        assert!(
            control.holds_ownership(),
            "the turn is taken while work is pending"
        );
        assert_eq!(control.end(), TurnRelease::NeverStarted);
        assert!(
            !control.holds_ownership(),
            "work that never started must not keep the delivery turn"
        );
        let refused = turn.begin().expect_err("revoked work must not begin");
        assert_eq!(refused, WorkEnded::Cancelled);
    }

    /// The bug itself, stated as an assertion: a revoked delivery whose work IS running keeps the
    /// turn. Releasing here is what lets a second `git-receive-pack` open on a live upload.
    #[test]
    fn revoking_running_work_does_not_hand_the_turn_back() {
        let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(60));
        let running = turn.begin().expect("work begins");
        assert_eq!(control.end(), TurnRelease::StillRunning);
        assert!(
            control.holds_ownership(),
            "the turn must stay taken while the actual work is still running"
        );
        assert_eq!(
            running.check().expect_err("revoked work refuses to continue"),
            WorkEnded::Cancelled
        );
        drop(running);
        assert!(
            !control.holds_ownership(),
            "the turn is handed back when the work actually stops"
        );
    }

    /// The absolute deadline is the work's, not the caller's: it refuses admission even when nobody
    /// cancelled anything.
    #[test]
    fn an_expired_deadline_refuses_admission_and_frees_the_turn() {
        let (control, turn) = delivery_turn((), Instant::now() - Duration::from_millis(1));
        let refused = turn.begin().expect_err("expired work must not begin");
        assert_eq!(refused, WorkEnded::DeadlineExceeded);
        assert!(
            control.work_ended(),
            "work refused at admission is finished work"
        );
        // The work is finished; the supervising arm is not. The turn is handed back the moment that
        // side is done too — which for the production wrapper is its own return.
        control.end();
        assert!(
            !control.holds_ownership(),
            "work refused at admission holds no turn"
        );
    }

    /// The other half of the same rule: the operation finishing does NOT free the turn while the
    /// arm that supervises it is still inside the section the turn excludes. Release it on the
    /// work's return alone and the next delivery enters while the previous one is still finishing.
    #[test]
    fn finished_work_keeps_the_turn_until_the_supervisor_is_done_too() {
        let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(60));
        let running = turn.begin().expect("work begins");
        drop(running);
        assert!(control.work_ended(), "the work has stopped");
        assert!(
            control.holds_ownership(),
            "the turn stays taken until the supervising side is finished with it"
        );
        control.end();
        assert!(
            !control.holds_ownership(),
            "both sides finished: the turn is handed back"
        );
    }

    /// A supervisor that disappears revokes exactly as one that returned.
    #[test]
    fn a_dropped_supervisor_revokes_pending_work() {
        let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(60));
        drop(control);
        assert_eq!(
            turn.begin().expect_err("a dropped supervisor revokes"),
            WorkEnded::Cancelled
        );
    }

    /// ...and a supervisor that disappears while the work RUNS leaves the turn with the work.
    #[test]
    fn a_dropped_supervisor_leaves_running_work_holding_the_turn() {
        let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(60));
        let running = turn.begin().expect("work begins");
        let lifetime = running.lifetime();
        drop(control);
        assert!(
            lifetime.check().is_err(),
            "the work must learn its owner is gone"
        );
        // The only observer left is the work itself; it still holds the turn until it stops.
        drop(running);
        assert!(lifetime.check().is_err());
    }

    /// The transport-shaped checker answers the same state, and names it.
    #[test]
    fn the_checker_names_the_refusal() {
        let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(60));
        let running = turn.begin().expect("work begins");
        let checker = running.lifetime().checker();
        assert!(checker().is_ok(), "live work may transmit");
        control.end();
        let message = checker().expect_err("revoked work may not transmit");
        assert!(
            message.contains("cancelled"),
            "the refusal must name the cancellation, got {message:?}"
        );
    }
}
