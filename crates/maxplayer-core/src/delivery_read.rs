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
//!   and refuses a tree that lays out a version-control repository (see [`extract_refusal`]). It is
//!   the only path in the binary that writes delivered files.

use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::collect;
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::home::MaxplayerHome;
use crate::job_lifecycle::{self, AcceptedBind};

/// Default and largest number of bytes [`read`] returns per call.
pub const READ_DEFAULT_BYTES: usize = 64 * 1024;
pub const READ_MAX_BYTES: usize = 256 * 1024;
/// Largest file [`read`] returns text from. Each paged read loads the whole blob, so a bigger file
/// is extracted instead (#1311 fable L6).
pub const READ_TEXT_MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Most paths one listing returns; the rest are paged with `offset` (#1311 fable L6).
pub const LIST_MAX_ENTRIES: usize = 2_000;

/// Version-control metadata folders other than `.git` (refused by name before payment): a delivered
/// one would make that VCS read seller config in the extracted folder, e.g. an `.hg/hgrc` hook that
/// runs on `hg status` (#1311 fable M7). VCS names, not harness names. Compared folded.
const VCS_METADATA_DIRS: &[&str] = &[".hg", ".jj", ".sl", ".svn", ".bzr"];

/// A Nostr event id: 64 lowercase hex characters. Job ids and follow-up ids are joined into file
/// paths under the home, so anything else is refused before it gets near one (#1311 fable M1).
pub fn is_event_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The accept-bind under `key` (a job id, or a session turn's follow-up id).
pub fn bind_for(home: &MaxplayerHome, key: &str) -> Result<AcceptedBind, String> {
    if !is_event_id(key) {
        return Err(format!(
            "{key:?} is not a job id (64 lowercase hex characters)"
        ));
    }
    job_lifecycle::load_accepted_bind(home, key)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!("no accepted delivery for {key} yet; wait until get_job shows it delivered")
        })
}

/// List the delivery's files (`path` = `None`; `offset` pages the list), or return one file's text
/// from byte `offset`, at most `max_bytes` (capped at [`READ_MAX_BYTES`]). A file that is not UTF-8
/// text returns its size and no content. Writes nothing.
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
    let repo = git2::Repository::open_bare(&store)
        .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
    let tree = collect::delivery_tree(&repo, &bind.commit_oid)?;
    let Some(path) = path else {
        return list(&tree, &bind.commit_oid, offset);
    };
    collect::validate_delivery_path(path).map_err(|error| format!("path refused: {error}"))?;
    let entry = tree
        .get_path(Path::new(path))
        .map_err(|_| format!("{path:?} is not a file in this delivery"))?;
    if entry.kind() != Some(git2::ObjectType::Blob) {
        return Err(format!(
            "{path:?} is a directory or a submodule; call without path to list the files"
        ));
    }
    let blob = repo
        .find_blob(entry.id())
        .map_err(|error| error.to_string())?;
    if blob.size() > READ_TEXT_MAX_FILE_BYTES {
        return Ok(json!({
            "path": path,
            "size": blob.size(),
            "too_large": true,
            "note": format!("larger than {READ_TEXT_MAX_FILE_BYTES} bytes; extract the delivery into a throwaway folder to inspect it"),
        }));
    }
    let mut value = text_slice(path, blob.content(), offset, max);
    if entry.filemode() == 0o120000 {
        // The content is the link target. extract writes it as a plain text file; a git merge of
        // the commit creates a real symlink.
        value["symlink"] = json!(true);
    }
    Ok(value)
}

