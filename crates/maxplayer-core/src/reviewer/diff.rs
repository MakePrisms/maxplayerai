//! Contextual delivery diffs. The baseline comes only from the authenticated offer.
use super::*;
use std::io::Write;

struct Requests {
    context: Value,
    changes: Vec<Value>,
    file: std::fs::File,
    ranges: Vec<std::ops::Range<usize>>,
    offset: usize,
    model: String,
    deadline: std::time::Instant,
}
impl Requests {
    fn state(&self, changes: &[Value]) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&json!({"format":"maxplayer-contextual-diff-v1", "context":self.context,
            "changes":changes, "scope":"Security screening of changes against the pinned baseline, not unchanged files. Requests are independent; cross-request interactions may be missed."}))
            .map_err(|_| "invalid_input".into())
    }
    fn flush(&mut self) -> Result<(), String> {
        if std::time::Instant::now() >= self.deadline {
            return Err("input_unavailable".into());
        }
        let body = provider_body(&self.state(&self.changes)?, &self.model)?;
        self.file
            .write_all(&body)
            .map_err(|_| "input_unavailable")?;
        self.ranges.push(self.offset..self.offset + body.len());
        self.offset += body.len();
        self.changes.clear();
        Ok(())
    }
    fn flush_rest(&mut self) -> Result<(), String> {
        if !self.changes.is_empty() || self.ranges.is_empty() {
            self.flush()?;
        }
        Ok(())
    }
    fn add(&mut self, value: Value) -> Result<(), String> {
        if std::time::Instant::now() >= self.deadline {
            return Err("input_unavailable".into());
        }
        self.changes.push(value.clone());
        if self.state(&self.changes)?.len() <= 24 * 1024 {
            return Ok(());
        }
        self.changes.pop();
        if !self.changes.is_empty() {
            self.flush()?;
        }
        if self.state(std::slice::from_ref(&value))?.len() <= 24 * 1024 {
            self.changes.push(value);
            return Ok(());
        }
        // Oversized hunks (including single long lines): retain paths, modes and hunk
        // coordinates on every piece. Prefer a line boundary, then a UTF-8 boundary.
        let text = value["patch"].as_str().ok_or("input_too_large")?;
        if text.len() < 2 {
            return Err("input_too_large".into());
        }
        let mut cut = text.len() / 2;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(line) = text[..cut].rfind('\n') {
            if line > 0 {
                cut = line + 1;
            }
        }
        if cut == 0 {
            return Err("input_too_large".into());
        }
        let start = value["hunk_byte_offset"].as_u64().unwrap_or(0);
        for (offset, part) in [(0, &text[..cut]), (cut, &text[cut..])] {
            let mut piece = value.clone();
            piece["patch"] = json!(part);
            piece["hunk_byte_offset"] = json!(start + offset as u64);
            piece["partial_hunk"] = json!(true);
            self.add(piece)?;
        }
        Ok(())
    }
}

