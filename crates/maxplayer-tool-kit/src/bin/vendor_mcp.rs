//! `vendor-mcp` — a fake VENDOR-HOSTED MCP server, over HTTP.
//!
//! It stands in for a remote MCP such as a vendor's own server (the shape behind the Proxy swap
//! route). It authenticates every call with a bearer token and exposes one tool. It is the
//! independent oracle for the proxy-swap demo: it counts real-token successes and bad-token
//! rejections, and it can watch for one specific token — the placeholder — so a test can prove the
//! placeholder never reached it.
//!
//! Transport is the MCP Streamable HTTP shape, in these modes:
//! - default: POST one JSON-RPC request to `/mcp`, get one JSON-RPC response as a JSON document;
//! - `--sse`: the same response arrives as `text/event-stream` (`event: message` / `data: {...}`)
//!   with chunked framing, the way a streaming server answers;
//! - `--server-request` (needs `--sse`): `tools/call` first sends the SERVER's own request
//!   (`ping`, id `srv-1`) as one event, holds the stream open, and waits for the client to POST
//!   its response. That POST is accepted with `202` and no body. Only then does the tool result
//!   arrive and the stream close. A client that waits for the end of the stream before it reads
//!   stdin never answers, and the call times out on the vendor side.
//!
//! - `--hold-stream` (needs `--sse`): after the result event the stream stays OPEN, with an SSE
//!   comment (`: keepalive`) every 200 ms, until the client closes its side. `streams_open` counts
//!   the streams held now. A client that reads a stream to its end never gets there.
//! - `--delay-ms <n>`: `tools/call` answers after `n` milliseconds, outside the shared lock, so
//!   many calls wait at once. For a test of a client's in-flight bound.
//!
//! `--session` adds the session rule: `initialize` issues an `Mcp-Session-Id`, and every later
//! request must echo it or is refused `400`. A notification (no `id`) is accepted with `202` and
//! an empty body in every mode. Each rule is counted, so a test can assert the client kept it.
//!
//! One thread per connection, because `--server-request` holds one response open while it
//! needs to accept another. The counters and the session live behind one lock.
//!
//! Synthetic throughout: the token is a literal passed on the command line for the fixture, never a
//! real credential.

use maxplayer_tool_kit::http::{
    read_request, write_chunk, write_last_chunk, write_response_head, write_response_with, BodyFraming, Request,
};
use serde_json::{json, Value};
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the vendor waits for the client's answer to the server request it sent.
const SERVER_REQUEST_WAIT: Duration = Duration::from_secs(5);
/// The keepalive period of a held stream, and how often the vendor looks for the client's close.
const KEEPALIVE_PERIOD: Duration = Duration::from_millis(200);
/// The longest a held stream stays open without the client closing, so a test double never hangs.
const HOLD_STREAM_MAX: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Counters {
    auth_ok: u64,
    auth_fail: u64,
    calls: u64,
    notifications: u64,
    /// Client responses (to a server request) that arrived while one was awaited.
    server_requests_answered: u64,
    /// Server requests whose answer did not arrive in time.
    server_requests_unanswered: u64,
    /// Client responses that arrived while no server request waited for one.
    stray_responses: u64,
    session_missing: u64,
    protocol_header_ok: u64,
    sessions_issued: u64,
    saw_watch_token: bool,
    /// Streams held open now under `--hold-stream`, and the total ever opened.
    streams_open: u64,
    streams_held_total: u64,
}

struct Modes {
    sse: bool,
    session: bool,
    server_request: bool,
    hold_stream: bool,
    delay: Duration,
}

/// What every connection shares.
struct State {
    counters: Counters,
    current_session: Option<String>,
    /// The channel the open `tools/call` waits on for the client's answer.
    pending_answer: Option<Sender<Value>>,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    chunked: bool,
}

/// How one request is answered.
enum Outcome {
    /// One complete reply.
    Reply(Reply),
    /// An SSE stream with a server request first: send it, wait for the answer on `answer`, then
    /// send `result` and close.
    Interactive { headers: Vec<(String, String)>, server_request: Value, answer: Receiver<Value>, result: Value },
}

