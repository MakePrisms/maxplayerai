//! Seed the buyer delivery store with a private contribution's base (#1096 review B2).
//!
//! The buyer's verify fetch runs on the 10s money-path client. A fresh store has no objects
//! of the base, so the first fetch of a delivered fork tip asks the relay for the WHOLE
//! history, and the relay packs all of it before its first byte (11.6s for 37 MB, 78s for
//! 300 MB, measured against a real relay). Collection of any medium repository then failed.
//!
//! Posting already fetched exactly that base. It leaves the packs here, keyed by base oid;
//! the verifier imports them into the store (it is the store's single writer) and points
//! `refs/maxplayer/bases/<oid>` at the base, so the fork fetch negotiates `have <base>` and the
//! relay packs only the seller's new commits.
//!
//! Advisory by design: a missing or failed seed only means the fetch is as slow as before.
//! Seeded objects never stand in for verification. The fork is still fetched and tip-matched,
//! and the base is still fetched from the pinned target before the descendant gate.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Seeds nobody collected are dropped after this long.
const SEED_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(14 * 24 * 3600);
/// The store ref the verifier's base fetch writes, so a seeded base is a negotiation `have`.
pub(crate) fn base_ref(base_oid: &str) -> String {
    format!("refs/maxplayer/bases/{base_oid}")
}

fn seeds_dir(store: &Path) -> PathBuf {
    store.with_file_name("store-seeds")
}

fn valid_oid(oid: &str) -> bool {
    oid.len() == 40 && oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Regular `.pack`/`.idx` pairs directly under `dir`, as `(pack, idx)`.
fn pack_pairs(dir: &Path) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let mut pairs = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(pairs),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pack")
            || !fs::symlink_metadata(&path)?.file_type().is_file()
        {
            continue;
        }
        let idx = path.with_extension("idx");
        if fs::symlink_metadata(&idx).is_ok_and(|m| m.file_type().is_file()) {
            pairs.push((path, idx));
        }
    }
    Ok(pairs)
}

fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Keep the packs of a freshly fetched base for the later verify. `source` is posting's own
/// staging repository. Never fails the post: callers log an error and carry on.
pub(crate) fn write(store: &Path, source: &Path, base_oid: &str) -> io::Result<()> {
    if !valid_oid(base_oid) {
        return Err(io::Error::other("invalid base oid"));
    }
    let seeds = seeds_dir(store);
    private_dir(&seeds)?;
    prune(&seeds);
    let dest = seeds.join(base_oid);
    if dest.exists() {
        return Ok(());
    }
    let pairs = pack_pairs(&source.join("objects/pack"))?;
    if pairs.is_empty() {
        return Err(io::Error::other("base staging holds no pack"));
    }
    let tmp = seeds.join(format!(
        ".tmp-{base_oid}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    private_dir(&tmp)?;
    let copied = (|| {
        for (pack, idx) in &pairs {
            fs::copy(pack, tmp.join(pack.file_name().unwrap()))?;
            fs::copy(idx, tmp.join(idx.file_name().unwrap()))?;
        }
        fs::rename(&tmp, &dest)
    })();
    if copied.is_err() {
        let _ = fs::remove_dir_all(&tmp);
        if dest.exists() {
            return Ok(()); // a concurrent post of the same base won the rename
        }
    }
    copied
}

fn prune(seeds: &Path) {
    let Ok(entries) = fs::read_dir(seeds) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > SEED_MAX_AGE);
        if stale && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Import a seed for `base_oid` into `store` (which must exist) and name the base so the next
/// fetch negotiates against it. Returns whether a seed was imported. The caller is the store's
/// single writer (`delivery_git`, under the buyer's money lock).
pub(crate) fn import(store: &Path, base_oid: &str) -> Result<bool, String> {
    if !valid_oid(base_oid) {
        return Ok(false);
    }
    let seed = seeds_dir(store).join(base_oid);
    if !seed.is_dir() {
        return Ok(false);
    }
    let pairs = pack_pairs(&seed).map_err(|e| format!("read seed: {e}"))?;
    let pack_dir = store.join("objects/pack");
    private_dir(&pack_dir).map_err(|e| format!("store pack dir: {e}"))?;
    // Files this import placed, so a failed import takes them back out of the store.
    let mut placed = Vec::new();
    let result = (|| {
        for (pack, idx) in &pairs {
            let (pack_name, idx_name) = (pack.file_name().unwrap(), idx.file_name().unwrap());
            if pack_dir.join(idx_name).exists() {
                continue;
            }
            // The pack lands before its index: libgit2 only loads a pack through its .idx, so
            // a crash between the two leaves an unused file, never a half-visible pack. The
            // staging name ends in `.part`, never `.idx`, so a torn copy is never scanned.
            for (from, name) in [(pack, pack_name), (idx, idx_name)] {
                let tmp = pack_dir.join(format!("tmp_seed_{}.part", name.to_string_lossy()));
                fs::copy(from, &tmp).map_err(|e| format!("copy seed: {e}"))?;
                let dest = pack_dir.join(name);
                fs::rename(&tmp, &dest).map_err(|e| format!("place seed: {e}"))?;
                placed.push(dest);
            }
        }
        let repo = git2::Repository::open_bare(store).map_err(|e| format!("open store: {e}"))?;
        let oid = git2::Oid::from_str(base_oid).map_err(|e| e.to_string())?;
        repo.find_commit(oid)
            .map_err(|_| "seed does not contain the base commit".to_owned())?;
        Ok((repo, oid))
    })();
    let (repo, oid) = match result {
        Ok(found) => found,
        Err(error) => {
            // Index files first, so no half-removed pack is ever visible.
            placed.sort_by_key(|p| p.extension().is_none_or(|e| e != "idx"));
            for path in placed {
                let _ = fs::remove_file(path);
            }
            let _ = fs::remove_dir_all(&seed);
            return Err(error);
        }
    };
    if repo.find_reference(&base_ref(base_oid)).is_err() {
        repo.reference(&base_ref(base_oid), oid, false, "maxplayer seeded base")
            .map_err(|e| format!("base ref: {e}"))?;
    }
    let _ = fs::remove_dir_all(&seed);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-store-seed-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// A bare repository whose base commit lives only in a pack, like a fetched staging repo.
    fn packed_source(root: &Path) -> (PathBuf, String) {
        let scratch = git2::Repository::init_bare(root.join("scratch.git")).unwrap();
        let blob = scratch.blob(b"base\n").unwrap();
        let mut tree = scratch.treebuilder(None).unwrap();
        tree.insert("README", blob, 0o100644).unwrap();
        let tree = scratch.find_tree(tree.write().unwrap()).unwrap();
        let sig = git2::Signature::new("b", "b@example.invalid", &git2::Time::new(0, 0)).unwrap();
        let oid = scratch
            .commit(None, &sig, &sig, "base", &tree, &[])
            .unwrap();
        let source = root.join("staging.git");
        git2::Repository::init_bare(&source).unwrap();
        let mut builder = scratch.packbuilder().unwrap();
        builder.insert_commit(oid).unwrap();
        let mut buf = git2::Buf::new();
        builder.write_buf(&mut buf).unwrap();
        let dest_repo = git2::Repository::open_bare(&source).unwrap();
        let odb = dest_repo.odb().unwrap();
        let mut writer = odb.packwriter().unwrap();
        std::io::Write::write_all(&mut writer, &buf).unwrap();
        writer.commit().unwrap();
        (source, oid.to_string())
    }

    #[test]
    fn a_seeded_base_is_imported_and_named_for_negotiation() {
        let root = temp("import");
        let (source, oid) = packed_source(&root);
        let store = root.join("store");
        write(&store, &source, &oid).unwrap();
        fs::remove_dir_all(&source).unwrap(); // posting's staging is gone by collect time
        git2::Repository::init_bare(&store).unwrap();
        assert_eq!(import(&store, &oid), Ok(true));
        let repo = git2::Repository::open_bare(&store).unwrap();
        assert!(repo.find_commit(git2::Oid::from_str(&oid).unwrap()).is_ok());
        assert_eq!(
            repo.refname_to_id(&base_ref(&oid)).unwrap().to_string(),
            oid,
            "the base is a ref, so the fork fetch sends `have <base>`"
        );
        assert!(
            !seeds_dir(&store).join(&oid).exists(),
            "a used seed is removed"
        );
        assert_eq!(import(&store, &oid), Ok(false), "nothing left to import");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_foreign_seeds_import_nothing() {
        let root = temp("missing");
        let store = root.join("store");
        git2::Repository::init_bare(&store).unwrap();
        assert_eq!(import(&store, &"ab".repeat(20)), Ok(false));
        assert_eq!(import(&store, "../escape"), Ok(false));
        assert!(write(&store, &root, "not-an-oid").is_err());
        let (source, oid) = packed_source(&root);
        let other = "cd".repeat(20);
        write(&store, &source, &other).unwrap();
        assert!(
            import(&store, &other).is_err(),
            "a seed without its base commit is refused, not named"
        );
        assert!(
            git2::Repository::open_bare(&store)
                .unwrap()
                .find_reference(&base_ref(&other))
                .is_err()
        );
        assert!(
            pack_pairs(&store.join("objects/pack")).unwrap().is_empty(),
            "a refused seed leaves none of its packs in the store"
        );
        let _ = (oid, fs::remove_dir_all(&root));
    }
}
