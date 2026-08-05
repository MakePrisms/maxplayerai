//! Crash-atomic durable file writes for money-state that is REWRITTEN in place. Append-only money
//! journals stay durable via `OpenOptions::append` + `File::sync_all` for each record, PLUS a
//! one-time [`sync_dir`] of the parent when the file is first created — otherwise a power-loss
//! before the directory entry is flushed can drop the whole journal (see the ledger/journal/key
//! creators, which fsync their parent dir on create).
//!
//! A truncating `File::create`/`fs::write` can leave a half-written or empty file if the process
//! dies mid-flush. For money-state that is disastrous: an empty accept-bind lets the daemon
//! re-accept a job and pay it twice; a truncated `config.toml` loses the budget caps / accepted
//! mints. [`write_atomic`] closes that class: write the full payload to a temp sibling, `sync_all`
//! it, atomic-`rename` over the target, then `sync_all` the parent directory so the rename itself
//! (a directory-entry mutation) is durable. A crash at any point leaves EITHER the prior file or
//! the complete new file — never an intermediate.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Atomically replace `path` with `bytes`, durable across power-loss. `path` MUST live directly in
/// `dir` (its parent). The temp file is a sibling so the `rename` stays within one filesystem
/// (cross-device rename fails). Returns the first I/O error encountered.
pub fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    // Unique-ish temp sibling so two concurrent writers of DIFFERENT targets never collide; a
    // caller serializing writes of the SAME target (e.g. the per-job accept lock) makes the rename
    // last-writer-wins as intended.
    let tmp = path.with_extension("tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_dir(dir)
}

/// `fsync` a directory so a just-completed `rename`/create within it survives power-loss.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "mobee-durable-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let dir = scratch("replace");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("state.json");
        write_atomic(&dir, &path, b"first").expect("first write");
        assert_eq!(fs::read(&path).expect("read"), b"first");
        // Overwrite is durable and complete.
        write_atomic(&dir, &path, b"second-longer").expect("second write");
        assert_eq!(fs::read(&path).expect("read"), b"second-longer");
        // No temp sibling lingers.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp leftover: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_creates_missing_dir() {
        let dir = scratch("mkdir").join("nested");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("x.json");
        write_atomic(&dir, &path, b"payload").expect("write into fresh dir");
        assert_eq!(fs::read(&path).expect("read"), b"payload");
        let _ = fs::remove_dir_all(&dir);
    }
}
