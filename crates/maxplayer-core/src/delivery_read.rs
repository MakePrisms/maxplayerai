//! Read or extract a settled git delivery from the buyer store (spec
//! `docs/specs/settle-without-unpacking.md` §2.3–2.4).
//!
//! Settle no longer writes a git delivery under `results/` (P1, v0.7.3-rc8 security review): in the
//! buyer store no delivered file exists under its own name, so no agent harness or git loads one on
//! its own. This module is the buyer's way in:
//!
//! - [`read`] lists the delivered files, or returns one file's text, straight from the store through
//!   libgit2. Nothing is written.
//! - [`extract`] writes the delivery into a throwaway folder (default: a new directory under the
//!   system temp dir, never inside the maxplayer home) with the same checked writer collect used,
//!   and refuses a tree that lays out a git repository (see [`git_repository_layout`]). It is the
//!   only path in the binary that writes delivered files.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::collect;
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::home::MaxplayerHome;
use crate::job_lifecycle::{self, AcceptedBind};

/// Default and largest number of bytes [`read`] returns per call.
pub const READ_DEFAULT_BYTES: usize = 64 * 1024;
pub const READ_MAX_BYTES: usize = 256 * 1024;

/// The accept-bind under `key` (a job id, or a session turn's follow-up id).
pub fn bind_for(home: &MaxplayerHome, key: &str) -> Result<AcceptedBind, String> {
    job_lifecycle::load_accepted_bind(home, key)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!("no accepted delivery for {key} yet; wait until get_job shows it delivered")
        })
}

/// List the delivery's files (`path` = `None`), or return one file's text from `offset`, at most
/// `max_bytes` (capped at [`READ_MAX_BYTES`]). A file that is not UTF-8 text returns its size and
/// no content. Writes nothing.
pub fn read(
    home: &MaxplayerHome,
    bind: &AcceptedBind,
    path: Option<&str>,
    offset: usize,
    max_bytes: Option<usize>,
) -> Result<Value, String> {
    let max = max_bytes
        .unwrap_or(READ_DEFAULT_BYTES)
        .clamp(1, READ_MAX_BYTES);
    if collect::bind_is_inline(bind) {
        let answer = bind.inline_answer.as_deref().unwrap_or_default();
        return match path {
            None => Ok(json!({ "delivery": "inline", "files": [collect::INLINE_ANSWER_FILENAME] })),
            Some(collect::INLINE_ANSWER_FILENAME) => Ok(text_slice(
                collect::INLINE_ANSWER_FILENAME,
                answer.as_bytes(),
                offset,
                max,
            )),
            Some(other) => Err(format!(
                "{other:?} is not in this delivery (an inline answer)"
            )),
        };
    }
    let store = collect::delivery_store_path(home);
    let Some(path) = path else {
        let files = collect::delivery_file_list(&store, &bind.commit_oid)?;
        return Ok(json!({
            "delivery": "store",
            "commit_oid": bind.commit_oid,
            "file_count": files.len(),
            "files": files,
        }));
    };
    collect::validate_delivery_path(path).map_err(|error| format!("path refused: {error}"))?;
    let repo = git2::Repository::open_bare(&store)
        .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
    let tree = collect::delivery_tree(&repo, &bind.commit_oid)?;
    let entry = tree
        .get_path(Path::new(path))
        .map_err(|_| format!("{path:?} is not a file in this delivery"))?;
    if entry.kind() != Some(git2::ObjectType::Blob) {
        return Err(format!(
            "{path:?} is a directory; call without path to list the files"
        ));
    }
    let blob = repo
        .find_blob(entry.id())
        .map_err(|error| error.to_string())?;
    Ok(text_slice(path, blob.content(), offset, max))
}

fn text_slice(path: &str, bytes: &[u8], offset: usize, max: usize) -> Value {
    let size = bytes.len();
    let text = match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => text,
        _ => {
            return json!({
                "path": path,
                "size": size,
                "binary": true,
                "note": "not UTF-8 text; extract the delivery into a throwaway folder to inspect it",
            });
        }
    };
    let mut start = offset.min(size);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = start.saturating_add(max).min(size);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut value = json!({
        "path": path,
        "size": size,
        "offset": start,
        "content": &text[start..end],
    });
    if end < size {
        value["next_offset"] = json!(end);
    }
    value
}

