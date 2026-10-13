//! MAXPLAYER: REQ behavior this fork keeps on top of upstream.
//!
//! - A failed history query ends the subscription with CLOSED, never EOSE.
//!   Upstream sends EOSE, which tells the client incomplete history is complete.
//! - `BUZZ_OPEN_READ` lets an unauthenticated connection read public history:
//!   stored global events and EOSE only, never channel history or live fan-out.
//!
//! The database is unreachable in every test here, so a history query fails
//! deterministically with no network or database service, and a test that
//! expects EOSE also proves no database access happened.
use super::*;
use crate::connection::tests::{pending_state, test_conn, TestConn};
use nostr::{Alphabet, SingleLetterTag};
use serde_json::json;

const UNREACHABLE_DATABASE: &str = "postgres://test:test@127.0.0.1:1/test";

/// Every data frame the connection has sent so far, parsed as JSON.
fn frames(
    rx: &mut tokio::sync::mpsc::Receiver<axum::extract::ws::Message>,
) -> Vec<serde_json::Value> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|msg| match msg {
            axum::extract::ws::Message::Text(text) => {
                serde_json::from_str(&text).expect("JSON frame")
            }
            other => panic!("expected a text frame, got {other:?}"),
        })
        .collect()
}

fn authenticated(pubkey: nostr::PublicKey) -> AuthState {
    AuthState::Authenticated(buzz_auth::AuthContext {
        pubkey,
        scopes: Vec::new(),
        channel_ids: None,
        auth_method: buzz_auth::AuthMethod::Nip42,
        agent_owner_pubkey: None,
    })
}

async fn state_with_open_read(open_read: bool) -> Arc<AppState> {
    crate::state::tests::test_state_with_config(UNREACHABLE_DATABASE, |config| {
        config.open_read = open_read;
    })
    .await
}

#[tokio::test]
async fn historical_database_failure_closes_without_eose_and_removes_subscription() {
    let state = crate::state::tests::test_state_with_database_url(UNREACHABLE_DATABASE).await;
    let keys = nostr::Keys::generate();
    let TestConn {
        conn, mut send_rx, ..
    } = test_conn(authenticated(keys.public_key()), None);
    // Make the history query the first database access, not access resolution.
    state.accessible_channels_cache.insert(
        (
            conn.tenant.community(),
            keys.public_key().to_bytes().to_vec(),
        ),
        Vec::new(),
    );

    handle_req(
        "failed-history".into(),
        vec![Filter::new()
            .kind(nostr::Kind::Custom(3403))
            .author(keys.public_key())],
        Vec::new(),
        conn.clone(),
        state.clone(),
    )
    .await;

    assert_eq!(
        frames(&mut send_rx),
        vec![json!([
            "CLOSED",
            "failed-history",
            HISTORICAL_QUERY_FAILED_CLOSED
        ])],
        "a failed history query must end with CLOSED and never EOSE"
    );
    assert!(conn.subscriptions.lock().await.is_empty());
    assert_eq!(state.sub_registry.total_subscriptions(), 0);
}

#[tokio::test]
async fn open_read_gives_an_anonymous_reader_no_channel_history() {
    let state = state_with_open_read(true).await;
    let TestConn {
        conn, mut send_rx, ..
    } = test_conn(pending_state(), None);

    handle_req(
        "anon-channel".into(),
        vec![Filter::new().kind(nostr::Kind::Custom(9)).custom_tag(
            SingleLetterTag::lowercase(Alphabet::H),
            uuid::Uuid::new_v4().to_string(),
        )],
        Vec::new(),
        conn.clone(),
        state.clone(),
    )
    .await;

    assert_eq!(
        frames(&mut send_rx),
        vec![json!(["EOSE", "anon-channel"])],
        "channel history is members-only: immediate EOSE, no query"
    );
    assert!(conn.subscriptions.lock().await.is_empty());
    assert_eq!(state.sub_registry.total_subscriptions(), 0);
}

#[tokio::test]
async fn open_read_anonymous_history_failure_closes_without_eose() {
    let state = state_with_open_read(true).await;
    let TestConn {
        conn, mut send_rx, ..
    } = test_conn(pending_state(), None);

    handle_req(
        "anon-global".into(),
        vec![Filter::new().kind(nostr::Kind::Custom(5109))],
        Vec::new(),
        conn.clone(),
        state.clone(),
    )
    .await;

    // Reaching the (failing) history query proves the anonymous reader was
    // admitted; the failure must still end in CLOSED, never EOSE.
    assert_eq!(
        frames(&mut send_rx),
        vec![json!([
            "CLOSED",
            "anon-global",
            HISTORICAL_QUERY_FAILED_CLOSED
        ])]
    );
    assert!(conn.subscriptions.lock().await.is_empty());
    assert_eq!(state.sub_registry.total_subscriptions(), 0);
}

#[tokio::test]
async fn without_open_read_an_unauthenticated_req_is_refused() {
    let state = state_with_open_read(false).await;
    let TestConn {
        conn, mut send_rx, ..
    } = test_conn(pending_state(), None);

    handle_req(
        "anon".into(),
        vec![Filter::new().kind(nostr::Kind::Custom(5109))],
        Vec::new(),
        conn.clone(),
        state.clone(),
    )
    .await;

    assert_eq!(
        frames(&mut send_rx),
        vec![
            json!(["NOTICE", "auth-required: authenticate before subscribing"]),
            json!(["CLOSED", "anon", "auth-required: not authenticated"]),
        ]
    );
    assert!(conn.subscriptions.lock().await.is_empty());
}

#[test]
fn open_read_query_params_pin_unscoped_reads_to_global_rows() {
    let community = buzz_core::tenant::CommunityId::from_uuid(uuid::Uuid::nil());
    let filter = Filter::new().kind(nostr::Kind::Custom(5109));

    let query = open_read_filter_to_query_params(&filter, None, community);

    assert!(query.global_only);
    assert!(query.channel_id.is_none());
    assert_eq!(query.kinds, Some(vec![5109]));
}

#[test]
fn open_read_query_params_preserve_explicit_channel_scope() {
    let community = buzz_core::tenant::CommunityId::from_uuid(uuid::Uuid::nil());
    let channel = uuid::Uuid::new_v4();
    let filter = Filter::new().kind(nostr::Kind::Custom(5109));

    let query = open_read_filter_to_query_params(&filter, Some(channel), community);

    assert!(!query.global_only);
    assert_eq!(query.channel_id, Some(channel));
}
