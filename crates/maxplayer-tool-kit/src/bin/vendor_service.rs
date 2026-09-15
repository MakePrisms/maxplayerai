//! A fake authenticated vendor service. Stands in for a third-party SaaS the seller has an
//! account with. Entirely synthetic: the credential it accepts is read from a file at startup
//! and never appears in an argv or a log line.
//!
//! It also keeps the counters the evidence relies on. They live **here**, on the vendor side,
//! because a holder reporting its own call count is not an independent observer of anything.

use maxplayer_tool_kit::http::{read_request, write_response, Request};
use maxplayer_tool_kit::Secret;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct State {
    client_id: String,
    client_secret: Secret,
    /// Live session tokens. Revoking clears them, which is how the harness forces the
    /// auth-failure path without touching the holder.
    tokens: BTreeSet<String>,
    counter: u64,
    login_count: u64,
    health_count: u64,
    transform_count: u64,
    auth_failures: u64,
}

impl State {
    fn mint(&mut self) -> String {
        self.counter += 1;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let token = format!("sess-{}-{}", self.counter, nanos);
        self.tokens.insert(token.clone());
        token
    }

    fn authorized(&mut self, req: &Request) -> bool {
        match req.bearer() {
            Some(t) if self.tokens.contains(t) => true,
            _ => {
                self.auth_failures += 1;
                false
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let listen = flag(&args, "--listen").unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let cred_file = flag(&args, "--credential-file").unwrap_or_else(|| {
        eprintln!("vendor-service: --credential-file <path> is required");
        std::process::exit(2);
    });

    // The expected credential arrives as a file, never as an argument.
    let raw = std::fs::read_to_string(&cred_file).unwrap_or_else(|e| {
        eprintln!("vendor-service: cannot read credential file: {e}");
        std::process::exit(2);
    });
    let parsed: Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        eprintln!("vendor-service: credential file is not JSON: {e}");
        std::process::exit(2);
    });
    let client_id = parsed["client_id"].as_str().unwrap_or_default().to_string();
    let client_secret = Secret::new(parsed["client_secret"].as_str().unwrap_or_default());
    if client_id.is_empty() || client_secret.is_empty() {
        eprintln!("vendor-service: credential file needs client_id and client_secret");
        std::process::exit(2);
    }

    let state = Arc::new(Mutex::new(State {
        client_id,
        client_secret,
        tokens: BTreeSet::new(),
        counter: 0,
        login_count: 0,
        health_count: 0,
        transform_count: 0,
        auth_failures: 0,
    }));

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| {
        eprintln!("vendor-service: bind {listen}: {e}");
        std::process::exit(1);
    });
    // Print the bound address so a harness can use port 0 and learn the real port.
    match listener.local_addr() {
        Ok(a) => println!("vendor-service listening on {a}"),
        Err(_) => println!("vendor-service listening on {listen}"),
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let stop = Arc::new(AtomicBool::new(false));
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(mut conn) = conn else { continue };
        let state_thread = Arc::clone(&state);
        let stop_thread = Arc::clone(&stop);
        let handle_thread = std::thread::spawn(move || {
            let peer = match conn.try_clone() {
                Ok(c) => c,
                Err(_) => return,
            };
            let req = match read_request(peer) {
                Ok(Some(r)) => r,
                _ => return,
            };
            let (status, body) = handle(&state_thread, &stop_thread, &req);
            let _ = write_response(&mut conn, status, body.to_string().as_bytes());
        });
        // Join so a shutdown request is fully answered before the loop re-checks the flag.
        // Concurrency is not a property this fixture needs; a deterministic stop is.
        let _ = handle_thread.join();
        if stop.load(Ordering::SeqCst) {
            break;
        }
    }
}

fn handle(state: &Mutex<State>, stop: &AtomicBool, req: &Request) -> (u16, Value) {
    let mut st = match state.lock() {
        Ok(s) => s,
        Err(_) => return (500, json!({"error": "state poisoned"})),
    };

    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/login") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let id = body["client_id"].as_str().unwrap_or_default();
            let secret = body["client_secret"].as_str().unwrap_or_default();
            // Constant-time comparison is not the point of this fixture; refusing to log the
            // attempted secret is.
            if id == st.client_id && secret == st.client_secret.expose() {
                st.login_count += 1;
                let token = st.mint();
                (200, json!({"session_token": token, "token_type": "bearer"}))
            } else {
                st.auth_failures += 1;
                (401, json!({"error": "invalid_client"}))
            }
        }

        ("GET", "/health") => {
            if !st.authorized(req) {
                return (401, json!({"error": "invalid_token"}));
            }
            st.health_count += 1;
            (200, json!({"status": "ok"}))
        }

        ("POST", "/v1/transform") => {
            if !st.authorized(req) {
                return (401, json!({"error": "invalid_token"}));
            }
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let Some(text) = body["text"].as_str() else {
                return (400, json!({"error": "text is required"}));
            };
            let mode = body["mode"].as_str().unwrap_or("upper");
            let result = match mode {
                "upper" => text.to_uppercase(),
                "lower" => text.to_lowercase(),
                "reverse" => text.chars().rev().collect(),
                other => return (400, json!({"error": format!("unknown mode {other}")})),
            };
            st.transform_count += 1;
            (200, json!({"result": result}))
        }

        // Harness controls. A real vendor would authenticate these; this one is reachable only
        // on the fixture network and exists to drive failure paths and to be the oracle.
        ("POST", "/admin/revoke") => {
            let n = st.tokens.len();
            st.tokens.clear();
            (200, json!({"revoked": n}))
        }
        ("GET", "/admin/stats") => (
            200,
            json!({
                "login_count": st.login_count,
                "health_count": st.health_count,
                "transform_count": st.transform_count,
                "auth_failures": st.auth_failures,
                "live_tokens": st.tokens.len(),
            }),
        ),
        ("POST", "/admin/shutdown") => {
            stop.store(true, Ordering::SeqCst);
            (200, json!({"stopping": true}))
        }

        ("GET", _) | ("POST", _) => (404, json!({"error": "not_found"})),
        _ => (405, json!({"error": "method_not_allowed"})),
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}
