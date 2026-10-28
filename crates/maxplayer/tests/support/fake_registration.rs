//! A fake of the email registration service (`crates/maxplayer-registration`) for CLI tests.
//!
//! It speaks the service's README contract on `127.0.0.1` and applies the same NIP-98 rules as its
//! `nip98.rs` (kind 27235, valid signature, `created_at` within 60 s, exactly one `u` tag equal to
//! the PUBLIC URL of the route, one `method` tag, a `payload` tag equal to the body's SHA-256 when
//! there is a body, each event accepted once) and the same error codes and limits as its `api.rs`
//! (6-digit code, 5 tries, 10 minutes, 60 s resend cooldown, 5 sends per key per hour, 409 for a key
//! linked to another email). It never sends email: the "emailed" code is set by the test.
//!
//! Shared by path from `src/account_cli.rs` (interactive unit tests) and `tests/account_cli.rs`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use nostr_sdk::nostr::base64::Engine as _;
use nostr_sdk::nostr::base64::engine::general_purpose::STANDARD;
use nostr_sdk::nostr::hashes::Hash as _;
use nostr_sdk::nostr::hashes::sha256;
use nostr_sdk::prelude::{Event, JsonUtil, Kind};
use serde_json::{Value, json};

pub const MAX_ATTEMPTS: u32 = 5;
pub const CODE_TTL_SECS: u64 = 600;

#[derive(Debug, Clone)]
pub struct Pending {
    pub email: String,
    pub label: String,
    pub code: String,
    pub attempts: u32,
    pub expires_at: u64,
    pub created_at: u64,
}

#[derive(Debug, Clone)]
pub struct Registration {
    pub email: String,
    pub label: String,
    pub entered_by: String,
    pub terms_version: String,
    pub privacy_version: String,
    pub notice_version: String,
}

/// One request as received: the decoded NIP-98 event and the exact body bytes.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub auth: Option<Event>,
    pub body: Vec<u8>,
    /// The error code returned, or `None` on success.
    pub error: Option<String>,
}

struct State {
    public_base: String,
    next_code: Option<String>,
    pending: HashMap<String, Pending>,
    regs: HashMap<String, Registration>,
    /// email -> subscribed
    marketing: HashMap<String, bool>,
    seen: HashSet<String>,
    sends: HashMap<String, u32>,
    send_limit: u32,
    cooldown_secs: u64,
    confirm_calls: usize,
    requests: Vec<Recorded>,
}