/// One page of the file list, plus every entry whose type the plain list hides (#1311 fable L8):
/// symlinks, submodules (gitlinks, never extracted) and executable files.
fn list(tree: &git2::Tree<'_>, commit_oid: &str, offset: usize) -> Result<Value, String> {
    let mut files = Vec::new();
    let mut special = Vec::new();
    let mut visited = 0usize;
    let walked = tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        visited += 1;
        if visited > collect::MAX_DELIVERY_ENTRIES {
            return git2::TreeWalkResult::Abort;
        }
        let Ok(name) = entry.name() else {
            return git2::TreeWalkResult::Ok;
        };
        let rel = format!("{root}{name}");
        let kind = match entry.filemode() {
            0o120000 => Some("symlink"),
            0o160000 => Some("submodule"),
            0o100755 => Some("executable"),
            _ => None,
        };
        if let Some(kind) = kind {
            special.push(json!({ "path": rel, "kind": kind }));
        }
        if entry.kind() == Some(git2::ObjectType::Blob) {
            files.push(rel);
        }
        git2::TreeWalkResult::Ok
    });
    if visited > collect::MAX_DELIVERY_ENTRIES {
        return Err(format!(
            "delivery has more than {} entries; not listed",
            collect::MAX_DELIVERY_ENTRIES
        ));
    }
    walked.map_err(|error| error.to_string())?;
    files.sort();
    let start = offset.min(files.len());
    let end = start.saturating_add(LIST_MAX_ENTRIES).min(files.len());
    special.truncate(LIST_MAX_ENTRIES);
    let mut value = json!({
        "delivery": "store",
        "commit_oid": commit_oid,
        "file_count": files.len(),
        "offset": start,
        "files": &files[start..end],
        "special": special,
    });
    if end < files.len() {
        value["next_offset"] = json!(end);
    }
    Ok(value)
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
    // A max smaller than the character at `start` would return nothing and the same offset, and a
    // pager would loop forever (#1311 fable L7): return that one whole character instead.
    if end == start && start < size {
        end = start + 1;
        while !text.is_char_boundary(end) {
            end += 1;
        }
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
/// `None`. `to` must be an absolute path with no `.` or `..` component, absent or an empty
/// directory, and neither it nor the default folder may be inside the maxplayer home (checked
/// again once the folder exists). A failure removes what it wrote. Returns the folder and the
/// sorted file list.
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
        if let Some(reason) = extract_refusal(&repo, &tree)? {
            return Err(format!(
                "refusing to extract: {reason}. Read those files with read_delivery instead; for a \
                 contribution job, fetch the commit into your own clone (the buyer-operate skill). \
                 Nothing was written"
            ));
        }
    }
    let home_root = canonical_or_self(&home.root);
    let (dest, created) = match to {
        Some(to) => prepare_target(to, &home_root)?,
        None => {
            let short = &bind.job_id[..bind.job_id.len().min(8)];
            let prefix = format!("maxplayer-{short}-");
            let dir = tempfile::Builder::new()
                .prefix(&prefix)
                .tempdir()
                .map_err(|error| format!("create a temp folder: {error}"))?
                .keep();
            let real = canonical_or_self(&dir);
            if real.starts_with(&home_root) {
                let _ = std::fs::remove_dir(&dir);
                return Err(format!(
                    "the system temp dir {} is inside the maxplayer home; pass --to",
                    dir.display()
                ));
            }
            (real, true)
        }
    };
    match collect::materialize_delivery(&store, &store_ref, &bind.commit_oid, &dest) {
        Ok(files) => Ok((dest, files)),
        Err(error) => {
            // The folder was new or empty: put it back the way it was.
            let _ = std::fs::remove_dir_all(&dest);
            if !created {
                let _ = std::fs::create_dir(&dest);
            }
            Err(format!("extract failed and wrote nothing: {error}"))
        }
    }
}

