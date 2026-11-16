//! Automatic online backups of the mint (no stopping it).
//!
//! `run` copies `mint.sqlite` into the backup directory (`<home>/mint/backups/`, or `backup_dir`
//! in `mint.toml`) with SQLite's `VACUUM INTO`: one read transaction, so every snapshot is a
//! consistent copy even while swaps commit. It snapshots once at start, then within
//! [`Schedule::min_interval`] of any committed change (`PRAGMA data_version`, which also sees
//! `issue` running in another process), and once more on a clean stop. Each snapshot is written
//! to a `0600` temp file, fsynced and renamed into place; the newest [`KEEP`] are kept.
//! `nostr.key`, `seed` and `mint.toml` are copied next to them when `run` starts, and each new
//! `issued/*.token` file with every snapshot, so the backup directory alone is a complete restore
//! set. It can issue credits: protect it like the mint.
//!
//! A failed snapshot is logged loudly and retried; it never stops the mint serving.
//!
//! Restore only the NEWEST snapshot ([`restore`]): an older one lacks proofs spent since, and
//! would let them be spent again. [`check_live`] makes `run` refuse a `mint.sqlite` that is
//! missing, or behind the newest snapshot.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use tokio::sync::watch;

use crate::home::MintHome;

/// Snapshots kept by default (the newest is always kept).
pub const KEEP: usize = 10;

/// The files that, with the newest snapshot, are the whole mint. `run` copies them into the backup
/// directory at start wherever its copy is missing or differs.
pub const RESTORE_SET: [&str; 3] = ["nostr.key", "seed", "mint.toml"];

const PREFIX: &str = "mint-";
const SUFFIX: &str = ".sqlite";
const SEQ_WIDTH: usize = 10;

/// The sequence number of a snapshot named `mint-<seq>-<stamp>.sqlite`.
fn sequence(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let digits = name.strip_prefix(PREFIX)?.get(..SEQ_WIDTH)?;
    digits.parse().ok()
}

/// When `run` snapshots.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    /// How often to check whether the database changed.
    pub poll: Duration,
    /// Least time between two snapshots (the debounce).
    pub min_interval: Duration,
    /// Snapshots kept (at least 1).
    pub keep: usize,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(5),
            min_interval: Duration::from_secs(30),
            keep: KEEP,
        }
    }
}

/// Takes snapshots of one database into one directory.
pub struct Snapshotter {
    conn: Connection,
    dir: PathBuf,
    keep: usize,
    /// `data_version` read just before the last successful snapshot.
    saved: Option<i64>,
}

impl Snapshotter {
    /// Open `db` (which must exist) for snapshots into `dir`. Nothing is written yet.
    pub fn open(db: &Path, dir: &Path, keep: usize) -> Result<Self> {
        let conn = open_existing(db)?;
        Ok(Self {
            conn,
            dir: dir.to_path_buf(),
            keep: keep.max(1),
            saved: None,
        })
    }

    fn data_version(&self) -> Result<i64> {
        self.conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .context("read data_version")
    }

    /// Whether anything was committed since the last snapshot (always, before the first).
    pub fn changed(&self) -> Result<bool> {
        Ok(self.saved != Some(self.data_version()?))
    }

    /// Write a consistent snapshot, then drop all but the newest `keep`. Returns its path.
    pub fn snapshot(&mut self) -> Result<PathBuf> {
        // Read BEFORE copying: a commit in between is in the copy AND marks the next poll changed
        // (one spare snapshot), never the other way round.
        let version = self.data_version()?;
        ensure_dir(&self.dir)?;
        // A sequence number orders snapshots, so a clock stepping back can't make rotation drop
        // the newest; the timestamp is for people.
        let seq = snapshots(&self.dir)?
            .last()
            .and_then(|p| sequence(p))
            .map_or(1, |n| n + 1);
        let name = format!(
            "{PREFIX}{seq:0width$}-{}{SUFFIX}",
            stamp(SystemTime::now()),
            width = SEQ_WIDTH
        );
        let path = self.dir.join(&name);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let _ = fs::remove_file(&tmp);
        let written = self.write(&tmp, &path);
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written?;
        self.saved = Some(version);
        if let Err(error) = rotate(&self.dir, self.keep) {
            eprintln!("maxplayer-mint: backup rotation in {}: {error:#}", self.dir.display());
        }
        Ok(path)
    }

