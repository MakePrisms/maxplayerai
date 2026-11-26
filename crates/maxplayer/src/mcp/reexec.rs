//! Re-exec the MCP server onto an upgraded binary, between two requests.
//!
//! An MCP client starts `maxplayer mcp` once and keeps that process for the whole session, so an
//! upgrade (`npm i -g maxplayer`, `install.sh`, a package manager) used to leave every later tool
//! call on the OLD code until a human restarted the client. The server now notices that the file it
//! was started from changed on disk, and before it serves the next request it `exec`s that file in
//! place: same pid, same stdin/stdout, so the client sees one continuous server and the request it
//! just sent is answered by the new version.
//!
//! What crosses the exec: the request already read (re-encoded as one JSON line) and every byte
//! still buffered behind it (a client may pipeline), written to an unlinked file whose descriptor the
//! new process reads before it returns to stdin (`maxplayer mcp --resume-fd <fd>`). Nothing else is
//! state: the server keeps none between requests.
//!
//! Fail-safe by construction: the new file must answer `--version` like a maxplayer binary AND
//! accept the handover argument before it is trusted (a half-written copy does neither; a build
//! older than this feature, after a downgrade, does only the first), and any failure leaves this
//! process serving on the old version, retrying only once the file changes again.

use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, Write};
use std::mem::ManuallyDrop;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The hidden `mcp` argument that carries the handover descriptor across the exec.
pub(crate) const RESUME_FD_ARG: &str = "--resume-fd";

/// How long the new binary gets to answer `--version` before it is judged unusable.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The MCP input stream: handover bytes from a previous exec first, then stdin. Reads stdin through
/// its own buffer over fd 0 (not `std::io::stdin()`'s shared one), so the bytes it has buffered but
/// not yet parsed are known and can be handed on to a re-exec.
pub(crate) struct McpInput {
    pending: Cursor<Vec<u8>>,
    stdin: BufReader<StdinFd>,
}

/// fd 0 as a `Read`, never closed by us.
pub(crate) struct StdinFd(ManuallyDrop<File>);

impl Read for StdinFd {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(buf)
    }
}

impl McpInput {
    /// stdin, preceded by `pending` (the handover of a previous process, or empty).
    pub(crate) fn new(pending: Vec<u8>) -> Self {
        // Safety: fd 0 is open for the life of the process; ManuallyDrop keeps us from closing it.
        let stdin = StdinFd(ManuallyDrop::new(unsafe { File::from_raw_fd(0) }));
        Self {
            pending: Cursor::new(pending),
            stdin: BufReader::new(stdin),
        }
    }

    fn pending_left(&self) -> bool {
        (self.pending.position() as usize) < self.pending.get_ref().len()
    }

    /// Every byte read from the client but not yet parsed into a request.
    pub(crate) fn unread(&self) -> Vec<u8> {
        let at = self.pending.position() as usize;
        let mut bytes = self.pending.get_ref()[at.min(self.pending.get_ref().len())..].to_vec();
        bytes.extend_from_slice(self.stdin.buffer());
        bytes
    }
}

impl Read for McpInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending_left() {
            return self.pending.read(buf);
        }
        self.stdin.read(buf)
    }
}

impl BufRead for McpInput {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.pending_left() {
            return self.pending.fill_buf();
        }
        self.stdin.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        if self.pending_left() {
            self.pending.consume(amount);
        } else {
            self.stdin.consume(amount);
        }
    }
}

/// Read a handover descriptor to its end and close it.
pub(crate) fn take_handover(fd: &str) -> Result<Vec<u8>, String> {
    let fd: i32 = fd
        .parse()
        .map_err(|_| format!("{RESUME_FD_ARG} expects a descriptor number, got {fd:?}"))?;
    if fd <= 2 {
        return Err(format!(
            "{RESUME_FD_ARG} {fd} is a standard stream, not a handover"
        ));
    }
    // Safety: the previous process image opened this descriptor for us and nothing else owns it;
    // the File closes it once read.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("reading the MCP handover: {error}"))?;
    Ok(bytes)
}

/// Identity of a file on disk: a replaced binary differs in at least one of these. `mode` and
/// `ctime` are included so an in-place copy that is made executable afterwards (`cp` then
/// `chmod`, which changes neither size nor mtime) is probed again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mode: meta.mode(),
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        ctime: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    })
}

/// Watches the binary this process was started from.
pub(crate) struct ExeWatch {
    path: PathBuf,
    /// The file as it was when this process started (or last refused an upgrade).
    seen: Stamp,
}

/// A replacement binary that answered `--version` and accepts the handover, ready to exec.
pub(crate) struct Upgrade {
    pub path: PathBuf,
    pub version_line: String,
    stamp: Stamp,
}

impl ExeWatch {
    /// The exec of `upgrade` failed: do not retry that same file on every request, but look
    /// again once it changes (a replace that was still in progress, say).
    pub(crate) fn exec_failed(&mut self, upgrade: &Upgrade) {
        self.seen = upgrade.stamp;
    }

