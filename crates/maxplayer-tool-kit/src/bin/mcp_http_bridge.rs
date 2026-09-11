//! `mcp-http-bridge` — the stdio-to-HTTP MCP transport shim for the Proxy swap route.
//!
//! This is the piece that runs INSIDE a job container to reach a vendor-hosted MCP server. The
//! agent spawns it and speaks MCP JSON-RPC over stdio; it forwards each request over HTTP to the
//! credential proxy, which swaps the placeholder it carries for the real vendor credential and
//! forwards it to the vendor. The vendor's JSON-RPC response comes back unchanged.
//!
//! It holds only the PLACEHOLDER, never the real credential. It is byte-faithful on the request
//! body; only the transport (stdio here, HTTP out) changes. Compromising it gains only what the
//! placeholder already allows, which is nothing without the proxy.
//!
//! This is the one genuinely new production component of the Proxy swap route. The credential swap
//! itself is the existing proxy (`#647`), extended with the vendor as one more credential; this
//! shim only bridges an MCP stdio client to that HTTP path.

use maxplayer_tool_kit::http;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let proxy_url = std::env::var("PROXY_URL").unwrap_or_else(|_| {
        eprintln!("mcp-http-bridge: PROXY_URL must be set");
        std::process::exit(2);
    });
    let placeholder = std::env::var("TOOL_PLACEHOLDER").unwrap_or_default();
    let path = std::env::var("MCP_PATH").unwrap_or_else(|_| "/mcp".to_string());

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A notification carries no id and draws no reply, exactly as over a socket.
        let id = serde_json::from_str::<Value>(trimmed).ok().and_then(|v| v.get("id").cloned());
        let is_notification = id.is_none();
        let id = id.unwrap_or(Value::Null);

        let bearer = if placeholder.is_empty() { None } else { Some(placeholder.as_str()) };
        let reply = match http::request(&proxy_url, "POST", &path, bearer, Some(trimmed.as_bytes())) {
            Ok(resp) if (200..300).contains(&resp.status) => String::from_utf8_lossy(&resp.body).trim().to_string(),
            // A non-2xx is a proxy refusal or a vendor rejection. Surface it as a JSON-RPC error on
            // the caller's id so the MCP client stays well-formed.
            Ok(resp) => {
                let detail = String::from_utf8_lossy(&resp.body);
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": format!("upstream HTTP {}: {}", resp.status, detail.trim())}})
                    .to_string()
            }
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": format!("proxy endpoint unavailable: {e}")}})
                .to_string(),
        };

        if !is_notification {
            let _ = writeln!(stdout, "{reply}");
            let _ = stdout.flush();
        }
    }
}
