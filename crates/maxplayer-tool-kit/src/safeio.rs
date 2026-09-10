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
    let fd = openat_raw(dir.as_raw_fd(), &last, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0)
        .map_err(|e| match e {
            libc::ELOOP => Reject::SymlinkedPath { param: param.to_string() },
            libc::ENOENT => Reject::MissingInput { param: param.to_string() },
            libc::ENOTDIR => Reject::MissingInput { param: param.to_string() },
            _ => Reject::MissingInput { param: param.to_string() },
        })?;
    let file = unsafe { File::from_raw_fd(fd) };
    // A directory, device or FIFO is not an input. fstat on the held descriptor, so this is a
    // fact about the object that was opened, not about a name that could since have changed.
    let md = file.metadata().map_err(|_| Reject::NotARegularFile { param: param.to_string() })?;
    if !md.file_type().is_file() {
        return Err(Reject::NotARegularFile { param: param.to_string() });
    }
    Ok(file)
}

/// Create (or truncate) an output file that a job named, resolving its parent inside `root` and
/// following no symlink on any component, including the final one. A symlink already sitting at
/// the output name is refused rather than written through, so a job cannot aim another job's file
/// — or the holder's own state — at its output slot.
pub fn create_output(root: &Path, rel: &Path, param: &str) -> Result<File, Reject> {
    let (dir, last) = walk_to_parent(root, rel, param, true)?;
    // O_CREAT | O_WRONLY | O_TRUNC to write it; O_NOFOLLOW so an existing symlink at this name is
    // an error (ELOOP), never a redirect. Mode 0600: an output file is not group- or world-open.
    let fd = openat_raw(
        dir.as_raw_fd(),
        &last,
        libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )
    .map_err(|e| match e {
        libc::ELOOP => Reject::SymlinkedPath { param: param.to_string() },
        libc::ENOENT | libc::ENOTDIR => Reject::OutputParentMissing { param: param.to_string() },
        _ => Reject::OutputParentMissing { param: param.to_string() },
    })?;
    Ok(unsafe { File::from_raw_fd(fd) })
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
