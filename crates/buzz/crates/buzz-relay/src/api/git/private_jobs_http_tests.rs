//! Real Git HTTP router + PostgreSQL ACL + Redis replay guard. Object storage is
//! a local in-memory fixture that accepts only the git store's create-only writes,
//! so a forbidden request must never touch it.
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
use nostr::{EventBuilder, Keys, Kind, Tag};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

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
#[tokio::test]
#[ignore = "requires disposable PRIVATE_JOB_TEST_DATABASE_URL and PRIVATE_JOB_TEST_REDIS_URL"]
async fn private_http_acl_precedes_manifest_cache_and_provision_replay_is_rejected() {
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
    let tenant = buzz_core::CommunityId::from_uuid(community);
    let buyer = Keys::generate();
    let seller = Keys::generate();
    let service = Keys::generate();
    let outsider = Keys::generate();
    let job = "17".repeat(32);
    let base = format!("https://{host}/git/{}/{job}", buyer.public_key().to_hex());
    let offer = EventBuilder::new(Kind::from(3401), "")
        .allow_self_tagging()
        .tags(
            [
                vec!["t".into(), "maxplayer".into()],
                vec!["v".into(), "2".into()],
                vec!["job".into(), job.clone()],
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
        .sign_with_keys(&buyer)
        .unwrap();
    let objects = Arc::new(std::sync::RwLock::new(BTreeMap::<String, Vec<u8>>::new()));
    let accesses = Arc::new(AtomicUsize::new(0));
    let object_app = Router::new().fallback({
        let objects = objects.clone();
        let accesses = accesses.clone();
        move |req: Request<Body>| {
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
                    // Provisioning seeds the empty-manifest pointer with `If-None-Match: *`.
                    let create_only = req.headers().get("if-none-match").is_some_and(|v| v == "*");
                    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    let mut objects = objects.write().unwrap();
                    if create_only && objects.contains_key(&key) {
                        return (StatusCode::PRECONDITION_FAILED, "exists").into_response();
                    }
                    objects.insert(key, bytes.to_vec());
                    return Response::builder()
                        .status(200)
                        .header("etag", "\"fixture\"")
                        .body(Body::empty())
                        .unwrap();
                }
                match objects.read().unwrap().get(&key) {
                    Some(bytes) => Response::builder()
                        .status(200)
                        .header("etag", "\"fixture\"")
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
        }
    });
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
    let (state, _shutdown) = crate::state::AppState::new(
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
    let state = Arc::new(state);
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
