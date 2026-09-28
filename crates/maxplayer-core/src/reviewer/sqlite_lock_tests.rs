//! Re-exec the test binary: another connection in this process shares POSIX locks.
use super::Store;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::process::Command;

const HELPER: &str = "reviewer::sqlite_lock_tests::sqlite_lock_child";

fn child(path: &Path, table: &str, expected: i64) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HELPER, "--ignored", "--nocapture"])
        .env("MAXPLAYER_SQLITE_LOCK_TEST_PATH", path)
        .env("MAXPLAYER_SQLITE_LOCK_TEST_TABLE", table)
        .env("MAXPLAYER_SQLITE_LOCK_TEST_COUNT", expected.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "SQLite child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("SQLITE_LOCK_CHILD_OK"));
}

#[test]
#[ignore = "helper invoked in a separate process by the lock regression tests"]
fn sqlite_lock_child() {
    let path = std::env::var_os("MAXPLAYER_SQLITE_LOCK_TEST_PATH").unwrap();
    let table = std::env::var("MAXPLAYER_SQLITE_LOCK_TEST_TABLE").unwrap();
    let expected: i64 = std::env::var("MAXPLAYER_SQLITE_LOCK_TEST_COUNT")
        .unwrap()
        .parse()
        .unwrap();
    let query = match table.as_str() {
        "reviews" => {
            "SELECT count(*) FROM reviews WHERE key='after-intruder' AND request='request'"
        }
        "content_offers" => "SELECT count(*) FROM content_offers",
        _ => panic!("unknown test table"),
    };
    let db =
        Connection::open_with_flags(Path::new(&path), OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let count: i64 = db.query_row(query, [], |row| row.get(0)).unwrap();
    assert_eq!(
        count, expected,
        "another process must see the committed row"
    );
    db.close().unwrap();
    println!("SQLITE_LOCK_CHILD_OK");
}

#[test]
fn reviewer_wal_survives_another_process_open_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("reviews.sqlite");
    let store = Store::open(&path).unwrap();
    // A read-write operator SELECT followed by close must not unlink our live WAL.
    child(&path, "reviews", 0);
    assert!(store.begin("after-intruder", "request").unwrap().is_none());
    child(&path, "reviews", 1);
    drop(store);
    child(&path, "reviews", 1);
}

#[test]
fn content_store_preserves_locks_on_an_existing_wal_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("content.sqlite");
    // WAL is persistent. Exercise the latent constructor bug without changing
    // ContentStore's production pragmas or using a second live in-process connection.
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    db.close().unwrap();
    let mut store = crate::private_content::store::ContentStore::open(&path).unwrap();
    child(&path, "content_offers", 0);
    store
        .reserve_offer(&"a".repeat(64), &"b".repeat(64), &"c".repeat(64))
        .unwrap();
    child(&path, "content_offers", 1);
    drop(store);
    child(&path, "content_offers", 1);
}

#[cfg(target_os = "linux")]
#[test]
fn reviewer_holds_a_posix_lock_on_the_database_inode() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reviews.sqlite");
    let store = Store::open(&path).unwrap();
    // stat does not open/close a database descriptor and cannot cancel its locks.
    let meta = std::fs::metadata(&path).unwrap();
    let locks = std::fs::read_to_string("/proc/locks").unwrap();
    let pid = std::process::id().to_string();
    assert!(
        locks.lines().any(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 6 || fields[1] != "POSIX" || fields[4] != pid {
                return false;
            }
            let dev_inode: Vec<_> = fields[5].split(':').collect();
            dev_inode.len() == 3
                && u64::from_str_radix(dev_inode[0], 16).ok()
                    == Some(libc::major(meta.dev()) as u64)
                && u64::from_str_radix(dev_inode[1], 16).ok()
                    == Some(libc::minor(meta.dev()) as u64)
                && dev_inode[2].parse::<u64>().ok() == Some(meta.ino())
        }),
        "reviewer must retain a POSIX lock on reviews.sqlite while its store is alive"
    );
    drop(store);
}