    fn write(&self, tmp: &Path, path: &Path) -> Result<()> {
        let target = tmp.to_str().context("backup path is not UTF-8")?;
        // Created empty and 0600 first (VACUUM INTO fills an empty file), so the copy is never
        // readable by others, even in a backup_dir with a looser mode.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        self.conn
            .execute("VACUUM INTO ?1", [target])
            .with_context(|| format!("VACUUM INTO {}", tmp.display()))?;
        // A single self-contained file: no -wal/-shm appear next to it when it is read later.
        Connection::open(tmp)
            .and_then(|c| c.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(())))
            .context("make snapshot standalone")?;
        let file = fs::File::open(tmp)?;
        file.sync_all()?;
        fs::rename(tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        sync_dir(&self.dir)
    }
}

/// What the background task has done so far.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub snapshots: u64,
    pub failures: u64,
    pub last: Option<PathBuf>,
    pub last_error: Option<String>,
}

/// The background backup task of a running mint.
pub struct BackupTask {
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
    status: Arc<Mutex<Status>>,
}

impl BackupTask {
    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Take a last snapshot if anything changed, then end the task.
    pub async fn stop(self) -> Status {
        let _ = self.stop.send(true);
        let status = self.status.clone();
        let _ = self.handle.await;
        status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Start backing up `db` (in `mint_dir`) into `dir`. Never fails: every problem is logged and
/// retried, and the mint keeps serving.
pub fn start(mint_dir: &Path, db: &Path, dir: &Path, schedule: Schedule) -> BackupTask {
    let status = Arc::new(Mutex::new(Status::default()));
    let worker = Arc::new(Mutex::new(Worker {
        mint_dir: mint_dir.to_path_buf(),
        db: db.to_path_buf(),
        dir: dir.to_path_buf(),
        keep: schedule.keep,
        snap: None,
        restore_set_saved: false,
        status: status.clone(),
        min_interval: schedule.min_interval,
    }));
    let (stop, mut stopped) = watch::channel(false);
    let handle = tokio::spawn(async move {
        remove_stale_temp(&worker.lock().unwrap_or_else(|p| p.into_inner()).dir);
        attempt(&worker).await;
        let mut last = Instant::now();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(schedule.poll) => {}
                _ = stopped.changed() => break,
            }
            if last.elapsed() >= schedule.min_interval && attempt(&worker).await {
                last = Instant::now();
            }
        }
        // Clean stop: one more if anything changed since the last snapshot.
        attempt(&worker).await;
    });
    BackupTask {
        stop,
        handle,
        status,
    }
}

/// One snapshot attempt off the async threads. Returns whether a snapshot was attempted.
async fn attempt(worker: &Arc<Mutex<Worker>>) -> bool {
    let worker = worker.clone();
    tokio::task::spawn_blocking(move || worker.lock().unwrap_or_else(|p| p.into_inner()).attempt())
        .await
        .unwrap_or_else(|error| {
            eprintln!("maxplayer-mint: BACKUP FAILED: snapshot task panicked: {error}");
            true
        })
}

struct Worker {
    mint_dir: PathBuf,
    db: PathBuf,
    dir: PathBuf,
    keep: usize,
    snap: Option<Snapshotter>,
    restore_set_saved: bool,
    status: Arc<Mutex<Status>>,
    min_interval: Duration,
}

