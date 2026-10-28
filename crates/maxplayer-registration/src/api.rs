//! HTTP API under `/account/*`.
//!
//! | Method | Path                          | Auth    | Purpose                                   |
//! |--------|-------------------------------|---------|-------------------------------------------|
//! | POST   | /account/link/request         | NIP-98  | email a 6-digit code (spec §3 steps 3–4)  |
//! | POST   | /account/link/confirm         | NIP-98  | check the code, link the key, NIP-43 add  |
//! | GET    | /account/status               | NIP-98  | this key's link (for `account status`)    |
//! | POST   | /account/marketing            | NIP-98  | product news on/off (`account marketing`) |
//! | POST   | /account/webhooks/resend      | Svix    | bounces, complaints, unsubscribes         |
//! | GET    | /account/health               | none    | liveness + database ping                  |
//!
//! NIP-98 `u` tags must be `<public_base_url><path>` exactly. Every error body is
//! `{"error": <stable code>, "message": <text>}` and never contains an email address.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::TimeDelta;
use nostr::PublicKey;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::AppState;
use crate::db::{self, ConfirmOutcome};
use crate::email::{CodeEmail, WelcomeEmail};
use crate::nip98;
use crate::sync;
use crate::validate;
use crate::webhook;

pub const PATH_LINK_REQUEST: &str = "/account/link/request";
pub const PATH_LINK_CONFIRM: &str = "/account/link/confirm";
pub const PATH_STATUS: &str = "/account/status";
pub const PATH_MARKETING: &str = "/account/marketing";
pub const PATH_WEBHOOK: &str = "/account/webhooks/resend";
pub const PATH_HEALTH: &str = "/account/health";

/// Request bodies are a few hundred bytes; Resend webhooks a few KiB.
const MAX_BODY: usize = 64 * 1024;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    retry_after: Option<i64>,
    extra: Option<serde_json::Value>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
            extra: None,
        }
    }
    fn bad(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }
    fn rate_limited(code: &'static str, retry_after: i64) -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            code,
            "too many requests, try again later",
        );
        e.retry_after = Some(retry_after.max(1));
        e
    }
    fn internal(err: anyhow::Error) -> Self {
        // The error chain can include SQL text but never row values; it is logged,
        // not returned.
        warn!(error = %format!("{err:#}"), "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.code, "message": self.message });
        if let (Some(serde_json::Value::Object(extra)), Some(obj)) =
            (self.extra, body.as_object_mut())
        {
            obj.extend(extra);
        }
        if let Some(r) = self.retry_after {
            body["retry_after_secs"] = json!(r);
        }
        let mut resp = (self.status, Json(body)).into_response();
        if let Some(v) = self
            .retry_after
            .and_then(|r| HeaderValue::from_str(&r.to_string()).ok())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        resp
    }
}

type ApiResult = Result<Response, ApiError>;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(PATH_LINK_REQUEST, post(link_request))
        .route(PATH_LINK_CONFIRM, post(link_confirm))
        .route(PATH_STATUS, get(status))
        .route(PATH_MARKETING, post(marketing))
        .route(PATH_WEBHOOK, post(resend_webhook))
        .route(PATH_HEALTH, get(health))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

/// Serve the API on `listener` (peer addresses are needed for per-IP limits).
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

// ------------------------------------------------------------------ helpers

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    method: &str,
    body: &[u8],
) -> Result<PublicKey, ApiError> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let verified = nip98::verify(
        auth,
        &state.cfg.public_url(path),
        method,
        body,
        state.clock.unix(),
        state.cfg.limits.nip98_max_skew_secs as u64,
    )
    .map_err(|e| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            e.code(),
            "NIP-98 authorization failed",
        )
    })?;
    if !db::mark_nip98_seen(&state.db, &verified.event_id.to_hex(), state.now()).await? {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "replayed_auth",
            "NIP-98 event already used",
        ));
    }
    Ok(verified.pubkey)
}

fn client_ip(state: &AppState, headers: &HeaderMap, peer: SocketAddr) -> String {
    let real_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<std::net::IpAddr>().ok());
    if let Some(ip) = real_ip.filter(|_| state.cfg.trust_x_real_ip) {
        return ip.to_string();
    }
    peer.ip().to_string()
}

