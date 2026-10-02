//! Private per-job Git legs. All network destinations derive from signed offer identity
//! and trusted deployment policy; manifests bind exact objects before execution.
use super::{Error, PreparedContent, Result, wire::HostPolicy};
use git2::{ObjectType, Oid, Repository};
use std::{collections::BTreeSet, path::Path};

/// Client preflight of every retained object, including imported bases/history.
/// Server quarantine/CAS quota checks remain authoritative against concurrent writes.
pub fn check_objects(repo: &Repository) -> Result<()> {
    inspect_objects(repo, true, Usage::default()).map(|_| ())
}
/// Like [`check_objects`], counting `used` from repositories staged for the same
/// job: one job repository receives them all, so the quota is their sum.
pub fn check_objects_after(repo: &Repository, used: Usage) -> Result<Usage> {
    inspect_objects(repo, true, used)
}
/// Shared quota-only check; public history need not obey private path/type policy.
pub fn check_object_quotas(repo: &Repository) -> Result<()> {
    inspect_objects(repo, false, Usage::default()).map(|_| ())
}
/// Objects and uncompressed bytes already counted against one job's quota.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    objects: usize,
    bytes: u64,
}
fn inspect_objects(repo: &Repository, private: bool, used: Usage) -> Result<Usage> {
    let odb = repo
        .odb()
        .map_err(|_| Error("input object database unavailable"))?;
    let mut ids = Vec::new();
    let mut overflow = false;
    odb.foreach(|oid| {
        if used.objects + ids.len() >= super::MAX_OBJECTS {
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
    let objects = used.objects + ids.len();
    let mut bytes = used.bytes;
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
        // Inspect each unique tree once, not every historical snapshot. Shared
        // trees in long histories must not multiply validation work.
        if private && kind == ObjectType::Tree {
            let tree = repo.find_tree(id).map_err(|_| Error("invalid input tree"))?;
            for entry in tree.iter() {
                let name = entry.name().ok_or(Error("invalid input path"))?;
                super::validate_path(name)?;
                if !matches!(entry.filemode(), 0o040000 | 0o100644 | 0o100755) {
                    return Err(Error(
                        "input snapshot refused: private job repositories cannot contain symlinks or submodules",
                    ));
                }
            }
        }
    }
    Ok(Usage { objects, bytes })
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
    .map_err(|e| upload_error(&e, "private input upload unavailable"))?;
    Ok(())
}

/// Name a permanent relay refusal of a private input upload in fixed words. The
/// server's text is matched, never echoed; anything else keeps `fallback`.
pub(super) fn upload_error(
    error: &crate::git_transport::TransportError,
    fallback: &'static str,
) -> Error {
    if !crate::git_transport::is_permanent_refusal(error) {
        return Error(fallback);
    }
    if error.to_string().contains("cannot contain symlinks or submodules") {
        Error("relay refused the private input: private job repositories cannot contain symlinks or submodules")
    } else if crate::git_transport::reported_http_status(error) == Some(413) {
        Error("relay refused the private input: it exceeds the private job repository limits")
    } else {
        Error("relay refused the private input upload (permanent HTTP refusal; not retried)")
    }
}

#[cfg(test)]
mod quota_tests {
    use super::*;

    #[test]
    fn staged_repositories_share_one_job_quota() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        repo.blob(b"one").unwrap();
        repo.blob(b"two").unwrap();
        let alone = check_objects_after(&repo, Usage::default()).unwrap();
        assert_eq!((alone.objects, alone.bytes), (2, 6));
        let near_count = Usage { objects: super::super::MAX_OBJECTS - 1, bytes: 0 };
        assert!(check_objects_after(&repo, near_count).is_err());
        let near_bytes = Usage { objects: 0, bytes: super::super::MAX_REPO_BYTES - 5 };
        assert!(check_objects_after(&repo, near_bytes).is_err());
    }
}