pub struct FakeRegistration {
    base: String,
    state: Arc<Mutex<State>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Answer {
    status: u16,
    body: Value,
    retry_after: Option<u64>,
}

fn ok(status: u16, body: Value) -> Answer {
    Answer {
        status,
        body,
        retry_after: None,
    }
}

fn fail(status: u16, code: &str, message: &str) -> Answer {
    ok(status, json!({ "error": code, "message": message }))
}

fn limited(code: &str, retry_after: u64) -> Answer {
    let retry_after = retry_after.max(1);
    Answer {
        status: 429,
        body: json!({
            "error": code,
            "message": "too many requests, try again later",
            "retry_after_secs": retry_after,
        }),
        retry_after: Some(retry_after),
    }
}

impl FakeRegistration {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake registration");
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}/account");
        let state = Arc::new(Mutex::new(State {
            public_base: base.clone(),
            next_code: None,
            pending: HashMap::new(),
            regs: HashMap::new(),
            marketing: HashMap::new(),
            seen: HashSet::new(),
            sends: HashMap::new(),
            send_limit: 5,
            cooldown_secs: 60,
            confirm_calls: 0,
            requests: Vec::new(),
        }));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self { base, state }
    }

    /// What `registration_url` must be set to.
    pub fn base_url(&self) -> String {
        self.base.clone()
    }

    /// The code the next `link/request` "emails".
    pub fn set_next_code(&self, code: &str) {
        self.state.lock().unwrap().next_code = Some(code.to_owned());
    }

    /// Pretend the service's public URL is something else (a client signing the wrong URL).
    pub fn set_public_base(&self, base: &str) {
        self.state.lock().unwrap().public_base = base.trim_end_matches('/').to_owned();
    }

    pub fn set_cooldown_secs(&self, secs: u64) {
        self.state.lock().unwrap().cooldown_secs = secs;
    }

    pub fn set_send_limit(&self, limit: u32) {
        self.state.lock().unwrap().send_limit = limit;
    }

    /// Let every waiting code run out.
    pub fn expire_codes(&self) {
        for pending in self.state.lock().unwrap().pending.values_mut() {
            pending.expires_at = 0;
        }
    }

    /// An existing link (for the 409 path).
    pub fn link_existing(&self, pubkey_hex: &str, email: &str) {
        let mut state = self.state.lock().unwrap();
        state.regs.insert(
            pubkey_hex.to_owned(),
            Registration {
                email: email.to_owned(),
                label: "earlier".into(),
                entered_by: "human".into(),
                terms_version: "v1".into(),
                privacy_version: "v1".into(),
                notice_version: "v1".into(),
            },
        );
        state.marketing.entry(email.to_owned()).or_insert(true);
    }

    pub fn registration(&self, pubkey_hex: &str) -> Option<Registration> {
        self.state.lock().unwrap().regs.get(pubkey_hex).cloned()
    }

    pub fn pending(&self, pubkey_hex: &str) -> Option<Pending> {
        self.state.lock().unwrap().pending.get(pubkey_hex).cloned()
    }

    pub fn marketing(&self, email: &str) -> Option<bool> {
        self.state.lock().unwrap().marketing.get(email).copied()
    }

    pub fn confirm_calls(&self) -> usize {
        self.state.lock().unwrap().confirm_calls
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }
}

fn serve(stream: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let mut auth = None;
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            return;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => auth = Some(value.to_owned()),
                "content-length" => length = value.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let answer = {
        let mut state = state.lock().unwrap();
        let (answer, event) = handle(&mut state, &method, &path, auth.as_deref(), &body);
        state.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            auth: event,
            body: body.clone(),
            error: answer.body["error"].as_str().map(str::to_owned),
        });
        answer
    };
    let text = answer.body.to_string();
    let reason = match answer.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        answer.status,
        text.len()
    );
    if let Some(r) = answer.retry_after {
        head.push_str(&format!("Retry-After: {r}\r\n"));
    }
    head.push_str("\r\n");
    let mut stream = stream;
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

fn tag_values(event: &Event, name: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter(|t| t.kind().as_str() == name)
        .filter_map(|t| t.content().map(str::to_owned))
        .collect()
}

/// The service's `nip98::verify` + single-use check, with its error codes.
fn authenticate(
    state: &mut State,
    header: Option<&str>,
    path: &str,
    method: &str,
    body: &[u8],
) -> Result<Event, (Answer, Option<Event>)> {
    let unauthorized = |code: &str| fail(401, code, "NIP-98 authorization failed");
    let Some(encoded) = header.and_then(|h| h.strip_prefix("Nostr ")) else {
        return Err((unauthorized("missing_auth"), None));
    };
    let Some(event) = STANDARD
        .decode(encoded.trim())
        .ok()
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|json| Event::from_json(json).ok())
    else {
        return Err((unauthorized("malformed_auth"), None));
    };
    let keep = Some(event.clone());
    if event.kind != Kind::HttpAuth {
        return Err((unauthorized("wrong_kind"), keep));
    }
    if event.verify().is_err() {
        return Err((unauthorized("bad_signature"), keep));
    }
    if event.created_at.as_secs().abs_diff(now()) > 60 {
        return Err((unauthorized("stale_auth"), keep));
    }
    // The real service mounts the routes under /account; the public URL is <base>/<route>.
    let route = path.strip_prefix("/account").unwrap_or(path);
    let expected = format!("{}{route}", state.public_base);
    let urls = tag_values(&event, "u");
    if urls.len() != 1 || urls[0].trim_end_matches('/') != expected {
        return Err((unauthorized("url_mismatch"), keep));
    }
    let methods = tag_values(&event, "method");
    if methods.len() != 1 || !methods[0].eq_ignore_ascii_case(method) {
        return Err((unauthorized("method_mismatch"), keep));
    }
    let payloads = tag_values(&event, "payload");
    match payloads.as_slice() {
        [] if !body.is_empty() => return Err((unauthorized("payload_missing"), keep)),
        [] => {}
        [one] if *one == sha256::Hash::hash(body).to_string() => {}
        _ => return Err((unauthorized("payload_mismatch"), keep)),
    }
    if !state.seen.insert(event.id.to_hex()) {
        return Err((
            fail(401, "replayed_auth", "NIP-98 event already used"),
            keep,
        ));
    }
    Ok(event)
}