/// Validate a caller-chosen `to`, create it when absent, and return its real path and whether it
/// was created here. Checked twice: lexically before anything exists, and on the canonical path
/// once the folder exists, so neither a `..` after a missing component nor an ancestor swapped for
/// a symlink can land it inside the home (#1311 fable M2).
fn prepare_target(to: &Path, home_root: &Path) -> Result<(PathBuf, bool), String> {
    if !to.is_absolute() {
        return Err(format!(
            "{} is relative; pass an absolute folder (the buyer daemon's working directory is not \
             yours)",
            to.display()
        ));
    }
    if to
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(format!(
            "{} contains a . or .. component; refusing",
            to.display()
        ));
    }
    if inside(to, home_root) {
        return Err(format!(
            "{} is inside the maxplayer home ({}); extract somewhere outside it",
            to.display(),
            home_root.display()
        ));
    }
    let created = match std::fs::symlink_metadata(to) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!("{} is a symlink; refusing", to.display()));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(format!("{} exists and is not a directory", to.display()));
        }
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            std::fs::create_dir(to).map_err(|e| format!("create {}: {e}", to.display()))?;
            true
        }
        Err(error) => return Err(format!("inspect {}: {error}", to.display())),
    };
    let undo = |message: String| {
        if created {
            let _ = std::fs::remove_dir(to);
        }
        Err(message)
    };
    let real = match std::fs::canonicalize(to) {
        Ok(real) => real,
        Err(error) => return undo(format!("resolve {}: {error}", to.display())),
    };
    if real.starts_with(home_root) {
        return undo(format!(
            "{} resolves to {}, inside the maxplayer home; extract somewhere outside it",
            to.display(),
            real.display()
        ));
    }
    match std::fs::read_dir(&real).map(|mut entries| entries.next().is_some()) {
        Ok(false) => Ok((real, created)),
        Ok(true) => undo(format!(
            "{} is not empty; extract into a new or empty folder",
            to.display()
        )),
        Err(error) => undo(format!("read {}: {error}", to.display())),
    }
}