/// Refuse when any bucket is at its hourly limit; otherwise count this request in
/// every bucket.
async fn rate_limit(
    state: &AppState,
    action: &'static str,
    buckets: &[(String, i32)],
    code: &'static str,
) -> Result<(), ApiError> {
    let now = state.now();
    let since = now - TimeDelta::hours(1);
    for (bucket, limit) in buckets {
        if db::count_rate(&state.db, bucket, action, since).await? >= i64::from(*limit) {
            info!(
                action,
                kind = bucket.split(':').next().unwrap_or(""),
                "rate limited"
            );
            return Err(ApiError::rate_limited(code, 3600));
        }
    }
    let names: Vec<String> = buckets.iter().map(|(b, _)| b.clone()).collect();
    db::record_rate(&state.db, &names, action, now).await?;
    Ok(())
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|_| ApiError::bad("invalid_body", "request body is not the expected JSON"))
}

fn ok(status: StatusCode, body: serde_json::Value) -> ApiResult {
    Ok((status, Json(body)).into_response())
}

fn marketing_label(c: &Option<db::Consent>) -> &'static str {
    match c {
        Some(c) if c.unsubscribed_at.is_none() => "subscribed",
        Some(_) => "unsubscribed",
        None => "none",
    }
}

// ------------------------------------------------------------------ link: request

#[derive(Deserialize)]
struct LinkRequest {
    email: String,
    label: String,
    terms_version: String,
    privacy_version: String,
    /// Version of the CLI's product-news notice text that was shown (spec §8.1).
    #[serde(default)]
    notice_version: Option<String>,
}

async fn link_request(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let pubkey = authenticate(&state, &headers, PATH_LINK_REQUEST, "POST", &body).await?;
    let pk_hex = pubkey.to_hex();
    let req: LinkRequest = parse_json(&body)?;
    let email = validate::normalize_email(&req.email)
        .ok_or_else(|| ApiError::bad("invalid_email", "not a valid email address"))?;
    let label = validate::normalize_label(&req.label).ok_or_else(|| {
        ApiError::bad(
            "invalid_label",
            "label must be 1-64 letters, digits, spaces, '.', '_' or '-'",
        )
    })?;
    let legal = &state.cfg.legal;
    if !legal.terms_versions.contains(&req.terms_version) {
        return Err(ApiError::bad(
            "unknown_terms_version",
            "terms version is not offered",
        ));
    }
    if !legal.privacy_versions.contains(&req.privacy_version) {
        return Err(ApiError::bad(
            "unknown_privacy_version",
            "privacy version is not offered",
        ));
    }
    let notice_version = match req.notice_version {
        Some(v) if legal.marketing_notice_versions.contains(&v) => v,
        Some(_) => {
            return Err(ApiError::bad(
                "unknown_notice_version",
                "notice version is not offered",
            ));
        }
        None => legal.default_notice_version().to_string(),
    };

    // v1 has no "change email" (spec §4): a key already linked to another address
    // is refused. This only reveals the caller's OWN key state, never whether the
    // submitted email is registered.
    let existing = db::get_registration(&state.db, &pk_hex).await?;
    if existing.is_some_and(|reg| reg.email != email) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "key_linked_to_other_email",
            "this key is already linked to a different email; changing it is not supported",
        ));
    }

    let now = state.now();
    let limits = &state.cfg.limits;
    if let Some(last) = db::last_code_created(&state.db, &pk_hex).await? {
        let wait = (last + TimeDelta::seconds(limits.resend_cooldown_secs) - now).num_seconds();
        if wait > 0 {
            return Err(ApiError::rate_limited("resend_cooldown", wait));
        }
    }
    let ip = client_ip(&state, &headers, peer);
    rate_limit(
        &state,
        "send",
        &[
            (format!("ip:{ip}"), limits.sends_per_ip_per_hour),
            (format!("pk:{pk_hex}"), limits.sends_per_pubkey_per_hour),
            (
                format!("email:{}", validate::email_bucket(&email)),
                limits.sends_per_email_per_hour,
            ),
        ],
        "rate_limited",
    )
    .await?;

    let code = validate::generate_code();
    let hash = validate::code_hash(&state.pepper, &pk_hex, &code);
    let code_id = db::issue_code(
        &state.db,
        &db::NewCode {
            email: &email,
            code_hash: &hash,
            pubkey: &pk_hex,
            label: &label,
            terms_version: &req.terms_version,
            privacy_version: &req.privacy_version,
            notice_version: &notice_version,
            expires_at: now + TimeDelta::seconds(limits.code_ttl_secs),
        },
        now,
    )
    .await?;

    let terms_url = legal.terms_url_for(&req.terms_version);
    let privacy_url = legal.privacy_url_for(&req.privacy_version);
    let npub_short = validate::npub_short(&pubkey);
    let sent = state
        .email
        .send_code(
            &email,
            &CodeEmail {
                code: &code,
                agent_label: &label,
                npub_short: &npub_short,
                terms_url: &terms_url,
                privacy_url: &privacy_url,
            },
            &format!("maxplayer-code-{code_id}"),
        )
        .await;
    if let Err(e) = sent {
        // An unsent code must not be confirmable.
        db::revoke_code(&state.db, code_id, state.now()).await?;
        warn!(pubkey = %pk_hex, error = %e, "code email failed");
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "email_send_failed",
            "could not send the code email, try again later",
        ));
    }
    info!(pubkey = %pk_hex, code_id, "code email sent");
    ok(
        StatusCode::ACCEPTED,
        json!({
            "status": "code_sent",
            "expires_in_secs": limits.code_ttl_secs,
            "max_attempts": limits.max_attempts,
            "terms_url": terms_url,
            "privacy_url": privacy_url,
        }),
    )
}