fn handle(
    state: &mut State,
    method: &str,
    path: &str,
    header: Option<&str>,
    body: &[u8],
) -> (Answer, Option<Event>) {
    let route = (method, path);
    let known = matches!(
        route,
        ("POST", "/account/link/request")
            | ("POST", "/account/link/confirm")
            | ("GET", "/account/status")
            | ("POST", "/account/marketing")
    );
    if !known {
        return (fail(404, "not_found", "no such route"), None);
    }
    let event = match authenticate(state, header, path, method, body) {
        Ok(event) => event,
        Err((answer, event)) => return (answer, event),
    };
    let pk = event.pubkey.to_hex();
    let parsed: Value = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => {
                return (
                    fail(400, "invalid_body", "request body is not the expected JSON"),
                    Some(event),
                );
            }
        }
    };
    let answer = match path {
        "/account/link/request" => link_request(state, &pk, &parsed),
        "/account/link/confirm" => link_confirm(state, &pk, &parsed),
        "/account/status" => status(state, &pk),
        _ => marketing(state, &pk, &parsed),
    };
    (answer, Some(event))
}

fn link_request(state: &mut State, pk: &str, body: &Value) -> Answer {
    let s = |k: &str| body[k].as_str().map(str::to_owned);
    let (Some(email), Some(label), Some(terms), Some(privacy)) = (
        s("email"),
        s("label"),
        s("terms_version"),
        s("privacy_version"),
    ) else {
        return fail(400, "invalid_body", "request body is not the expected JSON");
    };
    let email = email.trim().to_lowercase();
    if !email.contains('@') || !email.split('@').nth(1).is_some_and(|d| d.contains('.')) {
        return fail(400, "invalid_email", "not a valid email address");
    }
    let label = label.trim().to_owned();
    if label.is_empty()
        || label.chars().count() > 64
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
    {
        return fail(
            400,
            "invalid_label",
            "label must be 1-64 letters, digits, spaces, '.', '_' or '-'",
        );
    }
    if terms != "v1" {
        return fail(400, "unknown_terms_version", "terms version is not offered");
    }
    if privacy != "v1" {
        return fail(
            400,
            "unknown_privacy_version",
            "privacy version is not offered",
        );
    }
    let notice = s("notice_version").unwrap_or_else(|| "v1".into());
    if notice != "v1" {
        return fail(
            400,
            "unknown_notice_version",
            "notice version is not offered",
        );
    }
    if state.regs.get(pk).is_some_and(|r| r.email != email) {
        return fail(
            409,
            "key_linked_to_other_email",
            "this key is already linked to a different email; changing it is not supported",
        );
    }
    let t = now();
    if let Some(p) = state.pending.get(pk) {
        let wait = (p.created_at + state.cooldown_secs).saturating_sub(t);
        if wait > 0 {
            return limited("resend_cooldown", wait);
        }
    }
    let sends = state.sends.entry(pk.to_owned()).or_default();
    if *sends >= state.send_limit {
        return limited("rate_limited", 3600);
    }
    *sends += 1;
    let code = state.next_code.take().unwrap_or_else(|| "123456".into());
    state.pending.insert(
        pk.to_owned(),
        Pending {
            email,
            label,
            code,
            attempts: 0,
            expires_at: t + CODE_TTL_SECS,
            created_at: t,
        },
    );
    ok(
        202,
        json!({
            "status": "code_sent",
            "expires_in_secs": CODE_TTL_SECS,
            "max_attempts": MAX_ATTEMPTS,
            "terms_url": "https://www.maxplayer.ai/terms/v1",
            "privacy_url": "https://www.maxplayer.ai/privacy/v1",
        }),
    )
}

