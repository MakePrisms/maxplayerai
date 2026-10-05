//! Import only the pinned commit's reachable set, never the source's packs/refs.
use super::*;
use git2::RepositoryOpenFlags;

/// Stage locally if possible, otherwise retain the existing download path. A
/// failed import never leaves partial objects beside the downloaded pack.
pub fn prepare_private_input_base(
    repo: &Repository,
    local: Option<&Path>,
    remote: &str,
    commit: &str,
    mint: Option<AuthMinter>,
) -> Result<(), TransportError> {
    let reason = match local {
        None => "no local path supplied",
        Some(path) => match import(repo, path, commit) {
            Ok(()) => return Ok(()),
            Err(reason) => reason,
        },
    };
    crate::opline!("buyer base: {reason}; downloading target_repo_url");
    fetch_private_input_base(repo, remote, commit, mint)
}

fn import(destination: &Repository, path: &Path, commit: &str) -> Result<(), &'static str> {
    if !path.is_absolute() {
        return Err("local path is not absolute");
    }
    // NO_SEARCH prevents a non-repo directory inside a checkout from silently
    // selecting its ancestor. libgit2 opens bare repos and linked worktrees too.
    let source = Repository::open_ext(path, RepositoryOpenFlags::NO_SEARCH, &[] as &[&Path])
        .map_err(|_| "local repository cannot be opened")?;
    let oid = Oid::from_str(commit).map_err(|_| "local base pin is invalid")?;
    source
        .find_commit(oid)
        .map_err(|_| "pinned commit is absent locally")?;
    build(destination, &source, oid).map_err(|_| "local base could not be staged")
}

fn build(
    destination: &Repository,
    source: &Repository,
    oid: Oid,
) -> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir_in(destination.path())?;
    // The view has no source config, refs, replace refs or shallow boundary.
    // Borrow ONLY the object database. Missing ancestors therefore fail rather
    // than producing a successful but incomplete shallow import.
    let view = Repository::init_bare(scratch.path().join("view"))?;
    view.set_odb(&source.odb()?)?;
    // libgit2 1.8.1 honours windowMemory, but ignores pack.window/depth.
    // A 1 MiB search window measured ~10s / 77.5 MB on agicash here, versus
    // ~28s / 73.8 MB at the defaults. This affects only this temporary view.
    view.config()?.set_i64("pack.windowMemory", 1024 * 1024)?;
    let packed = Repository::init_bare(scratch.path().join("packed"))?;
    let mut walk = view.revwalk()?;
    walk.push(oid)?;
    let mut builder = view.packbuilder()?;
    builder.set_threads(0);
    builder.insert_walk(&mut walk)?;
    let odb = packed.odb()?;
    let mut writer = odb.packwriter()?;
    let mut bytes = 0usize;
    let mut write_error = None;
    builder.foreach(|chunk| {
        bytes = bytes.saturating_add(chunk.len());
        if bytes > crate::private_content::MAX_GIT_TRANSFER_BYTES {
            return false;
        }
        if let Err(error) = writer.write_all(chunk) {
            write_error = Some(error);
            return false;
        }
        true
    })?;
    if let Some(error) = write_error {
        return Err(error.into());
    }
    writer.commit()?;
    // Atomically replace the empty pack directory. No loose objects, alternates
    // or unrelated source packs are copied. All validation still runs afterward.
    std::fs::rename(
        packed.path().join("objects/pack"),
        destination.path().join("objects/pack"),
    )?;
    Ok(())
}
