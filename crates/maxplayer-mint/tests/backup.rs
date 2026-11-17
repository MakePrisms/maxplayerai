//! Automatic online backups (`maxplayer_mint::backup`): snapshots taken while the mint serves,
//! rotation, the change trigger, shutdown, failures that never stop serving, and restore.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cdk::Amount;
use cdk::Mint;
use cdk::amount::{FeeAndAmounts, SplitTarget};
use cdk::dhke::construct_proofs;
use cdk::nuts::{Id, PreMintSecrets, PublicKey, SwapRequest};
use maxplayer_mint::backup::{self, Schedule, Snapshotter};
use maxplayer_mint::home::{MintHome, mint_url};
use maxplayer_mint::{backend, issue};
use rusqlite::{Connection, OpenFlags};

/// A real mint home (`init`) with its mint started, as `run` has it.
struct Fixture {
    home: MintHome,
    mint: Mint,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let home = MintHome::at(dir.path());
    let keys = home.init().unwrap();
    let secrets = home.load().unwrap();
    let mint = backend::open(&home.db_path(), &secrets.seed, &mint_url(&keys).unwrap())
        .await
        .unwrap();
    Fixture {
        home,
        mint,
        _dir: dir,
    }
}

impl Fixture {
    fn backups(&self) -> PathBuf {
        self.home.dir().join("backups")
    }

    fn url(&self) -> String {
        mint_url(&self.home.load().unwrap().keys).unwrap()
    }

    /// Stop the mint and drop its -wal/-shm, as after a clean shutdown.
    async fn stop(self) -> (MintHome, tempfile::TempDir) {
        self.mint.stop().await.unwrap();
        drop(self.mint);
        let db = self.home.db_path();
        // Fold the WAL into mint.sqlite so file copies below are whole databases.
        Connection::open(&db)
            .unwrap()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        (self.home, self._dir)
    }
}

/// Put `from` in place as `mint.sqlite`, with no -wal/-shm left over.
fn replace_db(home: &MintHome, from: &Path) {
    let db = home.db_path();
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
    fs::copy(from, &db).unwrap();
}

fn premint(id: Id, amount: u64) -> PreMintSecrets {
    let amounts: Vec<u64> = (0..32).map(|i| 1u64 << i).collect();
    PreMintSecrets::random(
        id,
        Amount::from(amount),
        &SplitTarget::default(),
        &FeeAndAmounts::from((0, amounts)),
    )
    .unwrap()
}

/// One committed 1-sat swap: ONE input marked SPENT and ONE output signature stored. cdk's swap
/// saga commits twice (the input PENDING with an unsigned output, then SPENT with the signature),
/// so a snapshot can land between the two. Returns the spent input's Y.
async fn swap(mint: &Mint) -> PublicKey {
    let id = backend::active_keyset(mint).unwrap();
    let keys = mint.keyset(&id).unwrap().keys;
    let pre = premint(id, 1);
    // `blind_sign` stores nothing, so only the swap writes rows.
    let signatures = mint.blind_sign(pre.blinded_messages()).await.unwrap();
    let inputs = construct_proofs(signatures, pre.rs(), pre.secrets(), &keys).unwrap();
    let y = inputs[0].y().unwrap();
    let request = SwapRequest::new(inputs, premint(id, 1).blinded_messages());
    mint.process_swap_request(request).await.unwrap();
    y
}

