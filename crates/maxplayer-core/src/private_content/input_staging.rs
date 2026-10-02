//! Private input staging has its own namespace and lifetime lock. A restarted
//! daemon may reclaim dead preparations without deleting another live CLI's work
//! or any unrelated `.tmp*` directory in the buyer home.
use std::fs::{File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
use std::path::Path;

pub(super) struct Staging {
    dir: tempfile::TempDir,
    _lease: File,
}
impl Staging {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
    pub fn new(home: &Path) -> io::Result<Self> {
        let parent = home.join("private-input-staging");
        std::fs::create_dir_all(&parent)?;
        let dir = tempfile::Builder::new()
            .prefix("prep-")
            .tempdir_in(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let lease = options.open(dir.path().join(".lease"))?;
        lock(&lease)?;
        Ok(Self { dir, _lease: lease })
    }
}
fn lock(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: valid owned descriptor; flock has no pointer arguments.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
pub(crate) fn clean(home: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(home.join("private-input-staging")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || !entry.file_name().to_string_lossy().starts_with("prep-")
        {
            continue;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let lease = match options.open(entry.path().join(".lease")) {
            Ok(f) if f.metadata()?.is_file() => f,
            _ => continue,
        };
        if lock(&lease).is_ok() {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn staging_cleanup_preserves_live_and_unrelated_directories() {
        let home = tempfile::tempdir().unwrap();
        let live = Staging::new(home.path()).unwrap();
        let dead = Staging::new(home.path()).unwrap();
        let dead_path = dead.dir.keep();
        drop(dead._lease);
        let unrelated = home.path().join(".tmp-unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        clean(home.path()).unwrap();
        assert!(live.path().exists());
        assert!(!dead_path.exists());
        assert!(unrelated.exists());
    }
}
