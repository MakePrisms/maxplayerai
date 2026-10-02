//! Common public/private admission checks, run on the candidate before its CAS.
//! Count unique retained objects, not file paths or historical snapshots.
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use maxplayer_private_protocol::{MAX_FILE_BYTES, MAX_OBJECTS, MAX_REPO_BYTES};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub bytes: u64,
    pub file: u64,
    pub objects: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: MAX_REPO_BYTES,
            file: MAX_FILE_BYTES,
            objects: MAX_OBJECTS,
        }
    }
}
fn quota_error() -> Response {
    (StatusCode::PAYLOAD_TOO_LARGE, "repository quota exceeded").into_response()
}
fn invalid() -> Response {
    (StatusCode::BAD_REQUEST, "repository inspection failed").into_response()
}
/// The relay's own inspection machinery failed (spawn, pipe, exit): retryable,
/// unlike [`invalid`] content. Clients stop retrying a 400 (#1096).
fn unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "repository inspection unavailable").into_response()
}
fn spawn(repo: &Path, args: &[&str]) -> Result<tokio::process::Child, Response> {
    let mut cmd = tokio::process::Command::new("git");
    super::transport::harden_git_env(&mut cmd);
    cmd.args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| unavailable())
}

/// Apply the shared object/file/byte policy before publication.
pub async fn enforce(repo: &Path, private: bool) -> Result<(), Response> {
    enforce_with_limits(repo, private, Limits::default()).await
}
pub(super) async fn enforce_with_limits(
    repo: &Path,
    private: bool,
    limits: Limits,
) -> Result<(), Response> {
    tokio::time::timeout(Duration::from_secs(300), inspect(repo, private, limits))
        .await
        .map_err(|_| {
            (
                StatusCode::REQUEST_TIMEOUT,
                "repository inspection timed out",
            )
                .into_response()
        })?
}
async fn inspect(repo: &Path, private: bool, limits: Limits) -> Result<(), Response> {
    let mut child = spawn(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ],
    )?;
    let mut output = BufReader::new(child.stdout.take().ok_or_else(unavailable)?);
    let mut total = 0u64;
    let mut objects = 0usize;
    let mut trees = Vec::new();
    loop {
        let mut line = String::new();
        // Only fixed-size object headers are read, never blob content.
        let n = (&mut output)
            .take(128)
            .read_line(&mut line)
            .await
            .map_err(|_| unavailable())?;
        if n == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err(invalid());
        }
        objects += 1;
        if objects > limits.objects {
            return Err(quota_error());
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 3 || !maxplayer_private_protocol::is_hex(fields[0], 20) {
            return Err(invalid());
        }
        let size: u64 = fields[2].parse().map_err(|_| invalid())?;
        total = total.checked_add(size).ok_or_else(quota_error)?;
        if total > limits.bytes || (fields[1] == "blob" && size > limits.file) {
            return Err(quota_error());
        }
        if private && fields[1] == "tree" {
            trees.push(fields[0].to_owned());
        }
    }
    if !child.wait().await.map_err(|_| unavailable())?.success() {
        return Err(unavailable());
    }
    // Private execution rejects symlinks/submodules as before. This is not a
    // different quota. Read each unique tree once (without recursive expansion).
    for tree in trees {
        let mut child = spawn(repo, &["ls-tree", "-z", &tree])?;
        let mut output = BufReader::new(child.stdout.take().ok_or_else(unavailable)?);
        loop {
            let mut entry = Vec::new();
            // Existing private path policy bounds names; no file-count cap.
            let n = (&mut output)
                .take(8192)
                .read_until(0, &mut entry)
                .await
                .map_err(|_| unavailable())?;
            if n == 0 {
                break;
            }
            if entry.last() != Some(&0) {
                return Err(invalid());
            }
            if entry.starts_with(b"160000 ") || entry.starts_with(b"120000 ") {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "private job snapshots cannot contain symlinks or submodules",
                )
                    .into_response());
            }
        }
        if !child.wait().await.map_err(|_| unavailable())?.success() {
            return Err(unavailable());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(repo: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .current_dir(repo)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    /// The relay's own inspection failing (here: git exits non-zero outside a
    /// repository) is retryable 503, not a client-content 400 (#1096 advisor).
    #[tokio::test]
    async fn inspection_machinery_failure_is_service_unavailable_not_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_repo = dir.path().join("missing");
        std::fs::create_dir(&not_a_repo).unwrap();
        for private in [false, true] {
            let refused = enforce(&not_a_repo, private).await.unwrap_err();
            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[tokio::test]
    async fn public_and_private_accept_long_history_without_commit_cap() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        let mut child = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["fast-import", "--quiet"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        for n in 0..1001 {
            writeln!(
                input,
                "commit refs/heads/main\ncommitter Test <test@example.test> {} +0000\ndata 1\nx\n",
                n + 1
            )
            .unwrap();
        }
        drop(input);
        assert!(child.wait().unwrap().success());
        for private in [false, true] {
            enforce(dir.path(), private).await.unwrap();
        }
    }

    #[tokio::test]
    async fn public_and_private_apply_identical_object_file_and_total_limits() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("file"), vec![0u8; 4096]).unwrap();
        git(dir.path(), &["add", "file"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-qm",
                "test",
            ],
        );
        for private in [false, true] {
            enforce(dir.path(), private).await.unwrap();
            for limits in [
                Limits {
                    bytes: 4000,
                    ..Limits::default()
                },
                Limits {
                    file: 4000,
                    ..Limits::default()
                },
                Limits {
                    objects: 2,
                    ..Limits::default()
                },
            ] {
                assert_eq!(
                    enforce_with_limits(dir.path(), private, limits)
                        .await
                        .unwrap_err()
                        .status(),
                    StatusCode::PAYLOAD_TOO_LARGE
                );
            }
        }
    }
}
