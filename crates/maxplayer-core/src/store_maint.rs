//! Buyer delivery-store maintenance: bound the store's packfile count (#774).
//!
//! Every buyer verify-fetch leaves one more packfile behind — [`crate::delivery_git`] fetches the
//! delivered fork tip and the pinned base into the store, and each fetch writes its own pack.
//! libgit2 never triggers git's auto-gc, so nothing ever collapses them.
//!
//! ## What actually costs descriptors (measured, and not the obvious guess)
//!
//! A pack does NOT hold a descriptor by existing, and the fetch that creates it does not either.
//! Descriptors are spent by an operation that READS ACROSS packs: libgit2 opens a pack's `.pack`
//! the first time it must read an object out of it, and holds it for that `Repository` handle's
//! lifetime. The buyer's descendant gate ([`crate::delivery_git`]'s `assert_descendant`) walks the
//! delivered chain, which reads one commit out of every pack the store holds.
//!
//! Measured on this tree by sampling `/proc/self/fd` during that walk: peak descriptors track the
//! pack count almost exactly — 249 packs to 253 descriptors — and fall back to ~4 between
//! operations. The pressure is a spike inside a single verify, which is why an at-rest descriptor
//! count looks healthy right up until one fails.
//!
//! ## How it presents, which is worse than "EMFILE"
//!
//! At the ceiling libgit2 does NOT report "Too many open files". It fails to open the pack and
//! reports the object as ABSENT, which `assert_descendant` surfaces as
//! `GitCommandFailed { operation: "merge-base", .. }` — a REFUSAL of a delivery that is perfectly
//! valid. (Not a wrong descendant answer: that gate maps a read error and a negative result to
//! different variants, and `tests` pins that discrimination.) So the failure mode of pack growth is
//! a false refusal on the money path, not a legible resource error.
//!
//! The ceiling that turns growth into failure belongs to the platform — a launchd default soft
//! limit is 256, a systemd unit's is commonly ~512k — so the same store is survivable on one host
//! and fatal on another. **The unbounded growth is the bug; the ceiling only sets the date.**
//!
//! No system `git` here: `git repack` would be the obvious tool and is not available to us. Every
//! production git leg in this crate runs in-process through libgit2 (issue #55), a property
//! `tests/no_system_git.rs` enforces at runtime with a PATH tripwire. Compaction is therefore built
//! from libgit2's packbuilder and odb packwriter.
//!
//! # Money safety
//!
//! This store holds the delivery objects behind payments **already made**:
//! `refs/maxplayer/deliveries/<oid>` IS the buyer's evidence that the thing it paid for was
//! delivered. A compaction that dropped an object would destroy that evidence, and no test that
//! only asserts "the fetch still succeeds" would notice. Two properties make that outcome
//! unreachable here, both by construction rather than by care:
//!
//! 1. **The new pack is built from the ODB, never from the ref graph.** [`git2::Odb::foreach`]
//!    enumerates every object the store holds — reachable or not — and each is inserted
//!    individually. This is the `git repack -A` / `--keep-unreachable` property: an object no ref
//!    points at is preserved exactly like one that is. A reachability walk (`insert_walk`) would
//!    be the `repack -a -d` behaviour, which drops unreachable objects, and is deliberately unused.
//! 2. **Nothing is deleted until the replacement is proven a superset.** The old packs are moved
//!    aside with `rename` (same filesystem, atomic) into a quarantine directory; the store is then
//!    re-opened so it can serve reads ONLY from the new pack, and every oid observed before the
//!    compaction must read back. Only then is the quarantine removed. On any mismatch the old
//!    packs are renamed back and the store is left exactly as it was found.
//!
//! 3. **A compaction the process did not live to finish is recovered, not ignored.** Property 2
//!    is an error path, and a SIGKILL reaches no error path: a process that dies between the
//!    quarantine rename and the verify leaves the old packs aside and the new pack unproven. The
//!    leftover `maxplayer-compact-<pid>` directory under `objects/` is read as the sentinel of
//!    exactly that, and its packs are moved back on the next compaction attempt — never deleted
//!    unread, which would turn a recoverable state into a permanent loss. See
//!    [`recover_quarantines`].

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use git2::{Oid, Repository};