// ------------------------------------------------------------------ link: confirm

#[derive(Deserialize)]
struct LinkConfirm {
    code: String,
    /// `human` or `agent`, as reported by the CLI (spec §5 `code_entered_by`).
    #[serde(default)]
    entered_by: Option<String>,
}

async fn link_confirm(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let pubkey = authenticate(&state, &headers, PATH_LINK_CONFIRM, "POST", &body).await?;
    let pk_hex = pubkey.to_hex();
    let req: LinkConfirm = parse_json(&body)?;
    // Without a report the weaker claim is recorded.
    let entered_by = match req.entered_by.as_deref() {
        None => "agent",
        Some("human") => "human",
        Some("agent") => "agent",
        Some(_) => {
            return Err(ApiError::bad(
                "invalid_entered_by",
                "entered_by must be human or agent",
            ));
        }
    };
    let ip = client_ip(&state, &headers, peer);
    let limits = &state.cfg.limits;
    rate_limit(
        &state,
        "confirm",
        &[
            (format!("ip:{ip}"), limits.confirms_per_ip_per_hour),
            (format!("pk:{pk_hex}"), limits.confirms_per_pubkey_per_hour),
        ],
        "rate_limited",
    )
    .await?;

    // A malformed code still counts as a wrong try.
    let code = validate::normalize_code(&req.code);
    let outcome = db::confirm_code(
        &state.db,
        &state.pepper,
        &pk_hex,
        code.as_deref(),
        entered_by,
        limits.max_attempts,
        state.now(),
    )
    .await?;
    match outcome {
        ConfirmOutcome::NoPendingCode => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no_pending_code",
            "no code is waiting for this key; request a new one",
        )),
        ConfirmOutcome::Expired => Err(ApiError::new(
            StatusCode::GONE,
            "code_expired",
            "the code has expired; request a new one",
        )),
        ConfirmOutcome::Exhausted => Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_attempts",
            "too many wrong tries; request a new code",
        )),
        ConfirmOutcome::Wrong { attempts_left } => {
            let mut e = ApiError::bad("wrong_code", "that code is not right");
            e.extra = Some(json!({ "attempts_left": attempts_left }));
            Err(e)
        }
        ConfirmOutcome::ConflictingEmail => Err(ApiError::new(
            StatusCode::CONFLICT,
            "key_linked_to_other_email",
            "this key is already linked to a different email; changing it is not supported",
        )),
        ConfirmOutcome::Linked {
            registration,
            inserted,
            outbox_id,
            ..
        } => {
            info!(pubkey = %pk_hex, first_link = inserted, "key linked");
            let member = match sync::deliver(&state, outbox_id).await {
                Ok(true) => "added",
                Ok(false) => "pending",
                Err(e) => {
                    warn!(pubkey = %pk_hex, error = %format!("{e:#}"), "member add not delivered yet");
                    "pending"
                }
            };
            if inserted {
                let legal = &state.cfg.legal;
                let terms_url = legal.terms_url_for(&registration.terms_version);
                let privacy_url = legal.privacy_url_for(&registration.privacy_version);
                let welcome = WelcomeEmail {
                    agent_label: &registration.label,
                    terms_url: &terms_url,
                    privacy_url: &privacy_url,
                };
                if let Err(e) = state
                    .email
                    .send_welcome(
                        &registration.email,
                        &welcome,
                        &format!("maxplayer-welcome-{pk_hex}"),
                    )
                    .await
                {
                    // The link stands; the welcome email is informational.
                    warn!(pubkey = %pk_hex, error = %e, "welcome email failed");
                }
            }
            let consent = db::get_consent(&state.db, &registration.email).await?;
            ok(
                StatusCode::OK,
                json!({
                    "status": "linked",
                    "first_link": inserted,
                    "email": registration.email,
                    "label": registration.label,
                    "terms_version": registration.terms_version,
                    "privacy_version": registration.privacy_version,
                    "relay_member": member,
                    "marketing": marketing_label(&consent),
                }),
            )
        }
    }
}

