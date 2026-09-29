//! Private-job repository provisioning and mandatory pre-hydration authorization.
use crate::state::AppState;
use axum::{
    Json,
    body::Bytes,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine;
use buzz_core::TenantContext;
use buzz_db::private_jobs::PrivateJobRepo;
use maxplayer_private_protocol::{
    is_hex, strict_json, MAX_FILE_BYTES, MAX_OBJECTS, MAX_REPO_BYTES,
    wire::{self, HostPolicy},
};
use nostr::{Event, JsonUtil};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
fn deny() -> Response {
    (StatusCode::FORBIDDEN, "private repository access denied").into_response()
}
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "private repository policy unavailable",
    )
        .into_response()
}

/// This lookup always runs, including with the development flag off. Disabling a
/// feature must NEVER turn already-created private repositories public.
pub async fn authorize(
    state: &AppState,
    tenant: &TenantContext,
    buyer: &str,
    repo: &str,
    actor: &str,
    write: bool,
) -> Result<Option<PrivateJobRepo>, Response> {
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    // Random 64-hex job repos are a reserved namespace, and cannot fall through to
    // generic public hosting when a record is missing or provisioning is incomplete.
    if !is_hex(repo, 32) {
        return Ok(None);
    }
    let job = state
        .db
        .private_job_repo(tenant.community(), buyer, repo)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(deny)?;
    let allowed = if write {
        !job.closed
            && (actor == job.buyer && !job.input_frozen && job.target.is_some()
                || job.seller.as_deref() == Some(actor) && job.award_id.is_some())
    } else {
        job.can_read(actor)
    };
    if !allowed {
        return Err(deny());
    }
    Ok(Some(job))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provision {
    signed_offer: serde_json::Value,
    signed_award: Option<serde_json::Value>,
    signed_claim: Option<serde_json::Value>,
}
/// Bounded JSON uses strict API NIP-98 verification, not GitAuth's deliberate
/// streaming-transport method/replay exceptions. PUT URL, body and replay are bound.
pub async fn provision(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    Path(job_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, Response> {
    if !state.config.private_job_repos {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "private job repositories disabled",
        )
            .into_response());
    }
    if body.len() > 128 * 1024 || !is_hex(&job_id, 32) {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid private repository request",
        )
            .into_response());
    }
    let service = state
        .config
        .private_service_pubkey
        .as_deref()
        .filter(|s| is_hex(s, 32) && nostr::PublicKey::from_hex(s).is_ok())
        .ok_or_else(unavailable)?;
    if uri.query().is_some() {
        return Err(deny());
    }
    let tenant = crate::tenant::bind_community(
        &state.db,
        headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    )
    .await
    .map_err(|_| deny())?;
    let url = crate::api::bridge::nip98_expected_url(&state.config.relay_url, &tenant, uri.path());
    let (pubkey, event_id) = crate::api::bridge::verify_bridge_auth_with_options(
        &headers,
        "PUT",
        &url,
        Some(&body),
        true,
        true,
    )
    .map_err(IntoResponse::into_response)?;
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Nostr "))
        .ok_or_else(deny)?;
    let token = base64::engine::general_purpose::STANDARD
        .decode(token)
        .map_err(|_| deny())?;
    strict_json::validate(&token).map_err(|_| deny())?;
    let token = Event::from_json(token).map_err(|_| deny())?;
    for name in ["u", "method", "payload"] {
        let matching: Vec<_> = token
            .tags
            .iter()
            .filter(|t| t.as_slice().first().is_some_and(|v| v == name))
            .collect();
        if matching.len() != 1 || matching[0].as_slice().len() != 2 {
            return Err(deny());
        }
    }
    crate::api::bridge::check_nip98_replay(&state, &tenant, event_id)
        .await
        .map_err(IntoResponse::into_response)?;
    let event_auth = crate::handlers::auth::extract_auth_tag_json(&token);
    let auth_tag = event_auth
        .as_deref()
        .or_else(|| headers.get("x-auth-tag").and_then(|v| v.to_str().ok()));
    crate::api::relay_members::enforce_relay_membership(
        &state,
        tenant.community(),
        pubkey.as_bytes(),
        auth_tag,
    )
    .await
    .map_err(|_| deny())?;
    let digests: Vec<_> = token
        .tags
        .iter()
        .filter(|t| t.as_slice().first().is_some_and(|s| s == "payload"))
        .collect();
    let digest = hex::encode(Sha256::digest(&body));
    if digests.len() != 1 || digests[0].as_slice() != ["payload", digest.as_str()] {
        return Err(deny());
    }
    strict_json::validate(&body).map_err(|_| deny())?;
    let request: Provision = serde_json::from_slice(&body).map_err(|_| deny())?;
    let offer = wire::parse_signed(&request.signed_offer.to_string()).map_err(|_| deny())?;
    let prefix = format!("https://{}/git/", tenant.host());
    let host = HostPolicy {
        git_prefix: prefix.clone(),
        accepted_mints: vec![],
    };
    let tags = wire::validate_private(&offer, &host).map_err(|_| deny())?;
    if offer.kind.as_u16() != 3401 || tags.get("job") != Some(job_id.as_str()) {
        return Err(deny());
    }
    let buyer = offer.pubkey.to_hex();
    let actor = pubkey.to_hex();
    let target = tags.participants.iter().next().cloned();
    let mut job = PrivateJobRepo {
        buyer: buyer.clone(),
        job_id: job_id.clone(),
        offer_id: offer.id.to_hex(),
        target,
        seller: None,
        award_id: None,
        service: service.into(),
        closed: false,
        input_frozen: false,
    };
    if let Some(value) = request.signed_award {
        let award = wire::parse_signed(&value.to_string()).map_err(|_| deny())?;
        let claim = wire::parse_signed(&request.signed_claim.ok_or_else(deny)?.to_string())
            .map_err(|_| deny())?;
        let a = wire::validate_private(&award, &host).map_err(|_| deny())?;
        let c = wire::validate_private(&claim, &host).map_err(|_| deny())?;
        let seller = claim.pubkey.to_hex();
        let parties = std::collections::BTreeSet::from([buyer.clone(), seller.clone()]);
        if award.kind.as_u16() != 3405
            || claim.kind.as_u16() != 3402
            || award.pubkey != offer.pubkey
            || a.get("root") != Some(job.offer_id.as_str())
            || c.get("root") != Some(job.offer_id.as_str())
            || a.get("job") != Some(job_id.as_str())
            || c.get("job") != Some(job_id.as_str())
            || a.get("claim") != Some(claim.id.to_hex().as_str())
            || a.participants != parties
            || !c.participants.contains(&buyer)
            || c.participants.iter().any(|p| !parties.contains(p))
            || job.target.as_ref().is_some_and(|t| t != &seller)
            || (actor != buyer && actor != seller)
        {
            return Err(deny());
        }
        job.seller = Some(seller);
        job.award_id = Some(award.id.to_hex());
        job.input_frozen = true;
    } else if actor != buyer || job.target.is_none() || request.signed_claim.is_some() {
        return Err(deny());
    }
    // Never adopt an existing public repository's history as a private job.
    // New public announcements cannot enter this reserved namespace.
    if state
        .db
        .private_job_repo(tenant.community(), &buyer, &job_id)
        .await
        .map_err(|_| unavailable())?
        .is_none()
        && super::hydrate::load_manifest_for_read(&state.git_store, &tenant, &buyer, &job_id)
            .await
            .map_err(|_| unavailable())?
            .is_some()
    {
        return Err((
            StatusCode::CONFLICT,
            "repository already exists outside job binding",
        )
            .into_response());
    }
    if !state
        .db
        .ensure_private_job_repo(tenant.community(), &job)
        .await
        .map_err(|_| unavailable())?
    {
        return Err((StatusCode::CONFLICT, "private repository binding conflict").into_response());
    }
    // Announce seeds the empty-manifest pointer for a public repo; a private repo is
    // never announced, so seed it here, after the row exists. A missing pointer reads
    // as "repository never existed" (`info/refs` → 404), which would fail every first
    // push. Tolerant: a repeated or award-time provision leaves an existing pointer as-is.
    crate::handlers::side_effects::ensure_manifest_pointer(&state, &tenant, &buyer, &job_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "private repository pointer seed failed");
            unavailable()
        })?;
    Ok(Json(
        serde_json::json!({"repo":format!("{prefix}{buyer}/{job_id}"),"job_id":job_id,"max_file_bytes":MAX_FILE_BYTES,"max_repository_bytes":MAX_REPO_BYTES,"max_objects":MAX_OBJECTS,"max_pack_bytes":state.config.git_max_pack_bytes,"max_compressed_repository_bytes":state.config.git_max_repo_bytes}),
    ))
}

