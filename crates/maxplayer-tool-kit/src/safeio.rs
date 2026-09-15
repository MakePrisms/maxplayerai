//! Race-safe, no-follow file access confined to a job directory.
//!
//! The defect this module closes (advisor F2). The holder used to canonicalize a job-supplied
//! path, check it, and return a **string**. The trusted CLI later re-opened that string. Between
//! the check and the open, a job that can write its own work directory could replace the checked
//! name — or one of its parent directories — with a symlink pointing anywhere the holder can
//! reach. The later open followed the symlink. What was checked and what was read were then two
//! different files.
//!
//! The fix has one idea: **open once, following no symlink on any component, and hand back the
//! descriptor.** There is no separate "check" to race against, because the open *is* the check.
//! A name that resolves through a symlink is refused, whether the symlink was planted before the
//! call or a microsecond ago — `O_NOFOLLOW` does not follow it either way.
//!
//! Why `libc` and not `std`. A single `open(path, O_NOFOLLOW)` guards only the **final**
//! component; a symlinked parent is still followed, and `std` exposes no `openat`. Genuine
//! confinement needs each component opened relative to the previous directory's descriptor with
//! `O_DIRECTORY | O_NOFOLLOW`. That is `openat`, which is why this is the one module that reaches
//! past `std`.

use crate::validate::Reject;
use std::ffi::CString;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path};

/// Open an input file that a job named, resolving it inside `root` and following no symlink on
/// any component. The returned descriptor is the file that was checked: nothing the job does
/// afterwards can redirect a read from it.
///
/// `root` must already be the holder's canonical job root (the holder canonicalizes it once, at
/// attach time, and it is not job-writable as a whole). `rel` must be relative with only ordinary
/// components; `validate` guarantees that before this is called, and it is re-asserted here so the
/// function is safe on its own.
pub fn open_input(root: &Path, rel: &Path, param: &str) -> Result<File, Reject> {
    let (dir, last) = walk_to_parent(root, rel, param, false)?;
    // O_NOFOLLOW on the final component: a symlink here is refused, not resolved.
    //
    // O_NONBLOCK: a FIFO with no writer blocks a plain open until a writer appears. A job can
    // plant one and hold this thread forever, after its detach too. With O_NONBLOCK the open
    // returns at once, the type check below refuses the FIFO, and a regular file is not affected
    // by the flag. The flag is cleared again before the descriptor is handed out.
    let fd = openat_raw(
        dir.as_raw_fd(),
        &last,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|e| match e {
        libc::ELOOP => Reject::SymlinkedPath { param: param.to_string() },
        libc::ENOENT => Reject::MissingInput { param: param.to_string() },
        libc::ENOTDIR => Reject::MissingInput { param: param.to_string() },
        // A socket file: the kernel refuses to open it as a file.
        libc::ENXIO | libc::EOPNOTSUPP => Reject::NotARegularFile { param: param.to_string() },
        _ => Reject::MissingInput { param: param.to_string() },
    })?;
    let file = unsafe { File::from_raw_fd(fd) };
    // A directory, device or FIFO is not an input. fstat on the held descriptor, so this is a
    // fact about the object that was opened, not about a name that could since have changed.
    // Nothing is read before this check.
    let md = file.metadata().map_err(|_| Reject::NotARegularFile { param: param.to_string() })?;
    if !md.file_type().is_file() {
        return Err(Reject::NotARegularFile { param: param.to_string() });
    }
    clear_nonblock(&file);
    Ok(file)
}

