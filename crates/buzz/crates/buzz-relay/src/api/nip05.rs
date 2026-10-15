//! NIP-05 identity verification endpoint.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Json, Response},
};
use hex;
use serde::Deserialize;

use crate::state::AppState;

/// Query parameters for the NIP-05 identity verification endpoint.
#[derive(Deserialize)]
pub struct Nip05Query {
    /// The local part of the NIP-05 identifier to look up (e.g. `alice` from `alice@relay.example`).
    pub name: Option<String>,
}

/// `GET /.well-known/nostr.json` — NIP-05 identity verification.
/// No authentication required — public discovery endpoint.
pub async fn nostr_nip05(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<Nip05Query>,
) -> Response {
    let json = match (params.name, super::handles::tenant(&state, &headers).await) {
        (Some(requested), Ok(tenant)) => {
            let name = requested.to_ascii_lowercase();
            match state.db.lookup_handle(tenant.community(), &name).await {
                Ok(Some(pubkey)) => {
                    let hex_pubkey = hex::encode(pubkey);
                    let relay_url =
                        relay_url_for_tenant_host(&state.config.relay_url, tenant.host());
                    serde_json::json!({
                        "names": { (requested): hex_pubkey.clone() },
                        "relays": { (hex_pubkey): [relay_url] }
                    })
                }
                _ => serde_json::json!({ "names": {}, "relays": {} }),
            }
        }
        _ => serde_json::json!({ "names": {}, "relays": {} }),
    };

    let mut response = Json(json).into_response();
    response.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}

/// Build the relay URL advertised in the NIP-05 `relays` map for the bound
/// tenant. Preserve the deployment scheme from config, but never the configured
/// host: in a host-per-community deployment the request tenant host is the
/// relay identity clients must follow back.
pub(crate) fn relay_url_for_tenant_host(config_relay_url: &str, tenant_host: &str) -> String {
    let scheme = if config_relay_url.trim_start().starts_with("wss://") {
        "wss"
    } else {
        "ws"
    };
    format!("{scheme}://{tenant_host}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_url_for_tenant_host_uses_config_scheme_but_tenant_host() {
        assert_eq!(
            relay_url_for_tenant_host("wss://config.example", "tenant-b.example"),
            "wss://tenant-b.example"
        );
        assert_eq!(
            relay_url_for_tenant_host("ws://config.example", "localhost:3100"),
            "ws://localhost:3100"
        );
    }
}