// ------------------------------------------------------------------ status + marketing

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult {
    let pubkey = authenticate(&state, &headers, PATH_STATUS, "GET", b"").await?;
    let pk_hex = pubkey.to_hex();
    let Some(reg) = db::get_registration(&state.db, &pk_hex).await? else {
        return ok(StatusCode::OK, json!({ "registered": false }));
    };
    let consent = db::get_consent(&state.db, &reg.email).await?;
    let sync_state = db::member_sync_state(&state.db, &pk_hex).await?;
    ok(
        StatusCode::OK,
        json!({
            "registered": true,
            "email": reg.email,
            "label": reg.label,
            "terms_version": reg.terms_version,
            "privacy_version": reg.privacy_version,
            "accepted_at": reg.accepted_at.to_rfc3339(),
            "code_entered_by": reg.code_entered_by,
            "linked_at": reg.created_at.to_rfc3339(),
            "marketing": marketing_label(&consent),
            "relay_member": match sync_state { "done" => "added", "pending" => "pending", _ => "unknown" },
        }),
    )
}

#[derive(Deserialize)]
struct MarketingReq {
    enabled: bool,
    #[serde(default)]
    notice_version: Option<String>,
}

async fn marketing(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let pubkey = authenticate(&state, &headers, PATH_MARKETING, "POST", &body).await?;
    let req: MarketingReq = parse_json(&body)?;
    let Some(reg) = db::get_registration(&state.db, &pubkey.to_hex()).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_registered",
            "this key is not linked; run `maxplayer account link`",
        ));
    };
    let legal = &state.cfg.legal;
    let notice = match req.notice_version {
        Some(v) if legal.marketing_notice_versions.contains(&v) => v,
        Some(_) => {
            return Err(ApiError::bad(
                "unknown_notice_version",
                "notice version is not offered",
            ));
        }
        None => legal.default_notice_version().to_string(),
    };
    let now = state.now();
    if req.enabled {
        db::marketing_on(&state.db, &reg.email, &notice, now).await?;
    } else {
        db::marketing_off(&state.db, &reg.email, "cli", &notice, now).await?;
    }
    let consent = db::get_consent(&state.db, &reg.email).await?;
    ok(
        StatusCode::OK,
        json!({ "marketing": marketing_label(&consent) }),
    )
}

// ------------------------------------------------------------------ webhook + health

async fn resend_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let Some(secret) = state.webhook_secret.as_deref() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhook_not_configured",
            "webhook secret not configured",
        ));
    };
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    webhook::verify(
        secret,
        h("svix-id"),
        h("svix-timestamp"),
        h("svix-signature"),
        &body,
        state.now().timestamp(),
    )
    .map_err(|_| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "bad_webhook_signature",
            "webhook signature invalid",
        )
    })?;
    let id = h("svix-id").unwrap_or_default().to_string();
    let payload: serde_json::Value = parse_json(&body)?;
    let event_type = payload["type"]
        .as_str()
        .unwrap_or("unknown")
        .chars()
        .take(64)
        .collect::<String>();
    let now = state.now();
    if !db::mark_webhook_seen(&state.db, &id, &event_type, now).await? {
        return ok(StatusCode::OK, json!({ "status": "duplicate" }));
    }
    let at = payload["created_at"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or(now);
    let notice = state.cfg.legal.default_notice_version().to_string();
    match webhook::classify(&payload) {
        webhook::Action::Bounce { emails } => {
            for e in &emails {
                db::record_bounce(&state.db, e, at, &event_type, &id, now).await?;
            }
            info!(count = emails.len(), "bounce recorded");
        }
        webhook::Action::Complaint { emails } => {
            for e in &emails {
                db::record_bounce(&state.db, e, at, &event_type, &id, now).await?;
                db::marketing_off(&state.db, e, "complaint", &notice, now).await?;
            }
            info!(count = emails.len(), "complaint recorded");
        }
        webhook::Action::Unsubscribed { email } => {
            db::marketing_off(&state.db, &email, "link", &notice, now).await?;
            info!("unsubscribe recorded");
        }
        webhook::Action::Ignore => {}
    }
    ok(StatusCode::OK, json!({ "status": "ok" }))
}

async fn health(State(state): State<Arc<AppState>>) -> ApiResult {
    sqlx::query("SELECT 1")
        .execute(&state.db)
        .await
        .map_err(|e| ApiError::internal(e.into()))?;
    ok(StatusCode::OK, json!({ "ok": true }))
}