    /// `None` when the running binary cannot be located on disk (nothing to watch).
    pub(crate) fn current() -> Option<Self> {
        let path = std::env::current_exe().ok()?;
        let seen = stamp(&path)?;
        Some(Self { path, seen })
    }

    /// The upgrade to exec into, if the file changed since start and the new one is a working
    /// maxplayer. A file that changed but does not answer is remembered, so a broken copy is probed
    /// once, not on every request; it is probed again when it changes again.
    pub(crate) fn check(&mut self, err: &mut dyn Write) -> Option<Upgrade> {
        let now = stamp(&self.path)?; // mid-replace (no file yet): keep serving, look again later
        if now == self.seen {
            return None;
        }
        let probed = probe_version(&self.path)
            .and_then(|version_line| probe_handover(&self.path).map(|()| version_line));
        match probed {
            Ok(version_line) => Some(Upgrade {
                path: self.path.clone(),
                version_line,
                stamp: now,
            }),
            Err(Probe { why, transient }) => {
                let _ = writeln!(
                    err,
                    "maxplayer mcp: {} changed on disk but is not usable yet ({why}); still serving {}",
                    self.path.display(),
                    crate::build_stamp::version_line()
                );
                // A file still open for writing (ETXTBSY) is looked at again on the next request:
                // the writer closing it changes nothing in its stamp.
                if !transient {
                    self.seen = now;
                }
                None
            }
        }
    }
}

/// Why a replacement was not trusted; `transient` = look again on the next request even though
/// the file's stamp has not changed.
struct Probe {
    why: String,
    transient: bool,
}

impl From<String> for Probe {
    fn from(why: String) -> Self {
        Self {
            why,
            transient: false,
        }
    }
}

/// Run `<path> <args>` with a deadline; its exit status, stdout and stderr.
fn run_bounded(
    path: &Path,
    args: &[&str],
) -> Result<(std::process::ExitStatus, String, String), Probe> {
    let what = args.join(" ");
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Probe {
            transient: error.kind() == std::io::ErrorKind::ExecutableFileBusy,
            why: format!("cannot run it: {error}"),
        })?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < PROBE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`{what}` did not answer within {}s",
                    PROBE_TIMEOUT.as_secs()
                )
                .into());
            }
            Err(error) => return Err(format!("waiting for `{what}`: {error}").into()),
        }
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Ok((status, out, stderr))
}

/// Run `<path> --version` with a deadline; its first line, when it names a maxplayer build.
fn probe_version(path: &Path) -> Result<String, Probe> {
    let (status, out, _) = run_bounded(path, &["--version"])?;
    let line = out.lines().next().unwrap_or("").trim().to_owned();
    if !status.success() || !line.starts_with("maxplayer ") {
        return Err(format!("--version answered {status} {line:?}").into());
    }
    Ok(line)
}

/// Whether `<path>` takes the handover: `mcp --resume-fd 0` must be refused as a standard stream,
/// which is what a build with this feature answers at once, before any home or relay work. A build
/// older than it (a downgrade) answers with its usage text instead, and exec'ing it would end the
/// session on the client's next request.
fn probe_handover(path: &Path) -> Result<(), Probe> {
    let (status, _, stderr) = run_bounded(path, &["mcp", RESUME_FD_ARG, "0"])?;
    if stderr.contains(&format!("{RESUME_FD_ARG} 0 is a standard stream")) {
        return Ok(());
    }
    Err(format!(
        "it does not take `mcp {RESUME_FD_ARG}` (a build older than in-place upgrades? it answered {status})"
    )
    .into())
}

/// Replace this process with `upgrade`, handing it `handover` (the current request and everything
/// buffered behind it). Returns only on failure; the caller then keeps serving on this version.
pub(crate) fn exec_upgrade(upgrade: &Upgrade, handover: &[u8], scratch_dir: &Path) -> String {
    use std::os::unix::process::CommandExt;
    let file = match handover_file(handover, scratch_dir) {
        Ok(file) => file,
        Err(error) => return format!("could not prepare the handover: {error}"),
    };
    let fd = file.as_raw_fd();
    // Clear close-on-exec on exactly this descriptor so the new image inherits it.
    // Safety: plain fcntl on a descriptor we own.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        return format!(
            "could not pass the handover: {}",
            std::io::Error::last_os_error()
        );
    }
    let error = Command::new(&upgrade.path)
        .arg("mcp")
        .arg(RESUME_FD_ARG)
        .arg(fd.to_string())
        .exec();
    // Still here: the exec failed. Put close-on-exec back so later children don't inherit it.
    // Safety: as above.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    format!("exec {}: {error}", upgrade.path.display())
}

