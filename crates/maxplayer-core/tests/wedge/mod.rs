//! A child that will not stop until it is killed — as ONE process.
//!
//! # Why the wedge must not fork
//!
//! Every wedge in these suites used to be `while :; do sleep 0.05; done`: a shell forking a `sleep`
//! twenty times a second. The executor kills the child's process GROUP, and on macOS that kill is
//! not atomic against a fork in progress. `killpg` collects the group's members into a snapshot
//! under the group lock and then signals the snapshot (`pgrp_iterate`, xnu `bsd/kern/kern_proc.c`);
//! a `sleep` the shell is forking at that instant is inserted after the snapshot and is never
//! signalled. It inherits the child's stdout, and the executor — correctly — will not hand the seat
//! on until that pipe reaches end of file (`delivery_executor`, phase 12). Linux re-checks pending
//! signals inside `copy_process` and restarts the fork, so the window is a macOS one; the gate runs
//! on both.
//!
//! Measured on an idle M2 Pro (Darwin 25.6): 6 of 1500 group kills of the old wedge left such a
//! survivor, each holding the pipe for 53–63 ms — an escaped `sleep 0.05` living out its life. Under
//! load the survivor's life is the host's fork/exec latency, which the gate's forty parallel test
//! binaries have pushed past ten seconds on this branch (see `delivery_push_production_child.rs`,
//! `CHILD_STARTUP`). That is the shape of `CleanupUnbounded` reported at 5.1 s, and of an abort
//! handover measured at 1.8 s against a 50 ms poll: the kill was prompt; the seat then waited for a
//! process the kill never reached. A tail of `sleep 5` written right after the child's last frame is
//! the same race with worse odds, because the parent's kill lands within microseconds of the fork.
//!
//! The production child spawns no descendants, so a fixture standing in for it must not either.
//! This wedge blocks the shell ITSELF in a `read` on a FIFO it holds open for writing — no data ever
//! arrives and no writer ever closes, so the read never returns. `exec` and `read` are builtins:
//! the child is one process from its first line to its kill, and a group kill has exactly one member
//! to reach.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The shell lines that hold the fixture child in place until it is killed. Creates the FIFO the
/// child will block on inside `dir`, which must outlive the child.
///
/// Reads from descriptor 3, not from stdin: a fixture that must never read the parent's request,
/// or that has already closed its stdout, wedges exactly the same way.
pub fn wedge(dir: &Path) -> String {
    let fifo = dir.join("wedge.fifo");
    let path = CString::new(fifo.as_os_str().as_bytes()).expect("fifo path has no NUL");
    // SAFETY: `path` is a valid NUL-terminated C string for the duration of the call.
    let created = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    if created != 0 {
        let error = std::io::Error::last_os_error();
        assert_eq!(
            error.raw_os_error(),
            Some(libc::EEXIST),
            "mkfifo {}: {error}",
            fifo.display()
        );
    }
    format!(
        "exec 3<>'{}'\nwhile :; do read _wedge <&3; done\n",
        fifo.display()
    )
}
