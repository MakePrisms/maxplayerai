//! Two individually admissible uploads race against one parent. The losing upload
//! cannot publish its refs, and retrying on the winner cannot reset cumulative quota.
use super::{
    cas_publish::{self, CasError, ParentState, PublishLimits},
    manifest::pointer_key,
    private_jobs::enforce_quota,
    store::GitStore,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};
fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn snapshot(label: u8) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    let reference = format!("refs/heads/input/{}", hex::encode([label; 32]));
    git(dir.path(), &["symbolic-ref", "HEAD", &reference]);
    for i in 0..6u8 {
        std::fs::write(
            dir.path().join(format!("f{i}")),
            vec![label + i; 9 * 1024 * 1024],
        )
        .unwrap();
    }
    git(dir.path(), &["add", "--all"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "input",
        ],
    );
    dir
}
#[tokio::test]
async fn private_quota_cas_race_keeps_one_snapshot_and_rechecks_the_losers_retry() {
    let objects = Arc::new(Mutex::new(BTreeMap::<String, (String, Vec<u8>)>::new()));
    let app = Router::new().fallback({
        let objects = objects.clone();
        move |request: Request<Body>| {
            let objects = objects.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = to_bytes(body, 128 * 1024 * 1024).await.unwrap();
                let key = parts
                    .uri
                    .path()
                    .strip_prefix("/test/")
                    .unwrap_or("")
                    .to_owned();
                let mut objects = objects.lock().unwrap();
                if parts.method == Method::PUT {
                    let previous = objects.get(&key);
                    let if_none = parts
                        .headers
                        .get("if-none-match")
                        .and_then(|h| h.to_str().ok());
                    let if_match = parts.headers.get("if-match").and_then(|h| h.to_str().ok());
                    if (if_none == Some("*") && previous.is_some())
                        || if_match.is_some_and(|expected| {
                            previous.is_none_or(|(etag, _)| etag != expected)
                        })
                    {
                        return (StatusCode::PRECONDITION_FAILED, "precondition").into_response();
                    }
                    let etag = format!("\"{}\"", hex::encode(Sha256::digest(&bytes)));
                    objects.insert(key, (etag.clone(), bytes.to_vec()));
                    return Response::builder()
                        .status(200)
                        .header("etag", etag)
                        .body(Body::empty())
                        .unwrap();
                }
                match objects.get(&key) {
                    Some((etag, bytes)) => Response::builder()
                        .status(200)
                        .header("etag", etag)
                        .header("content-length", bytes.len())
                        .body(Body::from(if parts.method == Method::HEAD {
                            Vec::new()
                        } else {
                            bytes.clone()
                        }))
                        .unwrap(),
                    None => (StatusCode::NOT_FOUND, "absent").into_response(),
                }
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let store = GitStore::new(&endpoint, "test-key", "test-secret", "test", "test").unwrap();
    let tenant = buzz_core::TenantContext::resolved(
        buzz_core::CommunityId::from_uuid(uuid::Uuid::new_v4()),
        "test.example.invalid",
    );
    let buyer = "11".repeat(32);
    let job = "22".repeat(32);
    let a = snapshot(30);
    let b = snapshot(50);
    enforce_quota(a.path()).await.unwrap();
    enforce_quota(b.path()).await.unwrap();
    let parent = ParentState::fresh();
    let limits = PublishLimits {
        parent_hydrated_bytes: 0,
        max_pack_bytes: 100 * 1024 * 1024,
        max_repo_bytes: 100 * 1024 * 1024,
    };
    let (ra, rb) = tokio::join!(
        cas_publish::cas_publish(&store, &tenant, a.path(), &buyer, &job, &parent, limits),
        cas_publish::cas_publish(&store, &tenant, b.path(), &buyer, &job, &parent, limits)
    );
    let (winner, loser, success) = match (ra, rb) {
        (Ok(success), Err(CasError::Conflict { .. })) => (&a, &b, success),
        (Err(CasError::Conflict { .. }), Ok(success)) => (&b, &a, success),
        other => panic!("expected exactly one published snapshot: {other:?}"),
    };
    assert_eq!(success.manifest.refs.len(), 1);
    let pointer = pointer_key(tenant.community(), &buyer, &job);
    let before = store.get_pointer(&pointer).await.unwrap().unwrap();
    // Model the client's new hydrate/re-push: retain the winner's object graph in
    // addition to the losing snapshot. 54+54 MiB cannot fit the 100 MiB repo cap.
    git(
        loser.path(),
        &[
            "fetch",
            winner.path().to_str().unwrap(),
            "refs/heads/*:refs/heads/*",
        ],
    );
    assert_eq!(
        enforce_quota(loser.path()).await.unwrap_err().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let after = store.get_pointer(&pointer).await.unwrap().unwrap();
    assert_eq!(before, after, "quota refusal published a new pointer");
    assert_eq!(
        after.1.as_ref(),
        success
            .manifest_key
            .strip_prefix("manifests/")
            .unwrap()
            .as_bytes()
    );
    server.abort();
}
