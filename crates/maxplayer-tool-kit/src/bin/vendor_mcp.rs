//! `vendor-mcp` — a fake VENDOR-HOSTED MCP server, over HTTP.
//!
//! It stands in for a remote MCP such as a vendor's own server (the shape behind the Proxy swap
//! route). It authenticates every call with a bearer token and exposes one tool. It is the
//! independent oracle for the proxy-swap demo: it counts real-token successes and bad-token
//! rejections, and it can watch for one specific token — the placeholder — so a test can prove the
//! placeholder never reached it.
//!
//! Transport is a simplified HTTP MCP: POST one JSON-RPC request to `/mcp`, get one JSON-RPC
//! response. A real vendor may use the MCP Streamable HTTP transport with SSE; that does not change
//! the credential-swap mechanism this demo proves.
//!
//! Synthetic throughout: the token is a literal passed on the command line for the fixture, never a
//! real credential.

use maxplayer_tool_kit::http::{read_request, write_response, Request};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let listen = flag(&args, "--listen").unwrap_or_else(|| "127.0.0.1:0".to_string());
    let token = flag(&args, "--token").unwrap_or_else(|| fatal("--token <bearer> is required"));
    // A token the vendor flags if it ever arrives. The fixture sets this to the placeholder, so a
    // test can assert the placeholder never reached the vendor on the swap path.
    let watch = flag(&args, "--watch-token");

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| fatal(&format!("bind {listen}: {e}")));
    match listener.local_addr() {
        Ok(a) => println!("vendor-mcp listening on {a}"),
        Err(_) => println!("vendor-mcp listening on {listen}"),
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let auth_ok = AtomicU64::new(0);
    let auth_fail = AtomicU64::new(0);
    let calls = AtomicU64::new(0);
    let saw_watch = AtomicBool::new(false);

    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let Ok(peer) = conn.try_clone() else { continue };
        let req = match read_request(peer) {
            Ok(Some(r)) => r,
            _ => continue,
        };
        let (status, body) = handle(&req, &token, watch.as_deref(), &auth_ok, &auth_fail, &calls, &saw_watch);
        let _ = write_response(&mut conn, status, body.to_string().as_bytes());
    }
}

fn handle(
    req: &Request,
    token: &str,
    watch: Option<&str>,
    auth_ok: &AtomicU64,
    auth_fail: &AtomicU64,
    calls: &AtomicU64,
    saw_watch: &AtomicBool,
) -> (u16, Value) {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/admin/stats") => (
            200,
            json!({
                "auth_ok": auth_ok.load(Ordering::SeqCst),
                "auth_fail": auth_fail.load(Ordering::SeqCst),
                "calls": calls.load(Ordering::SeqCst),
                "saw_watch_token": saw_watch.load(Ordering::SeqCst),
            }),
        ),

        ("POST", "/mcp") => {
            let presented = req.bearer();
            // Record if the watched token (the placeholder) ever reaches the vendor.
            if let (Some(w), Some(p)) = (watch, presented) {
                if p == w {
                    saw_watch.store(true, Ordering::SeqCst);
                }
            }
            // Authenticate. Only the real token is accepted; a placeholder is rejected here, which
            // is what makes the placeholder worthless without the swap.
            if presented != Some(token) {
                auth_fail.fetch_add(1, Ordering::SeqCst);
                return (401, json!({"error": "invalid_token"}));
            }
            auth_ok.fetch_add(1, Ordering::SeqCst);

            let rpc: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let id = rpc.get("id").cloned().unwrap_or(Value::Null);
            let method = rpc.get("method").and_then(|m| m.as_str()).unwrap_or("");
            match method {
                "initialize" => ok(id, json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "vendor-mcp", "version": "0.1.0"},
                })),
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
                        return err(id, -32602, "unknown tool");
                    }
                    let Some(text) = params["arguments"]["text"].as_str() else {
                        return err(id, -32602, "arguments.text is required");
                    };
                    calls.fetch_add(1, Ordering::SeqCst);
                    ok(id, json!({
                        "content": [{"type": "text", "text": text.to_uppercase()}],
                        "isError": false,
                    }))
                }
                other => err(id, -32601, &format!("unknown method {other:?}")),
            }
        }

        _ => (404, json!({"error": "not_found"})),
    }
}

fn ok(id: Value, result: Value) -> (u16, Value) {
    (200, json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn err(id: Value, code: i64, message: &str) -> (u16, Value) {
    (200, json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn fatal(msg: &str) -> ! {
    eprintln!("vendor-mcp: {msg}");
    std::process::exit(2)
}
