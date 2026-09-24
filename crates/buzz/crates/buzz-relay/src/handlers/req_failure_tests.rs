//! Actual REQ handler with a closed SQL pool: no network/database service needed.
use super::*;

#[tokio::test]
async fn historical_database_failure_closes_without_eose_and_removes_subscription() {
    use std::{collections::HashMap, sync::atomic::AtomicU8};
    use tokio::sync::{Mutex, RwLock, mpsc};
    use tokio_util::sync::CancellationToken;
    let mut config = crate::config::Config::from_env().unwrap();
    config.redis_url = "redis://127.0.0.1:1".into();
    let pool = sqlx::PgPool::connect_lazy("postgres://test:test@127.0.0.1:1/test").unwrap();
    pool.close().await;
    let db = buzz_db::Db::from_pool(pool.clone());
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
    let search = buzz_search::SearchService::new(pool.clone());
    let engine = Arc::new(buzz_workflow::WorkflowEngine::new(
        db.clone(),
        buzz_workflow::WorkflowConfig::default(),
    ));
    let media = buzz_media::MediaStorage::new(&config.media).unwrap();
    let (state, _shutdown) = AppState::new(
        config,
        db,
        redis_pool,
        audit,
        pubsub,
        auth,
        search,
        engine,
        nostr::Keys::generate(),
        media,
    );
    let state = Arc::new(state);
    let tenant = TenantContext::resolved(
        buzz_core::CommunityId::from_uuid(uuid::Uuid::new_v4()),
        "test.invalid",
    );
    let keys = nostr::Keys::generate();
    // Ensure the failure is the history query, not access resolution.
    state.accessible_channels_cache.insert(
        (tenant.community(), keys.public_key().to_bytes().to_vec()),
        vec![],
    );
    let (send_tx, mut rx) = mpsc::channel(16);
    let (ctrl_tx, _ctrl_rx) = mpsc::channel(16);
    let conn = Arc::new(ConnectionState {
        conn_id: uuid::Uuid::new_v4(),
        tenant,
        remote_addr: "127.0.0.1:1".parse().unwrap(),
        auth_state: RwLock::new(AuthState::Authenticated(buzz_auth::AuthContext {
            pubkey: keys.public_key(),
            scopes: vec![],
            channel_ids: None,
            auth_method: buzz_auth::AuthMethod::Nip42,
            agent_owner_pubkey: None,
        })),
        subscriptions: Arc::new(Mutex::new(HashMap::new())),
        send_tx,
        ctrl_tx,
        cancel: CancellationToken::new(),
        backpressure_count: Arc::new(AtomicU8::new(0)),
        grace_limit: 3,
    });
    handle_req(
        "failed-history".into(),
        vec![
            Filter::new()
                .kind(nostr::Kind::Custom(3403))
                .author(keys.public_key()),
        ],
        conn.clone(),
        state.clone(),
    )
    .await;
    let frame = rx.try_recv().expect("query failure response");
    let axum::extract::ws::Message::Text(text) = frame else {
        panic!("not text")
    };
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value,
        serde_json::json!(["CLOSED", "failed-history", "error: historical query failed"])
    );
    assert!(rx.try_recv().is_err(), "must not follow failure with EOSE");
    assert!(conn.subscriptions.lock().await.is_empty());
    assert_eq!(state.sub_registry.total_subscriptions(), 0);
}
