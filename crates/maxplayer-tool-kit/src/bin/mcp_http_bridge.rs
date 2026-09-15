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
//! configuration, SSE, session id, protocol version, what a notification or a response draws —
//! live in `maxplayer_tool_kit::mcp_bridge`, where they are unit-tested.
//!
//! Two threads of control, because the Streamable HTTP transport needs both at once:
//! - The main thread reads stdin. Every message starts one worker thread that posts it.
//! - A worker reads its response as it arrives and writes each JSON-RPC message to stdout as soon
//!   as its SSE event is complete. It does not wait for the end of the stream.
//!
//! That is what lets a server ask the client something in the middle of a response: the server's
//! request is written out at once, the agent's answer arrives on stdin while the stream is still
//! open, a second worker posts it, and the server can then finish the first stream. A bridge that
//! reads stdin only between complete responses cannot do that; it waits for a stream that waits
//! for it.
//!
//! What it does not do: it opens no `GET` stream. A server message that does not ride on a
//! response to one of the client's requests is not received.

use maxplayer_tool_kit::http;
use maxplayer_tool_kit::mcp_bridge::{
    answers, classify, error_line, messages_in, reply_lines, transport_error_line, BridgeConfig,
    MessageKind, SessionState, SseSplitter,
};
use serde_json::Value;
use std::io::{BufRead, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long one read or write on the proxy connection may wait. Generous on purpose: a vendor
/// tool call can run for a while, and the job's own deadline bounds the run.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Largest error body the bridge quotes back into a JSON-RPC error.
const MAX_ERROR_BODY: u64 = 1 << 20;

/// What every worker shares: the configuration, the session facts, and the one stdout.
struct Shared {
    config: BridgeConfig,
    state: Mutex<SessionState>,
    stdout: Mutex<std::io::Stdout>,
}

impl Shared {
    /// Write one line. The lock makes a line atomic, so two workers never interleave bytes.
    fn emit(&self, line: &str) {
        if let Ok(mut out) = self.stdout.lock() {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match BridgeConfig::from_args_and_env(&args, &|name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("mcp-http-bridge: {error}");
            std::process::exit(2);
        }
    };
    let shared = Arc::new(Shared {
        config,
        state: Mutex::new(SessionState::default()),
        stdout: Mutex::new(std::io::stdout()),
    });

    let stdin = std::io::stdin();
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();

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
                shared.emit(&error_line(&Value::Null, -32700, &format!("request is not JSON: {error}")));
                continue;
            }
        };
        let kind = classify(&message);
        let id = message.get("id").cloned().unwrap_or(Value::Null);

        workers.retain(|worker| !worker.is_finished());
        let shared = Arc::clone(&shared);
        let text = trimmed.to_owned();
        workers.push(std::thread::spawn(move || post(&shared, &text, kind, &id)));
    }

    // Stdin closed: the agent is done. Give the workers that still run their own request timeout to
    // finish, then leave; a worker that hangs on the proxy must not keep the container alive.
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    for worker in workers {
        while !worker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if worker.is_finished() {
            let _ = worker.join();
        }
    }
    std::process::exit(0);
}

/// Post one message and forward what comes back, as it comes back.
fn post(shared: &Shared, text: &str, kind: MessageKind, id: &Value) {
    let headers = match shared.state.lock() {
        Ok(state) => state.request_headers(&shared.config.placeholder),
        Err(_) => return,
    };
    let is_request = kind == MessageKind::Request;
    let mut response = match http::request_streaming(
        &shared.config.proxy_url,
        "POST",
        &shared.config.path,
        &headers,
        Some(text.as_bytes()),
        REQUEST_TIMEOUT,
    ) {
        Ok(response) => response,
        Err(error) => {
            if is_request {
                shared.emit(&transport_error_line(id, &error));
            } else {
                eprintln!("mcp-http-bridge: a {kind:?} did not reach the proxy: {error}");
            }
            return;
        }
    };

    if !(200..300).contains(&response.status) {
        // A proxy refusal or a vendor rejection. A request gets it as one JSON-RPC error on its
        // own id; a notification or a response has no line to carry it, so it goes to stderr.
        let mut body = Vec::new();
        let _ = (&mut response).take(MAX_ERROR_BODY).read_to_end(&mut body);
        if is_request {
            shared.emit(&reply_lines(response.status, Ok(Vec::new()), &body, id, false).remove(0));
        } else {
            eprintln!("mcp-http-bridge: a {kind:?} was refused upstream: HTTP {}", response.status);
        }
        return;
    }

    // The session id is learned BEFORE the first message goes out, so the client's next request
    // (its `notifications/initialized`) already carries it.
    if let Ok(mut state) = shared.state.lock() {
        state.observe_headers(&response.headers);
    }
    let is_sse = response
        .header("content-type")
        .map(|c| c.trim().to_ascii_lowercase().starts_with("text/event-stream"))
        .unwrap_or(false);
    if is_sse {
        forward_stream(shared, &mut response, is_request, id);
    } else {
        forward_document(shared, &mut response, is_request, id);
    }
}