/// Why `extract` must not write this tree, or `None`. Two structural rules, checked in every
/// folder with names folded as collect folds them:
///
/// - A git repository layout: a blob named `HEAD` with a `refs/` tree, or with a `commondir` blob.
///   git, libgit2 and gitoxide all need the `HEAD` file. `objects/` is not required, because
///   `GIT_OBJECT_DIRECTORY` in the environment stands in for it (#1311 fable L1). The rule relies
///   on `extract` writing every entry as a plain non-executable file: no symlinks, no exec bits.
/// - Another VCS's metadata folder ([`VCS_METADATA_DIRS`]); `.git` is refused by name before pay.
pub fn extract_refusal(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
) -> Result<Option<String>, String> {
    fn check(
        repo: &git2::Repository,
        tree: &git2::Tree<'_>,
        dir: &str,
        budget: &mut usize,
    ) -> Result<Option<String>, String> {
        let shown = |dir: &str| {
            if dir.is_empty() {
                "the delivery's top folder".to_owned()
            } else {
                format!("{dir:?}")
            }
        };
        let (mut head, mut refs, mut commondir) = (false, false, false);
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
                    refs |= key == "refs";
                    let path = if dir.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{dir}/{name}")
                    };
                    if VCS_METADATA_DIRS.iter().any(|vcs| key == *vcs) {
                        return Ok(Some(format!(
                            "{path:?} is a version-control metadata folder, so that tool would \
                             read the delivered config in the extracted folder"
                        )));
                    }
                    subtrees.push((path, entry.id()));
                }
                _ => {}
            }
        }
        if head && (refs || commondir) {
            return Ok(Some(format!(
                "{} holds a git repository's internal files (a HEAD file with refs/, or with \
                 commondir), so git would treat that folder as a repository and read the \
                 delivered config",
                shown(dir)
            )));
        }
        for (path, id) in subtrees {
            let sub = repo.find_tree(id).map_err(|e| e.to_string())?;
            if let Some(found) = check(repo, &sub, &path, budget)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }
    // Same bound as the pre-pay check, which has already refused anything larger.
    let mut budget = collect::MAX_DELIVERY_ENTRIES;
    check(repo, tree, "", &mut budget)
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// True when `path` (resolved through its nearest existing ancestor) is `root` or under it. A
/// first, lexical check: [`prepare_target`] refuses `..` before calling it and re-checks the real
/// path once the folder exists.
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
        assert_eq!(dir, std::fs::canonicalize(&fresh).unwrap());
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
            // #1311 fable L1: no objects/ — GIT_OBJECT_DIRECTORY can stand in for it.
            vec![
                ("HEAD", "ref: x\n"),
                ("refs/x", "x"),
                ("config", "[core]\n"),
            ],
            // #1311 fable M7: another VCS's metadata folder.
            vec![(".hg/hgrc", "[hooks]\npre-status = touch PWNED\n")],
            vec![("sub/.JJ/repo/x", "x")],
            vec![(".svn/entries", "x")],
        ] {
            let (root, home, bind) = seeded("gitlayout", &layout);
            let to = root.join("out");
            let error = extract(&home, &bind, Some(&to)).expect_err("git layout refused");
            assert!(
                error.contains("git repository") || error.contains("version-control"),
                "{error}"
            );
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
            vec![("hg/hgrc", "x"), ("docs/svn.md", "x")],
        ] {
            let (root, home, bind) = seeded("notrepo", &layout);
            let to = root.join("out");
            extract(&home, &bind, Some(&to)).expect("ordinary tree extracts");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    // #1311 fable M2/M6: `..`, `.` and relative targets are refused before anything is created, so
    // a missing component followed by `..` cannot land the tree inside the home.
    #[test]
    fn a_dotdot_or_relative_to_cannot_land_inside_the_home() {
        let (root, home, bind) = seeded("dotdot", &[("CLAUDE.md", "seller says")]);
        let sneaky = root
            .join("zzz")
            .join("..")
            .join("home")
            .join("results")
            .join("job1");
        let error = extract(&home, &bind, Some(&sneaky)).expect_err("refused");
        assert!(error.contains(".."), "{error}");
        assert!(!home.root.join("results").join("job1").exists());
        assert!(!root.join("zzz").exists(), "nothing created");
        let error = extract(&home, &bind, Some(Path::new("rel/out"))).expect_err("relative");
        assert!(error.contains("relative"), "{error}");
        #[cfg(unix)]
        {
            // A symlinked ancestor that resolves into the home is refused, and nothing remains.
            let link = root.join("link");
            std::os::unix::fs::symlink(&home.root, &link).unwrap();
            let via = link.join("results").join("job2");
            assert!(extract(&home, &bind, Some(&via)).is_err());
            assert!(!home.root.join("results").join("job2").exists());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // #1311 fable M1: only an event id reaches the file system.
    #[test]
    fn bind_for_refuses_anything_but_an_event_id() {
        let (root, home, _bind) = seeded("ids", &[("a", "a")]);
        let abs = home.root.join("jobs").join("ab".repeat(32));
        for bad in [
            abs.to_str().unwrap(),
            "../jobs/x",
            "AB",
            "",
            &"g".repeat(64),
        ] {
            assert!(
                bind_for(&home, bad).unwrap_err().contains("not a job id"),
                "{bad:?}"
            );
        }
        assert!(is_event_id(&"ab".repeat(32)));
        let _ = std::fs::remove_dir_all(&root);
    }

    // #1311 fable L7: a max smaller than one character still advances; L6/L8: listings page and
    // name symlinks.
    #[test]
    fn paging_always_advances_and_listings_page() {
        let value = text_slice("f", "é!".as_bytes(), 0, 1);
        assert_eq!(value["content"], json!("é"));
        assert_eq!(value["next_offset"], json!(2));
        let names: Vec<String> = (0..LIST_MAX_ENTRIES + 3)
            .map(|i| format!("f{i:05}"))
            .collect();
        let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x")).collect();
        let (root, home, bind) = seeded("paging", &files);
        let first = read(&home, &bind, None, 0, None).unwrap();
        assert_eq!(first["file_count"], json!(LIST_MAX_ENTRIES + 3));
        assert_eq!(first["files"].as_array().unwrap().len(), LIST_MAX_ENTRIES);
        let next = first["next_offset"].as_u64().unwrap() as usize;
        let rest = read(&home, &bind, None, next, None).unwrap();
        assert_eq!(rest["files"].as_array().unwrap().len(), 3);
        assert!(rest.get("next_offset").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