struct Vendor {
    token: String,
    watch: Option<String>,
    modes: Modes,
    state: Mutex<State>,
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
        server_request: has_flag(&args, "--server-request"),
        hold_stream: has_flag(&args, "--hold-stream"),
        delay: Duration::from_millis(
            flag(&args, "--delay-ms")
                .map(|v| v.parse::<u64>().unwrap_or_else(|_| fatal("--delay-ms must be a whole number")))
                .unwrap_or(0),
        ),
    };
    if modes.server_request && !modes.sse {
        fatal("--server-request needs --sse: a server request rides an open event stream");
    }
    if modes.hold_stream && !modes.sse {
        fatal("--hold-stream needs --sse: only an event stream can stay open after the result");
    }

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| fatal(&format!("bind {listen}: {e}")));
    match listener.local_addr() {
        Ok(a) => println!("vendor-mcp listening on {a}"),
        Err(_) => println!("vendor-mcp listening on {listen}"),
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let vendor = Arc::new(Vendor {
        token,
        watch,
        modes,
        state: Mutex::new(State { counters: Counters::default(), current_session: None, pending_answer: None }),
    });

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let vendor = Arc::clone(&vendor);
        std::thread::spawn(move || vendor.serve(conn));
    }
}

impl Vendor {
    fn serve(&self, mut conn: TcpStream) {
        let Ok(peer) = conn.try_clone() else { return };
        let req = match read_request(peer) {
            Ok(Some(r)) => r,
            _ => return,
        };
        // The configured delay for a tool call, outside the shared lock, so many delayed calls
        // wait at the same time rather than one after another.
        if !self.modes.delay.is_zero() && req.method == "POST" && req.path == "/mcp" {
            let rpc: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            if rpc.get("method").and_then(|m| m.as_str()) == Some("tools/call") {
                std::thread::sleep(self.modes.delay);
            }
        }
        match self.handle(&req) {
            Outcome::Reply(reply) if self.modes.hold_stream && reply.chunked => self.hold_open(conn, reply),
            Outcome::Reply(reply) => {
                let headers: Vec<(&str, &str)> = reply.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let _ = write_response_with(&mut conn, reply.status, &headers, &reply.body, reply.chunked);
            }
            Outcome::Interactive { mut headers, server_request, answer, result } => {
                headers.push(("Content-Type".to_string(), "text/event-stream".to_string()));
                let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                if write_response_head(&mut conn, 200, &headers, BodyFraming::Chunked).is_err() {
                    return;
                }
                // The server's request goes out now, as its own event; the stream stays open.
                if write_chunk(&mut conn, sse_event(&server_request).as_bytes()).is_err() {
                    return;
                }
                let message = match answer.recv_timeout(SERVER_REQUEST_WAIT) {
                    Ok(_) => {
                        self.lock().counters.server_requests_answered += 1;
                        result
                    }
                    Err(_) => {
                        let mut state = self.lock();
                        state.counters.server_requests_unanswered += 1;
                        state.pending_answer = None;
                        err(
                            result["id"].clone(),
                            -32000,
                            "the client did not answer the server's request; the call was abandoned",
                        )
                    }
                };
                let _ = write_chunk(&mut conn, sse_event(&message).as_bytes());
                let _ = write_last_chunk(&mut conn);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `--hold-stream`: send the result event, then keep the stream open with a keepalive comment
    /// every [`KEEPALIVE_PERIOD`] until the client closes its side (a read that returns zero bytes
    /// or fails), or [`HOLD_STREAM_MAX`] passes. `streams_open` counts the streams held now.
    fn hold_open(&self, mut conn: TcpStream, reply: Reply) {
        let headers: Vec<(&str, &str)> = reply.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        if write_response_head(&mut conn, reply.status, &headers, BodyFraming::Chunked).is_err() {
            return;
        }
        if write_chunk(&mut conn, &reply.body).is_err() {
            return;
        }
        {
            let mut state = self.lock();
            state.counters.streams_open += 1;
            state.counters.streams_held_total += 1;
        }
        let _ = conn.set_read_timeout(Some(KEEPALIVE_PERIOD));
        let started = std::time::Instant::now();
        let mut probe = [0u8; 64];
        while started.elapsed() < HOLD_STREAM_MAX {
            match conn.read(&mut probe) {
                // The client closed its side: the stream is over.
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    if write_chunk(&mut conn, b": keepalive\r\n\r\n").is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = write_last_chunk(&mut conn);
        self.lock().counters.streams_open -= 1;
    }

    fn handle(&self, req: &Request) -> Outcome {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/admin/stats") => {
                let state = self.lock();
                let c = &state.counters;
                Outcome::Reply(json_reply(
                    200,
                    json!({
                        "auth_ok": c.auth_ok,
                        "auth_fail": c.auth_fail,
                        "calls": c.calls,
                        "notifications": c.notifications,
                        "server_requests_answered": c.server_requests_answered,
                        "server_requests_unanswered": c.server_requests_unanswered,
                        "stray_responses": c.stray_responses,
                        "session_missing": c.session_missing,
                        "protocol_header_ok": c.protocol_header_ok,
                        "sessions_issued": c.sessions_issued,
                        "saw_watch_token": c.saw_watch_token,
                        "streams_open": c.streams_open,
                        "streams_held_total": c.streams_held_total,
                    }),
                ))
            }

            ("POST", "/mcp") => {
                let mut state = self.lock();
                let presented = req.bearer();
                // Record if the watched token (the placeholder) ever reaches the vendor.
                if let (Some(w), Some(p)) = (self.watch.as_deref(), presented) {
                    if p == w {
                        state.counters.saw_watch_token = true;
                    }
                }
                // Authenticate. Only the real token is accepted; a placeholder is rejected here,
                // which is what makes the placeholder worthless without the swap.
                if presented != Some(self.token.as_str()) {
                    state.counters.auth_fail += 1;
                    return Outcome::Reply(json_reply(401, json!({"error": "invalid_token"})));
                }
                state.counters.auth_ok += 1;

                let rpc: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let method = rpc.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let is_initialize = method == "initialize";

                // The session rule, before the method: a request outside the session is refused.
                if self.modes.session
                    && !is_initialize
                    && (state.current_session.is_none()
                        || req.header("mcp-session-id") != state.current_session.as_deref())
                {
                    state.counters.session_missing += 1;
                    return Outcome::Reply(json_reply(400, json!({"error": "missing_or_wrong_session"})));
                }
                if !is_initialize && req.header("mcp-protocol-version").is_some() {
                    state.counters.protocol_header_ok += 1;
                }

                // The client's answer to the server's request: no method, an id, an outcome. It is
                // accepted with 202 and no body, and it wakes the call that waits for it.
                if rpc.get("method").is_none()
                    && rpc.get("id").is_some()
                    && (rpc.get("result").is_some() || rpc.get("error").is_some())
                {
                    match state.pending_answer.take() {
                        Some(waiting) if rpc["id"] == json!("srv-1") => {
                            let _ = waiting.send(rpc);
                        }
                        other => {
                            state.pending_answer = other;
                            state.counters.stray_responses += 1;
                        }
                    }
                    return Outcome::Reply(accepted());
                }

                // A notification: accepted, no body, no id to answer.
                let Some(id) = rpc.get("id").cloned() else {
                    state.counters.notifications += 1;
                    return Outcome::Reply(accepted());
                };

                let mut extra_headers = Vec::new();
                let message = match method {
                    "initialize" => {
                        if self.modes.session {
                            state.counters.sessions_issued += 1;
                            let session_id = format!("sess-{}", state.counters.sessions_issued);
                            extra_headers.push(("Mcp-Session-Id".to_string(), session_id.clone()));
                            state.current_session = Some(session_id);
                        }
                        ok(id, json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "vendor-mcp", "version": "0.3.0"},
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
                            state.counters.calls += 1;
                            let result = ok(id, json!({
                                "content": [{"type": "text", "text": text.to_uppercase()}],
                                "isError": false,
                            }));
                            if self.modes.server_request {
                                // Ask the client first; the result waits for its answer.
                                let (tx, rx) = mpsc::channel();
                                state.pending_answer = Some(tx);
                                return Outcome::Interactive {
                                    headers: extra_headers,
                                    server_request: json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"}),
                                    answer: rx,
                                    result,
                                };
                            }
                            result
                        } else {
                            err(id, -32602, "arguments.text is required")
                        }
                    }
                    other => err(id, -32601, &format!("unknown method {other:?}")),
                };
                Outcome::Reply(message_reply(message, extra_headers, self.modes.sse))
            }

            _ => Outcome::Reply(json_reply(404, json!({"error": "not_found"}))),
        }
    }
}

/// One JSON-RPC message as the vendor answers it: a JSON document, or an SSE event with chunked
/// framing in `--sse` mode.
fn message_reply(message: Value, mut headers: Vec<(String, String)>, sse: bool) -> Reply {
    if sse {
        headers.push(("Content-Type".to_string(), "text/event-stream".to_string()));
        Reply { status: 200, headers, body: sse_event(&message).into_bytes(), chunked: true }
    } else {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        Reply { status: 200, headers, body: message.to_string().into_bytes(), chunked: false }
    }
}

/// One SSE event that carries `message`.
fn sse_event(message: &Value) -> String {
    format!("event: message\r\ndata: {message}\r\n\r\n")
}

/// `202 Accepted`, no body: the answer to a notification and to a client response.
fn accepted() -> Reply {
    Reply { status: 202, headers: Vec::new(), body: Vec::new(), chunked: false }
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