/// Create (or truncate) an output file that a job named, resolving its parent inside `root` and
/// following no symlink on any component, including the final one. A symlink already sitting at
/// the output name is refused rather than written through, so a job cannot aim another job's file
/// — or the holder's own state — at its output slot.
///
/// The object at the name must be a regular file (or absent). A FIFO, a socket, a device or a
/// directory planted at the name is refused before any byte is written and before any truncation.
pub fn create_output(root: &Path, rel: &Path, param: &str) -> Result<File, Reject> {
    let (dir, last) = walk_to_parent(root, rel, param, true)?;
    // O_CREAT | O_WRONLY to write it; O_NOFOLLOW so an existing symlink at this name is an error
    // (ELOOP), never a redirect. Mode 0600: an output file is not group- or world-open.
    //
    // Deliberately NOT O_TRUNC, and WITH O_NONBLOCK. A FIFO planted at the output name would block
    // a plain open until a reader appears, and a later write would publish bytes into it. With
    // O_NONBLOCK a FIFO with no reader fails at once (ENXIO), a FIFO with a reader opens but is
    // refused by the type check below, and the truncation happens only after that check has
    // proven the object is a regular file.
    let fd = openat_raw(
        dir.as_raw_fd(),
        &last,
        libc::O_CREAT | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0o600,
    )
    .map_err(|e| match e {
        libc::ELOOP => Reject::SymlinkedPath { param: param.to_string() },
        libc::ENOENT | libc::ENOTDIR => Reject::OutputParentMissing { param: param.to_string() },
        // A FIFO with no reader, a socket file, or a directory at the output name.
        libc::ENXIO | libc::EOPNOTSUPP | libc::EISDIR => Reject::NotARegularFile { param: param.to_string() },
        _ => Reject::OutputParentMissing { param: param.to_string() },
    })?;
    let file = unsafe { File::from_raw_fd(fd) };
    let md = file.metadata().map_err(|_| Reject::NotARegularFile { param: param.to_string() })?;
    if !md.file_type().is_file() {
        return Err(Reject::NotARegularFile { param: param.to_string() });
    }
    // The truncation, now that the descriptor is known to be a regular file.
    file.set_len(0).map_err(|_| Reject::NotARegularFile { param: param.to_string() })?;
    clear_nonblock(&file);
    Ok(file)
}

/// Clear `O_NONBLOCK` on a descriptor that the type check proved to be a regular file.
///
/// Best effort: a regular file never returns `EAGAIN` from `read` or `write`, so the flag has no
/// effect on the reads and writes that follow. Clearing it keeps the descriptor ordinary for any
/// later consumer that does care.
fn clear_nonblock(file: &File) {
    let fd = file.as_raw_fd();
    // SAFETY: fcntl on a descriptor this function's caller owns for the duration of the call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 && flags & libc::O_NONBLOCK != 0 {
        // SAFETY: as above; a failure leaves the flag set, which is harmless on a regular file.
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    }
}

/// Open `root`, then descend through every component of `rel` except the last, opening each one
/// relative to the previous directory with `O_DIRECTORY | O_NOFOLLOW`. Returns the descriptor of
/// the final parent directory and the last component's name. A symlinked intermediate directory
/// fails here (ELOOP), which is what confines the walk to the real subtree of `root`.
fn walk_to_parent(root: &Path, rel: &Path, param: &str, for_output: bool) -> Result<(File, CString), Reject> {
    // A missing intermediate directory reads as a missing input for a read, and a missing output
    // parent for a write.
    let missing = |param: &str| {
        if for_output {
            Reject::OutputParentMissing { param: param.to_string() }
        } else {
            Reject::MissingInput { param: param.to_string() }
        }
    };
    // Re-assert the grammar `validate` already enforced: relative, ordinary components only. This
    // module never trusts its caller to have done that.
    let mut names: Vec<CString> = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(os) => names.push(
                cstr(os.as_bytes()).map_err(|_| Reject::NonNormalComponent { param: param.to_string() })?,
            ),
            _ => return Err(Reject::NonNormalComponent { param: param.to_string() }),
        }
    }
    let Some(last) = names.pop() else {
        return Err(Reject::EmptyPath { param: param.to_string() });
    };

    // The root itself is the holder's canonical directory. Open it O_DIRECTORY; it is not a
    // job-writable name, so O_NOFOLLOW on root is not the control that matters — the per-component
    // no-follow descent below is.
    let root_c = cstr(root.as_os_str().as_bytes()).map_err(|_| Reject::BadJobRoot)?;
    let root_fd = openat_raw(libc::AT_FDCWD, &root_c, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0)
        .map_err(|_| Reject::BadJobRoot)?;
    let mut dir = unsafe { File::from_raw_fd(root_fd) };

    for name in &names {
        let fd = openat_raw(
            dir.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|e| match e {
            // A symlinked or non-directory intermediate is how a path leaves its job subtree.
            libc::ELOOP | libc::ENOTDIR => Reject::EscapesJobDir { param: param.to_string() },
            libc::ENOENT => missing(param),
            _ => Reject::EscapesJobDir { param: param.to_string() },
        })?;
        dir = unsafe { File::from_raw_fd(fd) };
    }
    Ok((dir, last))
}