/// Compact once the store holds this many packfiles.
///
/// Chosen against the *descriptor ceiling*, not against any observed store. Measured cost is ~1
/// descriptor per pack during a cross-pack read (see the module docs), so a threshold of 16 caps a
/// verify's peak at roughly 20 including the harness and the pack the in-flight fetch is writing.
/// That clears even the smallest platform ceiling — a 256-descriptor launchd default — by an order
/// of magnitude, leaving the rest for the fetch's own sockets and temporary files.
///
/// ⚠ THE BOUND IS ON THE STEADY STATE, NOT ON THE FIRST RUN. Compaction must itself read the packs
/// it is collapsing, so its descriptor need is set by the pack count it INHERITS. A store that
/// predates this code can arrive with enough packs that the compaction hits the ceiling; that
/// aborts, changes nothing, and leaves the store no worse. Raising `RLIMIT_NOFILE` at startup
/// (`crate::buyer`) is what carries that first run through — the one place that defence is
/// load-bearing rather than belt-and-braces.
///
/// The cost side is amortisation — one repack per 16 deliveries — and it is cheap because the
/// object count of a delivery store is small.
pub(crate) const COMPACT_PACK_THRESHOLD: usize = 16;

/// Directory-name prefix for the quarantine holding pre-compaction packs. Placed under `objects/`
/// (never inside `objects/pack/`, which libgit2 scans for `*.idx`) and deliberately not a two-hex
/// name, so it cannot be mistaken for a loose-object fanout directory.
const QUARANTINE_PREFIX: &str = "maxplayer-compact-";

/// Every extension a packfile may carry alongside its `.pack`. Moved together so a quarantined
/// pack never leaves a widowed index behind.
const PACK_EXTENSIONS: [&str; 6] = ["pack", "idx", "rev", "mtimes", "bitmap", "promisor"];

/// What a compaction attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compaction {
    /// Pack count was under [`COMPACT_PACK_THRESHOLD`]; the store was not touched.
    Skipped { packs: usize },
    /// `packs_before` packfiles were replaced by one, preserving `objects` objects.
    Compacted { packs_before: usize, objects: usize },
}

#[derive(Debug)]
pub(crate) enum StoreMaintError {
    /// The compaction did not complete AND the store was left exactly as it was found. The caller
    /// may safely continue: see the advisory contract on [`compact_if_needed`].
    Aborted(String),
    /// The pre-compaction packs could not be restored after a failed verify. The store may now be
    /// missing objects — the one condition a caller must NOT continue past.
    RollbackFailed(String),
}

impl std::fmt::Display for StoreMaintError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Aborted(message) => write!(formatter, "store compaction aborted, store unchanged: {message}"),
            Self::RollbackFailed(message) => write!(
                formatter,
                "store compaction could not restore the original packs — the store may be incomplete: {message}"
            ),
        }
    }
}

impl std::error::Error for StoreMaintError {}

/// Collapse the store's packfiles into one if it has accumulated [`COMPACT_PACK_THRESHOLD`] of
/// them; otherwise do nothing.
///
/// **Advisory contract.** [`StoreMaintError::Aborted`] means the store is untouched, so a caller
/// on the pay path should log it and proceed: maintenance failing must never refuse a delivery
/// that would otherwise verify. [`StoreMaintError::RollbackFailed`] is the exception and must fail
/// closed — the store can no longer be vouched for.
///
/// Blocking disk I/O. Callers on the pay path already run this inside
/// [`crate::git_transport::off_runtime`], off any ambient async runtime.
pub(crate) fn compact_if_needed(store: &Path) -> Result<Compaction, StoreMaintError> {
    let pack_dir = store.join("objects").join("pack");
    // Before anything else: a quarantine left by a compaction that never finished is recovered,
    // not ignored and never deleted unread. See `recover_quarantines`.
    recover_quarantines(store, &pack_dir)?;
    let stems = pack_stems(&pack_dir)?;
    if stems.len() < COMPACT_PACK_THRESHOLD {
        return Ok(Compaction::Skipped { packs: stems.len() });
    }
    compact(store, &pack_dir, &stems)
}

