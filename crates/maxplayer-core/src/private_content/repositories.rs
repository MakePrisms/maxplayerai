//! Private per-job Git legs. All network destinations derive from signed offer identity
//! and trusted deployment policy; manifests bind exact objects before execution.
use super::{Error, PreparedContent, Result, wire::HostPolicy};
use git2::{ObjectType, Oid, Repository};
use std::{collections::BTreeSet, path::Path};

/// Client preflight of every retained object, including imported bases/history.
/// Server quarantine/CAS quota checks remain authoritative against concurrent writes.
pub fn check_objects(repo: &Repository) -> Result<()> {
    let odb = repo
        .odb()
        .map_err(|_| Error("input object database unavailable"))?;
    let mut ids = Vec::new();
    let mut overflow = false;
    odb.foreach(|oid| {
        if ids.len() >= 100_000 {
            overflow = true;
            return false;
        }
        ids.push(*oid);
        true
    })
    .map_err(|_| Error("input object enumeration refused"))?;
    if overflow {
        return Err(Error("input object count exceeded"));
    }
    let mut bytes = 0u64;
    let mut commits = 0usize;
    for id in ids {
        let (size, kind) = odb
            .read_header(id)
            .map_err(|_| Error("input object header unavailable"))?;
        bytes = bytes
            .checked_add(size as u64)
            .ok_or(Error("input object quota overflow"))?;
        if bytes > super::MAX_REPO_BYTES
            || (kind == ObjectType::Blob && size as u64 > super::MAX_FILE_BYTES)
        {
            return Err(Error("input object quota exceeded"));
        }
        if kind == ObjectType::Commit {
            commits += 1;
            if commits > 1000 {
                return Err(Error("input history too large"));
            }
            let commit = repo
                .find_commit(id)
                .map_err(|_| Error("invalid input commit"))?;
            let tree = commit.tree().map_err(|_| Error("invalid input tree"))?;
            let mut files = 0usize;
            let mut invalid = false;
            tree.walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
                let Some(name) = entry.name() else {
                    invalid = true;
                    return git2::TreeWalkResult::Abort;
                };
                if super::validate_path(&format!("{prefix}{name}")).is_err() {
                    invalid = true;
                    return git2::TreeWalkResult::Abort;
                }
                match entry.filemode() {
                    0o040000 => {}
                    0o100644 | 0o100755 => {
                        files += 1;
                    }
                    _ => {
                        invalid = true;
                        return git2::TreeWalkResult::Abort;
                    }
                }
                if files > super::MAX_FILES {
                    invalid = true;
                    return git2::TreeWalkResult::Abort;
                }
                git2::TreeWalkResult::Ok
            })
            .map_err(|_| Error("input snapshot refused"))?;
            if invalid {
                return Err(Error("input snapshot refused"));
            }
        }
    }
    Ok(())
}

/// Every manifest pin in the committed task participates in pre-claim staging,
/// including an optional imported contribution artifact.
pub fn input_manifest(task: &PreparedContent) -> Result<Vec<super::Attachment>> {
    let body = task.body();
    let mut entries = body.attachments.clone();
    if let Some(input) = body.contribution.as_ref().and_then(|c| c.input.as_ref()) {
        if let Some(existing) = entries.iter().find(|entry| entry.path == input.path) {
            if existing != input {
                return Err(Error("conflicting contribution input manifest"));
            }
        } else {
            entries.push(input.clone());
        }
    }
    if entries.len() > super::MAX_FILES {
        return Err(Error("too many pinned inputs"));
    }
    Ok(entries)
}

/// Fetch only the pinned commit objects into a caller-created bare repository. The
/// caller authenticates `task` against its signed offer before calling this function.
/// Destination is fresh; no existing execution tree is overwritten on retry.
pub fn fetch_inputs(
    repo: &Repository,
    task: &PreparedContent,
    buyer: &str,
    host: &HostPolicy,
    authorization: &str,
    destination: &Path,
) -> Result<()> {
    if !repo.is_bare() {
        return Err(Error("private fetch requires isolated bare staging"));
    }
    let body = task.body();
    if body.kind != super::ContentType::Task {
        return Err(Error("not an input task"));
    }
    let remote = host.job_repo(buyer, &body.job_id)?;
    let manifest = input_manifest(task)?;
    let pins: BTreeSet<_> = manifest
        .iter()
        .map(|entry| entry.commit_oid.as_str())
        .collect();
    if pins.is_empty() {
        return Err(Error("no input pins"));
    }
    let refs: Vec<_> = pins.into_iter().collect();
    crate::git_transport::fetch_private_objects(repo, &remote, &refs, authorization)
        .map_err(|_| Error("private input fetch unavailable"))?;
    check_objects(repo)?;
    super::inputs::materialize(repo, &body.job_id, &manifest, destination)
}

/// Upload exactly the snapshot committed in the immutable encrypted task manifest.
pub fn upload_inputs(
    repo: &Repository,
    task: &PreparedContent,
    snapshot: &super::inputs::InputSnapshot,
    host: &HostPolicy,
    mint: crate::git_transport::AuthMinter,
) -> Result<()> {
    let body = task.body();
    if body.kind != super::ContentType::Task
        || body.attachments != snapshot.manifest
        || body
            .attachments
            .iter()
            .any(|entry| entry.commit_oid != snapshot.commit_oid)
    {
        return Err(Error("input snapshot differs from task commitment"));
    }
    let oid =
        Oid::from_str(&snapshot.commit_oid).map_err(|_| Error("invalid input snapshot pin"))?;
    let reference = repo
        .find_reference(&snapshot.reference)
        .map_err(|_| Error("input snapshot ref missing"))?;
    if reference.target() != Some(oid) {
        return Err(Error("input snapshot ref moved"));
    }
    check_objects(repo)?;
    let remote = host.job_repo(&body.author, &body.job_id)?;
    crate::git_transport::push_private_input(
        repo,
        &remote,
        &snapshot.reference,
        &snapshot.commit_oid,
        mint,
    )
    .map_err(|_| Error("private input upload unavailable"))?;
    Ok(())
}