/// Compatibility entry point for private integration tests; production pushes use
/// the common quota checker for both public and private repositories.
#[cfg(test)]
pub async fn enforce_quota(repo: &std::path::Path) -> Result<(), Response> {
    super::quota::enforce(repo, true).await
}

/// All smart-HTTP responses are authenticated. Never place packs, ref advertisements
/// or even authorization errors in an intermediary cache shared with another key.
pub async fn no_store(mut response: Response) -> Response {
    use axum::http::{HeaderValue, header};
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Authorization"));
    response
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    fn git(path: &std::path::Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .current_dir(path)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        dir
    }
    fn commit(path: &std::path::Path) {
        git(path, &["add", "--all"]);
        git(
            path,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "test",
            ],
        );
    }
    #[tokio::test]
    async fn quota_counts_uncompressed_blobs_and_every_snapshot() {
        let dir = repo();
        std::fs::write(dir.path().join("small"), b"okay").unwrap();
        commit(dir.path());
        assert!(enforce_quota(dir.path()).await.is_ok());
        std::fs::write(
            dir.path().join("oversized"),
            vec![0u8; MAX_FILE_BYTES as usize + 1],
        )
        .unwrap();
        commit(dir.path());
        // Compresses to almost nothing: compressed pack length is not the quota.
        git(dir.path(), &["gc", "--quiet"]);
        assert_eq!(
            enforce_quota(dir.path()).await.unwrap_err().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        git(dir.path(), &["rm", "-q", "oversized"]);
        commit(dir.path());
        // Deleting a file does not reset the cumulative retained-object quota.
        assert!(enforce_quota(dir.path()).await.is_err());
    }
    #[tokio::test]
    async fn more_than_thousand_files_are_allowed_but_private_symlinks_are_not() {
        let dir = repo();
        for i in 0..1001 {
            std::fs::write(dir.path().join(format!("f{i}")), b"x").unwrap();
        }
        commit(dir.path());
        enforce_quota(dir.path()).await.unwrap();
        #[cfg(unix)]
        {
            let dir = repo();
            std::os::unix::fs::symlink("/etc/passwd", dir.path().join("link")).unwrap();
            commit(dir.path());
            assert_eq!(
                enforce_quota(dir.path()).await.unwrap_err().status(),
                StatusCode::BAD_REQUEST
            );
        }
    }
    #[tokio::test]
    async fn responses_cannot_be_shared_between_authorization_keys() {
        let response = no_store((StatusCode::FORBIDDEN, "denied").into_response()).await;
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        assert_eq!(response.headers()["vary"], "Authorization");
    }
}
