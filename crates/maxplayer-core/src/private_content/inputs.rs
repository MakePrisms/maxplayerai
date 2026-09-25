//! Immutable input snapshots. libgit2 object reads, never checkout/hooks/submodule
//! recursion. Only manifest-listed regular blobs reach a fresh job-scoped directory.
use super::{Attachment, Error, MAX_FILE_BYTES, MAX_FILES, MAX_REPO_BYTES, Result};
use git2::{IndexEntry, IndexTime, Oid, Repository};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputFile {
    pub source: PathBuf,
    pub path: String,
}
#[derive(Clone)]
pub struct InputSnapshot {
    pub reference: String,
    pub commit_oid: String,
    pub manifest: Vec<Attachment>,
}
fn git_error(_: git2::Error) -> Error {
    Error("private input Git operation failed")
}
fn io_error(_: std::io::Error) -> Error {
    Error("private input file operation failed")
}
fn private_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io_error)
}
/// Caller owns the fresh bare repository. Snapshot commits contain no task text or
/// user identity. Upload and publication happen only AFTER the complete manifest binds.
pub fn prepare(repo: &Repository, job: &str, files: &[InputFile]) -> Result<InputSnapshot> {
    super::require_hex(job, 32)?;
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(Error("invalid input file count"));
    }
    let mut paths = BTreeSet::new();
    let mut index = git2::Index::new().map_err(git_error)?;
    let mut manifest = Vec::new();
    let mut total = 0u64;
    for file in files {
        super::validate_path(&file.path)?;
        if !paths.insert(file.path.clone()) {
            return Err(Error("duplicate input path"));
        }
        // A regular no-follow handle is opened before metadata/reads. A source may
        // change concurrently, but only the exact bytes we read enter this snapshot.
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let source = options.open(&file.source).map_err(io_error)?;
        let metadata = source.metadata().map_err(io_error)?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(Error("input is not a bounded regular file"));
        }
        let mut bytes = Vec::new();
        source
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(Error("input file too large"));
        }
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or(Error("input total overflow"))?;
        if total > MAX_REPO_BYTES {
            return Err(Error("input snapshot too large"));
        }
        let oid = repo.blob(&bytes).map_err(git_error)?;
        index
            .add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                file_size: bytes.len() as u32,
                id: oid,
                flags: 0,
                flags_extended: 0,
                path: file.path.as_bytes().to_vec(),
            })
            .map_err(git_error)?;
        manifest.push(Attachment {
            repo_id: job.into(),
            commit_oid: String::new(),
            path: file.path.clone(),
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
        });
    }
    // Index insertion can replace a prefix path. Reject such ambiguity explicitly.
    reject_overlaps(&paths)?;
    let tree_id = index.write_tree_to(repo).map_err(git_error)?;
    let tree = repo.find_tree(tree_id).map_err(git_error)?;
    let signature =
        git2::Signature::new("Maxplayer", "job@maxplayer.invalid", &git2::Time::new(0, 0))
            .map_err(git_error)?;
    let reference = format!("refs/heads/input/{}", super::random_id()?);
    let commit_oid = repo
        .commit(
            Some(&reference),
            &signature,
            &signature,
            "Private job input",
            &tree,
            &[],
        )
        .map_err(git_error)?
        .to_string();
    for entry in &mut manifest {
        entry.commit_oid = commit_oid.clone();
    }
    Ok(InputSnapshot {
        reference,
        commit_oid,
        manifest,
    })
}
/// Verify everything before creating the destination. Existing directories are refused:
/// execution/restart must reuse its separately recorded verified snapshot, not overwrite.
pub fn materialize(
    repo: &Repository,
    job: &str,
    manifest: &[Attachment],
    destination: &Path,
) -> Result<()> {
    super::require_hex(job, 32)?;
    if manifest.len() > MAX_FILES {
        return Err(Error("too many input files"));
    }
    let mut entries = Vec::new();
    let mut paths = BTreeSet::new();
    let mut total = 0u64;
    for input in manifest {
        input.validate(job)?;
        if !paths.insert(input.path.clone()) {
            return Err(Error("duplicate materialized path"));
        }
        total = total
            .checked_add(input.size_bytes)
            .ok_or(Error("input total overflow"))?;
        if total > MAX_REPO_BYTES {
            return Err(Error("input snapshot too large"));
        }
        let commit = repo
            .find_commit(Oid::from_str(&input.commit_oid).map_err(git_error)?)
            .map_err(git_error)?;
        let tree = commit.tree().map_err(git_error)?;
        let entry = tree.get_path(Path::new(&input.path)).map_err(git_error)?;
        if ![0o100644, 0o100755].contains(&entry.filemode()) {
            return Err(Error("input is not a regular blob"));
        }
        let (size, kind) = repo
            .odb()
            .map_err(git_error)?
            .read_header(entry.id())
            .map_err(git_error)?;
        if kind != git2::ObjectType::Blob
            || size as u64 != input.size_bytes
            || size as u64 > MAX_FILE_BYTES
        {
            return Err(Error("input blob size mismatch"));
        }
        let blob = repo.find_blob(entry.id()).map_err(git_error)?;
        if blob.size() as u64 != input.size_bytes
            || hex::encode(Sha256::digest(blob.content())) != input.sha256
        {
            return Err(Error("input manifest integrity mismatch"));
        }
        entries.push((input.path.clone(), blob.content().to_vec()));
    }
    reject_overlaps(&paths)?;
    private_dir(destination)?;
    let mut dirs = BTreeSet::new();
    for (path, bytes) in entries {
        let mut parent = destination.to_path_buf();
        let components: Vec<_> = path.split('/').collect();
        for component in &components[..components.len() - 1] {
            parent.push(component);
            if dirs.insert(parent.clone()) {
                private_dir(&parent)?;
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(destination.join(path)).map_err(io_error)?;
        file.write_all(&bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
    }
    Ok(())
}

fn reject_overlaps(paths: &BTreeSet<String>) -> Result<()> {
    for path in paths {
        let mut prefix = path.as_str();
        while let Some((parent, _)) = prefix.rsplit_once('/') {
            if paths.contains(parent) {
                return Err(Error("overlapping input paths"));
            }
            prefix = parent;
        }
    }
    Ok(())
}