/// An SSE body: forward each event's message the moment the event is complete.
fn forward_stream(shared: &Shared, response: &mut http::StreamingResponse, is_request: bool, id: &Value) {
    let mut splitter = SseSplitter::default();
    let mut emitted = 0usize;
    let mut answered = false;
    let mut buffer = [0u8; 8192];
    loop {
        let payloads = match response.read(&mut buffer) {
            Ok(0) => {
                let tail = splitter.finish();
                if !forward_payloads(shared, &tail, is_request, id, &mut emitted, &mut answered) {
                    return;
                }
                break;
            }
            Ok(n) => splitter.feed(&buffer[..n]),
            Err(error) => {
                // The stream broke. A request that has its answer lost nothing; one that has not
                // must not wait for the rest.
                if is_request && !answered {
                    shared.emit(&transport_error_line(id, &error));
                } else if !is_request {
                    eprintln!("mcp-http-bridge: the response stream ended early: {error}");
                }
                return;
            }
        };
        if !forward_payloads(shared, &payloads, is_request, id, &mut emitted, &mut answered) {
            return;
        }
    }
    if is_request && emitted == 0 {
        shared.emit(&error_line(id, -32000, "upstream returned an empty body for a request"));
    }
}

/// Forward the messages in `payloads`. Returns `false` when the stream must be abandoned: a
/// payload that is not JSON, which a request is told about once.
fn forward_payloads(
    shared: &Shared,
    payloads: &[String],
    is_request: bool,
    id: &Value,
    emitted: &mut usize,
    answered: &mut bool,
) -> bool {
    for payload in payloads {
        if payload.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(payload) {
            Ok(message) => message,
            Err(error) => {
                if is_request && !*answered {
                    shared.emit(&error_line(id, -32700, &format!("upstream body could not be parsed: SSE data is not JSON: {error}")));
                } else {
                    eprintln!("mcp-http-bridge: SSE data is not JSON: {error}");
                }
                return false;
            }
        };
        if let Ok(mut state) = shared.state.lock() {
            state.observe_message(&message);
        }
        if is_request && answers(&message, id) {
            *answered = true;
        }
        shared.emit(&message.to_string());
        *emitted += 1;
    }
    true
}

/// A JSON body (or an empty one): read it whole, then forward its messages. For a request, the
/// old rules hold: an empty 2xx body or a body that is not JSON is one error line on its id.
fn forward_document(shared: &Shared, response: &mut http::StreamingResponse, is_request: bool, id: &Value) {
    let mut body = Vec::new();
    if let Err(error) = response.read_to_end(&mut body) {
        if is_request {
            shared.emit(&transport_error_line(id, &error));
        } else {
            eprintln!("mcp-http-bridge: the response body ended early: {error}");
        }
        return;
    }
    let parsed = messages_in(response.header("content-type"), &body);
    if let Ok(messages) = &parsed {
        if let Ok(mut state) = shared.state.lock() {
            for message in messages {
                state.observe_message(message);
            }
        }
    }
    if is_request {
        for line in reply_lines(response.status, parsed, &body, id, false) {
            shared.emit(&line);
        }
        return;
    }
    // A notification or a response owes the client nothing, and a `202` with no body is the
    // normal answer. A message the server put in the body is still the server's message.
    match parsed {
        Ok(messages) => {
            for message in messages {
                shared.emit(&message.to_string());
            }
        }
        Err(why) => eprintln!("mcp-http-bridge: a body that answered a notification or a response could not be parsed: {why}"),
    }
}
