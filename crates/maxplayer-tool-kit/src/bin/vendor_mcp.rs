//! `vendor-mcp` — a fake VENDOR-HOSTED MCP server, over HTTP.
//!
//! It stands in for a remote MCP such as a vendor's own server (the shape behind the Proxy swap
//! route). It authenticates every call with a bearer token and exposes one tool. It is the
//! independent oracle for the proxy-swap demo: it counts real-token successes and bad-token
//! rejections, and it can watch for one specific token — the placeholder — so a test can prove the
//! placeholder never reached it.
//!
//! Transport is the MCP Streamable HTTP shape, in two modes:
//! - default: POST one JSON-RPC request to `/mcp`, get one JSON-RPC response as a JSON document;
//! - `--sse`: the same response arrives as `text/event-stream` (`event: message` / `data: {...}`)
//!   with chunked framing, the way a streaming server answers.
//!
//! `--session` adds the session rule: `initialize` issues an `Mcp-Session-Id`, and every later
//! request must echo it or is refused `400`. A notification (no `id`) is accepted with `202` and
//! an empty body in every mode. Each rule is counted, so a test can assert the client kept it.
//!
//! Synthetic throughout: the token is a literal passed on the command line for the fixture, never a
//! real credential.

use maxplayer_tool_kit::http::{read_request, write_response_with, Request};
use serde_json::{json, Value};
use std::net::TcpListener;

struct Counters {
    auth_ok: u64,
    auth_fail: u64,
    calls: u64,
    notifications: u64,
    session_missing: u64,
    protocol_header_ok: u64,
    sessions_issued: u64,
    saw_watch_token: bool,
}

struct Modes {
    sse: bool,
    session: bool,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    chunked: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let listen = flag(&args, "--listen").unwrap_or_else(|| "127.0.0.1:0".to_string());
    let token = flag(&args, "--token").unwrap_or_else(|| fatal("--token <bearer> is required"));
    // A token the vendor flags if it ever arrives. The fixture sets this to the placeholder, so a
    // test can assert the placeholder never reached the vendor on the swap path.
    let watch = flag(&args, "--watch-token");
    let modes = Modes {
        sse: has_flag(&args, "--sse"),
        session: has_flag(&args, "--session"),
    };

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| fatal(&format!("bind {listen}: {e}")));
    match listener.local_addr() {
        Ok(a) => println!("vendor-mcp listening on {a}"),
        Err(_) => println!("vendor-mcp listening on {listen}"),
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let mut counters = Counters {
        auth_ok: 0,
        auth_fail: 0,
        calls: 0,
        notifications: 0,
        session_missing: 0,
        protocol_header_ok: 0,
        sessions_issued: 0,
        saw_watch_token: false,
    };
    let mut current_session: Option<String> = None;

    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let Ok(peer) = conn.try_clone() else { continue };
        let req = match read_request(peer) {
            Ok(Some(r)) => r,
            _ => continue,
        };
        let reply = handle(&req, &token, watch.as_deref(), &modes, &mut counters, &mut current_session);
        let headers: Vec<(&str, &str)> = reply.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let _ = write_response_with(&mut conn, reply.status, &headers, &reply.body, reply.chunked);
    }
}

fn handle(
    req: &Request,
    token: &str,
    watch: Option<&str>,
    modes: &Modes,
    counters: &mut Counters,
    current_session: &mut Option<String>,
) -> Reply {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/admin/stats") => json_reply(
            200,
            json!({
                "auth_ok": counters.auth_ok,
                "auth_fail": counters.auth_fail,
                "calls": counters.calls,
                "notifications": counters.notifications,
                "session_missing": counters.session_missing,
                "protocol_header_ok": counters.protocol_header_ok,
                "sessions_issued": counters.sessions_issued,
                "saw_watch_token": counters.saw_watch_token,
            }),
        ),

        ("POST", "/mcp") => {
            let presented = req.bearer();
            // Record if the watched token (the placeholder) ever reaches the vendor.
            if let (Some(w), Some(p)) = (watch, presented) {
                if p == w {
                    counters.saw_watch_token = true;
                }
            }
            // Authenticate. Only the real token is accepted; a placeholder is rejected here, which
            // is what makes the placeholder worthless without the swap.
            if presented != Some(token) {
                counters.auth_fail += 1;
                return json_reply(401, json!({"error": "invalid_token"}));
            }
            counters.auth_ok += 1;

            let rpc: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let method = rpc.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let is_initialize = method == "initialize";

            // The session rule, before the method: a request outside the session is refused.
            if modes.session
                && !is_initialize
                && (current_session.is_none()
                    || req.header("mcp-session-id") != current_session.as_deref())
            {
                counters.session_missing += 1;
                return json_reply(400, json!({"error": "missing_or_wrong_session"}));
            }
            if !is_initialize && req.header("mcp-protocol-version").is_some() {
                counters.protocol_header_ok += 1;
            }

            // A notification: accepted, no body, no id to answer.
            let Some(id) = rpc.get("id").cloned() else {
                counters.notifications += 1;
                return Reply { status: 202, headers: Vec::new(), body: Vec::new(), chunked: false };
            };

            let mut extra_headers = Vec::new();
            let message = match method {
                "initialize" => {
                    if modes.session {
                        counters.sessions_issued += 1;
                        let session_id = format!("sess-{}", counters.sessions_issued);
                        extra_headers.push(("Mcp-Session-Id".to_string(), session_id.clone()));
                        *current_session = Some(session_id);
                    }
                    ok(id, json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "vendor-mcp", "version": "0.2.0"},
                    }))
                }
                "tools/list" => ok(id, json!({
                    "tools": [{
                        "name": "vendor-echo",
                        "description": "Uppercase the given text, on the vendor's side.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                            "additionalProperties": false,
                        },
                    }],
                })),
                "tools/call" => {
                    let params = &rpc["params"];
                    if params["name"].as_str() != Some("vendor-echo") {
                        err(id, -32602, "unknown tool")
                    } else if let Some(text) = params["arguments"]["text"].as_str() {
                        counters.calls += 1;
                        ok(id, json!({
                            "content": [{"type": "text", "text": text.to_uppercase()}],
                            "isError": false,
                        }))
                    } else {
                        err(id, -32602, "arguments.text is required")
                    }
                }
                other => err(id, -32601, &format!("unknown method {other:?}")),
            };
            message_reply(message, extra_headers, modes.sse)
        }

        _ => json_reply(404, json!({"error": "not_found"})),
    }
}

/// One JSON-RPC message as the vendor answers it: a JSON document, or an SSE event with chunked
/// framing in `--sse` mode.
fn message_reply(message: Value, mut headers: Vec<(String, String)>, sse: bool) -> Reply {
    if sse {
        headers.push(("Content-Type".to_string(), "text/event-stream".to_string()));
        let body = format!("event: message\r\ndata: {message}\r\n\r\n").into_bytes();
        Reply { status: 200, headers, body, chunked: true }
    } else {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        Reply { status: 200, headers, body: message.to_string().into_bytes(), chunked: false }
    }
}

fn json_reply(status: u16, body: Value) -> Reply {
    Reply {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: body.to_string().into_bytes(),
        chunked: false,
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn fatal(msg: &str) -> ! {
    eprintln!("vendor-mcp: {msg}");
    std::process::exit(2)
}