/// Extract the delivery into `to`, or into a new folder under the system temp dir when `to` is
/// `None`. `to` must be absent or an empty directory, and neither it nor the default folder may be
/// inside the maxplayer home. Returns the folder and the sorted file list.
pub fn extract(
    home: &MaxplayerHome,
    bind: &AcceptedBind,
    to: Option<&Path>,
) -> Result<(PathBuf, Vec<String>), String> {
    if collect::bind_is_inline(bind) {
        return Err(
            "this is an inline answer; it is already written as answer.txt (get_job \
             settlement.results_path), and read_delivery returns it"
                .to_owned(),
        );
    }
    let store = collect::delivery_store_path(home);
    let store_ref = PayPathDeliveryVerifier::store_ref_for(&bind.commit_oid);
    // Refuse before creating anything: a bad tree leaves no folder behind.
    collect::validate_delivery_tree_in_store(&store, &store_ref, &bind.commit_oid)?;
    {
        let repo = git2::Repository::open_bare(&store)
            .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
        let tree = collect::delivery_tree(&repo, &bind.commit_oid)?;
        if let Some(dir) = git_repository_layout(&repo, &tree)? {
            return Err(format!(
                "refusing to extract: {} holds a git repository's internal files (a HEAD file \
                 with objects/ and refs/, or with commondir), so git would treat that folder as a \
                 repository and read the delivered config. Read those files with read_delivery \
                 instead. Nothing was written",
                if dir.is_empty() {
                    "the delivery's top folder".to_owned()
                } else {
                    format!("{dir:?}")
                }
            ));
        }
    }
    let home_root = canonical_or_self(&home.root);
    let dest = match to {
        Some(to) => {
            let absolute = if to.is_absolute() {
                to.to_path_buf()
            } else {
                std::env::current_dir().map_err(|e| e.to_string())?.join(to)
            };
            if inside(&absolute, &home_root) {
                return Err(format!(
                    "{} is inside the maxplayer home ({}); extract somewhere outside it",
                    absolute.display(),
                    home_root.display()
                ));
            }
            match std::fs::symlink_metadata(&absolute) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(format!("{} is a symlink; refusing", absolute.display()));
                }
                Ok(meta) if !meta.is_dir() => {
                    return Err(format!(
                        "{} exists and is not a directory",
                        absolute.display()
                    ));
                }
                Ok(_) => {
                    let mut entries = std::fs::read_dir(&absolute)
                        .map_err(|e| format!("read {}: {e}", absolute.display()))?;
                    if entries.next().is_some() {
                        return Err(format!(
                            "{} is not empty; extract into a new or empty folder",
                            absolute.display()
                        ));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("inspect {}: {error}", absolute.display())),
            }
            absolute
        }
        None => {
            let short = &bind.job_id[..bind.job_id.len().min(8)];
            let prefix = format!("maxplayer-{short}-");
            let dir = tempfile::Builder::new()
                .prefix(&prefix)
                .tempdir()
                .map_err(|error| format!("create a temp folder: {error}"))?
                .keep();
            if inside(&canonical_or_self(&dir), &home_root) {
                let _ = std::fs::remove_dir(&dir);
                return Err(format!(
                    "the system temp dir {} is inside the maxplayer home; pass --to",
                    dir.display()
                ));
            }
            dir
        }
    };
    collect::check_dest(&dest)?;
    let files = collect::materialize_delivery(&store, &store_ref, &bind.commit_oid, &dest)?;
    Ok((dest, files))
}

