//! Real Git HTTP router + PostgreSQL ACL + Redis replay guard. Object storage is
//! a local in-memory fixture with the git store's write rules (create-only writes
//! and `If-Match` pointer CAS), so a forbidden request must never touch it.
use axum::{
    Router,
    body::Body,
    http::{Method, Request},
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::Engine;
use nostr::JsonUtil;
use nostr::{Event, EventBuilder, Keys, Kind, Tag};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

type Objects = Arc<RwLock<BTreeMap<String, Vec<u8>>>>;

fn token(keys: &Keys, url: &str, method: &str, body: Option<&[u8]>) -> String {
    let mut tags = vec![
        Tag::parse(["u", url]).unwrap(),
        Tag::parse(["method", method]).unwrap(),
    ];
    if let Some(body) = body {
        tags.push(Tag::parse(["payload", hex::encode(Sha256::digest(body)).as_str()]).unwrap());
    }
    let event = EventBuilder::new(Kind::from(27235), "")
        .tags(tags)
        .sign_with_keys(keys)
        .unwrap();
    format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(event.as_json())
    )
}
/// Git token scoped to one ref, as the client signs an input push: the pre-receive
/// hook then refuses every other ref, on top of the private repository rules.
fn scoped_git_token(keys: &Keys, repo_root: &str, reference: &str) -> String {
    let event = EventBuilder::new(Kind::from(27235), "")
        .tags([
            Tag::parse(["u", repo_root]).unwrap(),
            Tag::parse(["method", "GET"]).unwrap(),
            Tag::parse(["ref", reference]).unwrap(),
        ])
        .sign_with_keys(keys)
        .unwrap();
    format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(event.as_json())
    )
}
async fn request(
    app: &Router,
    host: &str,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    bytes: Vec<u8>,
) -> Response {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", host);
    if let Some(auth) = auth {
        req = req.header("authorization", auth);
    }
    app.clone()
        .oneshot(req.body(Body::from(bytes)).unwrap())
        .await
        .unwrap()
}
/// Run the real `git` client against the test relay: no system or user config, no
/// prompts, the tenant's `Host` header, and an optional NIP-98 `Authorization` header.
async fn git(dir: &Path, host: &str, auth: Option<&str>, args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "buyer")
        .env("GIT_AUTHOR_EMAIL", "buyer@example.invalid")
        .env("GIT_COMMITTER_NAME", "buyer")
        .env("GIT_COMMITTER_EMAIL", "buyer@example.invalid")
        .arg("-c")
        .arg(format!("http.extraHeader=Host: {host}"));
    if let Some(auth) = auth {
        command
            .arg("-c")
            .arg(format!("http.extraHeader=Authorization: {auth}"));
    }
    command.args(args).output().await.unwrap()
}
fn stdout(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .trim()
        .to_owned()
}
fn etag(bytes: &[u8]) -> String {
    format!("\"{}\"", hex::encode(Sha256::digest(bytes)))
}
/// In-memory S3 bucket `test`. Writes follow the git store: `If-None-Match: *` for
/// packs, manifests and the seeded pointer, and `If-Match` for pointer CAS.
fn object_store(objects: Objects, accesses: Arc<AtomicUsize>) -> Router {
    Router::new().fallback(move |req: Request<Body>| {
        let objects = objects.clone();
        let accesses = accesses.clone();
        async move {
            accesses.fetch_add(1, Ordering::SeqCst);
            let key = req
                .uri()
                .path()
                .strip_prefix("/test/")
                .unwrap_or("")
                .to_owned();
            if req.method() == Method::PUT {
                let (create_only, if_match) = {
                    let headers = req.headers();
                    (
                        headers.get("if-none-match").is_some_and(|v| v == "*"),
                        headers
                            .get("if-match")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                    )
                };
                let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let mut objects = objects.write().unwrap();
                let current = objects.get(&key).map(|b| etag(b));
                if (create_only && current.is_some())
                    || if_match.is_some_and(|tag| current.as_deref() != Some(tag.as_str()))
                {
                    return (StatusCode::PRECONDITION_FAILED, "precondition failed")
                        .into_response();
                }
                let tag = etag(&bytes);
                objects.insert(key, bytes.to_vec());
                return Response::builder()
                    .status(200)
                    .header("etag", tag)
                    .body(Body::empty())
                    .unwrap();
            }
            match objects.read().unwrap().get(&key) {
                Some(bytes) => Response::builder()
                    .status(200)
                    .header("etag", etag(bytes))
                    .header("content-length", bytes.len())
                    .body(Body::from(if req.method() == Method::HEAD {
                        Vec::new()
                    } else {
                        bytes.clone()
                    }))
                    .unwrap(),
                None => (StatusCode::NOT_FOUND, "absent").into_response(),
            }
        }
    })
}
fn private_offer(buyer: &Keys, seller: &Keys, job: &str) -> Event {
    EventBuilder::new(Kind::from(3401), "")
        .allow_self_tagging()
        .tags(
            [
                vec!["t".into(), "maxplayer".into()],
                vec!["v".into(), "2".into()],
                vec!["job".into(), job.to_owned()],
                vec!["visibility".into(), "private".into()],
                vec!["discovery".into(), "targeted".into()],
                vec!["output".into(), "text".into()],
                vec!["amount".into(), "0".into(), "sat".into()],
                vec!["param".into(), "payment".into(), "none".into()],
                vec!["param".into(), "deadline".into(), "2000000000".into()],
                vec!["p".into(), seller.public_key().to_hex()],
                vec!["content-id".into(), "19".repeat(32)],
                vec!["content-commitment".into(), "20".repeat(32)],
            ]
            .into_iter()
            .map(|t: Vec<String>| Tag::parse(t).unwrap()),
        )
        .sign_with_keys(buyer)
        .unwrap()
}
struct Harness {
    host: String,
    tenant: buzz_core::CommunityId,
    state: Arc<crate::state::AppState>,
    objects: Objects,
    accesses: Arc<AtomicUsize>,
    object_server: tokio::task::JoinHandle<()>,
    _shutdown: crate::state::AuditShutdownHandle,
    _temp: tempfile::TempDir,
}
/// A fresh tenant with private job repositories on. `bind_addr` must be the address
/// of a real listener when a test pushes: the pre-receive hook calls back to it.
async fn harness(service: &Keys, bind_addr: Option<std::net::SocketAddr>) -> Harness {
    let db_url = std::env::var("PRIVATE_JOB_TEST_DATABASE_URL")
        .expect("explicit disposable database required");
    let redis_url =
        std::env::var("PRIVATE_JOB_TEST_REDIS_URL").expect("explicit disposable Redis required");
    let temp = tempfile::tempdir().unwrap();
    let pool = sqlx::PgPool::connect(&db_url).await.unwrap();
    buzz_db::migration::run_migrations(&pool).await.unwrap();
    let community = uuid::Uuid::new_v4();
    let host = format!("{}.example.invalid", community);
    sqlx::query("INSERT INTO communities(id,host) VALUES($1,$2)")
        .bind(community)
        .bind(&host)
        .execute(&pool)
        .await
        .unwrap();
    let objects = Objects::default();
    let accesses = Arc::new(AtomicUsize::new(0));
    let object_app = object_store(objects.clone(), accesses.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let object_server = tokio::spawn(async move {
        axum::serve(listener, object_app).await.unwrap();
    });
    let mut config = crate::config::Config::from_env().unwrap();
    config.database_url = db_url;
    config.redis_url = redis_url.clone();
    config.relay_url = format!("wss://{host}");
    config.require_auth_token = true;
    config.require_relay_membership = false;
    config.git_public_read = true; // Deliberately enable the generic public-read exception.
    config.private_job_repos = true;
    config.private_service_pubkey = Some(service.public_key().to_hex());
    config.git_pack_cache_path = temp.path().join("cache");
    config.git_repo_path = temp.path().join("repos");
    std::fs::create_dir_all(&config.git_repo_path).unwrap();
    if let Some(bind_addr) = bind_addr {
        config.bind_addr = bind_addr;
    }
    config.media.s3_endpoint = endpoint;
    config.media.s3_access_key = "public-test-key".into();
    config.media.s3_secret_key = "public-test-secret".into();
    config.media.s3_bucket = "test".into();
    config.media.s3_region = "test".into();
    let db = buzz_db::Db::from_pool(pool.clone());
    let redis = deadpool_redis::Config::from_url(&redis_url)
        .create_pool(Some(deadpool_redis::Runtime::Tokio1))
        .unwrap();
    let pubsub = Arc::new(
        buzz_pubsub::PubSubManager::new(&redis_url, redis.clone())
            .await
            .unwrap(),
    );
    let audit = buzz_audit::AuditService::new(pool.clone());
    let auth = buzz_auth::AuthService::new(config.auth.clone());
    let search = buzz_search::SearchService::new(pool.clone());
    let workflow = Arc::new(buzz_workflow::WorkflowEngine::new(
        db.clone(),
        buzz_workflow::WorkflowConfig::default(),
    ));
    let media = buzz_media::MediaStorage::new(&config.media).unwrap();
    let (state, shutdown) = crate::state::AppState::new(
        config,
        db,
        redis,
        audit,
        pubsub,
        auth,
        search,
        workflow,
        Keys::generate(),
        media,
    );
    Harness {
        host,
        tenant: buzz_core::CommunityId::from_uuid(community),
        state: Arc::new(state),
        objects,
        accesses,
        object_server,
        _shutdown: shutdown,
        _temp: temp,
    }
}
/// Read the manifest the repository pointer names, as the relay would.
fn published_manifest(
    objects: &Objects,
    tenant: buzz_core::CommunityId,
    owner: &str,
    job: &str,
) -> super::manifest::Manifest {
    let objects = objects.read().unwrap();
    let pointer = &objects[&super::manifest::pointer_key(tenant, owner, job)];
    let digest = std::str::from_utf8(pointer).unwrap().trim();
    super::manifest::Manifest::from_bytes(&objects[&format!("manifests/{digest}")]).unwrap()
}
#[tokio::test]
#[ignore = "requires disposable PRIVATE_JOB_TEST_DATABASE_URL and PRIVATE_JOB_TEST_REDIS_URL"]
async fn private_http_acl_precedes_manifest_cache_and_provision_replay_is_rejected() {
    let buyer = Keys::generate();
    let seller = Keys::generate();
    let service = Keys::generate();
    let outsider = Keys::generate();
    let Harness {
        host,
        tenant,
        state,
        objects,
        accesses,
        object_server,
        _shutdown,
        _temp,
    } = harness(&service, None).await;
    let job = "17".repeat(32);
    let base = format!("https://{host}/git/{}/{job}", buyer.public_key().to_hex());
    let offer = private_offer(&buyer, &seller, &job);
    let app = super::transport::git_router(state.clone());
    let provision_path = format!("/api/jobs/private/{job}");
    let body = serde_json::to_vec(&serde_json::json!({"signed_offer":offer})).unwrap();
    let url = format!("https://{host}{provision_path}");
    let auth = token(&buyer, &url, "PUT", Some(&body));
    assert_eq!(
        request(
            &app,
            &host,
            "PUT",
            &provision_path,
            Some(&auth),
            body.clone()
        )
        .await
        .status(),
        StatusCode::OK
    );
    // Production shared Redis replay verifier, not an always-fresh unit stub.
    assert!(
        !request(
            &app,
            &host,
            "PUT",
            &provision_path,
            Some(&auth),
            body.clone()
        )
        .await
        .status()
        .is_success()
    );
    let wrong_method = token(&buyer, &url, "POST", Some(&body));
    assert!(
        !request(
            &app,
            &host,
            "PUT",
            &provision_path,
            Some(&wrong_method),
            body.clone()
        )
        .await
        .status()
        .is_success()
    );
    let wrong_payload = token(&buyer, &url, "PUT", Some(b"wrong"));
    assert!(
        !request(
            &app,
            &host,
            "PUT",
            &provision_path,
            Some(&wrong_payload),
            body.clone()
        )
        .await
        .status()
        .is_success()
    );
    // A private repo is never announced, so provisioning must seed the empty-manifest
    // pointer that announce seeds for a public repo. Without it, the first push (the
    // buyer's input upload) failed at the receive-pack advertisement with 404.
    assert!(
        objects
            .read()
            .unwrap()
            .contains_key(&super::manifest::pointer_key(
                tenant,
                &buyer.public_key().to_hex(),
                &job
            )),
        "provisioning must seed the private repository pointer"
    );
    for service_name in ["git-receive-pack", "git-upload-pack"] {
        let path = format!(
            "/git/{}/{job}/info/refs?service={service_name}",
            buyer.public_key().to_hex()
        );
        let auth = token(&buyer, &base, "GET", None);
        let response = request(&app, &host, "GET", &path, Some(&auth), vec![]).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "fresh private repository {service_name} advertisement"
        );
    }
    let path = format!(
        "/git/{}/{job}/info/refs?service=git-upload-pack",
        buyer.public_key().to_hex()
    );
    // Warm the published manifest path using an authorized participant.
    let reference = format!("refs/heads/input/{}", "21".repeat(32));
    let manifest = super::manifest::Manifest {
        version: 1,
        head: reference.clone(),
        refs: BTreeMap::from([(reference, "22".repeat(20))]),
        packs: vec![],
        parent: None,
    };
    let bytes = manifest.canonical_bytes().unwrap();
    let digest = hex::encode(Sha256::digest(&bytes));
    objects
        .write()
        .unwrap()
        .insert(format!("manifests/{digest}"), bytes);
    objects.write().unwrap().insert(
        super::manifest::pointer_key(tenant, &buyer.public_key().to_hex(), &job),
        digest.into_bytes(),
    );
    for participant in [&buyer, &seller, &service] {
        let auth = token(participant, &base, "GET", None);
        let response = request(&app, &host, "GET", &path, Some(&auth), vec![]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
    }
    let before = accesses.load(Ordering::SeqCst);
    let outsider_auth = token(&outsider, &base, "GET", None);
    for (method, suffix) in [
        ("GET", "info/refs?service=git-upload-pack"),
        ("POST", "git-upload-pack"),
        ("GET", "info/refs?service=git-receive-pack"),
        ("POST", "git-receive-pack"),
    ] {
        let path = format!("/git/{}/{job}/{suffix}", buyer.public_key().to_hex());
        let response = request(&app, &host, method, &path, Some(&outsider_auth), vec![]).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {suffix}"
        );
        assert_eq!(response.headers()["cache-control"], "private, no-store");
    }
    assert_eq!(
        accesses.load(Ordering::SeqCst),
        before,
        "outsider reached object storage after authorized warm reads"
    );
    assert_eq!(
        request(&app, &host, "GET", &path, None, vec![])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // Turning off provisioning must not turn existing private repositories public.
    drop(app);
    let mut state = Arc::try_unwrap(state).ok().expect("router dropped");
    Arc::make_mut(&mut state.config).private_job_repos = false;
    let app = super::transport::git_router(Arc::new(state));
    assert_eq!(
        request(&app, &host, "GET", &path, Some(&outsider_auth), vec![])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    object_server.abort();
}
/// The first write into a fresh private repository, end to end with the real `git`
/// client over real HTTP: the buyer's scoped input push passes the pre-receive hook
/// and publishes a manifest; the targeted seller fetches the exact commit before any
/// award; an outsider's push is refused and changes nothing.
#[tokio::test]
#[ignore = "requires disposable PRIVATE_JOB_TEST_DATABASE_URL and PRIVATE_JOB_TEST_REDIS_URL"]
async fn fresh_private_repo_takes_real_input_push_and_targeted_fetch() {
    let buyer = Keys::generate();
    let seller = Keys::generate();
    let service = Keys::generate();
    let outsider = Keys::generate();
    // The hook posts to `bind_addr`, so the relay must listen before the state exists.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let h = harness(&service, Some(addr)).await;
    let app = super::transport::git_router(h.state.clone());
    let served = app
        .clone()
        .merge(super::git_policy_router(h.state.clone()))
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    });
    let job = "31".repeat(32);
    let owner = buyer.public_key().to_hex();
    let provision_path = format!("/api/jobs/private/{job}");
    let body = serde_json::to_vec(&serde_json::json!({
        "signed_offer": private_offer(&buyer, &seller, &job)
    }))
    .unwrap();
    let auth = token(
        &buyer,
        &format!("https://{}{provision_path}", h.host),
        "PUT",
        Some(&body),
    );
    let response = request(&app, &h.host, "PUT", &provision_path, Some(&auth), body).await;
    assert_eq!(response.status(), StatusCode::OK);

    let repo_root = format!("https://{}/git/{owner}/{job}", h.host);
    let remote = format!("http://{addr}/git/{owner}/{job}");
    let reference = format!("refs/heads/input/{}", "32".repeat(32));
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source");
    let fetched = work.path().join("fetched");
    for dir in [&source, &fetched] {
        std::fs::create_dir(dir).unwrap();
        stdout(&git(dir, &h.host, None, &["init", "-q"]).await);
    }
    std::fs::write(source.join("brief.txt"), "private input\n").unwrap();
    stdout(&git(&source, &h.host, None, &["add", "brief.txt"]).await);
    stdout(&git(&source, &h.host, None, &["commit", "-q", "-m", "input"]).await);
    let commit = stdout(&git(&source, &h.host, None, &["rev-parse", "HEAD"]).await);

    let push_auth = scoped_git_token(&buyer, &repo_root, &reference);
    let refspec = format!("HEAD:{reference}");
    let push = git(
        &source,
        &h.host,
        Some(&push_auth),
        &["push", &remote, &refspec],
    )
    .await;
    stdout(&push);
    let manifest = published_manifest(&h.objects, h.tenant, &owner, &job);
    assert_eq!(manifest.refs.get(&reference), Some(&commit));

    let fetch_auth = token(&seller, &repo_root, "GET", None);
    let fetch = git(
        &fetched,
        &h.host,
        Some(&fetch_auth),
        &["fetch", "-q", &remote, &reference],
    )
    .await;
    stdout(&fetch);
    let fetched_commit = git(&fetched, &h.host, None, &["rev-parse", "FETCH_HEAD"]).await;
    assert_eq!(stdout(&fetched_commit), commit);
    let brief = git(&fetched, &h.host, None, &["show", "FETCH_HEAD:brief.txt"]).await;
    assert_eq!(stdout(&brief), "private input");

    let foreign = format!("refs/heads/input/{}", "33".repeat(32));
    let outsider_auth = scoped_git_token(&outsider, &repo_root, &foreign);
    let foreign_refspec = format!("HEAD:{foreign}");
    let refused = git(
        &source,
        &h.host,
        Some(&outsider_auth),
        &["push", &remote, &foreign_refspec],
    )
    .await;
    assert!(!refused.status.success(), "outsider push must be refused");
    assert_eq!(
        published_manifest(&h.objects, h.tenant, &owner, &job).refs,
        manifest.refs
    );
    server.abort();
    h.object_server.abort();
}
