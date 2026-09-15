//! What the parent OWES while a delivery push child is alive: every phase of it bounded, and the
//! seat released only on an exit this process observed.
//!
//! Each test here is written to be *detected by a mutation*: remove the bound and the test fails
//! rather than hangs, kill the pid instead of the group and the test fails rather than passes
//! quietly. See `.gate/mutate.py` and the M7/M8 controls in this PR's evidence.
//!
//! Platform: POSIX. These tests use `SIGKILL`, process groups and `waitpid`, which is the same
//! contract `delivery_executor` documents for the three shipped platforms. They are MEASURED on
//! whatever host runs them and claim nothing about a host they did not run on.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::{
    ExecutorError, KillableChild, MAX_FRAME_BYTES, PushRequest, REAP_BOUND, encode_frame,
    run_push_in_child,
};
use maxplayer_core::git_transport::{AuthMinter, AuthorityCheck};

/// A minter that must never be reached by these tests.
fn no_mint() -> AuthMinter {
    Arc::new(|_destination: &str| {
        panic!("these tests never get far enough to authorize a leg");
    })
}

/// An authority that is still live.
fn still_ours() -> AuthorityCheck {
    Arc::new(|| Ok(()))
}

fn shell_child(script: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mp-custody-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let path = dir.join("child.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("fixture script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

/// Run the supervisor on its own thread and REFUSE to wait for it longer than `patience`.
///
/// The defect these tests are about is a parent that never comes back. A test that simply calls the
/// supervisor would reproduce that defect as a hung test binary, which reads as infrastructure
/// trouble rather than as a failure. Bounded here, a lost bound is a red test.
fn supervise_within(
    patience: Duration,
    program: PathBuf,
    request: PushRequest,
    deadline: Instant,
) -> Result<(Result<String, ExecutorError>, Duration), RecvTimeoutError> {
    let (done, answer) = channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = run_push_in_child(&program, &request, deadline, no_mint(), still_ours());
        let _ = done.send((outcome, started.elapsed()));
    });
    answer.recv_timeout(patience)
}

/// The parent stamps a remaining duration AND the same deadline as an absolute wall-clock instant,
/// so the child can charge the pipe transit to itself. Tests that build a request by hand stamp
/// both from one moment, exactly as the parent does.
fn unix_ms_from_now(budget_ms: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
        .saturating_add(budget_ms)
}

fn request_with_branch(branch: String) -> PushRequest {
    PushRequest {
        workdir: std::env::temp_dir(),
        remote_url: "https://relay.invalid/repo.git".to_owned(),
        branch,
        gated_oid: "0".repeat(40),
        authenticated: false,
        budget_ms: 1_000,
        deadline_unix_ms: unix_ms_from_now(1_000),
    }
}

/// F1 / the parent's own WRITES.
///
/// A child that never reads its stdin blocks the parent's write once the pipe buffer is full. That
/// write used to happen on the drive thread, before the first check of the deadline, and the drive
/// thread is the only thread that can issue the kill: the parent parked there with the kill
/// unreachable behind it, for as long as the child felt like not reading. Ordinary backpressure —
/// not the documented uninterruptible-sleep exception.
///
/// So: a request too large for a pipe buffer, a child that reads nothing, and a short deadline. The
/// supervisor must come back inside the deadline plus the reap bound, with the overrun it measured.
#[test]
fn a_parent_write_to_a_child_that_never_reads_is_bounded_by_the_deadline() {
    // Comfortably past any platform's pipe buffer (64 KiB on Linux, 16–64 KiB on darwin) and
    // comfortably inside the protocol's own frame cap, so this is a stalled WRITE and not a
    // refused frame.
    let oversized_for_a_pipe = 512 * 1024;
    assert!(
        oversized_for_a_pipe < MAX_FRAME_BYTES,
        "this test must stall a write, not trip the frame cap"
    );
    let program = shell_child("sleep 60");
    let budget = Duration::from_millis(400);
    let deadline = Instant::now() + budget;

    let (outcome, took) = supervise_within(
        budget + REAP_BOUND + Duration::from_secs(10),
        program,
        request_with_branch("b".repeat(oversized_for_a_pipe)),
        deadline,
    )
    .expect(
        "the supervisor never returned: a parent write to a child that does not read is outside \
         the delivery's deadline",
    );

    match outcome {
        Err(ExecutorError::Killed { .. }) => {}
        other => panic!(
            "a stalled parent write must end as a deadline kill, not as {:?}",
            other.map_err(|error| error.to_string())
        ),
    }
    assert!(
        took <= budget + REAP_BOUND + Duration::from_secs(5),
        "the supervisor took {took:?}, which is outside the deadline ({budget:?}) plus the reap \
         bound ({REAP_BOUND:?})"
    );
}