fn cstr(bytes: &[u8]) -> Result<CString, ()> {
    CString::new(bytes).map_err(|_| ())
}

/// Thin `openat` wrapper returning the raw fd or the `errno` on failure. `EINTR` is retried.
///
/// `openat` is variadic in C; the mode argument is read only when `O_CREAT` is in `flags`, and is
/// passed here as `c_uint` under the usual default argument promotion.
fn openat_raw(dirfd: RawFd, name: &CString, flags: libc::c_int, mode: libc::mode_t) -> Result<RawFd, libc::c_int> {
    loop {
        // SAFETY: `name` is a valid NUL-terminated C string held for the duration of the call.
        let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags, mode as libc::c_uint) };
        if fd >= 0 {
            return Ok(fd);
        }
        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if err == libc::EINTR {
            continue;
        }
        return Err(err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A fresh directory that plays one job's root. Removed on drop.
    struct Root(std::path::PathBuf);

    impl Root {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("mtk-safeio-{}-{n}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the test root");
            Root(dir.canonicalize().expect("canonicalize the test root"))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mkfifo(path: &Path) {
        let c = CString::new(path.as_os_str().as_bytes()).expect("a C path");
        // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
    }

    /// Run `f` on its own thread and require an answer within `limit`. A call that blocks on a
    /// FIFO would hang this test forever without the bound; with it, the hang is a failure.
    fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(limit).expect("the call must return, not block on the object at the name")
    }

    fn is_nonblocking(file: &File) -> bool {
        // SAFETY: fcntl on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        flags >= 0 && flags & libc::O_NONBLOCK != 0
    }

    #[test]
    fn a_fifo_at_the_input_name_is_refused_at_once() {
        let root = Root::new("fifo-in");
        mkfifo(&root.path().join("input.txt"));
        let dir = root.path().to_path_buf();
        let outcome = within(Duration::from_secs(2), move || {
            open_input(&dir, Path::new("input.txt"), "input").map(|_| ())
        });
        assert!(
            matches!(outcome, Err(Reject::NotARegularFile { .. })),
            "a FIFO with no writer must be refused as not a regular file, got {outcome:?}"
        );
    }

    #[test]
    fn a_fifo_at_the_input_name_with_a_writer_is_refused_too() {
        let root = Root::new("fifo-in-writer");
        let fifo = root.path().join("input.txt");
        mkfifo(&fifo);
        // A job holds a nonblocking writer open; the open then succeeds and the type check refuses.
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let writer = openat_raw(libc::AT_FDCWD, &c, libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC, 0)
            .expect("hold the FIFO open");
        let writer = unsafe { File::from_raw_fd(writer) };
        let dir = root.path().to_path_buf();
        let outcome = within(Duration::from_secs(2), move || {
            open_input(&dir, Path::new("input.txt"), "input").map(|_| ())
        });
        assert!(matches!(outcome, Err(Reject::NotARegularFile { .. })), "got {outcome:?}");
        drop(writer);
    }

    #[test]
    fn a_fifo_at_the_output_name_is_refused_and_receives_nothing() {
        let root = Root::new("fifo-out");
        let fifo = root.path().join("out.txt");
        mkfifo(&fifo);

        // No reader: the nonblocking open fails at once.
        let dir = root.path().to_path_buf();
        let outcome = within(Duration::from_secs(2), move || {
            create_output(&dir, Path::new("out.txt"), "output").map(|_| ())
        });
        assert!(
            matches!(outcome, Err(Reject::NotARegularFile { .. })),
            "a FIFO with no reader must be refused as not a regular file, got {outcome:?}"
        );

        // A reader held by the job: the open succeeds, the type check refuses, and no byte reaches
        // the reader.
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let reader = openat_raw(libc::AT_FDCWD, &c, libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC, 0)
            .expect("hold a reader on the FIFO");
        let mut reader = unsafe { File::from_raw_fd(reader) };
        let dir = root.path().to_path_buf();
        let outcome = within(Duration::from_secs(2), move || {
            create_output(&dir, Path::new("out.txt"), "output").map(|_| ())
        });
        assert!(matches!(outcome, Err(Reject::NotARegularFile { .. })), "got {outcome:?}");
        let mut buf = [0u8; 8];
        let read = reader.read(&mut buf);
        assert!(
            matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock) || matches!(read, Ok(0)),
            "nothing may have been written into the FIFO, got {read:?}"
        );
        // The FIFO is still a FIFO: nothing replaced or truncated it.
        assert!(std::fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
    }

    #[test]
    fn a_regular_file_still_opens_for_input_and_for_output() {
        let root = Root::new("regular");
        std::fs::write(root.path().join("in.txt"), "hello").unwrap();
        std::fs::write(root.path().join("out.txt"), "OLD CONTENT THAT MUST GO").unwrap();

        let mut input = open_input(root.path(), Path::new("in.txt"), "input").expect("a regular input opens");
        assert!(!is_nonblocking(&input), "the flag is cleared before the descriptor is handed out");
        let mut text = String::new();
        input.read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello");

        let mut output = create_output(root.path(), Path::new("out.txt"), "output").expect("a regular output opens");
        assert!(!is_nonblocking(&output));
        output.write_all(b"new").unwrap();
        drop(output);
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "new", "the output is truncated first");

        let mut fresh = create_output(root.path(), Path::new("fresh.txt"), "output").expect("a new output is created");
        fresh.write_all(b"made").unwrap();
        drop(fresh);
        assert_eq!(std::fs::read_to_string(root.path().join("fresh.txt")).unwrap(), "made");
    }

    #[test]
    fn a_directory_at_the_output_name_is_refused() {
        let root = Root::new("dir-out");
        std::fs::create_dir(root.path().join("out.txt")).unwrap();
        let outcome = create_output(root.path(), Path::new("out.txt"), "output").map(|_| ());
        assert!(matches!(outcome, Err(Reject::NotARegularFile { .. })), "got {outcome:?}");
    }

    #[test]
    fn a_symlink_at_either_name_is_still_refused() {
        let root = Root::new("symlink");
        let outside = Root::new("symlink-outside");
        std::fs::write(outside.path().join("secret.txt"), "SENTINEL").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.path().join("in.txt")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.path().join("out.txt")).unwrap();

        let input = open_input(root.path(), Path::new("in.txt"), "input").map(|_| ());
        assert!(matches!(input, Err(Reject::SymlinkedPath { .. })), "got {input:?}");
        let output = create_output(root.path(), Path::new("out.txt"), "output").map(|_| ());
        assert!(matches!(output, Err(Reject::SymlinkedPath { .. })), "got {output:?}");
        // Not read through, not truncated through.
        assert_eq!(std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(), "SENTINEL");
    }
}