impl Worker {
    fn attempt(&mut self) -> bool {
        if !self.restore_set_saved {
            match save_restore_set(&self.mint_dir, &self.dir) {
                Ok(()) => self.restore_set_saved = true,
                Err(error) => self.fail(error.context("copy nostr.key, seed and mint.toml")),
            }
        }
        if self.snap.is_none() {
            match Snapshotter::open(&self.db, &self.dir, self.keep) {
                Ok(snap) => self.snap = Some(snap),
                Err(error) => {
                    self.fail(error);
                    return true;
                }
            }
        }
        // Taken out while in use, so `fail` can borrow `self`; put back unless it broke.
        let mut snap = self.snap.take().expect("opened above");
        match snap.changed() {
            Ok(false) => {
                self.snap = Some(snap);
                return false;
            }
            Ok(true) => {}
            Err(error) => {
                self.fail(error);
                return true;
            }
        }
        if let Err(error) = save_issued(&self.mint_dir, &self.dir) {
            self.fail(error.context("copy issued token files"));
        }
        match snap.snapshot() {
            Ok(path) => {
                eprintln!("maxplayer-mint: backup saved {}", path.display());
                let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
                status.snapshots += 1;
                status.last = Some(path);
                status.last_error = None;
            }
            Err(error) => self.fail(error),
        }
        self.snap = Some(snap);
        true
    }

    fn fail(&self, error: anyhow::Error) {
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        status.failures += 1;
        status.last_error = Some(format!("{error:#}"));
        let newest = status
            .last
            .as_ref()
            .map_or_else(|| "none yet".to_owned(), |p| p.display().to_string());
        eprintln!(
            "maxplayer-mint: BACKUP FAILED into {}: {error:#}. Still serving; retrying in {}s. Newest good backup: {newest}",
            self.dir.display(),
            self.min_interval.as_secs()
        );
    }
}

/// Snapshots in `dir`, oldest first. A missing directory has none.
pub fn snapshots(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("list {}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(PREFIX)
            && name.ends_with(SUFFIX)
            && sequence(&entry.path()).is_some()
            && entry.file_type()?.is_file()
        {
            found.push(entry.path());
        }
    }
    // Names start with a fixed-width sequence number, so name order is snapshot order.
    found.sort();
    Ok(found)
}

/// The newest snapshot in `dir`.
pub fn newest(dir: &Path) -> Result<Option<PathBuf>> {
    Ok(snapshots(dir)?.pop())
}

fn rotate(dir: &Path, keep: usize) -> Result<()> {
    let all = snapshots(dir)?;
    let excess = all.len().saturating_sub(keep.max(1));
    for old in &all[..excess] {
        fs::remove_file(old).with_context(|| format!("remove {}", old.display()))?;
    }
    Ok(())
}

fn remove_stale_temp(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Copy `nostr.key`, `seed` and `mint.toml` into `dir` where its copy is missing or differs.
pub fn save_restore_set(mint_dir: &Path, dir: &Path) -> Result<()> {
    ensure_dir(dir)?;
    for name in RESTORE_SET {
        let bytes = match fs::read(mint_dir.join(name)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && name == "mint.toml" => {
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("read {name}")),
        };
        let target = dir.join(name);
        if fs::read(&target).ok().as_deref() != Some(bytes.as_slice()) {
            write_atomic(&target, &bytes)?;
        }
    }
    Ok(())
}

/// Copy `issued/*.token` files the backup directory lacks (they never change once written).
pub fn save_issued(mint_dir: &Path, dir: &Path) -> Result<()> {
    let entries = match fs::read_dir(mint_dir.join("issued")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("list issued"),
    };
    let target = dir.join("issued");
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        if text.starts_with('.') || !text.ends_with(".token") || target.join(&name).exists() {
            continue;
        }
        ensure_dir(&target)?;
        let bytes = fs::read(entry.path()).with_context(|| format!("read issued/{text}"))?;
        write_atomic(&target.join(&name), &bytes)?;
    }
    Ok(())
}

