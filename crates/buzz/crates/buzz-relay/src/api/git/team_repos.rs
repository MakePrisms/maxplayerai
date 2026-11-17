//! MAXPLAYER: team repository provisioning (docs/specs/maxplayer-team.md §3).
//!
//! `PUT /api/teams/{team_id}` creates a team (`expected_version: 0`) or replaces its member
//! list; only the owner (the NIP-98 signer, whose namespace holds the repo) may call it.
//! `GET /api/teams/{owner}/{team_id}` returns the list to members and the service.
//! Both take the same checks as private job provisioning: strict NIP-98 with a bound body
//! digest, replay refusal, and relay membership.
use crate::state::AppState;
use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use buzz_core::TenantContext;
use buzz_db::team_repos::{TeamRepo, MAX_TEAM_MEMBERS};
use maxplayer_private_protocol::{is_hex, strict_json};
use serde::Deserialize;
use std::sync::Arc;

fn deny() -> Response {
    (StatusCode::FORBIDDEN, "team repository access denied").into_response()
}
fn unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "team repository policy unavailable").into_response()
}
fn bad(msg: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, msg).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutTeam {
    members: Vec<String>,
    expected_version: i64,
}

/// Strict NIP-98 (`require_payload` when a body is bound), replay refusal, relay
/// membership: the private-provisioning checks, in the same order.
async fn verified_caller(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    method: &str,
    body: Option<&[u8]>,
) -> Result<(TenantContext, String), Response> {
    if uri.query().is_some() {
        return Err(deny());
    }
    let tenant = crate::tenant::bind_community(
        &state.db,
        headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or(""),
    )
    .await
    .map_err(|_| deny())?;
    let url = crate::api::bridge::nip98_expected_url(&state.config.relay_url, &tenant, uri.path());
    let verified = crate::api::bridge::verify_bridge_auth_with_options(
        headers,
        method,
        &url,
        body,
        true,
        body.is_some(),
    )
    .map_err(IntoResponse::into_response)?;
    crate::api::bridge::check_nip98_replay(state, &tenant, verified.event_id_bytes)
        .await
        .map_err(IntoResponse::into_response)?;
    crate::api::relay_members::enforce_relay_membership(
        state,
        tenant.community(),
        verified.pubkey.as_bytes(),
        headers.get("x-auth-tag").and_then(|v| v.to_str().ok()),
        verified.signed_created_at,
    )
    .await
    .map_err(|_| deny())?;
    Ok((tenant, verified.pubkey.to_hex()))
}

fn view(state: &AppState, tenant: &TenantContext, team: &TeamRepo) -> Json<serde_json::Value> {
    let scheme = if state.config.relay_url.trim_start().starts_with("wss://") {
        "https"
    } else {
        "http"
    };
    Json(serde_json::json!({
        "owner": team.owner,
        "team_id": team.team_id,
        "members": team.members,
        "service": team.service,
        "version": team.version,
        "repo": format!("{scheme}://{}/git/{}/{}", tenant.host(), team.owner, team.team_id),
    }))
}

/// Validate, deduplicate and sort a requested member list; the owner is always added.
fn member_list(owner: &str, requested: Vec<String>) -> Result<Vec<String>, Response> {
    let mut members = std::collections::BTreeSet::new();
    members.insert(owner.to_owned());
    for m in requested {
        if !is_hex(&m, 32) || nostr::PublicKey::from_hex(&m).is_err() {
            return Err(bad("members must be 64 lowercase hex public keys"));
        }
        members.insert(m);
    }
    if members.len() > MAX_TEAM_MEMBERS {
        return Err(bad("too many team members"));
    }
    Ok(members.into_iter().collect())
}

pub async fn put_team(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    Path(team_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, Response> {
    if !state.config.team_repos {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "team repositories disabled").into_response());
    }
    if body.len() > 16 * 1024 || !is_hex(&team_id, 32) {
        return Err(bad("invalid team repository request"));
    }
    let service = state
        .config
        .private_service_pubkey
        .clone()
        .filter(|s| is_hex(s, 32) && nostr::PublicKey::from_hex(s).is_ok())
        .ok_or_else(unavailable)?;
    let (tenant, owner) = verified_caller(&state, &headers, &uri, "PUT", Some(&body)).await?;
    strict_json::validate(&body).map_err(|_| bad("invalid team repository request"))?;
    let request: PutTeam =
        serde_json::from_slice(&body).map_err(|_| bad("invalid team repository request"))?;
    if request.expected_version < 0 {
        return Err(bad("invalid team repository request"));
    }
    let members = member_list(&owner, request.members)?;
    if request.expected_version == 0 {
        // Never adopt an existing (public or job) repository's history as a team.
        if super::hydrate::load_manifest_for_read(&state.git_store, &tenant, &owner, &team_id)
            .await
            .map_err(|_| unavailable())?
            .is_some()
        {
            return Err((StatusCode::CONFLICT, "repository already exists").into_response());
        }
    }
    let team = state
        .db
        .put_team_repo(
            tenant.community(),
            &owner,
            &team_id,
            &members,
            &service,
            request.expected_version,
        )
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "team already exists, version changed, or id in use",
            )
                .into_response()
        })?;
    if request.expected_version == 0 {
        // A team repo is never announced, so seed the empty-manifest pointer here, as
        // private provisioning does; otherwise the first push reads "never existed".
        crate::handlers::side_effects::ensure_manifest_pointer(&state, &tenant, &owner, &team_id)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "team repository pointer seed failed");
                unavailable()
            })?;
    }
    Ok(view(&state, &tenant, &team))
}

pub async fn get_team(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    Path((owner, team_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, Response> {
    if !is_hex(&owner, 32) || !is_hex(&team_id, 32) {
        return Err(bad("invalid team repository request"));
    }
    let (tenant, actor) = verified_caller(&state, &headers, &uri, "GET", None).await?;
    let team = state
        .db
        .team_repo(tenant.community(), &owner, &team_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(deny)?;
    if !team.can_read(&actor) {
        return Err(deny());
    }
    Ok(view(&state, &tenant, &team))
}
