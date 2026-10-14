//! Real Postgres + Redis tests. Explicitly ignored so missing services cannot masquerade as a pass.
//! Run with BUZZ_TEST_DATABASE_URL and HANDLE_TEST_REDIS_URL, --include-ignored.
use crate::state::AppState;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use axum::{
    extract::ConnectInfo,
    http::{HeaderMap, StatusCode},
};
use base64::Engine;
use buzz_core::TenantContext;
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, sync::Arc};
use tower::ServiceExt;

async fn setup() -> Arc<AppState> {
    let mut config = crate::config::Config::for_test();
    config.database_url =
        std::env::var("BUZZ_TEST_DATABASE_URL").expect("disposable PostgreSQL required");
    config.redis_url = std::env::var("HANDLE_TEST_REDIS_URL").expect("disposable Redis required");
    config.relay_url = format!("wss://{}.test", uuid::Uuid::new_v4());
    config.nip05_domain = Some(format!("{}.example", uuid::Uuid::new_v4()));
    config.cors_origins = vec!["https://restricted.example".into()];
    config.require_relay_membership = true;
    // Public registration must also work in fail-closed NIP-FI mode.
    config.nip_fi.mode = buzz_auth::NipFiMode::DenyProtected;
    let pool = sqlx::PgPool::connect(&config.database_url).await.unwrap();
    buzz_db::migration::run_migrations(&pool).await.unwrap();
    let db = buzz_db::Db::from_pool(pool.clone());
    db.ensure_configured_community(&crate::tenant::relay_url_authority(&config.relay_url))
        .await
        .unwrap();
    let redis_pool = deadpool_redis::Config::from_url(&config.redis_url)
        .create_pool(Some(deadpool_redis::Runtime::Tokio1))
        .unwrap();
    let pubsub = Arc::new(
        buzz_pubsub::PubSubManager::new(&config.redis_url, redis_pool.clone())
            .await
            .unwrap(),
    );
    let audit = buzz_audit::AuditService::new(pool.clone());
    let auth = buzz_auth::AuthService::new(config.auth.clone());
    let search = buzz_search::SearchService::new(pool);
    let engine = Arc::new(buzz_workflow::WorkflowEngine::new(
        db.clone(),
        buzz_workflow::WorkflowConfig::default(),
    ));
    let media = buzz_media::MediaStorage::new(&config.media).unwrap();
    let (state, _) = AppState::new(
        config,
        db,
        redis_pool,
        audit,
        pubsub,
        auth,
        search,
        engine,
        Keys::generate(),
        media,
    );
    Arc::new(state)
}
fn host(s: &AppState) -> String {
    crate::tenant::relay_url_authority(&s.config.relay_url)
}
async fn ctx(s: &AppState) -> TenantContext {
    crate::tenant::bind_community(&s.db, &host(s))
        .await
        .unwrap()
}
fn signed(keys: &Keys, url: &str, method: &str, body: &str, stale: bool) -> String {
    let hash = hex::encode(Sha256::digest(body.as_bytes()));
    let mut builder = EventBuilder::new(Kind::HttpAuth, "").tags([
        Tag::parse(["u", url]).unwrap(),
        Tag::parse(["method", method]).unwrap(),
        Tag::parse(["payload", &hash]).unwrap(),
        Tag::parse(["nonce", &uuid::Uuid::new_v4().to_string()]).unwrap(),
    ]);
    if stale {
        builder = builder.custom_created_at(Timestamp::from(Timestamp::now().as_secs() - 120));
    }
    let event = builder.sign_with_keys(keys).unwrap();
    format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&event).unwrap())
    )
}
async fn request(
    s: Arc<AppState>,
    authority: &str,
    path: &str,
    body: Option<&str>,
    auth: Option<String>,
) -> (StatusCode, HeaderMap, Value) {
    let mut r = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("host", authority)
        .header("origin", "https://other.example");
    if let Some(auth) = auth {
        r = r.header("authorization", auth);
    }
    // Unique test peer; rate-limit tests exercise the limiter directly.
    let peer = SocketAddr::from(([127, 1, 0, 1], 1234));
    let mut r = r.body(Body::from(body.unwrap_or("").to_owned())).unwrap();
    r.extensions_mut().insert(ConnectInfo(peer));
    let response = crate::router::build_router(s).oneshot(r).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)}));
    (status, headers, value)
}
async fn post(
    s: Arc<AppState>,
    keys: &Keys,
    path: &str,
    body: &str,
) -> (StatusCode, HeaderMap, Value) {
    let h = host(&s);
    let auth = signed(keys, &format!("https://{h}{path}"), "POST", body, false);
    request(s, &h, path, Some(body), Some(auth)).await
}
async fn get(s: Arc<AppState>, path: &str) -> (StatusCode, HeaderMap, Value) {
    let h = host(&s);
    request(s, &h, path, None, None).await
}