/// Refuse to serve a database that would let spent credits be spent again: `mint.sqlite` missing
/// while snapshots exist, or missing spent proofs or signatures the newest snapshot has. Also
/// refuses a backup directory holding a different mint's key or seed. An unreadable backup
/// directory is only warned about: backups must never keep the mint from starting.
pub fn check_live(mint_dir: &Path, db: &Path, dir: &Path) -> Result<()> {
    for name in ["nostr.key", "seed"] {
        if let Ok(theirs) = fs::read(dir.join(name)) {
            let ours = fs::read(mint_dir.join(name)).with_context(|| format!("read {name}"))?;
            if theirs != ours {
                bail!(
                    "{} holds the backups of a DIFFERENT mint (its {name} differs). Set backup_dir in mint.toml to this mint's own backup directory or an empty one",
                    dir.display()
                );
            }
        }
    }
    let newest = match newest(dir) {
        Ok(Some(newest)) => newest,
        Ok(None) => return Ok(()),
        Err(error) => {
            eprintln!(
                "maxplayer-mint: WARNING: cannot read backups in {}: {error:#}; not checking mint.sqlite against them",
                dir.display()
            );
            return Ok(());
        }
    };
    if !db.exists() {
        bail!(
            "{} is MISSING but {} exists. Starting would make an empty mint that forgets every spent credit. Run `maxplayer-mint restore` to put back the newest backup",
            db.display(),
            newest.display()
        );
    }
    let missing = match missing_from(db, &newest) {
        Ok(missing) => missing,
        Err(error) => {
            eprintln!(
                "maxplayer-mint: WARNING: cannot compare {} with {}: {error:#}",
                db.display(),
                newest.display()
            );
            return Ok(());
        }
    };
    if missing > 0 {
        bail!(
            "{} is OLDER than the newest backup {} ({missing} spent proofs or signatures missing). Serving it could let spent credits be spent again. If it was restored from an old copy, run `maxplayer-mint restore` to put back the newest backup",
            db.display(),
            newest.display()
        );
    }
    Ok(())
}