fn read_only(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// (spent proofs, stored output signatures) in a database file.
fn counts(path: &Path) -> (i64, i64) {
    let conn = read_only(path);
    let spent = conn
        .query_row("SELECT count(*) FROM proof WHERE state = 'SPENT'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let signed = conn
        .query_row(
            "SELECT count(*) FROM blind_signature WHERE c IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    (spent, signed)
}

fn is_spent(path: &Path, y: &PublicKey) -> bool {
    read_only(path)
        .query_row(
            "SELECT count(*) FROM proof WHERE y = ?1 AND state = 'SPENT'",
            [y.to_bytes().to_vec()],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn fast() -> Schedule {
    Schedule {
        poll: Duration::from_millis(20),
        min_interval: Duration::from_millis(100),
        keep: 10,
    }
}

/// Snapshots taken WHILE swaps commit are whole: each opens, passes `integrity_check`, and holds
/// exactly as many spent proofs as signatures (a torn copy would split a swap's transaction). The
/// last one holds every spent proof.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_while_swaps_commit_is_consistent() {
    let f = fixture().await;
    let db = f.home.db_path();
    let dir = f.backups();
    let writing = Arc::new(AtomicBool::new(true));
    let snapper = {
        let (db, dir, writing) = (db.clone(), dir.clone(), writing.clone());
        tokio::task::spawn_blocking(move || {
            let mut snap = Snapshotter::open(&db, &dir, 1000).unwrap();
            let mut taken = Vec::new();
            while writing.load(Ordering::SeqCst) {
                taken.push(snap.snapshot().unwrap());
            }
            taken
        })
    };
    let mut spent = Vec::new();
    for _ in 0..40 {
        spent.push(swap(&f.mint).await);
    }
    writing.store(false, Ordering::SeqCst);
    let taken = snapper.await.unwrap();
    assert!(taken.len() >= 2, "snapshots overlapped the writes");

    let mut partial = 0;
    for path in &taken {
        let check: String = read_only(path)
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok", "{}", path.display());
        let (spent_rows, signed_rows) = counts(path);
        assert_eq!(spent_rows, signed_rows, "torn snapshot {}", path.display());
        if (1..40).contains(&spent_rows) {
            partial += 1;
        }
        assert_eq!(mode(path), 0o600);
        for suffix in ["-wal", "-shm", "-journal"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            assert!(!side.exists(), "{} left behind", side.display());
        }
    }
    assert!(partial > 0, "no snapshot landed mid-run; the test proved nothing");
    assert_eq!(mode(&dir), 0o700);

    let last = Snapshotter::open(&db, &dir, 1000)
        .unwrap()
        .snapshot()
        .unwrap();
    for y in &spent {
        assert!(is_spent(&last, y), "spent proof missing from the snapshot");
    }
    assert!(
        fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")),
        "no temp file left after a snapshot"
    );
}

/// Only the newest `keep` snapshots stay, and the newest is always among them.
#[tokio::test(flavor = "multi_thread")]
async fn rotation_keeps_the_newest() {
    let f = fixture().await;
    let mut snap = Snapshotter::open(&f.home.db_path(), &f.backups(), 3).unwrap();
    let mut taken = Vec::new();
    for _ in 0..5 {
        swap(&f.mint).await;
        taken.push(snap.snapshot().unwrap());
    }
    assert_eq!(backup::snapshots(&f.backups()).unwrap(), taken[2..]);
    assert_eq!(
        backup::newest(&f.backups()).unwrap().as_ref(),
        taken.last()
    );
    // keep = 0 still keeps the newest.
    let mut none = Snapshotter::open(&f.home.db_path(), &f.backups(), 0).unwrap();
    let only = none.snapshot().unwrap();
    assert_eq!(backup::snapshots(&f.backups()).unwrap(), vec![only]);
}

/// The background task: a snapshot (and the key, seed and mint.toml) at start, none while
/// nothing changes, one shortly after a committed swap, and a last one on stop.
#[tokio::test(flavor = "multi_thread")]
async fn task_snapshots_at_start_after_each_change_and_on_stop() {
    let f = fixture().await;
    let dir = f.backups();
    let task = backup::start(f.home.dir(), &f.home.db_path(), &dir, fast());
    wait_for("the startup snapshot", || task.status().snapshots == 1).await;
    for name in backup::RESTORE_SET {
        assert_eq!(
            fs::read(dir.join(name)).unwrap(),
            fs::read(f.home.dir().join(name)).unwrap(),
            "{name} copied"
        );
        assert_eq!(mode(&dir.join(name)), 0o600);
    }

    // Nothing committed: no new snapshot, however long it waits.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(task.status().snapshots, 1, "unchanged database re-snapshotted");

    let y = swap(&f.mint).await;
    // Not `snapshots == 2`: a snapshot between the saga's two commits has the input PENDING.
    wait_for("a snapshot with the spent proof", || {
        backup::newest(&dir)
            .unwrap()
            .is_some_and(|p| is_spent(&p, &y))
    })
    .await;

    // Committed right before a clean stop: the stop snapshot has it.
    let last = swap(&f.mint).await;
    let status = task.stop().await;
    assert_eq!(status.failures, 0, "{:?}", status.last_error);
    let newest = backup::newest(&dir).unwrap().unwrap();
    assert!(is_spent(&newest, &last), "stop snapshot missed the last swap");
}

/// The backup directory can't be written: every attempt fails loudly, the mint keeps serving, and
/// backups resume by themselves once the directory works.
#[tokio::test(flavor = "multi_thread")]
async fn failing_backups_never_stop_the_mint() {
    let f = fixture().await;
    let blocked = f.home.dir().join("not-a-dir");
    fs::write(&blocked, b"a file where the backup directory should be").unwrap();
    let dir = blocked.join("backups");
    let task = backup::start(f.home.dir(), &f.home.db_path(), &dir, fast());
    wait_for("a failed attempt", || task.status().failures >= 1).await;
    // Serving goes on: swaps still commit, and the task keeps retrying.
    for _ in 0..3 {
        swap(&f.mint).await;
    }
    wait_for("retries", || task.status().failures >= 3).await;
    assert_eq!(task.status().snapshots, 0);
    assert!(task.status().last_error.is_some());

    fs::remove_file(&blocked).unwrap();
    wait_for("backups to resume", || task.status().snapshots >= 1).await;
    let status = task.stop().await;
    assert!(status.last_error.is_none());
    let (spent, _) = counts(&backup::newest(&dir).unwrap().unwrap());
    assert_eq!(spent, 3);
}

/// `run`'s guard: a missing or older `mint.sqlite` is refused; `restore` puts back the NEWEST
/// snapshot and moves the old file aside; afterwards the guard passes.
#[tokio::test(flavor = "multi_thread")]
async fn older_or_missing_database_is_refused_and_restore_takes_the_newest() {
    let f = fixture().await;
    let (db, dir) = (f.home.db_path(), f.backups());
    let mut snap = Snapshotter::open(&db, &dir, 10).unwrap();
    let old = snap.snapshot().unwrap();
    let y = swap(&f.mint).await;
    let newest = snap.snapshot().unwrap();
    backup::save_restore_set(f.home.dir(), &dir).unwrap();
    drop(snap);
    f.mint.stop().await.unwrap();
    drop(f.mint);
    backup::check_live(f.home.dir(), &db, &dir).unwrap();

    // An operator "restores" an old copy by hand.
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
    fs::copy(&old, &db).unwrap();
    let refused = backup::check_live(f.home.dir(), &db, &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("OLDER"), "{refused:#}");

    fs::remove_file(&db).unwrap();
    let refused = backup::check_live(f.home.dir(), &db, &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("MISSING"), "{refused:#}");

    fs::copy(&old, &db).unwrap();
    // The restored copy is staged before the live file moves: a copy that fails leaves it.
    let stage = f.home.dir().join(".mint.sqlite.restore.tmp");
    fs::create_dir(&stage).unwrap();
    backup::restore(&f.home, &dir).unwrap_err();
    assert!(db.exists(), "a failed restore removed mint.sqlite");
    assert!(
        !fs::read_dir(f.home.dir())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("replaced-")),
        "a failed restore moved mint.sqlite aside"
    );
    fs::remove_dir(&stage).unwrap();
    let done = backup::restore(&f.home, &dir).unwrap();
    assert_eq!(done.snapshot, newest);
    assert!(done.replaced.unwrap().join("mint.sqlite").exists(), "old db kept aside");
    assert!(is_spent(&db, &y));
    assert_eq!(mode(&db), 0o600);
    backup::check_live(f.home.dir(), &db, &dir).unwrap();

    // Nothing newer to put back: refused rather than churned.
    let again = backup::restore(&f.home, &dir).unwrap_err();
    assert!(format!("{again:#}").contains("nothing to restore"), "{again:#}");
}

/// The backup directory alone rebuilds a lost `<home>/mint/`: same key, seed and spent proofs.
#[tokio::test(flavor = "multi_thread")]
async fn backup_dir_alone_restores_a_lost_mint() {
    let f = fixture().await;
    let elsewhere = tempfile::tempdir().unwrap();
    let dir = elsewhere.path().join("mint-backups");
    let task = backup::start(f.home.dir(), &f.home.db_path(), &dir, fast());
    wait_for("the startup snapshot", || task.status().snapshots == 1).await;
    let y = swap(&f.mint).await;
    let token = issue::issue(&f.mint, f.home.dir(), &f.url(), 16).await.unwrap();
    let name = token.file.file_name().unwrap().to_owned();
    let bytes = fs::read(&token.file).unwrap();
    let status = task.stop().await;
    assert!(status.last_error.is_none(), "{:?}", status.last_error);
    assert_eq!(fs::read(dir.join("issued").join(&name)).unwrap(), bytes);
    let before = f.home.load().unwrap();
    f.mint.stop().await.unwrap();
    drop(f.mint);

    fs::remove_dir_all(f.home.dir()).unwrap();
    let done = backup::restore(&f.home, &dir).unwrap();
    assert_eq!(done.copied, vec!["nostr.key", "seed", "mint.toml"]);
    // The unreceived token comes back too: it is the credits, and only the backup has it now.
    assert_eq!(done.issued, 1);
    let restored = f.home.dir().join("issued").join(&name);
    assert_eq!(fs::read(&restored).unwrap(), bytes);
    assert_eq!(mode(&restored), 0o600);
    assert_eq!(mode(f.home.dir()), 0o700);
    let after = f.home.load().unwrap();
    assert_eq!(after.keys.public_key(), before.keys.public_key());
    assert_eq!(after.seed, before.seed);
    assert!(is_spent(&f.home.db_path(), &y));
}

/// Another mint's backups are refused by both `run`'s guard and `restore`.
#[tokio::test(flavor = "multi_thread")]
async fn another_mints_backups_are_refused() {
    let ours = fixture().await;
    let theirs = fixture().await;
    let dir = theirs.backups();
    backup::save_restore_set(theirs.home.dir(), &dir).unwrap();
    Snapshotter::open(&theirs.home.db_path(), &dir, 10)
        .unwrap()
        .snapshot()
        .unwrap();
    let refused =
        backup::check_live(ours.home.dir(), &ours.home.db_path(), &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("DIFFERENT mint"), "{refused:#}");
    ours.mint.stop().await.unwrap();
    let refused = backup::restore(&ours.home, &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("DIFFERENT mint"), "{refused:#}");
}

/// `backup_dir` in mint.toml is honoured (relative to `<home>/mint/`); unset means `backups/`.
#[test]
fn backup_dir_setting() {
    let dir = tempfile::tempdir().unwrap();
    let home = MintHome::at(dir.path());
    home.init().unwrap();
    let config = home.config().unwrap();
    assert_eq!(config.backup_dir_in(home.dir()), home.dir().join("backups"));
    let toml_path = home.dir().join("mint.toml");
    let mut text = fs::read_to_string(&toml_path).unwrap();
    text.push_str("backup_dir = \"/mnt/offbox/mint\"\n");
    fs::write(&toml_path, text).unwrap();
    assert_eq!(
        home.config().unwrap().backup_dir_in(home.dir()),
        PathBuf::from("/mnt/offbox/mint")
    );
}

/// A `mint.sqlite` that exists but can't be read (0 bytes, never set up, damaged) is refused while
/// backups exist: starting would build an empty mint from the seed that forgets every spend.
#[tokio::test(flavor = "multi_thread")]
async fn unreadable_database_is_refused() {
    let f = fixture().await;
    let dir = f.backups();
    swap(&f.mint).await;
    Snapshotter::open(&f.home.db_path(), &dir, 10)
        .unwrap()
        .snapshot()
        .unwrap();
    let (home, _keep) = f.stop().await;
    let db = home.db_path();
    fs::File::create(&db).unwrap();
    let refused = backup::check_live(home.dir(), &db, &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("cannot be read"), "{refused:#}");
    fs::write(&db, b"not a database at all, but long enough to look like a header....").unwrap();
    let refused = backup::check_live(home.dir(), &db, &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("cannot be read"), "{refused:#}");
}

/// A damaged newest snapshot doesn't blind the guard: it checks against the newest one that reads.
#[tokio::test(flavor = "multi_thread")]
async fn damaged_newest_snapshot_falls_back_to_an_older_one() {
    let f = fixture().await;
    let dir = f.backups();
    let mut snap = Snapshotter::open(&f.home.db_path(), &dir, 10).unwrap();
    let before = snap.snapshot().unwrap();
    swap(&f.mint).await;
    let readable = snap.snapshot().unwrap();
    swap(&f.mint).await;
    let damaged = snap.snapshot().unwrap();
    drop(snap);
    let (home, _keep) = f.stop().await;
    fs::write(&damaged, vec![0x5a; 8192]).unwrap();
    // mint.sqlite put back from before both swaps.
    replace_db(&home, &before);
    let refused = backup::check_live(home.dir(), &home.db_path(), &dir).unwrap_err();
    let text = format!("{refused:#}");
    assert!(text.contains("OLDER"), "{text}");
    assert!(text.contains(&readable.display().to_string()), "{text}");
}

/// A backup_dir symlinked to storage that isn't mounted is an error to read, not "no backups";
/// `run`'s guard only warns about it (backups never keep the mint from starting).
#[test]
fn dangling_backup_dir_symlink_is_not_an_empty_backup_dir() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("backups");
    std::os::unix::fs::symlink(dir.path().join("unmounted/offbox"), &link).unwrap();
    let error = backup::snapshots(&link).unwrap_err();
    assert!(format!("{error:#}").contains("symlink"), "{error:#}");
    let home = MintHome::at(dir.path());
    home.init().unwrap();
    backup::check_live(home.dir(), &home.db_path(), &link).unwrap();
}

/// A bare snapshot (no nostr.key or seed beside it) from another mint is refused; one of this
/// mint's own is restored.
#[tokio::test(flavor = "multi_thread")]
async fn another_mints_bare_snapshot_is_refused() {
    let ours = fixture().await;
    let theirs = fixture().await;
    let bare = tempfile::tempdir().unwrap();
    swap(&theirs.mint).await;
    Snapshotter::open(&theirs.home.db_path(), bare.path(), 10)
        .unwrap()
        .snapshot()
        .unwrap();
    let y = swap(&ours.mint).await;
    let own = tempfile::tempdir().unwrap();
    Snapshotter::open(&ours.home.db_path(), own.path(), 10)
        .unwrap()
        .snapshot()
        .unwrap();
    let (home, _keep) = ours.stop().await;
    fs::remove_file(home.db_path()).unwrap();
    let refused = backup::restore(&home, bare.path()).unwrap_err();
    assert!(format!("{refused:#}").contains("DIFFERENT mint"), "{refused:#}");
    assert!(!home.db_path().exists(), "another mint's database was restored");
    backup::restore(&home, own.path()).unwrap();
    assert!(is_spent(&home.db_path(), &y));
}

/// A key or seed in the backup directory that can't be read is an error, not "absent".
#[tokio::test(flavor = "multi_thread")]
async fn unreadable_seed_in_backups_is_an_error() {
    let f = fixture().await;
    let dir = f.backups();
    backup::save_restore_set(f.home.dir(), &dir).unwrap();
    Snapshotter::open(&f.home.db_path(), &dir, 10)
        .unwrap()
        .snapshot()
        .unwrap();
    let (home, _keep) = f.stop().await;
    fs::set_permissions(dir.join("seed"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(dir.join("seed")).is_ok() {
        return; // running as root: modes don't stop reads
    }
    let refused = backup::check_live(home.dir(), &home.db_path(), &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("seed"), "{refused:#}");
    fs::remove_file(home.db_path()).unwrap();
    let refused = backup::restore(&home, &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("seed"), "{refused:#}");
    assert!(!home.db_path().exists());
}

/// `mint.sqlite` and the newest snapshot each hold a spend the other lacks: restoring either way
/// forgets one, so restore refuses and leaves mint.sqlite alone.
#[tokio::test(flavor = "multi_thread")]
async fn diverged_database_and_snapshot_are_refused() {
    let f = fixture().await;
    let dir = f.backups();
    let mut snap = Snapshotter::open(&f.home.db_path(), &dir, 10).unwrap();
    let base = snap.snapshot().unwrap();
    let in_snapshot = swap(&f.mint).await;
    snap.snapshot().unwrap();
    drop(snap);
    let (home, _keep) = f.stop().await;
    replace_db(&home, &base);
    let secrets = home.load().unwrap();
    let url = mint_url(&secrets.keys).unwrap();
    let mint = backend::open(&home.db_path(), &secrets.seed, &url).await.unwrap();
    let in_live = swap(&mint).await;
    mint.stop().await.unwrap();
    drop(mint);

    let refused = backup::restore(&home, &dir).unwrap_err();
    assert!(format!("{refused:#}").contains("DIVERGED"), "{refused:#}");
    assert!(is_spent(&home.db_path(), &in_live), "mint.sqlite was replaced");
    assert!(!is_spent(&home.db_path(), &in_snapshot));
}

/// A token file the backup couldn't copy is retried with no further database change, and the
/// error stays reported until it is copied.
#[tokio::test(flavor = "multi_thread")]
async fn failed_token_copy_is_retried_while_idle() {
    let f = fixture().await;
    let dir = f.backups();
    let task = backup::start(f.home.dir(), &f.home.db_path(), &dir, fast());
    wait_for("the startup snapshot", || task.status().snapshots == 1).await;
    let blocker = dir.join("issued");
    fs::write(&blocker, b"a file where issued/ should be").unwrap();
    let token = issue::issue(&f.mint, f.home.dir(), &f.url(), 16).await.unwrap();
    wait_for("a failed copy", || task.status().failures >= 1).await;
    // Several debounce intervals: every commit of the issue is snapshotted by now.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let status = task.status();
    assert!(status.last_error.is_some(), "a failed token copy was cleared");
    let snapshots = status.snapshots;

    fs::remove_file(&blocker).unwrap();
    let copy = blocker.join(token.file.file_name().unwrap());
    wait_for("the token copy", || copy.exists()).await;
    assert_eq!(fs::read(&copy).unwrap(), fs::read(&token.file).unwrap());
    assert_eq!(mode(&copy), 0o600);
    wait_for("the error to clear", || task.status().last_error.is_none()).await;
    assert_eq!(task.status().snapshots, snapshots, "no database change was needed");
    task.stop().await;
}

/// Past 9999999999 the sequence number grows a digit; ordering stays numeric, so rotation keeps
/// the new snapshot. Names whose sequence isn't all digits aren't snapshots.
#[tokio::test(flavor = "multi_thread")]
async fn sequence_order_survives_a_wider_number() {
    let f = fixture().await;
    let dir = f.backups();
    fs::create_dir_all(&dir).unwrap();
    for seq in ["9999999997", "9999999998", "9999999999"] {
        fs::write(dir.join(format!("mint-{seq}-20261116T000000.000Z.sqlite")), b"").unwrap();
    }
    let signed = dir.join("mint-+000000001-20261116T000000.000Z.sqlite");
    fs::write(&signed, b"").unwrap();
    let newest = Snapshotter::open(&f.home.db_path(), &dir, 3)
        .unwrap()
        .snapshot()
        .unwrap();
    assert!(
        newest.file_name().unwrap().to_string_lossy().starts_with("mint-10000000000-"),
        "{}",
        newest.display()
    );
    assert!(newest.exists(), "rotation removed the newest snapshot");
    assert_eq!(backup::newest(&dir).unwrap().as_ref(), Some(&newest));
    let all = backup::snapshots(&dir).unwrap();
    assert_eq!(all.len(), 3);
    assert!(!all.contains(&signed));
}

/// The real binary: SIGTERM (a plain `kill`, a supervisor) is a clean stop that takes the last
/// snapshot, like Ctrl-C.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_takes_the_stop_snapshot() {
    use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    let relay_url = relay.url().await.to_string();
    let dir = tempfile::tempdir().unwrap();
    let home = MintHome::at(dir.path());
    home.init().unwrap();
    fs::write(
        home.dir().join("mint.toml"),
        format!("relays = [{relay_url:?}]\nrate_limit = 20\n"),
    )
    .unwrap();
    let root = dir.path().to_owned();
    let backups = home.dir().join("backups");
    tokio::task::spawn_blocking(move || {
        let bin = env!("CARGO_BIN_EXE_maxplayer-mint");
        let mut run = Command::new(bin)
            .arg("run")
            .env("MAXPLAYER_HOME", &root)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let stderr = run.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) if line.contains("backup saved") => break,
                Ok(_) => continue,
                Err(_) => {
                    let _ = run.kill();
                    panic!("run never took its startup snapshot");
                }
            }
        }
        // A change inside the 30 s debounce: only the stop snapshot can hold it.
        let issued = Command::new(bin)
            .args(["issue", "8"])
            .env("MAXPLAYER_HOME", &root)
            .output()
            .unwrap();
        assert!(issued.status.success(), "{}", String::from_utf8_lossy(&issued.stderr));
        let killed = Command::new("kill")
            .args(["-TERM", &run.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = run.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = run.kill();
                panic!("run did not stop on SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success(), "SIGTERM was not a clean stop: {status:?}");
    })
    .await
    .unwrap();
    let (_, signed) = counts(&backup::newest(&backups).unwrap().unwrap());
    assert!(signed > 0, "the stop snapshot is missing the issue");
    assert_eq!(
        fs::read_dir(backups.join("issued")).unwrap().count(),
        1,
        "issued token copied on stop"
    );
}