/// F1 / M8's target: the kill goes to the process GROUP, and the proof is a DESCENDANT's exit.
///
/// The child here leaves a grandchild holding the write end of the same stdout pipe. Killing the
/// direct child is not enough: the pipe stays open, the parent's cleanup cannot observe the end of
/// the output, and this delivery's local phase is not demonstrably over — which is exactly the
/// `CleanupUnbounded` the executor fails closed with.
///
/// Passing therefore says something stronger than "the child died": it says the parent's kill
/// reached a process it never named, and the delivery's whole process group stopped. That is the
/// descendant-exit evidence the group kill was previously asserted without.
#[test]
fn the_kill_reaches_a_grandchild_that_inherited_the_pipe() {
    // A background descendant, holding the inherited stdout, outliving its own parent's exit.
    let program = shell_child("sleep 60 &\nexec sleep 60");
    let budget = Duration::from_millis(300);
    let deadline = Instant::now() + budget;

    let (outcome, took) = supervise_within(
        budget + REAP_BOUND + Duration::from_secs(10),
        program,
        request_with_branch("descendants".to_owned()),
        deadline,
    )
    .expect("the supervisor never returned");

    match outcome {
        Err(ExecutorError::Killed { .. }) => {}
        Err(ExecutorError::CleanupUnbounded { waited }) => panic!(
            "something that inherited this delivery's pipe was still holding it {waited:?} after \
             the kill: the kill did not reach the whole process group"
        ),
        other => panic!(
            "expected a deadline kill, got {:?}",
            other.map_err(|error| error.to_string())
        ),
    }
    // The cleanup wait is bounded by REAP_BOUND; a run that needed all of it is a run where the
    // descendant did NOT go with the group, even if it eventually did.
    assert!(
        took < budget + REAP_BOUND,
        "the supervisor needed {took:?}, i.e. it sat out the cleanup bound waiting for a pipe the \
         group kill should already have closed"
    );
}

/// F2 / M7's target: `kill_and_reap` returns only on an exit the KERNEL reported.
///
/// "Killed" and "exited" are different facts, and the seat is released on the second one. This pins
/// the difference in the only way that does not need a wedged kernel: after `kill_and_reap` says
/// `Ok`, THIS process must already have reaped that pid — a second `waitpid` must find no such
/// child. A `kill_and_reap` that returned as soon as the signal was issued would leave the child
/// un-waited-for, and that `waitpid` would find it.
#[test]
fn a_confirmed_exit_means_this_process_already_reaped_the_child() {
    let program = shell_child("sleep 60");
    let mut child = KillableChild::spawn(&program, &["__delivery-push"]).expect("spawn");
    let pid = child.pid();

    let confirmed = child.kill_and_reap().expect("a killable child is reapable");
    assert!(child.is_reaped(), "a confirmed exit is recorded as one");
    assert!(
        confirmed < REAP_BOUND,
        "a runnable process that was SIGKILLed took {confirmed:?} to confirm"
    );

    // ECHILD: no child by that pid exists for this process to wait for, because this process
    // already waited for it. Anything else — 0 (still running) or the pid (a zombie nobody
    // reaped) — means the exit was not confirmed when we said it was.
    let mut status = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert_eq!(
        (waited, errno),
        (-1, Some(libc::ECHILD)),
        "after a confirmed exit, waitpid({pid}) returned {waited} (errno {errno:?}); the child was \
         signalled but its exit was never collected, so 'the kernel reported the exit' is false"
    );
}

/// F1 / the protocol's cap is a cap on the PROTOCOL, not on one direction of it.
#[test]
fn an_oversized_frame_is_refused_by_the_writer_not_discovered_by_the_reader() {
    let refused = encode_frame(&PushRequest {
        workdir: PathBuf::from("/tmp"),
        remote_url: "https://relay.invalid/repo.git".to_owned(),
        branch: "x".repeat(MAX_FRAME_BYTES + 1),
        gated_oid: "0".repeat(40),
        authenticated: false,
        budget_ms: 1,
        deadline_unix_ms: unix_ms_from_now(1),
    })
    .expect_err("a frame over the cap must not be written");
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);

    let accepted = encode_frame(&PushRequest {
        workdir: PathBuf::from("/tmp"),
        remote_url: "https://relay.invalid/repo.git".to_owned(),
        branch: "x".repeat(1024),
        gated_oid: "0".repeat(40),
        authenticated: false,
        budget_ms: 1,
        deadline_unix_ms: unix_ms_from_now(1),
    })
    .expect("an ordinary frame is written");
    assert!(accepted.ends_with('\n'), "frames are newline-delimited");
}