/// The handover bytes in an already-unlinked private file, positioned at its start.
fn handover_file(bytes: &[u8], dir: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(format!(".mcp-handover-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let unlinked = std::fs::remove_file(&path);
    file.write_all(bytes)?;
    file.rewind()?;
    unlinked?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handed-over bytes come first, and whatever is left of them counts as unread.
    #[test]
    fn handover_bytes_are_read_before_stdin_and_count_as_unread() {
        let mut input = McpInput {
            pending: Cursor::new(b"{\"id\":2}\n{\"id\":3}\n".to_vec()),
            stdin: BufReader::new(StdinFd(ManuallyDrop::new(File::open("/dev/null").unwrap()))),
        };
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"id\":2}\n");
        assert_eq!(input.unread(), b"{\"id\":3}\n");
        line.clear();
        input.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"id\":3}\n");
        assert!(input.unread().is_empty());
        line.clear();
        assert_eq!(
            input.read_line(&mut line).unwrap(),
            0,
            "then stdin (EOF here)"
        );
    }

    /// A file that is not a working maxplayer is refused, and remembered until it changes again.
    #[test]
    fn a_replacement_that_does_not_answer_version_is_not_trusted() {
        let dir = std::env::temp_dir().join(format!("mcp-reexec-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("maxplayer");
        let write = |body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let tmp = dir.join("next");
            std::fs::write(&tmp, body).unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&tmp, &exe).unwrap();
        };
        write("#!/bin/sh\necho 'maxplayer 0.0.1 (old)'\n");
        let mut watch = ExeWatch {
            path: exe.clone(),
            seen: stamp(&exe).unwrap(),
        };
        let mut err = Vec::new();
        assert!(watch.check(&mut err).is_none(), "unchanged");

        write("#!/bin/sh\nexit 3\n"); // a broken copy
        assert!(
            watch.check(&mut err).is_none(),
            "a broken copy is not exec'd"
        );
        assert!(
            String::from_utf8_lossy(&err).contains("not usable yet"),
            "{}",
            String::from_utf8_lossy(&err)
        );
        err.clear();
        assert!(watch.check(&mut err).is_none());
        assert!(err.is_empty(), "the same broken copy is not probed again");

        write(&current_build("maxplayer 9.9.9 (new)"));
        let upgrade = watch
            .check(&mut err)
            .expect("a working replacement is an upgrade");
        assert_eq!(upgrade.version_line, "maxplayer 9.9.9 (new)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A script that answers like a build with in-place upgrades: `--version`, and `mcp
    /// --resume-fd 0` refused as a standard stream (the real binary's exact text).
    fn current_build(version: &str) -> String {
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo '{version}';;\nmcp) echo 'maxplayer mcp: {RESUME_FD_ARG} 0 is a standard stream, not a handover' >&2; exit 2;;\nesac\n"
        )
    }

    /// A downgrade to a build older than in-place upgrades answers `--version` but not the
    /// handover: it is not exec'd (that would end the session), and is not probed again until the
    /// file changes. RED ON REVERT: without `probe_handover` the old build is returned as an upgrade.
    #[test]
    fn a_downgrade_to_a_build_without_the_handover_is_not_execd() {
        let dir = std::env::temp_dir().join(format!("mcp-reexec-downgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("maxplayer");
        let write = |body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let tmp = dir.join("next");
            std::fs::write(&tmp, body).unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&tmp, &exe).unwrap();
        };
        write(&current_build("maxplayer 9.9.9 (new)"));
        let mut watch = ExeWatch {
            path: exe.clone(),
            seen: stamp(&exe).unwrap(),
        };
        // An older build: answers --version, prints its usage for `mcp --resume-fd` and exits 1.
        write(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'maxplayer 0.8.1 (old)';;\n*) echo 'Usage:' >&2; exit 1;;\nesac\n",
        );
        let mut err = Vec::new();
        assert!(
            watch.check(&mut err).is_none(),
            "an old build is not exec'd"
        );
        let said = String::from_utf8_lossy(&err).into_owned();
        assert!(said.contains("does not take"), "{said}");
        err.clear();
        assert!(watch.check(&mut err).is_none());
        assert!(err.is_empty(), "the same old build is not probed again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A copy made executable after it was written (`cp` then `chmod`: same size and mtime) is
    /// probed again, and a failed exec is retried only once the file changes.
    #[test]
    fn a_chmod_after_copy_is_seen_and_a_failed_exec_waits_for_a_change() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mcp-reexec-chmod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("maxplayer");
        std::fs::write(&exe, current_build("maxplayer 0.0.1 (old)")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut watch = ExeWatch {
            path: exe.clone(),
            seen: stamp(&exe).unwrap(),
        };
        // Written in place, not yet executable.
        std::fs::write(&exe, current_build("maxplayer 9.9.9 (new)")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut err = Vec::new();
        assert!(watch.check(&mut err).is_none(), "not executable yet");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let upgrade = watch
            .check(&mut err)
            .expect("the chmod makes it an upgrade");
        watch.exec_failed(&upgrade);
        assert!(
            watch.check(&mut err).is_none(),
            "the same file is not retried"
        );
        std::fs::write(&exe, current_build("maxplayer 9.9.10 (newer)")).unwrap();
        assert_eq!(
            watch
                .check(&mut err)
                .expect("a changed file is tried again")
                .version_line,
            "maxplayer 9.9.10 (newer)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
