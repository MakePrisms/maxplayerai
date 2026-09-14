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

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
    deadline: Instant,
    ownership: Mutex<Option<Box<dyn Send>>>,
}

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
            self.release_ownership();
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
    /// revocation in its own immediate check and refuses. Exactly one of the two sides ever releases
    /// ownership.
    pub fn end(&self) -> TurnRelease {
        self.turn.cancelled.store(true, Ordering::SeqCst);
        match self
            .turn
            .state
            .compare_exchange(PENDING, ENDED, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {
                self.turn.release_ownership();
                TurnRelease::NeverStarted
            }
            Err(RUNNING) => TurnRelease::StillRunning,
            Err(_) => TurnRelease::AlreadyEnded,
        }
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
            !control.holds_ownership(),
            "work refused at admission holds no turn"
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