pub(super) fn plan(
    path: &Path,
    commit: &str,
    base: Option<&str>,
    task: &str,
    subject: &Subject,
    model: &str,
    deadline: std::time::Instant,
) -> Result<Plan, String> {
    let repo = git2::Repository::open_bare(path).map_err(|_| "input_unavailable")?;
    crate::private_content::repositories::check_object_quotas(&repo)
        .map_err(|_| "input_too_large")?;
    let oid = git2::Oid::from_str(commit).map_err(|_| "invalid_subject")?;
    verify_object(&repo, oid, git2::ObjectType::Commit)?;
    let tip = repo.find_commit(oid).map_err(|_| "input_unavailable")?;
    let tree = tip.tree().map_err(|_| "input_unavailable")?;
    verify_object(&repo, tree.id(), git2::ObjectType::Tree)?;
    let baseline = match base {
        Some(base) => {
            let id = git2::Oid::from_str(base).map_err(|_| "invalid_subject")?;
            verify_object(&repo, id, git2::ObjectType::Commit)?;
            if oid != id
                && !repo
                    .graph_descendant_of(oid, id)
                    .map_err(|_| "input_unavailable")?
            {
                return Err("input_integrity".into());
            }
            let tree = repo
                .find_commit(id)
                .and_then(|c| c.tree())
                .map_err(|_| "input_unavailable")?;
            verify_object(&repo, tree.id(), git2::ObjectType::Tree)?;
            Some(tree)
        }
        None => None, // New artifact job: compare with the empty tree, never a seller-chosen parent.
    };
    let mut options = git2::DiffOptions::new();
    // Do not let repository attributes suppress text from the security review.
    // Binary/non-UTF-8 blobs are rejected explicitly below.
    options.context_lines(3).force_text(true);
    let diff = repo
        .diff_tree_to_tree(baseline.as_ref(), Some(&tree), Some(&mut options))
        .map_err(|_| "input_unavailable")?;
    let mut requests = Requests {
        context: json!({"task":task,"subject":subject,"base_commit":base,"delivered_commit":commit}),
        changes: vec![],
        file: private_spool()?,
        ranges: vec![],
        offset: 0,
        model: model.into(),
        deadline,
    };
    if requests.state(&[])?.len() > 24 * 1024 {
        return Err("input_too_large".into());
    }
    add_changes(&repo, &diff, &mut requests, None, deadline)?;
    if !requests.changes.is_empty() || requests.ranges.is_empty() {
        requests.flush()?;
    }
    Plan::prepared(requests.file, requests.ranges, model, deadline)
}


/// Every delta of `diff` as bounded change items. Symlinks, submodules, binary and non-UTF-8
/// blobs fail closed. `against` names the parent for a team commit (one diff per parent).
fn add_changes(
    repo: &git2::Repository,
    diff: &git2::Diff<'_>,
    requests: &mut Requests,
    against: Option<&str>,
    deadline: std::time::Instant,
) -> Result<(), String> {
    for (i, delta) in diff.deltas().enumerate() {
        if std::time::Instant::now() >= deadline {
            return Err("input_unavailable".into());
        }
        for file in [delta.old_file(), delta.new_file()] {
            if file.id().is_zero() {
                continue;
            }
            if !matches!(
                file.mode(),
                git2::FileMode::Blob | git2::FileMode::BlobExecutable
            ) {
                return Err("unsupported_input".into());
            }
            let path = file
                .path()
                .and_then(Path::to_str)
                .ok_or("unsupported_input")?;
            crate::private_content::validate_path(path).map_err(|_| "unsupported_input")?;
            if path.len() > 4096 {
                return Err("unsupported_input".into());
            }
            let (len, _) = repo
                .odb()
                .and_then(|o| o.read_header(file.id()))
                .map_err(|_| "input_unavailable")?;
            if len > crate::private_content::MAX_FILE_BYTES as usize {
                return Err("input_too_large".into());
            }
            verify_object(repo, file.id(), git2::ObjectType::Blob)?;
            let blob = repo.find_blob(file.id()).map_err(|_| "input_unavailable")?;
            if blob.is_binary()
                || blob.content().contains(&0)
                || std::str::from_utf8(blob.content()).is_err()
            {
                return Err("unsupported_input".into());
            }
        }
        let mut header = json!({"status":format!("{:?}",delta.status()),"old_path":delta.old_file().path().and_then(Path::to_str),"new_path":delta.new_file().path().and_then(Path::to_str),"old_mode":i32::from(delta.old_file().mode()),"new_mode":i32::from(delta.new_file().mode())});
        if let Some(parent) = against {
            header["against_parent"] = json!(parent);
        }
        let patch = git2::Patch::from_diff(diff, i).map_err(|_| "input_unavailable")?;
        if let Some(patch) = patch {
            if patch.num_hunks() == 0 {
                requests.add(header.clone())?;
            }
            for h in 0..patch.num_hunks() {
                let (hunk, count) = patch.hunk(h).map_err(|_| "input_unavailable")?;
                let mut text = String::new();
                for n in 0..count {
                    let line = patch.line_in_hunk(h, n).map_err(|_| "input_unavailable")?;
                    if matches!(line.origin(), '+' | '-' | ' ') {
                        text.push(line.origin());
                    }
                    text.push_str(
                        std::str::from_utf8(line.content()).map_err(|_| "unsupported_input")?,
                    );
                }
                let mut change = header.clone();
                change["hunk"] =
                    json!(std::str::from_utf8(hunk.header()).map_err(|_| "unsupported_input")?);
                change["patch"] = json!(text);
                requests.add(change)?;
            }
        } else {
            requests.add(header)?;
        }
    }
    Ok(())
}