fn link_confirm(state: &mut State, pk: &str, body: &Value) -> Answer {
    let Some(raw) = body["code"].as_str() else {
        return fail(400, "invalid_body", "request body is not the expected JSON");
    };
    let entered_by = match body["entered_by"].as_str() {
        None | Some("agent") => "agent",
        Some("human") => "human",
        Some(_) => {
            return fail(
                400,
                "invalid_entered_by",
                "entered_by must be human or agent",
            );
        }
    };
    state.confirm_calls += 1;
    let code: String = raw.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    let Some(pending) = state.pending.get_mut(pk) else {
        return fail(
            404,
            "no_pending_code",
            "no code is waiting for this key; request a new one",
        );
    };
    if pending.expires_at <= now() {
        return fail(
            410,
            "code_expired",
            "the code has expired; request a new one",
        );
    }
    if pending.attempts >= MAX_ATTEMPTS {
        return fail(
            429,
            "too_many_attempts",
            "too many wrong tries; request a new code",
        );
    }
    if code != pending.code {
        pending.attempts += 1;
        let left = MAX_ATTEMPTS - pending.attempts;
        if left == 0 {
            return fail(
                429,
                "too_many_attempts",
                "too many wrong tries; request a new code",
            );
        }
        let mut answer = fail(400, "wrong_code", "that code is not right");
        answer.body["attempts_left"] = json!(left);
        return answer;
    }
    let pending = state.pending.remove(pk).expect("pending");
    if state.regs.get(pk).is_some_and(|r| r.email != pending.email) {
        return fail(
            409,
            "key_linked_to_other_email",
            "this key is already linked to a different email; changing it is not supported",
        );
    }
    let first_link = !state.regs.contains_key(pk);
    state.regs.insert(
        pk.to_owned(),
        Registration {
            email: pending.email.clone(),
            label: pending.label.clone(),
            entered_by: entered_by.to_owned(),
            terms_version: "v1".into(),
            privacy_version: "v1".into(),
            notice_version: "v1".into(),
        },
    );
    let subscribed = *state.marketing.entry(pending.email.clone()).or_insert(true);
    ok(
        200,
        json!({
            "status": "linked",
            "first_link": first_link,
            "email": pending.email,
            "label": pending.label,
            "terms_version": "v1",
            "privacy_version": "v1",
            "relay_member": "added",
            "marketing": if subscribed { "subscribed" } else { "unsubscribed" },
        }),
    )
}

fn status(state: &State, pk: &str) -> Answer {
    let Some(reg) = state.regs.get(pk) else {
        return ok(200, json!({ "registered": false }));
    };
    let subscribed = state.marketing.get(&reg.email).copied().unwrap_or(false);
    ok(
        200,
        json!({
            "registered": true,
            "email": reg.email,
            "label": reg.label,
            "terms_version": reg.terms_version,
            "privacy_version": reg.privacy_version,
            "accepted_at": "2026-10-28T12:00:00+00:00",
            "code_entered_by": reg.entered_by,
            "linked_at": "2026-10-28T12:00:00+00:00",
            "marketing": if subscribed { "subscribed" } else { "unsubscribed" },
            "relay_member": "added",
        }),
    )
}

fn marketing(state: &mut State, pk: &str, body: &Value) -> Answer {
    let Some(enabled) = body["enabled"].as_bool() else {
        return fail(400, "invalid_body", "request body is not the expected JSON");
    };
    let Some(reg) = state.regs.get(pk) else {
        return fail(
            404,
            "not_registered",
            "this key is not linked; run `maxplayer account link`",
        );
    };
    let email = reg.email.clone();
    state.marketing.insert(email, enabled);
    ok(
        200,
        json!({ "marketing": if enabled { "subscribed" } else { "unsubscribed" } }),
    )
}