#[tokio::test]
#[ignore = "requires disposable Postgres and Redis"]
async fn handles_claim_rename_release_cooldown() {
    let s = setup().await;
    let a = Keys::generate();
    let b = Keys::generate();
    assert_eq!(
        post(s.clone(), &a, "/api/handles/claim", r#"{"name":"Alice"}"#)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        get(s.clone(), "/api/handles/available?name=ALICE").await.2["reason"],
        "taken"
    );
    assert_eq!(
        post(s.clone(), &b, "/api/handles/claim", r#"{"name":"alice"}"#)
            .await
            .2["reason"],
        "taken"
    );
    // Another signer can release only its own claim, never Alice's.
    assert_eq!(
        post(s.clone(), &b, "/api/handles/release", "{}").await.0,
        StatusCode::OK
    );
    assert_eq!(
        get(s.clone(), "/.well-known/nostr.json?name=alice").await.2["names"]["alice"],
        a.public_key().to_hex()
    );
    assert_eq!(
        post(s.clone(), &a, "/api/handles/claim", r#"{"name":"bravo"}"#)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        post(s.clone(), &b, "/api/handles/claim", r#"{"name":"alice"}"#)
            .await
            .2["reason"],
        "cooldown"
    );
    assert_eq!(
        post(s.clone(), &a, "/api/handles/claim", r#"{"name":"alice"}"#)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        post(s.clone(), &a, "/api/handles/release", "{}").await.0,
        StatusCode::OK
    );
    assert_eq!(
        get(s.clone(), "/.well-known/nostr.json?name=alice").await.2["names"],
        json!({})
    );
    let tenant = ctx(&s).await;
    sqlx::query("UPDATE handles SET released_at=now()-INTERVAL '31 days' WHERE community_id=$1 AND name='alice'").bind(tenant.community().as_uuid()).execute(s.db.pool()).await.unwrap();
    assert_eq!(
        post(s.clone(), &b, "/api/handles/claim", r#"{"name":"alice"}"#)
            .await
            .0,
        StatusCode::OK
    );
}
#[tokio::test]
#[ignore = "requires disposable Postgres and Redis"]
async fn handles_reserved_invalid_failed_rename() {
    let s = setup().await;
    let a = Keys::generate();
    let tenant = ctx(&s).await;
    sqlx::query("INSERT INTO reserved_handles (community_id,name) VALUES ($1,'admin')")
        .bind(tenant.community().as_uuid())
        .execute(s.db.pool())
        .await
        .unwrap();
    assert_eq!(
        post(s.clone(), &a, "/api/handles/claim", r#"{"name":"alice"}"#)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        post(s.clone(), &a, "/api/handles/claim", r#"{"name":"ADMIN"}"#)
            .await
            .2["reason"],
        "reserved"
    );
    assert_eq!(
        get(s.clone(), "/api/handles/available?name=admin").await.2["reason"],
        "reserved"
    );
    for name in ["ab", "_alice", "al+ice", "a@b"] {
        assert_eq!(
            post(
                s.clone(),
                &a,
                "/api/handles/claim",
                &json!({"name":name}).to_string()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(s.clone(), &format!("/api/handles/available?name={name}"))
                .await
                .2["reason"],
            "invalid"
        );
    }
    assert_eq!(
        get(s.clone(), "/.well-known/nostr.json?name=alice").await.2["names"]["alice"],
        a.public_key().to_hex()
    );
}
#[tokio::test]
#[ignore = "requires disposable Postgres and Redis"]
async fn handles_nip98_failures_and_replay() {
    let s = setup().await;
    let a = Keys::generate();
    let h = host(&s);
    let path = "/api/handles/claim";
    let body = r#"{"name":"alice"}"#;
    let url = format!("https://{h}{path}");
    for auth in [
        None,
        Some(signed(
            &a,
            "https://wrong.example/api/handles/claim",
            "POST",
            body,
            false,
        )),
        Some(signed(&a, &url, "GET", body, false)),
        Some(signed(&a, &url, "POST", body, true)),
        Some(signed(&a, &url, "POST", r#"{"name":"bob"}"#, false)),
    ] {
        assert_eq!(
            request(s.clone(), &h, path, Some(body), auth).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let auth = signed(&a, &url, "POST", body, false);
    assert_eq!(
        request(s.clone(), &h, path, Some(body), Some(auth.clone()))
            .await
            .0,
        StatusCode::OK
    );
    assert_ne!(
        request(s.clone(), &h, path, Some(body), Some(auth)).await.0,
        StatusCode::OK
    );
    let b = Keys::generate();
    assert_eq!(
        post(
            s.clone(),
            &b,
            path,
            &json!({"name":"alice","pubkey":a.public_key().to_hex()}).to_string()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
#[ignore = "requires disposable Postgres and Redis"]
async fn handles_batch_nostr_headers_and_hosts() {
    let s = setup().await;
    let a = Keys::generate();
    let apex = s.config.nip05_domain.clone().unwrap();
    let body = r#"{"name":"alice"}"#;
    let auth = signed(
        &a,
        &format!("https://{apex}/api/handles/claim"),
        "POST",
        body,
        false,
    );
    assert_eq!(
        request(
            s.clone(),
            &apex,
            "/api/handles/claim",
            Some(body),
            Some(auth)
        )
        .await
        .0,
        StatusCode::OK
    );
    for h in [host(&s), apex] {
        let (status, headers, v) = request(
            s.clone(),
            &h,
            "/.well-known/nostr.json?name=ALIce",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(v["names"]["ALIce"], a.public_key().to_hex());
        assert_eq!(
            v["relays"][a.public_key().to_hex()],
            json!([s.config.relay_url])
        );
    }
    assert_eq!(
        get(s.clone(), "/.well-known/nostr.json").await.2,
        json!({"names":{},"relays":{}})
    );
    assert_eq!(
        request(
            s.clone(),
            "unknown.example",
            "/.well-known/nostr.json?name=alice",
            None,
            None
        )
        .await
        .2["names"],
        json!({})
    );
    let (status, headers, v) = get(
        s.clone(),
        &format!(
            "/api/handles?pubkeys={},{}",
            a.public_key().to_hex(),
            Keys::generate().public_key().to_hex()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert_eq!(v, json!({a.public_key().to_hex():"alice"}));
    assert_eq!(
        get(s.clone(), "/api/handles?pubkeys=bad").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(
            s.clone(),
            &format!(
                "/api/handles?pubkeys={}",
                vec![a.public_key().to_hex(); 501].join(",")
            )
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
#[ignore = "requires disposable Postgres and Redis"]
async fn handles_kind0_is_advertisement_only() {
    let s = setup().await;
    let a = Keys::generate();
    let t = ctx(&s).await;
    assert_eq!(
        post(s.clone(), &a, "/api/handles/claim", r#"{"name":"alice"}"#)
            .await
            .0,
        StatusCode::OK
    );
    for content in [
        json!({"nip05":format!("imposter@{}",host(&s)),"name":"Fake"}),
        json!({}),
    ] {
        let event = EventBuilder::new(Kind::Metadata, content.to_string())
            .sign_with_keys(&a)
            .unwrap();
        crate::handlers::side_effects::handle_side_effects(&t, 0, &event, &s)
            .await
            .unwrap();
        let value: Option<String> = sqlx::query_scalar(
            "SELECT nip05_handle FROM users WHERE community_id=$1 AND pubkey=$2",
        )
        .bind(t.community().as_uuid())
        .bind(a.public_key().to_bytes().to_vec())
        .fetch_one(s.db.pool())
        .await
        .unwrap();
        assert!(value.is_none());
        assert_eq!(
            s.db.lookup_handle(t.community(), "alice").await.unwrap(),
            Some(a.public_key().to_bytes().to_vec())
        );
        assert!(s
            .db
            .lookup_handle(t.community(), "imposter")
            .await
            .unwrap()
            .is_none());
    }
}
#[tokio::test]
#[ignore = "requires disposable Postgres and Redis"]
async fn handles_concurrent_claims_and_tenant_isolation() {
    let s = setup().await;
    let t = ctx(&s).await;
    let a = Keys::generate().public_key().to_bytes();
    let b = Keys::generate().public_key().to_bytes();
    let (x, y) = tokio::join!(
        s.db.claim_handle(t.community(), "alice", &a),
        s.db.claim_handle(t.community(), "alice", &b)
    );
    assert_eq!(
        [x.unwrap(), y.unwrap()]
            .iter()
            .filter(|r| r.is_none())
            .count(),
        1
    );
    // Concurrent renames of one key still leave exactly one active name.
    let (x, y) = tokio::join!(
        s.db.claim_handle(t.community(), "bravo", &a),
        s.db.claim_handle(t.community(), "charlie", &a)
    );
    assert!(x.unwrap().is_none());
    assert!(y.unwrap().is_none());
    assert_eq!(
        s.db.batch_handles(t.community(), &[a.to_vec()])
            .await
            .unwrap()
            .len(),
        1
    );
    let other = setup().await;
    let ot = ctx(&other).await;
    assert!(other
        .db
        .lookup_handle(ot.community(), "alice")
        .await
        .unwrap()
        .is_none());
    assert!(other
        .db
        .claim_handle(ot.community(), "alice", &b)
        .await
        .unwrap()
        .is_none());
}
#[tokio::test]
#[ignore = "requires disposable Postgres and Redis"]
async fn handles_shared_rate_limits() {
    let s = setup().await;
    let t = ctx(&s).await;
    let a = Keys::generate().public_key();
    for _ in 0..10 {
        assert!(
            s.admission_rate_limiter
                .check_handle_pubkey(&t, &a)
                .await
                .unwrap()
                .allowed
        );
    }
    assert!(
        !s.admission_rate_limiter
            .check_handle_pubkey(&t, &a)
            .await
            .unwrap()
            .allowed
    );
    let ip = "192.0.2.114".parse().unwrap();
    for _ in 0..60 {
        assert!(
            s.admission_rate_limiter
                .check_handle_ip(&ip)
                .await
                .unwrap()
                .allowed
        );
    }
    assert!(
        !s.admission_rate_limiter
            .check_handle_ip(&ip)
            .await
            .unwrap()
            .allowed
    );
}