/// A team commit (docs/specs/maxplayer-team.md §5): raw header and message first, then the
/// commit's tree against EVERY parent (empty tree for a root, or for a parent missing at the
/// shallow boundary), so a merge is never classified against one chosen side only.
pub(super) fn team_plan(
    path: &Path,
    commit: &str,
    subject: &Subject,
    model: &str,
    deadline: std::time::Instant,
) -> Result<Plan, String> {
    let repo = git2::Repository::open_bare(path).map_err(|_| "input_unavailable")?;
    crate::private_content::repositories::check_object_quotas(&repo)
        .map_err(|_| "input_too_large")?;
    let oid = git2::Oid::from_str(commit).map_err(|_| "invalid_subject")?;
    verify_object(&repo, oid, git2::ObjectType::Commit)?;
    let tip = repo.find_commit(oid).map_err(|_| "input_unavailable")?;
    let tree = tip.tree().map_err(|_| "input_unavailable")?;
    verify_object(&repo, tree.id(), git2::ObjectType::Tree)?;
    let header = std::str::from_utf8(tip.raw_header_bytes()).map_err(|_| "unsupported_input")?;
    let message = std::str::from_utf8(tip.message_raw_bytes()).map_err(|_| "unsupported_input")?;
    if header.contains('\0') || message.contains('\0') {
        return Err("unsupported_input".into());
    }
    let mut requests = Requests {
        context: json!({"subject":subject,"team_commit":commit}),
        changes: vec![],
        file: private_spool()?,
        ranges: vec![],
        offset: 0,
        model: model.into(),
        deadline,
    };
    if requests.state(&[])?.len() > 24 * 1024 {
        return Err("input_too_large".into());
    }
    requests.add(json!({"commit_header_and_message":true,"patch":format!("{header}\n{message}")}))?;
    let mut options = git2::DiffOptions::new();
    options.context_lines(3).force_text(true);
    let mut bases: Vec<(Option<git2::Tree<'_>>, Option<String>)> = Vec::new();
    for parent in tip.parent_ids() {
        // A parent outside the fetched depth is absent: review against the empty tree.
        let base = match repo.find_commit(parent) {
            Ok(p) => {
                verify_object(&repo, parent, git2::ObjectType::Commit)?;
                let t = p.tree().map_err(|_| "input_unavailable")?;
                verify_object(&repo, t.id(), git2::ObjectType::Tree)?;
                Some(t)
            }
            Err(_) => None,
        };
        bases.push((base, Some(parent.to_string())));
    }
    if bases.is_empty() {
        bases.push((None, None));
    }
    for (base, parent) in &bases {
        let diff = repo
            .diff_tree_to_tree(base.as_ref(), Some(&tree), Some(&mut options))
            .map_err(|_| "input_unavailable")?;
        add_changes(&repo, &diff, &mut requests, parent.as_deref(), deadline)?;
    }
    requests.flush_rest()?;
    Plan::prepared(requests.file, requests.ranges, model, deadline)
}

pub(super) fn relay_plan(
    relay: &str,
    url: &str,
    branch: &str,
    commit: &str,
    base: Option<&str>,
    task: &str,
    subject: &Subject,
    model: &str,
    keys: &Keys,
    deadline: std::time::Instant,
) -> Result<Plan, String> {
    validate_git_destination(relay, url)?;
    let source = review_source_ref(branch)?;
    let scratch = ReviewScratch::new()?;
    let repo = git2::Repository::init_bare(&scratch.0).map_err(|_| "input_unavailable")?;
    let header = crate::git_transport::nip98_authorization_header_with_keys(url, keys, None, None)
        .map_err(|_| "input_unavailable")?;
    crate::git_transport::fetch_review_ref(
        &repo,
        url,
        &format!("+{source}:refs/review/delivery"),
        header,
        MAX_GIT_FETCH_BYTES,
        deadline,
    )
    .map_err(|_| "input_unavailable")?;
    verify_fetched_tip(&repo, commit)?;
    plan(&scratch.0, commit, base, task, subject, model, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn commit(
        repo: &git2::Repository,
        files: &[(&str, &[u8], i32)],
        parent: Option<git2::Oid>,
    ) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        for (name, bytes, mode) in files {
            builder
                .insert(name, repo.blob(bytes).unwrap(), *mode)
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let sig = git2::Signature::now("test", "test@example.test").unwrap();
        let parent = parent.map(|p| repo.find_commit(p).unwrap());
        repo.commit(
            None,
            &sig,
            &sig,
            "test",
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    }
    fn states(plan: &Plan) -> Vec<Value> {
        (0..plan.batch_count())
            .map(|i| {
                let body: Value = serde_json::from_slice(&plan.body(i).unwrap()).unwrap();
                serde_json::from_str(body["state"].as_str().unwrap()).unwrap()
            })
            .collect()
    }
    #[test]
    fn diff_includes_add_edit_delete_modes_but_not_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let base = commit(
            &repo,
            &[
                ("edit", b"old\n", 0o100644),
                ("delete", b"removed\n", 0o100644),
                ("unchanged", b"UNTOUCHED_CANARY\n", 0o100644),
                ("mode", b"mode\n", 0o100644),
            ],
            None,
        );
        let tip = commit(
            &repo,
            &[
                ("edit", b"new\n", 0o100644),
                ("add", b"added\n", 0o100644),
                ("unchanged", b"UNTOUCHED_CANARY\n", 0o100644),
                ("mode", b"mode\n", 0o100755),
            ],
            Some(base),
        );
        let subject = super::super::tests::subject();
        let plan = plan(
            dir.path(),
            &tip.to_string(),
            Some(&base.to_string()),
            "TASK",
            &subject,
            "model",
            std::time::Instant::now() + WINDOW,
        )
        .unwrap();
        assert_eq!(plan.batch_count(), 1);
        let states = states(&plan);
        let text = states[0].to_string();
        for expected in [
            "+new", "-old", "+added", "-removed", "Deleted", "Added", "old_mode", "TASK",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!text.contains("UNTOUCHED_CANARY"));
        assert!(!text.contains("\"unchanged\""));
        assert_eq!(states[0]["context"]["base_commit"], base.to_string());
    }
    #[test]
    fn diff_uses_pinned_base_not_last_parent_and_rejects_missing_or_unrelated_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let base = commit(&repo, &[("file", b"original\n", 0o100644)], None);
        let mid = commit(
            &repo,
            &[("file", b"earlier seller change\n", 0o100644)],
            Some(base),
        );
        let tip = commit(
            &repo,
            &[
                ("file", b"earlier seller change\n", 0o100644),
                ("second", b"later\n", 0o100644),
            ],
            Some(mid),
        );
        let subject = super::super::tests::subject();
        let run = |base: Option<&str>| {
            plan(
                dir.path(),
                &tip.to_string(),
                base,
                "task",
                &subject,
                "model",
                std::time::Instant::now() + WINDOW,
            )
        };
        let p = run(Some(&base.to_string())).unwrap();
        assert!(
            serde_json::to_string(&states(&p))
                .unwrap()
                .contains("-original")
        );
        assert!(run(Some(&"f".repeat(40))).is_err());
        let unrelated = commit(&repo, &[("other", b"unrelated", 0o100644)], None);
        assert!(run(Some(&unrelated.to_string())).is_err());
        let empty = run(None).unwrap();
        assert!(
            serde_json::to_string(&states(&empty))
                .unwrap()
                .contains("+earlier seller change")
        );
        assert_ne!(p.digest, empty.digest);
    }
    #[test]
    fn large_hunk_keeps_task_paths_coordinates_and_every_byte_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let text = "🦀 quoted \\\" text\n".repeat(9000);
        let tip = commit(&repo, &[("large.txt", text.as_bytes(), 0o100644)], None);
        let p = plan(
            dir.path(),
            &tip.to_string(),
            None,
            "TASK_IN_EVERY_REQUEST",
            &super::super::tests::subject(),
            "model",
            std::time::Instant::now() + WINDOW,
        )
        .unwrap();
        assert!(p.batch_count() > 1);
        let mut reconstructed = String::new();
        for state in states(&p) {
            assert_eq!(state["context"]["task"], "TASK_IN_EVERY_REQUEST");
            assert!(serde_json::to_vec(&state).unwrap().len() <= 24 * 1024);
            for change in state["changes"].as_array().unwrap() {
                assert_eq!(change["new_path"], "large.txt");
                assert!(change["hunk"].as_str().unwrap().starts_with("@@"));
                reconstructed.push_str(change["patch"].as_str().unwrap());
            }
        }
        let expected: String = text
            .split_inclusive('\n')
            .map(|line| format!("+{line}"))
            .collect();
        assert_eq!(reconstructed, expected);
    }
    #[test]
    fn changed_binary_fails_but_unchanged_binary_does_not_enter_diff() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let base = commit(&repo, &[("binary", b"\0bytes", 0o100644)], None);
        let tip = commit(
            &repo,
            &[
                ("binary", b"\0bytes", 0o100644),
                ("text", b"ok\n", 0o100644),
            ],
            Some(base),
        );
        let subject = super::super::tests::subject();
        assert!(
            plan(
                dir.path(),
                &tip.to_string(),
                Some(&base.to_string()),
                "task",
                &subject,
                "model",
                std::time::Instant::now() + WINDOW
            )
            .is_ok()
        );
        assert!(
            plan(
                dir.path(),
                &tip.to_string(),
                None,
                "task",
                &subject,
                "model",
                std::time::Instant::now() + WINDOW
            )
            .is_err()
        );
    }
    #[test]
    fn long_line_splits_without_losing_utf8_or_task_and_oversized_context_fails() {
        let text = "🦀\\\"".repeat(12000);
        let mut requests = Requests {
            context: json!({"task":"TASK"}),
            changes: vec![],
            file: private_spool().unwrap(),
            ranges: vec![],
            offset: 0,
            model: "model".into(),
            deadline: std::time::Instant::now() + WINDOW,
        };
        requests
            .add(json!({"new_path":"long.txt", "hunk":"@@ -0,0 +1 @@", "patch":text}))
            .unwrap();
        requests.flush().unwrap();
        let p = Plan::prepared(
            requests.file,
            requests.ranges,
            "model",
            std::time::Instant::now() + WINDOW,
        )
        .unwrap();
        let mut result = String::new();
        for state in states(&p) {
            assert_eq!(state["context"]["task"], "TASK");
            for piece in state["changes"].as_array().unwrap() {
                assert_eq!(
                    piece["hunk_byte_offset"].as_u64().unwrap(),
                    result.len() as u64
                );
                result.push_str(piece["patch"].as_str().unwrap());
            }
        }
        assert_eq!(result, text);
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let tip = commit(&repo, &[("file", b"hello", 0o100644)], None);
        assert!(
            plan(
                dir.path(),
                &tip.to_string(),
                None,
                &"x".repeat(30000),
                &super::super::tests::subject(),
                "model",
                std::time::Instant::now() + WINDOW
            )
            .is_err()
        );
    }

    fn team(repo: &git2::Repository, dir: &Path, tip: git2::Oid) -> Result<Vec<Value>, String> {
        let _ = repo;
        team_plan(dir, &tip.to_string(), &super::super::tests::subject(), "model",
            std::time::Instant::now() + WINDOW).map(|p| states(&p))
    }
    fn merge(repo: &git2::Repository, files: &[(&str, &[u8])], parents: &[git2::Oid], msg: &str) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        for (name, bytes) in files {
            builder.insert(name, repo.blob(bytes).unwrap(), 0o100644).unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let sig = git2::Signature::now("test", "test@example.test").unwrap();
        let parents: Vec<_> = parents.iter().map(|p| repo.find_commit(*p).unwrap()).collect();
        repo.commit(None, &sig, &sig, msg, &tree, &parents.iter().collect::<Vec<_>>()).unwrap()
    }
    #[test]
    fn team_merge_is_diffed_against_every_parent_with_message_first() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let base = merge(&repo, &[("f", b"base\n")], &[], "base");
        let main = merge(&repo, &[("f", b"main change\n")], &[base], "main");
        let side = merge(&repo, &[("f", b"base\n"), ("x", b"side canary\n")], &[base], "side");
        // Foxtrot: the side branch is the FIRST parent, so a first-parent-only review would
        // see only `main change` and miss nothing of side; a second-parent-only one would miss it.
        let tip = merge(&repo, &[("f", b"main change\n"), ("x", b"side canary\n")], &[side, main], "MERGE_MSG_CANARY");
        let states = team(&repo, dir.path(), tip).unwrap();
        let text = serde_json::to_string(&states).unwrap();
        let first = &states[0]["changes"][0];
        assert_eq!(first["commit_header_and_message"], true);
        assert!(first["patch"].as_str().unwrap().contains("MERGE_MSG_CANARY"));
        assert!(first["patch"].as_str().unwrap().contains(&format!("parent {main}")));
        for parent in [side, main] {
            assert!(text.contains(&format!("\"against_parent\":\"{parent}\"")), "{text}");
        }
        assert!(text.contains("+side canary"), "change against the second parent: {text}");
        assert!(text.contains("+main change"), "change against the first parent: {text}");
    }
    #[test]
    fn team_commit_message_over_24k_is_split_not_errored_and_root_is_full_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let message = "long message line\n".repeat(2500);
        let tip = merge(&repo, &[("file", b"ROOT_CONTENT\n")], &[], &message);
        let states = team(&repo, dir.path(), tip).unwrap();
        assert!(states.len() > 1);
        let text = serde_json::to_string(&states).unwrap();
        assert!(text.contains("+ROOT_CONTENT"));
        let joined: String = states.iter().flat_map(|s| s["changes"].as_array().unwrap().clone())
            .filter(|c| c["commit_header_and_message"] == true)
            .map(|c| c["patch"].as_str().unwrap().to_owned()).collect();
        assert!(joined.ends_with(&message));
    }
    #[test]
    fn team_parent_missing_at_shallow_boundary_is_reviewed_as_full_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("file", repo.blob(b"UNSEEN_PARENT_CONTENT\n").unwrap(), 0o100644).unwrap();
        let tree = builder.write().unwrap();
        let raw = format!("tree {tree}\nparent {}\nauthor a <a@x> 1 +0000\ncommitter a <a@x> 1 +0000\n\nshallow\n", "f".repeat(40));
        let tip = repo.odb().unwrap().write(git2::ObjectType::Commit, raw.as_bytes()).unwrap();
        let text = serde_json::to_string(&team(&repo, dir.path(), tip).unwrap()).unwrap();
        assert!(text.contains("+UNSEEN_PARENT_CONTENT"), "{text}");
    }
    #[test]
    fn team_commit_refuses_binary_and_nul_message() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let tip = merge(&repo, &[("bin", b"\0bytes")], &[], "binary");
        assert_eq!(team(&repo, dir.path(), tip).unwrap_err(), "unsupported_input");
    }

    #[tokio::test]
    async fn contextual_diff_failure_never_approves_partial_review() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let text = "line\n".repeat(9000);
        let tip = commit(&repo, &[("file", text.as_bytes(), 0o100644)], None);
        let subject = super::super::tests::subject();
        let p = plan(
            dir.path(),
            &tip.to_string(),
            None,
            "task",
            &subject,
            "model",
            std::time::Instant::now() + WINDOW,
        )
        .unwrap();
        let count = p.batch_count();
        assert!(count > 1 && count <= 4);
        let mut responses = vec![(200, None, super::super::tests::response(0.01)); count];
        responses[count - 1] = (400, None, vec![]);
        let (provider, _, server) = super::super::tests::http_provider(responses).await;
        assert_eq!(
            p.classify(&provider, &subject, tokio::time::Instant::now() + WINDOW)
                .await
                .unwrap_err(),
            "provider_rejected"
        );
        server.await.unwrap();
    }
}
