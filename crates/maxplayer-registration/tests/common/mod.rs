//! Integration-test harness: a fresh Postgres database + non-superuser role per test,
//! a fake relay that applies buzz-relay's NIP-43 admin rules, and a fake Resend API.
//!
//! Needs `MAXPLAYER_REGISTRATION_TEST_DATABASE_URL`: a Postgres URL for a role that may
//! create roles and databases (CI: the `postgres` service container; locally see
//! README "Tests"). Without it the tests FAIL rather than pass silently.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use maxplayer_registration::config::Config;
use maxplayer_registration::{AppState, Clock, api, nip98};
use nostr::{
    Alphabet, Event, EventBuilder, JsonUtil, Keys, Kind, SingleLetterTag, Tag, TagKind, Timestamp,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub const ENV_DB: &str = "MAXPLAYER_REGISTRATION_TEST_DATABASE_URL";
pub const RESEND_KEY: &str = "re_test_key_not_real";
pub const WEBHOOK_SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
pub const ROLE_PASSWORD: &str = "regtest-password";

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).unwrap();
    hex::encode(b)
}

/// A fresh database owned by a fresh NOSUPERUSER role, and a pool connected AS that role.
pub async fn fresh_db() -> PgPool {
    let admin_url = std::env::var(ENV_DB).unwrap_or_else(|_| {
        panic!("{ENV_DB} is not set: these tests need a Postgres admin URL (see crates/maxplayer-registration/README.md)")
    });
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect admin db");
    let name = format!("regtest_{}", rand_hex(6));
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {name} LOGIN PASSWORD '{ROLE_PASSWORD}' NOSUPERUSER NOCREATEDB NOCREATEROLE"
    )))
    .execute(&admin)
    .await
    .expect("create role");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE {name} OWNER {name}"
    )))
    .execute(&admin)
    .await
    .expect("create database");
    admin.close().await;
    let opts = PgConnectOptions::from_str(&admin_url)
        .unwrap()
        .username(&name)
        .password(ROLE_PASSWORD)
        .database(&name);
    PgPoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .expect("connect as service role")
}

async fn bind() -> (tokio::net::TcpListener, String) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    (l, base)
}

