//! An unconfirmed child CLOSES this process's delivery lane.
//!
//! `KillableChild::drop` has nowhere to return an error: a kill it could not confirm is recorded in
//! a process-wide count instead. A count nobody reads is a statistic, not custody — so this pins the
//! CONSUMER, on the production dispatch, end to end: record an unconfirmed child, then ask the real
//! `neutralize_then_push_in_child_off_runtime` for a delivery and watch it refuse before it begins
//! the turn, spawns a child, or touches the workdir.
//!
//! **One test, its own file, on purpose.** The count is process-wide and never decremented — that is
//! the point of it — so a test that raises it would refuse every other delivery test sharing its
//! process. This file is that process.
//!
//! Platform: POSIX (the executor's `SIGKILL`/`waitpid` contract). Measured on the host that ran it.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::{record_unconfirmed_child, unconfirmed_children};
use maxplayer_core::delivery_turn::{TurnRelease, delivery_turn};
use maxplayer_core::seller_git::neutralize_then_push_in_child_off_runtime;

#[test]
fn a_child_this_process_could_not_confirm_dead_refuses_the_next_delivery() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    assert_eq!(
        unconfirmed_children(),
        0,
        "this process starts with every child accounted for"
    );

    // A workdir that does not exist and a program that does not exist: if the refusal below ever
    // stops happening, the delivery would fail for one of those reasons instead — a DIFFERENT
    // error, which is what makes this assertion about the lane rule and not about the push.
    let workdir = PathBuf::from("/nonexistent/delivery/workdir");
    let program = PathBuf::from("/nonexistent/delivery/child");
    let deadline = Instant::now() + Duration::from_secs(30);

    let (control, turn) = delivery_turn((), deadline);
    record_unconfirmed_child();
    assert_eq!(unconfirmed_children(), 1);

    let refused = runtime
        .block_on(neutralize_then_push_in_child_off_runtime(
            program,
            workdir,
            "https://relay.invalid/repo.git".to_owned(),
            "delivery".to_owned(),
            "0".repeat(40),
            None,
            None,
            turn,
        ))
        .expect_err("a process with an unconfirmed delivery child must not start another delivery");

    let said = refused.to_string();
    assert!(
        said.contains("could not be confirmed to have exited"),
        "the refusal must name the unconfirmed child as its reason; it said: {said}"
    );
    // `StillRunning` here would mean the refusal let work begin anyway. The turn was never begun:
    // it was dropped at the refusal, which ends it without ever having run.
    assert_eq!(
        control.end(),
        TurnRelease::AlreadyEnded,
        "the refusal happens BEFORE the turn begins: no work may start on a lane that is closed"
    );
    assert!(
        !control.holds_ownership(),
        "a delivery that never started holds no exclusion"
    );
}