/// Move back any packs left in a quarantine by a compaction that did not finish.
///
/// ⛔ WHY THIS EXISTS, AND WHY IT IS NOT A CLEANUP. The rollback in `compact` covers a failed
/// VERIFY. It cannot cover the process DYING between the quarantine rename and the verify — a
/// SIGKILL never reaches an error path. In that window the old packs sit in the quarantine, the
/// new pack is in place, and its sufficiency was never proven. Nothing is destroyed (rename never
/// destroys), but nothing would ever look at those bytes again either: the store would come back
/// up potentially missing delivery objects with the recovery sitting unread on disk, and money-path
/// data that is recoverable but never recovered is lost in practice.
///
/// So a leftover quarantine directory is read as what it is — the on-disk sentinel of an
/// unfinished compaction — and its packs are put back. The store then holds the old packs AND the
/// unverified new one, which is a superset either way; the compaction that follows redoes the work
/// and this time proves it before destroying anything. Deleting a quarantine without reading it
/// would convert a recoverable state into a permanent loss, so this never does that.
///
/// SAFE ONLY BECAUSE THE STORE IS SINGLE-WRITER. Cited by symbol as well as line, because a bare
/// line number into a file this change also edits is self-invalidating — these two were stale in
/// review for exactly that reason:
/// - **One daemon per home**: `buyer::lock::HomeLock::acquire` takes `flock(LOCK_EX|LOCK_NB)`
///   (`buyer/lock.rs:74`); `buyer::bootstrap` calls it (`buyer/mod.rs:231`). A second daemon fails
///   closed.
/// - **One in-process route to the store**: `buyer::settle_job` (`buyer/mod.rs:1025`) holds
///   `BuyerContext::money_lock` (declared `buyer/mod.rs:163`, taken `buyer/mod.rs:1031`), and its
///   own doc records that the `collect` RPC and the delivery watcher call it and nothing else.
///
/// A quarantine found here therefore cannot belong to a live writer, even though its name carries
/// another process's pid.
fn recover_quarantines(store: &Path, pack_dir: &Path) -> Result<(), StoreMaintError> {
    let objects = store.join("objects");
    let entries = match fs::read_dir(&objects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(StoreMaintError::Aborted(format!(
                "read {}: {error}",
                objects.display()
            )))
        }
    };

    for entry in entries {
        let entry = entry
            .map_err(|error| StoreMaintError::Aborted(format!("read {}: {error}", objects.display())))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(QUARANTINE_PREFIX) || !entry.path().is_dir() {
            continue;
        }
        let quarantine = entry.path();
        let held = fs::read_dir(&quarantine).map_err(|error| {
            StoreMaintError::Aborted(format!("read {}: {error}", quarantine.display()))
        })?;
        let mut restored = 0usize;
        for file in held {
            let file = file.map_err(|error| {
                StoreMaintError::Aborted(format!("read {}: {error}", quarantine.display()))
            })?;
            let target = pack_dir.join(file.file_name());
            if target.exists() {
                // Pack filenames are content hashes, so a same-named pack already in place holds
                // the same bytes. The quarantined copy is redundant, not evidence.
                let _ = fs::remove_file(file.path());
                continue;
            }
            fs::create_dir_all(pack_dir).map_err(|error| {
                StoreMaintError::Aborted(format!("create {}: {error}", pack_dir.display()))
            })?;
            fs::rename(file.path(), &target).map_err(|error| {
                StoreMaintError::Aborted(format!(
                    "restore {} -> {}: {error}",
                    file.path().display(),
                    target.display()
                ))
            })?;
            restored += 1;
        }
        // Only after every file has been moved back or shown redundant.
        let _ = fs::remove_dir(&quarantine);
        crate::opline!(
            "buyer store: recovered an unfinished compaction — restored {restored} pack file(s) \
             from {}",
            quarantine.display()
        );
    }
    Ok(())
}

/// The `pack-<hash>` stems of every packfile in the store, EXCLUDING any that carries a `.keep`
/// marker — git's "something is relying on this pack" flag, which is never ours to move.
fn pack_stems(pack_dir: &Path) -> Result<Vec<String>, StoreMaintError> {
    let entries = match fs::read_dir(pack_dir) {
        Ok(entries) => entries,
        // A store that has never been fetched into has no pack directory yet: zero packs, not a
        // failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(StoreMaintError::Aborted(format!(
                "read {}: {error}",
                pack_dir.display()
            )))
        }
    };
    let mut stems = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            StoreMaintError::Aborted(format!("read {}: {error}", pack_dir.display()))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".pack") else {
            continue;
        };
        if pack_dir.join(format!("{stem}.keep")).exists() {
            continue;
        }
        stems.push(stem.to_owned());
    }
    Ok(stems)
}