fn json_err(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

// ------------------------------------------------------------------ fake relay

/// Mirrors buzz-relay `handlers/relay_admin.rs` for 9030/9031 received over the
/// HTTP bridge (`POST /events`, NIP-98): signer must equal the NIP-98 key and hold
/// `admin`/`owner`, `created_at` within ±120 s, target = first `p` tag, add of an
/// existing member is a silent no-op, remove of a missing one is
/// `invalid: member not found: <hex>`. `POST /query` returns a relay-signed 13534.
pub struct FakeRelay {
    pub base: String,
    pub keys: Keys,
    pub members: Mutex<BTreeMap<String, String>>,
    pub events: Mutex<Vec<Event>>,
    pub fail_next: AtomicUsize,
    pub queries: AtomicUsize,
    /// Origin the relay expects in NIP-98 `u` tags (default: its own address).
    pub expect_origin: Mutex<String>,
    /// When set, every request must carry this `Host` header (buzz binds the tenant
    /// from Host, so a loopback request must name the public host).
    pub expect_host: Mutex<Option<String>>,
    clock: Arc<Clock>,
}

impl FakeRelay {
    // Test-only fake: returning the ready-made refusal response keeps the handlers short.
    #[allow(clippy::result_large_err)]
    fn check(
        &self,
        headers: &HeaderMap,
        path: &str,
        body: &[u8],
    ) -> Result<nip98::Verified, axum::response::Response> {
        if let Some(host) = self.expect_host.lock().unwrap().clone() {
            let got = headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if got != host {
                return Err(json_err(
                    StatusCode::NOT_FOUND,
                    "relay: no community is configured for this host",
                ));
            }
        }
        let url = format!("{}{path}", self.expect_origin.lock().unwrap());
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
        nip98::verify(auth, &url, "POST", body, self.clock.unix(), 60)
            .map_err(|e| json_err(StatusCode::UNAUTHORIZED, &format!("NIP-98: {}", e.code())))
    }
}

impl FakeRelay {
    pub fn is_member(&self, pk: &str) -> bool {
        self.members.lock().unwrap().contains_key(pk)
    }
    pub fn commands(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

async fn relay_events(
    State(r): State<Arc<FakeRelay>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let verified = match r.check(&headers, "/events", &body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Ok(event) = Event::from_json(&body) else {
        return json_err(StatusCode::BAD_REQUEST, "invalid event");
    };
    if event.verify().is_err() {
        return json_err(StatusCode::BAD_REQUEST, "invalid: bad signature");
    }
    r.events.lock().unwrap().push(event.clone());
    // Injected outage: the command was received but not applied.
    if r.fail_next.load(Ordering::SeqCst) > 0 {
        r.fail_next.fetch_sub(1, Ordering::SeqCst);
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal server error");
    }
    if event.pubkey != verified.pubkey {
        return json_err(
            StatusCode::FORBIDDEN,
            "invalid: event pubkey does not match authenticated identity",
        );
    }
    let kind = event.kind.as_u16();
    if kind != 9030 && kind != 9031 {
        return json_err(StatusCode::BAD_REQUEST, "invalid: unexpected kind");
    }
    let delta = event.created_at.as_secs() as i64 - r.clock.unix() as i64;
    if delta.abs() > 120 {
        return json_err(
            StatusCode::BAD_REQUEST,
            &format!("invalid: event timestamp out of range: delta={delta}s (max ±120s)"),
        );
    }
    let sender = event.pubkey.to_hex();
    let target = event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        (s.first().map(String::as_str) == Some("p"))
            .then(|| s.get(1).cloned())
            .flatten()
    });
    let Some(target) = target.filter(|t| t.len() == 64) else {
        return json_err(StatusCode::BAD_REQUEST, "invalid: missing or invalid p tag");
    };
    let mut members = r.members.lock().unwrap();
    let role = members.get(&sender).cloned().unwrap_or_default();
    if role != "admin" && role != "owner" {
        return json_err(
            StatusCode::BAD_REQUEST,
            "invalid: actor not authorized: must be admin or owner",
        );
    }
    if kind == 9030 {
        members.entry(target).or_insert_with(|| "member".into());
    } else {
        if target == sender {
            return json_err(StatusCode::BAD_REQUEST, "invalid: cannot remove yourself");
        }
        match members.get(&target).map(String::as_str) {
            None => {
                return json_err(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid: member not found: {target}"),
                );
            }
            Some("owner") => {
                return json_err(
                    StatusCode::BAD_REQUEST,
                    "invalid: cannot remove the relay owner",
                );
            }
            Some("admin") if role == "admin" => {
                return json_err(
                    StatusCode::BAD_REQUEST,
                    "invalid: actor not authorized: admins can only remove members",
                );
            }
            _ => {
                members.remove(&target);
            }
        }
    }
    Json(json!({ "event_id": event.id.to_hex(), "accepted": true, "message": "" })).into_response()
}

async fn relay_query(
    State(r): State<Arc<FakeRelay>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    if let Err(resp) = r.check(&headers, "/query", &body) {
        return resp;
    }
    r.queries.fetch_add(1, Ordering::SeqCst);
    let mut tags = vec![Tag::protected()];
    for (pk, role) in r.members.lock().unwrap().iter() {
        tags.push(Tag::parse(["member", pk.as_str(), role.as_str()]).unwrap());
    }
    let ev = EventBuilder::new(Kind::Custom(13534), "")
        .tags(tags)
        .sign_with_keys(&r.keys)
        .unwrap();
    Json(json!([
        serde_json::from_str::<Value>(&ev.as_json()).unwrap()
    ]))
    .into_response()
}

// ------------------------------------------------------------------ fake Resend

#[derive(Debug, Clone)]
pub struct SentEmail {
    pub body: Value,
    pub idempotency_key: String,
}

impl SentEmail {
    pub fn to(&self) -> &str {
        self.body["to"][0].as_str().unwrap_or_default()
    }
    pub fn template(&self) -> &str {
        self.body["template"]["id"].as_str().unwrap_or_default()
    }
    pub fn var(&self, k: &str) -> &str {
        self.body["template"]["variables"][k]
            .as_str()
            .unwrap_or_default()
    }
}

pub struct FakeResend {
    pub sent: Mutex<Vec<SentEmail>>,
    pub fail_next: AtomicUsize,
}

async fn resend_emails(
    State(r): State<Arc<FakeResend>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {RESEND_KEY}"))
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"name":"validation_error","message":"API key is invalid"})),
        )
            .into_response();
    }
    if r.fail_next.load(Ordering::SeqCst) > 0 {
        r.fail_next.fetch_sub(1, Ordering::SeqCst);
        let to = body["to"][0].as_str().unwrap_or_default();
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"name":"internal_server_error","message":format!("failed for {to}")})),
        )
            .into_response();
    }
    if body.get("html").is_some() || body["template"]["id"].as_str().is_none() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"name":"validation_error"})),
        )
            .into_response();
    }
    let idem = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    r.sent.lock().unwrap().push(SentEmail {
        body,
        idempotency_key: idem,
    });
    Json(json!({ "id": format!("email_{}", rand_hex(8)) })).into_response()
}