/// Spent proofs and signatures in `snapshot` that `live` lacks.
pub fn missing_from(live: &Path, snapshot: &Path) -> Result<u64> {
    let conn = open_existing(live)?;
    let snapshot = snapshot.to_str().context("backup path is not UTF-8")?;
    conn.execute("ATTACH DATABASE ?1 AS snap", [snapshot])
        .context("attach backup")?;
    conn.query_row(
        "SELECT
           (SELECT count(*) FROM snap.proof s WHERE s.state = 'SPENT' AND NOT EXISTS
              (SELECT 1 FROM main.proof m WHERE m.y = s.y AND m.state = 'SPENT'))
         + (SELECT count(*) FROM snap.blind_signature s WHERE s.c IS NOT NULL AND NOT EXISTS
              (SELECT 1 FROM main.blind_signature m
                WHERE m.blinded_message = s.blinded_message AND m.c IS NOT NULL))",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n as u64)
    .with_context(|| format!("compare {} with {snapshot}", live.display()))
}

/// What [`restore`] did.
#[derive(Debug)]
pub struct Restored {
    pub snapshot: PathBuf,
    /// Where the replaced `mint.sqlite` (and its -wal/-shm) went, if there was one.
    pub replaced: Option<PathBuf>,
    /// Restore-set files copied into `<home>/mint/` because they were missing.
    pub copied: Vec<&'static str>,
}

/// Put the NEWEST snapshot in `from` back as `mint.sqlite` (never an older one). Creates
/// `<home>/mint/` from the restore set when it is gone. Refuses while `run` serves, when the
/// backups belong to another mint, and when `mint.sqlite` already holds everything the snapshot
/// has. The replaced database is moved aside, never deleted.
pub fn restore(home: &MintHome, from: &Path) -> Result<Restored> {
    let Some(snapshot) = newest(from)? else {
        bail!("no backups (mint-*.sqlite) in {}", from.display());
    };
    let check: String = Connection::open_with_flags(&snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|c| c.query_row("PRAGMA quick_check", [], |row| row.get(0)))
        .with_context(|| format!("read {}", snapshot.display()))?;
    if check != "ok" {
        bail!("{} is damaged ({check})", snapshot.display());
    }
    let dir = home.dir();
    for name in ["nostr.key", "seed"] {
        if !dir.join(name).exists() && !from.join(name).exists() {
            bail!(
                "{} has no {name} and {} has none to restore",
                dir.display(),
                from.display()
            );
        }
    }
    if !dir.exists() {
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir(dir).with_context(|| format!("create {}", dir.display()))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let _lock = home.lock_run()?;
    let mut copied = Vec::new();
    for name in RESTORE_SET {
        let Ok(theirs) = fs::read(from.join(name)) else {
            continue;
        };
        match fs::read(dir.join(name)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_atomic(&dir.join(name), &theirs)?;
                copied.push(name);
            }
            Err(error) => return Err(error).with_context(|| format!("read {name}")),
            // A different mint.toml is the operator's newer setting: keep it.
            Ok(ours) if ours != theirs && name != "mint.toml" => bail!(
                "{} holds the backups of a DIFFERENT mint (its {name} differs from {})",
                from.display(),
                dir.display()
            ),
            Ok(_) => {}
        }
    }
    let db = home.db_path();
    if db.exists() {
        match missing_from(&db, &snapshot) {
            Ok(0) => bail!(
                "{} already holds everything in {}: nothing to restore",
                db.display(),
                snapshot.display()
            ),
            Ok(missing) => eprintln!(
                "maxplayer-mint: {} lacks {missing} spent proofs or signatures; replacing it",
                db.display()
            ),
            Err(error) => eprintln!(
                "maxplayer-mint: {} is unreadable ({error:#}); replacing it",
                db.display()
            ),
        }
    }
    let mut replaced = None;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let file = dir.join(format!("mint.sqlite{suffix}"));
        if file.exists() {
            let aside = replaced.get_or_insert_with(|| {
                dir.join(format!("replaced-{}", stamp(SystemTime::now())))
            });
            if !aside.exists() {
                fs::create_dir(&*aside)?;
                fs::set_permissions(&*aside, fs::Permissions::from_mode(0o700))?;
            }
            fs::rename(&file, aside.join(file.file_name().expect("named")))?;
        }
    }
    let tmp = dir.join(".mint.sqlite.restore.tmp");
    let _ = fs::remove_file(&tmp);
    fs::copy(&snapshot, &tmp).with_context(|| format!("copy {}", snapshot.display()))?;
    let file = fs::File::open(&tmp)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.sync_all()?;
    fs::rename(&tmp, &db)?;
    sync_dir(dir)?;
    Ok(Restored {
        snapshot,
        replaced,
        copied,
    })
}

fn open_existing(db: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    Ok(conn)
}

/// Create `dir` `0700` if it is missing (an existing one keeps its mode; snapshots are `0600`).
fn ensure_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", dir.display()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
    sync_dir(path.parent().context("no parent")?)
}

fn sync_dir(dir: &Path) -> Result<()> {
    fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("fsync {}", dir.display()))
}

/// `YYYYMMDDTHHMMSS.mmmZ` (UTC, fixed width, so it sorts by time).
pub fn stamp(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let day = secs % 86_400;
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}.{:03}Z",
        day / 3600,
        day / 60 % 60,
        day % 60,
        since.subsec_millis()
    )
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_is_utc_and_fixed_width() {
        let at = UNIX_EPOCH + Duration::from_millis(1_794_474_381_042);
        assert_eq!(stamp(at), "20261112T090621.042Z");
        assert_eq!(stamp(UNIX_EPOCH), "19700101T000000.000Z");
        assert_eq!(
            stamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "20000229T000000.000Z"
        );
    }
}