fn compact(
    store: &Path,
    pack_dir: &Path,
    stems: &[String],
) -> Result<Compaction, StoreMaintError> {
    // 1. Snapshot every object the store holds, BEFORE anything moves. This one list is both the
    //    input to the new pack and the preservation set the verify below checks against, so the
    //    thing we promise to keep and the thing we actually pack cannot drift apart.
    let before = enumerate_objects(store)?;
    if before.is_empty() {
        return Ok(Compaction::Skipped { packs: stems.len() });
    }

    // 2. Write ONE pack holding all of them. The store now serves every object twice over — from
    //    the old packs and from the new one — which is what makes the next step reversible.
    let fresh = write_combined_pack(store, pack_dir, &before, stems)?;

    // 3. Move the pre-existing packs aside. Rename, not unlink: the bytes stay on the filesystem
    //    until the verify below has proven they are redundant.
    let quarantine = store
        .join("objects")
        .join(format!("{QUARANTINE_PREFIX}{}", std::process::id()));
    let moved = quarantine_packs(pack_dir, &quarantine, stems)?;

    // 4. Prove the superset BEFORE anything is destroyed: re-open the store, which can now serve
    //    reads only from the new pack (and any loose objects), and require every retained ref and
    //    every snapshotted oid to read back. Nothing has been unlinked at this point — step 3 only
    //    renamed — so this is a decision point, not a post-mortem.
    match verify_store_intact(store, &before) {
        Ok(()) => {
            // Proven redundant — now, and only now, the old packs are actually destroyed.
            if let Err(error) = fs::remove_dir_all(&quarantine) {
                // The store is correct and complete; a stranded quarantine is disk litter, not a
                // reason to fail a delivery.
                crate::opline!(
                    "buyer store: could not remove compaction quarantine {}: {error}",
                    quarantine.display()
                );
            }
            Ok(Compaction::Compacted {
                packs_before: stems.len(),
                objects: before.len(),
            })
        }
        Err(reason) => {
            // Put the store back exactly as it was found, then report a no-op. `restore` returns
            // RollbackFailed if it cannot, which propagates as the fail-closed case.
            restore(&moved)?;
            let _ = fs::remove_dir_all(&quarantine);
            remove_packs(pack_dir, &fresh);
            Err(StoreMaintError::Aborted(format!(
                "new pack did not preserve the store, original packs restored: {reason}"
            )))
        }
    }
}

/// Every oid the store holds, across all backends — packed AND loose, reachable AND unreachable.
fn enumerate_objects(store: &Path) -> Result<Vec<Oid>, StoreMaintError> {
    let repo = open_store(store)?;
    let odb = repo
        .odb()
        .map_err(|error| StoreMaintError::Aborted(format!("open odb: {error}")))?;
    let mut oids = Vec::new();
    odb.foreach(|oid| {
        oids.push(*oid);
        true
    })
    .map_err(|error| StoreMaintError::Aborted(format!("enumerate odb: {error}")))?;
    Ok(oids)
}

/// Build one packfile containing exactly `oids` and commit it into the store's odb. Returns the
/// stems that appeared, so a later abort can remove what this wrote.
///
/// The pack is streamed: `git_packbuilder_foreach` hands out the pack in chunks, which go straight
/// into the odb's packwriter (the same writer a fetch uses, so the `.idx` is generated by the same
/// code path as every other pack in the store). Nothing buffers the whole pack in memory — the base
/// fetch is a full fetch of a possibly large repository, so `write_buf` would be a real cost.
fn write_combined_pack(
    store: &Path,
    pack_dir: &Path,
    oids: &[Oid],
    existing: &[String],
) -> Result<Vec<String>, StoreMaintError> {
    {
        let repo = open_store(store)?;
        let odb = repo
            .odb()
            .map_err(|error| StoreMaintError::Aborted(format!("open odb: {error}")))?;
        let mut builder = repo
            .packbuilder()
            .map_err(|error| StoreMaintError::Aborted(format!("packbuilder: {error}")))?;
        for oid in oids {
            // insert_object, NOT insert_recursive/insert_walk: each object is packed on its own
            // account, so preservation does not depend on anything referencing it.
            builder.insert_object(*oid, None).map_err(|error| {
                StoreMaintError::Aborted(format!("pack object {oid}: {error}"))
            })?;
        }
        let mut writer = odb
            .packwriter()
            .map_err(|error| StoreMaintError::Aborted(format!("packwriter: {error}")))?;
        let mut write_error: Option<String> = None;
        builder
            .foreach(|chunk| match writer.write_all(chunk) {
                Ok(()) => true,
                Err(error) => {
                    write_error = Some(error.to_string());
                    false
                }
            })
            .map_err(|error| StoreMaintError::Aborted(format!("write pack: {error}")))?;
        if let Some(error) = write_error {
            return Err(StoreMaintError::Aborted(format!("write pack: {error}")));
        }
        writer
            .commit()
            .map_err(|error| StoreMaintError::Aborted(format!("commit pack: {error}")))?;
    }

    // Whatever is in the pack directory that was not there before is what we just wrote. Deriving
    // it by difference avoids having to predict libgit2's pack naming.
    let fresh: Vec<String> = pack_stems(pack_dir)?
        .into_iter()
        .filter(|stem| !existing.contains(stem))
        .collect();
    if fresh.is_empty() {
        return Err(StoreMaintError::Aborted(
            "packbuilder committed no new packfile".to_owned(),
        ));
    }
    Ok(fresh)
}