// ------------------------------------------------------------------ the app

pub struct TestApp {
    pub state: Arc<AppState>,
    pub base: String,
    pub http: reqwest::Client,
    pub relay: Arc<FakeRelay>,
    pub resend: Arc<FakeResend>,
    pub admin: Keys,
    pub db: PgPool,
}

pub struct Resp {
    pub status: u16,
    pub body: Value,
    pub headers: reqwest::header::HeaderMap,
}

impl TestApp {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(tweak: impl FnOnce(&mut Config)) -> Self {
        let db = fresh_db().await;
        maxplayer_registration::migrate(&db)
            .await
            .expect("migrations as non-superuser");
        let clock = Arc::new(Clock::default());
        let admin = Keys::generate();

        let (relay_l, relay_base) = bind().await;
        let relay = Arc::new(FakeRelay {
            base: relay_base.clone(),
            keys: Keys::generate(),
            members: Mutex::new(BTreeMap::from([(
                admin.public_key().to_hex(),
                "admin".to_string(),
            )])),
            events: Mutex::new(Vec::new()),
            fail_next: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            expect_origin: Mutex::new(relay_base.clone()),
            expect_host: Mutex::new(None),
            clock: clock.clone(),
        });
        let router = axum::Router::new()
            .route("/events", post(relay_events))
            .route("/query", post(relay_query))
            .with_state(relay.clone());
        tokio::spawn(async move { axum::serve(relay_l, router).await.unwrap() });

        let (resend_l, resend_base) = bind().await;
        let resend = Arc::new(FakeResend {
            sent: Mutex::new(Vec::new()),
            fail_next: AtomicUsize::new(0),
        });
        let router = axum::Router::new()
            .route("/emails", post(resend_emails))
            .with_state(resend.clone());
        tokio::spawn(async move { axum::serve(resend_l, router).await.unwrap() });

        let mut cfg = Config::default();
        cfg.relay.base_url = relay_base;
        cfg.relay.relay_pubkey = Some(relay.keys.public_key().to_hex());
        cfg.email.resend_api_base = resend_base;
        tweak(&mut cfg);
        cfg.validate().expect("test config valid");

        let webhook = maxplayer_registration::webhook::parse_secret(WEBHOOK_SECRET);
        let mut state = AppState::new(
            cfg,
            db.clone(),
            admin.clone(),
            Some(RESEND_KEY.into()),
            webhook,
        )
        .unwrap();
        state.clock = clock;
        let state = Arc::new(state);
        let (api_l, base) = bind().await;
        let s = state.clone();
        tokio::spawn(async move { api::serve(api_l, s, std::future::pending()).await.unwrap() });

        Self {
            state,
            base,
            http: reqwest::Client::new(),
            relay,
            resend,
            admin,
            db,
        }
    }

    pub fn now_ts(&self) -> Timestamp {
        Timestamp::from(self.state.clock.unix())
    }

