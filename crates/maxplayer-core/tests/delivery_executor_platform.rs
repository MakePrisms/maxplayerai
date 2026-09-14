//! Feasibility gate F3 (platform) for the killable delivery-push executor.
//!
//! The design rests on one kernel property: `SIGKILL` cannot be caught, blocked or ignored, and a
//! process that has been reaped is a process that has stopped. Every claim in
//! `delivery_executor`'s documentation — and the whole reason the local pack phase moves into a
//! child at all — reduces to that. So it is exercised here against a child that *deliberately
//! refuses* the polite signal, rather than assumed from the man page.
//!
//! All three platforms this product ships (`.github/release-platforms.json`: two linux-musl targets
//! and aarch64-apple-darwin) are POSIX, so what this file proves on one of them is the same
//! mechanism the others run. There is no Windows artifact and therefore no `TerminateProcess` path
//! to test: a platform that is not on that list is not a platform this executor claims.

#![cfg(feature = "git-delivery")]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::{KillableChild, REAP_BOUND};

/// True while a pid still exists. Signal 0 performs the permission and existence checks and delivers
/// nothing, which is exactly the question "is it gone".
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// F3, the core claim: a child that ignores `SIGTERM` and never returns to its own control flow is
/// still ended by the deadline, and the parent learns it has ACTUALLY exited.
///
/// This is the shape of the failure the executor exists for. libgit2's delta search does not ignore
/// a signal by choice, but it refuses to look at the one cancellation answer it is offered
/// (`pack-objects.c:979`), which leaves the parent in the same position: no cooperation available.
#[test]
fn a_child_that_refuses_to_stop_is_stopped_anyway_and_its_exit_is_confirmed() {
    // `trap '' TERM` installs the ignore; the loop never checks anything. Nothing short of SIGKILL
    // ends this process.
    let mut child = KillableChild::spawn(
        Path::new("/bin/sh"),
        &["-c", "trap '' TERM; while :; do sleep 0.05; done"],
    )
    .expect("spawn a shell");
    let pid = child.pid();

    // Give it long enough to have installed the trap and entered the loop, so the kill lands on a
    // process that is genuinely refusing, not on one still starting up.
    std::thread::sleep(Duration::from_millis(200));
    assert!(alive(pid), "the fixture child died before the test began");

    let started = Instant::now();
    let reap = child
        .kill_and_reap()
        .expect("a runnable child is reapable well inside the bound");
    let measured = started.elapsed();

    assert!(
        child.is_reaped(),
        "kill_and_reap returned without the kernel confirming the exit; the turn must never be \
         released on an unconfirmed kill"
    );
    assert!(
        reap <= REAP_BOUND && measured <= REAP_BOUND,
        "a runnable child took {measured:?} to die, past the {REAP_BOUND:?} the executor budgets"
    );
    assert!(
        !alive(pid),
        "pid {pid} still exists after kill_and_reap said it was gone"
    );
}

/// F3, second half: the kill reaches the process GROUP.
///
/// libgit2 spawns nothing today, so this is defence rather than a live bug — but a bound that holds
/// only while no descendant exists is not a bound, and the kill is written to the group precisely so
/// the guarantee does not depend on that remaining true.
#[test]
fn the_kill_reaches_a_descendant_that_would_otherwise_outlive_the_delivery() {
    // The shell reports its background grandchild's pid on stdout, then both ignore TERM and block.
    let mut child = KillableChild::spawn(
        Path::new("/bin/sh"),
        &[
            "-c",
            "trap '' TERM; (trap '' TERM; while :; do sleep 0.05; done) & echo $!; \
             while :; do sleep 0.05; done",
        ],
    )
    .expect("spawn a shell");
    let parent_pid = child.pid();
    let stdout = child.stdout().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut reported = String::new();
    reader.read_line(&mut reported).expect("the grandchild pid");
    let grandchild: i32 = reported.trim().parse().expect("a pid");

    std::thread::sleep(Duration::from_millis(200));
    assert!(alive(grandchild), "the fixture grandchild never started");

    child.kill_and_reap().expect("the group is reapable");

    // The grandchild is not ours to reap — it is reparented to init — so allow the kernel a moment
    // to tear it down, then require it gone.
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(grandchild) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !alive(grandchild),
        "grandchild {grandchild} outlived the delivery; the kill did not reach the process group, \
         so work could continue after the seat was handed on"
    );
    assert!(!alive(parent_pid));
}

/// Dropping the handle kills and reaps too: success, error, panic and early return all leave the
/// same state behind, which is what lets the executor promise that no path out of it leaves a
/// delivery packing.
#[test]
fn dropping_the_handle_leaves_no_process_behind() {
    let pid = {
        let child = KillableChild::spawn(
            Path::new("/bin/sh"),
            &["-c", "trap '' TERM; while :; do sleep 0.05; done"],
        )
        .expect("spawn a shell");
        let pid = child.pid();
        std::thread::sleep(Duration::from_millis(150));
        assert!(alive(pid));
        pid
    };
    let deadline = Instant::now() + REAP_BOUND;
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !alive(pid),
        "pid {pid} survived the handle that owned it; a forgotten child is a delivery still running"
    );
}

/// A second reap is a no-op rather than a second kill: once the exit is confirmed the pid may have
/// been reused, and signalling it again would be signalling a stranger.
#[test]
fn reaping_twice_does_not_signal_a_reused_pid() {
    let mut child =
        KillableChild::spawn(Path::new("/bin/sh"), &["-c", "sleep 30"]).expect("spawn a shell");
    child.kill_and_reap().expect("first reap");
    assert!(child.is_reaped());
    let again = child.kill_and_reap().expect("second reap is a no-op");
    assert_eq!(
        again,
        Duration::ZERO,
        "the second reap did work; a confirmed-dead child must never be signalled again"
    );
}
