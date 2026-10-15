//! Public handle discovery and NIP-98-owned registration (no relay membership required).
use super::{api_error, bridge, internal_error};
use crate::state::AppState;
use axum::{
    body::Bytes,
    extract::{ConnectInfo, OriginalUri, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use buzz_core::{tenant::normalize_host, TenantContext};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// Alias only the explicitly configured domain to the deployment community.
/// Forwarded headers are intentionally not authority; arbitrary hosts still fail closed.
pub(crate) async fn tenant(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<TenantContext, (StatusCode, Json<Value>)> {
    let host = normalize_host(
        headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or(""),
    );
    let lookup = handle_lookup_host(
        &host,
        state.config.nip05_domain.as_deref(),
        &state.config.relay_url,
    );
    crate::tenant::bind_community(&state.db, &lookup)
        .await
        .map_err(|_| api_error(StatusCode::NOT_FOUND, "not found"))
}
fn handle_lookup_host(host: &str, domain: Option<&str>, relay_url: &str) -> String {
    if !host.is_empty() && domain == Some(host) {
        crate::tenant::relay_url_authority(relay_url)
    } else {
        host.to_owned()
    }
}

/// Public availability request.
#[derive(Deserialize)]
pub struct AvailableQuery {
    /// Requested local part, normalized on input.
    pub name: String,
}
/// GET availability; the previous owner may still reclaim a cooldown name.
pub async fn available(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AvailableQuery>,
) -> ApiResult {
    let tenant = tenant(&state, &headers).await?;
    let Some(name) = buzz_db::handles::normalize_name(&q.name) else {
        return Ok(Json(json!({"available":false,"reason":"invalid"})));
    };
    let reason = state
        .db
        .handle_reason(tenant.community(), &name, None)
        .await
        .map_err(|e| internal_error(&e.to_string()))?;
    Ok(Json(
        json!({"available": reason.is_none(), "reason": reason}),
    ))
}
/// Claim body; the owner is exclusively the authenticated signer.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    /// Requested local part, normalized on input.
    pub name: String,
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    body: &[u8],
    peer: SocketAddr,
) -> Result<(TenantContext, nostr::PublicKey), (StatusCode, Json<Value>)> {
    let limited = state
        .admission_rate_limiter
        .check_handle_ip(&peer.ip())
        .await
        .map_err(|_| api_error(StatusCode::SERVICE_UNAVAILABLE, "rate limiter unavailable"))?;
    if !limited.allowed {
        return Err(api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many handle requests",
        ));
    }
    let tenant = tenant(state, headers).await?;
    // Sign the actual request authority, not the alias's storage authority.
    let host = normalize_host(
        headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or(""),
    );
    let path = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(uri.path());
    let scheme = if state.config.relay_url.trim_start().starts_with("wss://") {
        "https"
    } else {
        "http"
    };
    let url = format!("{scheme}://{host}{path}");
    let auth = bridge::verify_nip98_exempt_handles(headers, &url, body)?;
    let limited = state
        .admission_rate_limiter
        .check_handle_pubkey(&tenant, &auth.pubkey)
        .await
        .map_err(|_| api_error(StatusCode::SERVICE_UNAVAILABLE, "rate limiter unavailable"))?;
    if !limited.allowed {
        return Err(api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many handle requests",
        ));
    }
    bridge::check_nip98_replay(state, &tenant, auth.event_id_bytes).await?;
    Ok((tenant, auth.pubkey))
}
/// POST a claim or atomic rename authenticated with NIP-98.
pub async fn claim(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let (tenant, owner) = authenticate(&state, &headers, &uri, &body, peer).await?;
    let claim: Claim = serde_json::from_slice(&body)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid claim JSON"))?;
    let name = buzz_db::handles::normalize_name(&claim.name)
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "invalid"))?;
    if let Some(reason) = state
        .db
        .claim_handle(tenant.community(), &name, &owner.to_bytes())
        .await
        .map_err(|e| internal_error(&e.to_string()))?
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"available":false,"reason":reason})),
        ));
    }
    Ok(Json(json!({"name":name,"pubkey":owner.to_hex()})))
}
/// POST release of the authenticated signer's current handle.
pub async fn release(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let (tenant, owner) = authenticate(&state, &headers, &uri, &body, peer).await?;
    state
        .db
        .release_handle(tenant.community(), &owner.to_bytes())
        .await
        .map_err(|e| internal_error(&e.to_string()))?;
    Ok(Json(json!({"released":true})))
}
/// Bounded explicit-pubkey lookup request.
#[derive(Deserialize)]
pub struct BatchQuery {
    /// Comma-separated 32-byte hex pubkeys, at most 500.
    pub pubkeys: String,
}
/// GET public handles keyed by canonical hex pubkey.
pub async fn batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<BatchQuery>,
) -> ApiResult {
    let tenant = tenant(&state, &headers).await?;
    let keys = parse_pubkeys(&q.pubkeys)
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "expected at most 500 hex pubkeys"))?;
    let rows = state
        .db
        .batch_handles(tenant.community(), &keys)
        .await
        .map_err(|e| internal_error(&e.to_string()))?;
    Ok(Json(Value::Object(
        rows.into_iter()
            .map(|(key, name)| (hex::encode(key), Value::String(name)))
            .collect(),
    )))
}
fn parse_pubkeys(raw: &str) -> Option<Vec<Vec<u8>>> {
    if raw.is_empty() {
        return Some(vec![]);
    }
    let parts: Vec<_> = raw.split(',').collect();
    if parts.len() > 500 {
        return None;
    }
    parts
        .into_iter()
        .map(|s| {
            if s.len() == 64 {
                hex::decode(s).ok()
            } else {
                None
            }
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batch_input_is_bounded() {
        assert!(parse_pubkeys("").unwrap().is_empty());
        assert!(parse_pubkeys("nothex").is_none());
        assert!(parse_pubkeys(&vec!["ab".repeat(32); 500].join(",")).is_some());
        assert!(parse_pubkeys(&vec!["ab".repeat(32); 501].join(",")).is_none());
    }
    #[test]
    fn domain_alias_does_not_create_a_default_tenant() {
        assert_eq!(
            handle_lookup_host(
                "maxplayer.ai",
                Some("maxplayer.ai"),
                "wss://relay.maxplayer.ai"
            ),
            "relay.maxplayer.ai"
        );
        for host in ["relay.maxplayer.ai", "other.example", ""] {
            assert_eq!(
                handle_lookup_host(host, Some("maxplayer.ai"), "wss://relay.maxplayer.ai"),
                host
            );
        }
    }
}