    /// A NIP-98 header for the service's public URL of `path`, with a random `nonce`
    /// tag so identical requests in the same second are distinct events.
    pub fn auth(&self, keys: &Keys, path: &str, method: &str, body: Option<&[u8]>) -> String {
        auth_header(
            keys,
            &self.state.cfg.public_url(path),
            method,
            body,
            self.now_ts(),
        )
    }

    pub async fn send(
        &self,
        method: &str,
        path: &str,
        auth: Option<String>,
        body: Option<Vec<u8>>,
        extra: &[(&str, String)],
    ) -> Resp {
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => self.http.get(url),
            _ => self.http.post(url),
        };
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        for (k, v) in extra {
            req = req.header(*k, v);
        }
        if let Some(b) = body {
            req = req.header("content-type", "application/json").body(b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Resp {
            status,
            body,
            headers,
        }
    }

    pub async fn post(&self, keys: &Keys, path: &str, body: Value) -> Resp {
        let b = body.to_string().into_bytes();
        let auth = self.auth(keys, path, "POST", Some(&b));
        self.send("POST", path, Some(auth), Some(b), &[]).await
    }

    pub async fn get(&self, keys: &Keys, path: &str) -> Resp {
        let auth = self.auth(keys, path, "GET", None);
        self.send("GET", path, Some(auth), None, &[]).await
    }

    pub async fn request_link(&self, keys: &Keys, email: &str, label: &str) -> Resp {
        self.post(
            keys,
            api::PATH_LINK_REQUEST,
            json!({ "email": email, "label": label, "terms_version": "20261029", "privacy_version": "20261029" }),
        )
        .await
    }

    pub async fn confirm(&self, keys: &Keys, code: &str) -> Resp {
        self.post(
            keys,
            api::PATH_LINK_CONFIRM,
            json!({ "code": code, "entered_by": "human" }),
        )
        .await
    }

    pub fn emails(&self) -> Vec<SentEmail> {
        self.resend.sent.lock().unwrap().clone()
    }

    /// The CODE from the newest code email sent to `email`.
    pub fn last_code(&self, email: &str) -> String {
        self.emails()
            .iter()
            .rev()
            .find(|e| e.to() == email && e.template() == "maxplayer-code")
            .map(|e| e.var("CODE").to_string())
            .expect("a code email")
    }

    /// Request + confirm. Returns the confirm response.
    pub async fn link(&self, keys: &Keys, email: &str, label: &str) -> Resp {
        let r = self.request_link(keys, email, label).await;
        assert_eq!(r.status, 202, "{:?}", r.body);
        let code = self.last_code(&email.trim().to_lowercase());
        self.confirm(keys, &code).await
    }

    pub async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.db).await.unwrap()
    }
}

pub fn auth_header(
    keys: &Keys,
    url: &str,
    method: &str,
    body: Option<&[u8]>,
    ts: Timestamp,
) -> String {
    let mut tags = vec![
        Tag::custom(
            TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U)),
            [url.to_string()],
        ),
        Tag::custom(TagKind::Method, [method.to_string()]),
        Tag::custom(TagKind::custom("nonce"), [rand_hex(8)]),
    ];
    if let Some(b) = body {
        tags.push(Tag::custom(TagKind::Payload, [nip98::body_sha256_hex(b)]));
    }
    let ev = EventBuilder::new(Kind::HttpAuth, "")
        .tags(tags)
        .custom_created_at(ts)
        .sign_with_keys(keys)
        .unwrap();
    format!("Nostr {}", STANDARD.encode(ev.as_json()))
}

/// Decode a header, let `f` edit the event JSON, re-encode (signature untouched).
pub fn tamper(header: &str, f: impl FnOnce(&mut Value)) -> String {
    let raw = STANDARD
        .decode(header.strip_prefix("Nostr ").unwrap())
        .unwrap();
    let mut v: Value = serde_json::from_slice(&raw).unwrap();
    f(&mut v);
    format!("Nostr {}", STANDARD.encode(v.to_string()))
}

pub fn _unused(_: SocketAddr) {}