/// Move each pack's files into `quarantine`, undoing the moves already made if any rename fails —
/// a partially quarantined store must never be left short of a pack it still needs.
fn quarantine_packs(
    pack_dir: &Path,
    quarantine: &Path,
    stems: &[String],
) -> Result<Vec<(PathBuf, PathBuf)>, StoreMaintError> {
    fs::create_dir_all(quarantine).map_err(|error| {
        StoreMaintError::Aborted(format!("create {}: {error}", quarantine.display()))
    })?;
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for stem in stems {
        for extension in PACK_EXTENSIONS {
            let from = pack_dir.join(format!("{stem}.{extension}"));
            if !from.exists() {
                continue;
            }
            let to = quarantine.join(format!("{stem}.{extension}"));
            if let Err(error) = fs::rename(&from, &to) {
                restore(&moved)?;
                let _ = fs::remove_dir_all(quarantine);
                return Err(StoreMaintError::Aborted(format!(
                    "quarantine {}: {error}",
                    from.display()
                )));
            }
            moved.push((from, to));
        }
    }
    Ok(moved)
}

/// Rename every quarantined file back to where it came from.
fn restore(moved: &[(PathBuf, PathBuf)]) -> Result<(), StoreMaintError> {
    let mut failures = Vec::new();
    for (from, to) in moved {
        if let Err(error) = fs::rename(to, from) {
            failures.push(format!("{} -> {}: {error}", to.display(), from.display()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(StoreMaintError::RollbackFailed(failures.join("; ")))
    }
}

/// Delete packs this compaction wrote (abort path only — never used on pre-existing packs).
fn remove_packs(pack_dir: &Path, stems: &[String]) {
    for stem in stems {
        for extension in PACK_EXTENSIONS {
            let _ = fs::remove_file(pack_dir.join(format!("{stem}.{extension}")));
        }
    }
}

/// Prove the store is intact from a FRESHLY opened handle, BEFORE anything is unlinked.
///
/// Two legs, because they fail differently:
/// - **Every retained ref resolves and its target object reads.** `refs/maxplayer/deliveries/*` is
///   the evidence behind a settled payment, so this is the money-safety leg stated in its own
///   terms rather than inferred from the object sweep below.
/// - **Every oid observed before the compaction reads.** Strictly stronger than the ref leg (ref
///   targets are a subset of the object set) and it is what covers UNREACHABLE objects, which no
///   ref names by definition.
///
/// `read_header` rather than `exists`: it reads the object's header out of whichever pack now
/// claims it, so an index entry pointing at a pack that is no longer present fails here instead of
/// answering from a cached listing. The handle is new so it cannot answer from pack state mapped
/// before the quarantine.
fn verify_store_intact(store: &Path, oids: &[Oid]) -> Result<(), String> {
    let repo = Repository::open_bare(store).map_err(|error| format!("re-open store: {error}"))?;
    let odb = repo.odb().map_err(|error| format!("re-open odb: {error}"))?;

    let references = repo
        .references()
        .map_err(|error| format!("re-open refs: {error}"))?;
    for reference in references {
        let reference = reference.map_err(|error| format!("read ref: {error}"))?;
        let name = reference.name().unwrap_or("<non-utf8>").to_owned();
        if !name.starts_with("refs/maxplayer/") {
            continue;
        }
        let target = reference
            .target()
            .ok_or_else(|| format!("ref {name} no longer resolves"))?;
        odb.read_header(target)
            .map_err(|error| format!("ref {name} -> {target} unreadable: {error}"))?;
    }

    for oid in oids {
        odb.read_header(*oid)
            .map_err(|error| format!("object {oid} unreadable after compaction: {error}"))?;
    }
    Ok(())
}

fn open_store(store: &Path) -> Result<Repository, StoreMaintError> {
    Repository::open_bare(store)
        .map_err(|error| StoreMaintError::Aborted(format!("open store {}: {error}", store.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        source: Repository,
        store_path: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        /// A bare store plus a scratch source repository. Objects are authored in the source and
        /// pushed into the store one packfile at a time, which is how a store with a chosen pack
        /// count is built without a system `git` and without any loose objects to confuse the
        /// packed-object accounting.
        fn new(label: &str) -> Self {
            let id = NEXT.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "maxplayer-store-maint-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("fixture root");
            let source = Repository::init_bare(root.join("source.git")).expect("init source");
            let store_path = root.join("store.git");
            Repository::init_bare(&store_path).expect("init store");
            Self {
                root,
                source,
                store_path,
            }
        }

        fn store(&self) -> Repository {
            Repository::open_bare(&self.store_path).expect("open store")
        }

        fn pack_dir(&self) -> PathBuf {
            self.store_path.join("objects").join("pack")
        }

        fn pack_count(&self) -> usize {
            pack_stems(&self.pack_dir()).expect("list packs").len()
        }

        /// Author a commit in the source repo (not referenced there — only its oid matters).
        fn commit(&self, message: &str) -> Oid {
            let blob = self.source.blob(message.as_bytes()).expect("blob");
            let mut tree = self.source.treebuilder(None).expect("treebuilder");
            tree.insert("file.txt", blob, 0o100644).expect("insert blob");
            let tree_oid = tree.write().expect("write tree");
            let tree = self.source.find_tree(tree_oid).expect("find tree");
            let who = git2::Signature::new("t", "t@e", &git2::Time::new(1_700_000_000, 0))
                .expect("signature");
            self.source
                .commit(None, &who, &who, message, &tree, &[])
                .expect("commit")
        }

        fn blob(&self, content: &str) -> Oid {
            self.source.blob(content.as_bytes()).expect("blob")
        }

        /// Copy `oids` (and everything they reference) into the store as ONE new packfile.
        fn push_pack(&self, oids: &[Oid]) {
            let mut builder = self.source.packbuilder().expect("packbuilder");
            for oid in oids {
                builder.insert_recursive(*oid, None).expect("insert object");
            }
            let store = self.store();
            let odb = store.odb().expect("odb");
            let mut writer = odb.packwriter().expect("packwriter");
            builder
                .foreach(|chunk| writer.write_all(chunk).is_ok())
                .expect("stream pack");
            writer.commit().expect("commit pack");
        }

        /// Add filler packs until the store holds exactly `total` packfiles.
        fn fill_to(&self, total: usize) {
            let mut next = 0;
            while self.pack_count() < total {
                let oid = self.blob(&format!("filler {next}\n"));
                self.push_pack(&[oid]);
                next += 1;
            }
            assert_eq!(self.pack_count(), total, "fixture must land on {total} packs");
        }
    }

    /// Every `refs/maxplayer/**` ref and the oid it points at.
    fn maxplayer_refs(store: &Repository) -> Vec<(String, Oid)> {
        let mut refs: Vec<(String, Oid)> = store
            .references()
            .expect("references")
            .filter_map(|reference| reference.ok())
            .filter_map(|reference| {
                let name = reference.name()?.to_owned();
                if !name.starts_with("refs/maxplayer/") {
                    return None;
                }
                Some((name, reference.target()?))
            })
            .collect();
        refs.sort();
        refs
    }

    // ⛔ THE MONEY-SAFETY TEST. `refs/maxplayer/deliveries/*` is the buyer's evidence that a thing
    // it PAID FOR was delivered, so a compaction that dropped an object would destroy the artifact
    // behind a settled payment. Asserting "the store still works" would not catch that; this
    // asserts the specific objects.
    //
    // It also pins the property that separates this compaction from `git repack -a -d`: an
    // UNREACHABLE object — one no ref points at — survives too. That is the `-A` /
    // `--keep-unreachable` behaviour, and it is the assertion that goes red if anyone rebuilds the
    // pack from a reachability walk (`insert_walk`/`insert_recursive` over refs) instead of from
    // the object database.
    #[test]
    fn compaction_preserves_every_object_including_unreachable_ones() {
        let fixture = Fixture::new("preserve");

        // A paid delivery and its pinned base, each retained under the ref shape the pay path
        // writes (delivery_git::fetch / fetch_base).
        let delivery = fixture.commit("delivered work");
        let base = fixture.commit("pinned base");
        fixture.push_pack(&[delivery]);
        fixture.push_pack(&[base]);
        {
            let store = fixture.store();
            store
                .reference(
                    &format!("refs/maxplayer/deliveries/{delivery}"),
                    delivery,
                    true,
                    "retain",
                )
                .expect("delivery ref");
            store
                .reference(&format!("refs/maxplayer/bases/{base}"), base, true, "retain")
                .expect("base ref");
        }

        // An object NO ref points at — what `repack -a -d` would silently drop.
        let unreachable = fixture.blob("unreachable but paid-for\n");
        fixture.push_pack(&[unreachable]);

        fixture.fill_to(COMPACT_PACK_THRESHOLD);

        // Snapshot before: refs and their targets, and the full object set.
        let (before_refs, before_objects) = {
            let store = fixture.store();
            let refs = maxplayer_refs(&store);
            let objects = enumerate_objects(&fixture.store_path).expect("enumerate");
            (refs, objects)
        };
        // Positive control: the assertions below are only meaningful if there was something to
        // preserve. A fixture that silently produced no refs would make them pass vacuously.
        assert_eq!(before_refs.len(), 2, "fixture must retain 2 refs: {before_refs:?}");
        assert!(
            before_objects.contains(&unreachable),
            "fixture must actually hold the unreachable object"
        );

        let outcome = compact_if_needed(&fixture.store_path).expect("compaction must succeed");
        assert_eq!(
            outcome,
            Compaction::Compacted {
                packs_before: COMPACT_PACK_THRESHOLD,
                objects: before_objects.len(),
            },
            "compaction must report collapsing every pack"
        );

        // The fd win actually happened.
        assert_eq!(fixture.pack_count(), 1, "every pack must collapse into one");

        let store = fixture.store();
        // Every ref still resolves, to the SAME oid.
        assert_eq!(
            maxplayer_refs(&store),
            before_refs,
            "every maxplayer ref must survive compaction pointing at the same oid"
        );
        // ...and each ref's object is genuinely readable, not just named by a dangling ref.
        let odb = store.odb().expect("odb");
        for (name, oid) in &before_refs {
            odb.read_header(*oid)
                .unwrap_or_else(|error| panic!("ref {name} -> {oid} unreadable after compaction: {error}"));
        }
        // Every object at all — including the unreachable one.
        for oid in &before_objects {
            odb.read_header(*oid)
                .unwrap_or_else(|error| panic!("object {oid} unreadable after compaction: {error}"));
        }
        odb.read_header(unreachable)
            .expect("an unreachable object must survive compaction (-A, not -a -d)");
    }

    // Below the threshold the store is not touched at all: no repack cost on the pay path, and no
    // pack churn for a buyer that collects occasionally.
    #[test]
    fn under_threshold_leaves_the_store_untouched() {
        let fixture = Fixture::new("under");
        fixture.fill_to(COMPACT_PACK_THRESHOLD - 1);
        let before = pack_stems(&fixture.pack_dir()).expect("list packs");

        let outcome = compact_if_needed(&fixture.store_path).expect("under-threshold is a no-op");

        assert_eq!(
            outcome,
            Compaction::Skipped {
                packs: COMPACT_PACK_THRESHOLD - 1
            }
        );
        let mut after = pack_stems(&fixture.pack_dir()).expect("list packs");
        let mut before = before;
        before.sort();
        after.sort();
        assert_eq!(after, before, "an under-threshold store must be byte-identical");
    }

    // ⛔ PROCESS DEATH IS NOT ON THE ERROR PATH. The rollback in `compact` handles a failed verify;
    // a SIGKILL between the quarantine rename and the verify reaches no error path at all. This
    // simulates exactly that window — packs moved aside, verify never ran — and requires the next
    // compaction to READ the quarantine and put them back.
    //
    // The assertion that matters is the last one: an object reachable ONLY from the quarantined
    // pack must be readable again afterwards. A "recovery" that deleted the quarantine instead of
    // reading it would satisfy every other assertion here and fail that one, which is the whole
    // point — deleting unread converts a recoverable state into a permanent loss.
    #[test]
    fn an_unfinished_compaction_is_recovered_not_discarded() {
        let fixture = Fixture::new("recover");
        // Sized so the store lands UNDER the threshold once recovery puts the pack back. That
        // isolates the property under test: a compaction running afterwards would collapse the
        // restored pack into a new one and the "it is back" assertions could not tell recovery
        // apart from a re-pack.
        fixture.fill_to(COMPACT_PACK_THRESHOLD - 2);

        // A delivery whose objects live in ONE pack, which is the pack we strand.
        let stranded_commit = fixture.commit("delivery stranded mid-compaction");
        fixture.push_pack(&[stranded_commit]);
        {
            let store = fixture.store();
            store
                .reference(
                    &format!("refs/maxplayer/deliveries/{stranded_commit}"),
                    stranded_commit,
                    true,
                    "retain",
                )
                .expect("delivery ref");
        }
        let stranded_stem = pack_stems(&fixture.pack_dir())
            .expect("list packs")
            .into_iter()
            .max_by_key(|stem| {
                fs::metadata(fixture.pack_dir().join(format!("{stem}.pack")))
                    .and_then(|meta| meta.modified())
                    .ok()
            })
            .expect("newest pack");

        // Interrupt: move that pack aside exactly as `compact` step 3 does, then stop — no verify,
        // no rollback, no cleanup. This is the on-disk state a killed daemon leaves.
        let quarantine = fixture
            .store_path
            .join("objects")
            .join(format!("{QUARANTINE_PREFIX}999999"));
        fs::create_dir_all(&quarantine).expect("quarantine dir");
        let mut stranded_files = 0;
        for extension in PACK_EXTENSIONS {
            let from = fixture.pack_dir().join(format!("{stranded_stem}.{extension}"));
            if from.exists() {
                fs::rename(&from, quarantine.join(format!("{stranded_stem}.{extension}")))
                    .expect("strand pack");
                stranded_files += 1;
            }
        }
        assert!(stranded_files >= 2, "fixture must strand a .pack and its .idx");
        // Positive control: with the pack stranded the object is genuinely unreachable, so the
        // recovery below is doing real work rather than confirming a state that never broke.
        {
            let store = fixture.store();
            let odb = store.odb().expect("odb");
            assert!(
                odb.read_header(stranded_commit).is_err(),
                "fixture did not actually strand the delivery object"
            );
        }

        // Any subsequent compaction attempt must recover first. With the pack back the store sits
        // one under the threshold, so nothing compacts and what follows observes recovery alone.
        let outcome = compact_if_needed(&fixture.store_path).expect("recovery must not fail");
        assert_eq!(
            outcome,
            Compaction::Skipped {
                packs: COMPACT_PACK_THRESHOLD - 1
            },
            "the restored pack must be counted, and nothing should compact at this size"
        );

        assert!(
            !quarantine.exists(),
            "a fully recovered quarantine should be gone"
        );
        assert!(
            fixture
                .pack_dir()
                .join(format!("{stranded_stem}.pack"))
                .exists(),
            "the stranded pack must be restored to objects/pack"
        );
        let store = fixture.store();
        let odb = store.odb().expect("odb");
        odb.read_header(stranded_commit)
            .expect("the stranded delivery object must be readable again after recovery");
    }

    // A `.keep` marker is git's "something is relying on this pack" flag. We never move one, and it
    // is excluded from the count that decides whether to compact at all.
    #[test]
    fn kept_packs_are_never_counted_or_moved() {
        let fixture = Fixture::new("keep");
        fixture.fill_to(COMPACT_PACK_THRESHOLD);
        let kept = pack_stems(&fixture.pack_dir()).expect("list packs")[0].clone();
        fs::write(fixture.pack_dir().join(format!("{kept}.keep")), b"held\n").expect("write keep");

        // The kept pack drops out of the count, which puts the store back under the threshold.
        let outcome = compact_if_needed(&fixture.store_path).expect("compaction");
        assert_eq!(
            outcome,
            Compaction::Skipped {
                packs: COMPACT_PACK_THRESHOLD - 1
            },
            "a .keep pack must not be counted"
        );
        assert!(
            fixture.pack_dir().join(format!("{kept}.pack")).exists(),
            "a .keep pack must never be moved"
        );
    }
}
