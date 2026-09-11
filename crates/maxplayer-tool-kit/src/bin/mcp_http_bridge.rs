//! `mcp-http-bridge` — the stdio-to-HTTP MCP transport shim for the Proxy swap route.
//!
//! This is the piece that runs INSIDE a job container to reach a vendor-hosted MCP server. The
//! agent spawns it as a stdio MCP server and speaks JSON-RPC over stdio; it posts each message over
//! HTTP to the credential proxy (`maxplayer-core` `#647`), which swaps the placeholder it carries
//! for the real vendor credential and forwards to the vendor. Every JSON-RPC message in the vendor's
//! response comes back as one line.
//!
//! It holds only the PLACEHOLDER, never the real credential. It is byte-faithful on the request
//! body; only the transport (stdio here, HTTP out) changes. Compromising it gains only what the
//! placeholder already allows, which is nothing without the proxy.
//!
//! The host passes its three facts as flags (`--proxy-url`, `--path`, `--placeholder`) in the MCP
//! server entry it puts on the agent's session (`seller_exec::mcp_tool_server`). The rules —
//! configuration, SSE, session id, protocol version, what a notification draws — live in
//! `maxplayer_tool_kit::mcp_bridge`, where they are unit-tested.

use maxplayer_tool_kit::http;
use maxplayer_tool_kit::mcp_bridge::{
    error_line, messages_in, reply_lines, transport_error_line, BridgeConfig, SessionState,
};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::time::Duration;

/// How long one request may wait on the proxy and, behind it, the vendor. Generous on purpose: a
/// vendor tool call can run for a while, and the job's own deadline bounds the run.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match BridgeConfig::from_args_and_env(&args, &|name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("mcp-http-bridge: {error}");
            std::process::exit(2);
        }
    };
    let mut state = SessionState::default();

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A line that is not JSON is answered, not forwarded: the proxy would only refuse it later,
        // and the client would wait for a reply that never comes.
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(message) => message,
            Err(error) => {
                let _ = writeln!(
                    stdout,
                    "{}",
                    error_line(&Value::Null, -32700, &format!("request is not JSON: {error}"))
                );
                let _ = stdout.flush();
                continue;
            }
        };
        // A notification carries no id and draws no reply, exactly as over a socket.
        let id = message.get("id").cloned();
        let is_notification = id.is_none();
        let id = id.unwrap_or(Value::Null);

        let headers = state.request_headers(&config.placeholder);
        let is_success = |status: u16| (200..300).contains(&status);
        let lines = match http::request_with(
            &config.proxy_url,
            "POST",
            &config.path,
            &headers,
            Some(trimmed.as_bytes()),
            REQUEST_TIMEOUT,
        ) {
            Ok(response) => {
                let parsed = messages_in(response.header("content-type"), &response.body);
                if is_success(response.status) {
                    if let Ok(messages) = &parsed {
                        state.observe(&response.headers, messages);
                    }
                } else if is_notification {
                    eprintln!(
                        "mcp-http-bridge: a notification was refused upstream: HTTP {}",
                        response.status
                    );
                }
                reply_lines(response.status, parsed, &response.body, &id, is_notification)
            }
            Err(error) => {
                if is_notification {
                    eprintln!("mcp-http-bridge: a notification did not reach the proxy: {error}");
                    Vec::new()
                } else {
                    vec![transport_error_line(&id, &error)]
                }
            }
        };

        for reply in lines {
            let _ = writeln!(stdout, "{reply}");
        }
        let _ = stdout.flush();
    }
}