/// The first directory (`""` = the top) whose entries make git treat it as a repository: a blob
/// named `HEAD` (folded as collect folds names) with either `objects/` and `refs/` trees, or a
/// `commondir` file. git, libgit2 and gitoxide all require the `HEAD` file. One structural rule
/// about git, not a list of harness file names.
pub fn git_repository_layout(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
) -> Result<Option<String>, String> {
    fn check(
        repo: &git2::Repository,
        tree: &git2::Tree<'_>,
        dir: &str,
        budget: &mut usize,
    ) -> Result<Option<String>, String> {
        let (mut head, mut objects, mut refs, mut commondir) = (false, false, false, false);
        let mut subtrees = Vec::new();
        for entry in tree.iter() {
            *budget = budget
                .checked_sub(1)
                .ok_or("delivery has too many entries")?;
            let Ok(name) = entry.name() else { continue };
            let key = collect::collision_key(name);
            match entry.kind() {
                Some(git2::ObjectType::Blob) => {
                    head |= key == "head";
                    commondir |= key == "commondir";
                }
                Some(git2::ObjectType::Tree) => {
                    objects |= key == "objects";
                    refs |= key == "refs";
                    subtrees.push((name.to_owned(), entry.id()));
                }
                _ => {}
            }
        }
        if head && ((objects && refs) || commondir) {
            return Ok(Some(dir.to_owned()));
        }
        for (name, id) in subtrees {
            let sub = repo.find_tree(id).map_err(|e| e.to_string())?;
            let path = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if let Some(found) = check(repo, &sub, &path, budget)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }
    // Same bound as the pre-pay check, which has already refused anything larger.
    let mut budget = 100_000usize;
    check(repo, tree, "", &mut budget)
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// True when `path` (resolved through its nearest existing ancestor) is `root` or under it.
fn inside(path: &Path, root: &Path) -> bool {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (
            existing.file_name().map(|n| n.to_owned()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut resolved = canonical_or_self(&existing);
    for name in rest.into_iter().rev() {
        resolved.push(name);
    }
    resolved.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "maxplayer-dread-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// A home whose store holds one delivery of `files` (`(path, contents)`), and its bind.
    fn seeded(label: &str, files: &[(&str, &str)]) -> (PathBuf, MaxplayerHome, AcceptedBind) {
        fn build(repo: &git2::Repository, files: &[(Vec<&str>, &str)]) -> git2::Oid {
            let mut top = repo.treebuilder(None).expect("tree");
            let mut subdirs: std::collections::BTreeMap<&str, Vec<(Vec<&str>, &str)>> =
                Default::default();
            for (parts, contents) in files {
                if parts.len() == 1 {
                    let blob = repo.blob(contents.as_bytes()).expect("blob");
                    top.insert(parts[0], blob, 0o100644).expect("insert");
                } else {
                    subdirs
                        .entry(parts[0])
                        .or_default()
                        .push((parts[1..].to_vec(), contents));
                }
            }
            for (name, children) in subdirs {
                let oid = build(repo, &children);
                top.insert(name, oid, 0o040000).expect("subtree");
            }
            top.write().expect("write tree")
        }
        // The home is a subfolder, so `root/<x>` is a target outside it.
        let root = temp_root(label);
        let home = home::bootstrap(root.join("home")).expect("home");
        let repo = git2::Repository::init_bare(collect::delivery_store_path(&home)).expect("store");
        let split: Vec<(Vec<&str>, &str)> = files
            .iter()
            .map(|(p, c)| (p.split('/').collect(), *c))
            .collect();
        let tree = repo.find_tree(build(&repo, &split)).expect("tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let oid = repo
            .commit(None, &sig, &sig, "delivery", &tree, &[])
            .expect("commit");
        repo.reference(
            &PayPathDeliveryVerifier::store_ref_for(&oid.to_string()),
            oid,
            true,
            "r",
        )
        .expect("ref");
        let bind: AcceptedBind = serde_json::from_value(json!({
            "job_id": "ab".repeat(32),
            "claim_id": "c".repeat(64),
            "result_id": "d".repeat(64),
            "seller_pubkey": "e".repeat(64),
            "commit_oid": oid.to_string(),
            "repo": "",
            "branch": "",
            "job_hash": "f".repeat(64),
            "amount_sats": 0,
            "accepted_at": 1,
            "accept_event_id": "1".repeat(64),
            "seller_signature": "2".repeat(128),
        }))
        .expect("bind");
        (root, home, bind)
    }

    #[test]
    fn read_lists_and_returns_files_without_writing_anything() {
        let (root, home, bind) = seeded(
            "read",
            &[
                ("README.md", "hello"),
                ("CLAUDE.md", "seller says"),
                ("src/a.rs", "fn a(){}"),
            ],
        );
        let listed = read(&home, &bind, None, 0, None).expect("list");
        assert_eq!(listed["delivery"], json!("store"));
        assert_eq!(
            listed["files"],
            json!(["CLAUDE.md", "README.md", "src/a.rs"])
        );
        let file = read(&home, &bind, Some("CLAUDE.md"), 0, None).expect("read");
        assert_eq!(file["content"], json!("seller says"));
        let part = read(&home, &bind, Some("README.md"), 1, Some(2)).expect("slice");
        assert_eq!(
            (part["content"].clone(), part["next_offset"].clone()),
            (json!("el"), json!(3))
        );
        for bad in ["../x", "/etc/passwd", "src", "missing.md", ""] {
            assert!(
                read(&home, &bind, Some(bad), 0, None).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(!home.root.join("results").exists(), "a read writes nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_writes_into_a_temp_folder_outside_home_and_refuses_bad_targets() {
        let (root, home, bind) = seeded("extract", &[("AGENTS.md", "x"), ("src/a.rs", "a")]);
        let (dir, files) = extract(&home, &bind, None).expect("extract");
        assert!(dir.starts_with(std::env::temp_dir()) && !dir.starts_with(&home.root));
        assert_eq!(files, vec!["AGENTS.md".to_owned(), "src/a.rs".to_owned()]);
        assert_eq!(std::fs::read_to_string(dir.join("src/a.rs")).unwrap(), "a");
        let _ = std::fs::remove_dir_all(&dir);

        let full = root.join("full");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("mine"), "m").unwrap();
        assert!(
            extract(&home, &bind, Some(&full))
                .unwrap_err()
                .contains("not empty")
        );
        let in_home = home.root.join("x");
        assert!(
            extract(&home, &bind, Some(&in_home))
                .unwrap_err()
                .contains("inside the maxplayer home")
        );
        assert!(!in_home.exists());
        let fresh = root.join("fresh");
        let (dir, _) = extract(&home, &bind, Some(&fresh)).expect("fresh dir");
        assert_eq!(dir, fresh);
        assert!(fresh.join("AGENTS.md").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_refuses_a_git_repository_layout_anywhere_and_writes_nothing() {
        for layout in [
            vec![
                ("HEAD", "ref: refs/heads/main\n"),
                ("objects/info/x", "x"),
                ("refs/x", "x"),
                ("config", "[core]\n"),
            ],
            vec![
                ("sub/HEAD", "ref: x\n"),
                ("sub/objects/x", "x"),
                ("sub/refs/x", "x"),
            ],
            vec![("a/b/head", "ref: x\n"), ("a/b/commondir", "../c\n")],
        ] {
            let (root, home, bind) = seeded("gitlayout", &layout);
            let to = root.join("out");
            let error = extract(&home, &bind, Some(&to)).expect_err("git layout refused");
            assert!(error.contains("git repository"), "{error}");
            assert!(!to.exists(), "nothing written");
            assert!(
                read(&home, &bind, Some(layout[0].0), 0, None).is_ok(),
                "still readable"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
        // Not a repository: no HEAD file, or HEAD as a directory, or HEAD without refs/.
        for layout in [
            vec![("objects/x", "x"), ("refs/x", "x"), ("config", "c")],
            vec![("HEAD/x", "x"), ("objects/x", "x"), ("refs/x", "x")],
            vec![("HEAD", "x"), ("objects/x", "x"), ("HEADER.md", "h")],
        ] {
            let (root, home, bind) = seeded("notrepo", &layout);
            let to = root.join("out");
            extract(&home, &bind, Some(&to)).expect("ordinary tree extracts");
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
