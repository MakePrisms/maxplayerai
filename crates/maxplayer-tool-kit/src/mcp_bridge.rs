//! The logic of `mcp-http-bridge`, the Proxy swap transport shim, kept out of the binary so every
//! rule is unit-testable without a process, a socket, or a proxy.
//!
//! The bridge runs INSIDE a job container as a stdio MCP server. It reads one JSON-RPC message per
//! line, posts it to the credential proxy over HTTP with the per-job PLACEHOLDER as its bearer, and
//! writes back every JSON-RPC message the vendor's response carries, one per line. It holds no real
//! credential. Compromising it gains only what the placeholder already allows, which is nothing
//! without the proxy.
//!
//! The vendor side is the MCP Streamable HTTP transport. Four of its rules land here:
//! - A response is either one JSON document or an SSE stream (`text/event-stream`) whose `data:`
//!   payloads are JSON-RPC messages. Both shapes are read; each message is one output line.
//!   [`SseSplitter`] cuts the stream into events as the bytes arrive, so a message is forwarded
//!   before the stream ends.
//! - A server may issue `Mcp-Session-Id` on the `initialize` response; the client echoes it on every
//!   later request. [`SessionState`] carries it.
//! - After `initialize`, the client sends `MCP-Protocol-Version` with the version the server
//!   negotiated. [`SessionState`] carries that too.
//! - A server may send its own request inside an open SSE stream and wait for the client's answer
//!   before it finishes the stream. The client's answer is a JSON-RPC RESPONSE, posted as its own
//!   request while the stream is open; the server accepts it with `202` and no body.
//!   [`classify`] tells a response apart from a request and a notification.
//!
//! A notification (no `id`) and a response (no `method`) are posted and draw no output line,
//! whatever the server answers.

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The vendor endpoint path the bridge posts to when none is configured.
pub const DEFAULT_PATH: &str = "/mcp";

/// What the bridge needs to reach the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    /// The proxy's base URL as the container reaches it (`http://host:port`).
    pub proxy_url: String,
    /// The vendor endpoint's path, posted to the proxy verbatim (e.g. `/mcp/`).
    pub path: String,
    /// The per-job placeholder. Empty means "send no bearer", which the proxy refuses.
    pub placeholder: String,
}

impl BridgeConfig {
    /// Read the configuration from argv flags first, then the environment.
    ///
    /// Flags: `--proxy-url URL`, `--path PATH`, `--placeholder VALUE`, `--placeholder-env NAME`
    /// (read the placeholder from that variable). Environment fallbacks: `PROXY_URL`, `MCP_PATH`,
    /// `TOOL_PLACEHOLDER`. The host passes flags, because a stdio MCP server's `args` reach the
    /// child whenever a stdio server works at all, while its `env` reaches it only if the harness
    /// maps it. The environment path stays for a hand-run bridge.
    pub fn from_args_and_env(
        args: &[String],
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        let non_empty = |value: Option<String>| value.filter(|v| !v.trim().is_empty());
        let proxy_url = non_empty(flag("--proxy-url").or_else(|| env("PROXY_URL")))
            .ok_or_else(|| "--proxy-url <http://host:port> (or PROXY_URL) is required".to_string())?;
        let path = non_empty(flag("--path").or_else(|| env("MCP_PATH")))
            .unwrap_or_else(|| DEFAULT_PATH.to_string());
        if !path.starts_with('/') {
            return Err(format!("--path must start with '/', got {path:?}"));
        }
        let placeholder = match flag("--placeholder") {
            Some(value) => value,
            None => match flag("--placeholder-env") {
                Some(name) => env(&name)
                    .ok_or_else(|| format!("--placeholder-env names {name}, which is not set"))?,
                None => env("TOOL_PLACEHOLDER").unwrap_or_default(),
            },
        };
        Ok(Self {
            proxy_url: proxy_url.trim().to_string(),
            path,
            placeholder,
        })
    }
}

/// What the Streamable HTTP transport asks a client to carry from one request to the next.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionState {
    /// The `Mcp-Session-Id` the server issued, echoed on every later request.
    pub session_id: Option<String>,
    /// The `protocolVersion` the server negotiated in its `initialize` result, sent as
    /// `MCP-Protocol-Version` on every later request.
    pub protocol_version: Option<String>,
}

impl SessionState {
    /// The headers for the next request: JSON in, JSON or SSE out, the placeholder bearer, and
    /// whatever session facts the server issued so far.
    pub fn request_headers(&self, placeholder: &str) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json, text/event-stream".to_string()),
        ];
        if !placeholder.is_empty() {
            // The placeholder goes in a header, where the proxy substitutes; never in the body.
            headers.push(("Authorization".to_string(), format!("Bearer {placeholder}")));
        }
        if let Some(session_id) = &self.session_id {
            headers.push(("Mcp-Session-Id".to_string(), session_id.clone()));
        }
        if let Some(version) = &self.protocol_version {
            headers.push(("MCP-Protocol-Version".to_string(), version.clone()));
        }
        headers
    }

    /// Learn from one successful response: a session id header, and the protocol version an
    /// `initialize` result names.
    pub fn observe(&mut self, response_headers: &BTreeMap<String, String>, messages: &[Value]) {
        self.observe_headers(response_headers);
        for message in messages {
            self.observe_message(message);
        }
    }

    /// Learn the session id from the headers of one successful response. Call it before the first
    /// message of that response is written out, so the client's next request carries the id.
    pub fn observe_headers(&mut self, response_headers: &BTreeMap<String, String>) {
        if let Some(id) = response_headers.get("mcp-session-id") {
            let id = id.trim();
            if !id.is_empty() {
                self.session_id = Some(id.to_string());
            }
        }
    }

    /// Learn the protocol version from one message, when it is an `initialize` result.
    pub fn observe_message(&mut self, message: &Value) {
        if let Some(version) = message
            .get("result")
            .and_then(|result| result.get("protocolVersion"))
            .and_then(Value::as_str)
        {
            self.protocol_version = Some(version.to_string());
        }
    }
}

/// What one JSON-RPC message from the client is, by its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// Has an `id`: the server owes it exactly one response.
    Request,
    /// Has no `id`: the server owes it nothing.
    Notification,
    /// Has an `id` and a `result` or an `error`, and no `method`: the client's answer to a request
    /// the server sent. The server owes it nothing; it accepts it with `202`.
    Response,
}

/// Classify one message from the client. An `id` member that is present, even `null`, counts as
/// an id: that is how the old rule read it, and a client that sends `"id": null` still waits.
pub fn classify(message: &Value) -> MessageKind {
    let has_id = message.get("id").is_some();
    let has_method = message.get("method").is_some();
    let has_outcome = message.get("result").is_some() || message.get("error").is_some();
    if has_id && !has_method && has_outcome {
        MessageKind::Response
    } else if has_id {
        MessageKind::Request
    } else {
        MessageKind::Notification
    }
}

/// Whether `message` is the server's response to the client request with id `request_id`: it
/// carries that id, a `result` or an `error`, and no `method`.
pub fn answers(message: &Value, request_id: &Value) -> bool {
    message.get("method").is_none()
        && (message.get("result").is_some() || message.get("error").is_some())
        && message.get("id") == Some(request_id)
}

/// An SSE stream cut into events as its bytes arrive. Feed it what the wire delivered; it returns
/// the `data:` payload of every event that is complete. Events end at a blank line; several `data:`
/// lines in one event join with `\n`; `event:`, `id:`, `retry:` and comment lines are skipped.
/// `\r\n` line ends are accepted. [`Self::finish`] returns a final event without a trailing blank
/// line, which counts.
#[derive(Debug, Default)]
pub struct SseSplitter {
    /// Bytes of the line that has no line end yet.
    partial: Vec<u8>,
    /// The `data:` lines of the event that has no blank line yet.
    data: Vec<String>,
}

impl SseSplitter {
    /// Take in `bytes` and return the payloads of the events they complete, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            self.partial.extend_from_slice(&rest[..newline]);
            rest = &rest[newline + 1..];
            let line = std::mem::take(&mut self.partial);
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(payload) = self.line(line.strip_suffix('\r').unwrap_or(&line)) {
                out.push(payload);
            }
        }
        self.partial.extend_from_slice(rest);
        out
    }

    /// The stream ended. A last line without a line end and a last event without a blank line
    /// both count.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(payload) = self.line(line.strip_suffix('\r').unwrap_or(&line)) {
                out.push(payload);
            }
        }
        if !self.data.is_empty() {
            out.push(std::mem::take(&mut self.data).join("\n"));
        }
        out
    }

    /// One complete line. A blank line closes the event and returns its payload.
    fn line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            return Some(std::mem::take(&mut self.data).join("\n"));
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        if field == "data" {
            self.data.push(value.to_string());
        }
        None
    }
}

/// The JSON-RPC messages one response body carries, by content type. An SSE body yields one message
/// per `data:` payload; a JSON body yields one message, or each element of a batch array; an empty
/// body yields none.
pub fn messages_in(content_type: Option<&str>, body: &[u8]) -> Result<Vec<Value>, String> {
    let text = std::str::from_utf8(body).map_err(|_| "body is not UTF-8".to_string())?;
    let is_sse = content_type
        .map(|c| c.trim().to_ascii_lowercase().starts_with("text/event-stream"))
        .unwrap_or(false);
    if is_sse {
        return sse_data_payloads(text)
            .iter()
            .filter(|payload| !payload.trim().is_empty())
            .map(|payload| {
                serde_json::from_str::<Value>(payload)
                    .map_err(|error| format!("SSE data is not JSON: {error}"))
            })
            .collect();
    }
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("body is not JSON: {error}"))?;
    Ok(match value {
        Value::Array(items) => items,
        other => vec![other],
    })
}

/// The `data:` payloads of a complete SSE stream, one per event. [`SseSplitter`] run over text
/// already in memory.
pub fn sse_data_payloads(text: &str) -> Vec<String> {
    let mut splitter = SseSplitter::default();
    let mut out = splitter.feed(text.as_bytes());
    out.extend(splitter.finish());
    out
}

/// The lines the bridge writes to stdout for one HTTP outcome.
///
/// - A notification draws no line, whatever the status: nothing could correlate a reply to it.
/// - A non-2xx status is a proxy refusal or a vendor rejection, surfaced as one JSON-RPC error on
///   the caller's id so the MCP client stays well-formed and is not left waiting.
/// - A 2xx with messages yields one line per message; with none, an error, for the same reason.
pub fn reply_lines(
    status: u16,
    parsed: Result<Vec<Value>, String>,
    body: &[u8],
    id: &Value,
    is_notification: bool,
) -> Vec<String> {
    if is_notification {
        return Vec::new();
    }
    if !(200..300).contains(&status) {
        let detail = String::from_utf8_lossy(body);
        let detail: String = detail.trim().chars().take(512).collect();
        return vec![error_line(id, -32000, &format!("upstream HTTP {status}: {detail}"))];
    }
    match parsed {
        Ok(messages) if messages.is_empty() => {
            vec![error_line(id, -32000, "upstream returned an empty body for a request")]
        }
        Ok(messages) => messages.iter().map(Value::to_string).collect(),
        Err(why) => vec![error_line(
            id,
            -32700,
            &format!("upstream body could not be parsed: {why}"),
        )],
    }
}

/// One JSON-RPC error line addressed to `id`.
pub fn error_line(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// The line for a request that never reached the proxy.
pub fn transport_error_line(id: &Value, error: &dyn std::fmt::Display) -> String {
    error_line(id, -32603, &format!("proxy endpoint unavailable: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_none(_: &str) -> Option<String> {
        None
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn flags_win_over_the_environment_and_the_path_defaults() {
        let env = |name: &str| match name {
            "PROXY_URL" => Some("http://env:1".to_string()),
            "TOOL_PLACEHOLDER" => Some("env-ph".to_string()),
            _ => None,
        };
        let cfg = BridgeConfig::from_args_and_env(
            &args(&["--proxy-url", "http://flag:2", "--placeholder", "flag-ph"]),
            &env,
        )
        .expect("config");
        assert_eq!(cfg.proxy_url, "http://flag:2");
        assert_eq!(cfg.placeholder, "flag-ph");
        assert_eq!(cfg.path, DEFAULT_PATH);

        let from_env = BridgeConfig::from_args_and_env(&[], &env).expect("env config");
        assert_eq!(from_env.proxy_url, "http://env:1");
        assert_eq!(from_env.placeholder, "env-ph");
    }

    #[test]
    fn a_missing_proxy_url_is_refused_and_a_placeholder_env_is_resolved() {
        let error = BridgeConfig::from_args_and_env(&[], &env_none).expect_err("no proxy url");
        assert!(error.contains("--proxy-url"), "{error}");

        let env = |name: &str| (name == "MY_PH").then(|| "resolved".to_string());
        let cfg = BridgeConfig::from_args_and_env(
            &args(&["--proxy-url", "http://p:1", "--placeholder-env", "MY_PH", "--path", "/mcp/"]),
            &env,
        )
        .expect("config");
        assert_eq!(cfg.placeholder, "resolved");
        assert_eq!(cfg.path, "/mcp/");

        let error = BridgeConfig::from_args_and_env(
            &args(&["--proxy-url", "http://p:1", "--placeholder-env", "UNSET"]),
            &env,
        )
        .expect_err("unset placeholder env");
        assert!(error.contains("UNSET"), "{error}");

        let error = BridgeConfig::from_args_and_env(
            &args(&["--proxy-url", "http://p:1", "--path", "mcp"]),
            &env,
        )
        .expect_err("a relative path");
        assert!(error.contains("--path"), "{error}");
    }

    #[test]
    fn sse_payloads_join_multi_line_data_and_skip_the_rest() {
        let stream = ": keep-alive\r\nevent: message\r\nid: 7\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\ndata:{\"b\":2}\n\nretry: 5\n\ndata: {\"c\":3}";
        assert_eq!(
            sse_data_payloads(stream),
            vec!["{\"a\":\n1}".to_string(), "{\"b\":2}".to_string(), "{\"c\":3}".to_string()]
        );
    }

    #[test]
    fn the_splitter_returns_an_event_as_soon_as_its_blank_line_arrives_whatever_the_cuts() {
        let stream = b": keep-alive\r\nevent: message\r\nid: 7\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\ndata:{\"b\":2}\n\nretry: 5\n\ndata: {\"c\":3}";
        // Byte by byte: every cut point a socket could produce.
        let mut splitter = SseSplitter::default();
        let mut seen = Vec::new();
        for byte in stream.iter() {
            seen.extend(splitter.feed(&[*byte]));
        }
        assert_eq!(seen, vec!["{\"a\":\n1}".to_string(), "{\"b\":2}".to_string()], "two events are complete on the wire");
        seen.extend(splitter.finish());
        assert_eq!(seen.len(), 3, "the last event has no trailing blank line and counts at the end");
        assert_eq!(seen[2], "{\"c\":3}");
        assert!(splitter.finish().is_empty(), "a second finish returns nothing");

        // The first event is available before the second has arrived.
        let mut splitter = SseSplitter::default();
        let first = splitter.feed(b"data: {\"x\":1}\n\ndata: {\"y\"");
        assert_eq!(first, vec!["{\"x\":1}".to_string()]);
        assert!(splitter.feed(b":2}\n").is_empty(), "the second event has no blank line yet");
        assert_eq!(splitter.feed(b"\n"), vec!["{\"y\":2}".to_string()]);
    }

    #[test]
    fn a_message_is_a_request_a_notification_or_a_response_by_its_members() {
        assert_eq!(classify(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})), MessageKind::Request);
        assert_eq!(classify(&json!({"jsonrpc":"2.0","id":null,"method":"x"})), MessageKind::Request, "a null id still waits");
        assert_eq!(classify(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})), MessageKind::Notification);
        assert_eq!(classify(&json!({"jsonrpc":"2.0","id":"srv-1","result":{}})), MessageKind::Response);
        assert_eq!(classify(&json!({"jsonrpc":"2.0","id":"srv-1","error":{"code":1,"message":"m"}})), MessageKind::Response);
        assert_eq!(classify(&json!({"jsonrpc":"2.0","id":9})), MessageKind::Request, "an id without an outcome is a request");
        assert_eq!(classify(&json!([1, 2])), MessageKind::Notification, "an array has no id");

        let id = json!(3);
        assert!(answers(&json!({"jsonrpc":"2.0","id":3,"result":{}}), &id));
        assert!(answers(&json!({"jsonrpc":"2.0","id":3,"error":{"code":1,"message":"m"}}), &id));
        assert!(!answers(&json!({"jsonrpc":"2.0","id":"srv-1","method":"ping"}), &id), "a server request is not the answer");
        assert!(!answers(&json!({"jsonrpc":"2.0","method":"notifications/progress"}), &id));
        assert!(!answers(&json!({"jsonrpc":"2.0","id":4,"result":{}}), &id), "another id");
    }

    #[test]
    fn the_session_learns_the_id_from_headers_and_the_version_from_a_message_separately() {
        let mut state = SessionState::default();
        let mut headers = BTreeMap::new();
        headers.insert("mcp-session-id".to_string(), "sess-9".to_string());
        state.observe_headers(&headers);
        assert_eq!(state.session_id.as_deref(), Some("sess-9"));
        assert!(state.protocol_version.is_none());
        state.observe_message(&json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}));
        assert_eq!(state.protocol_version.as_deref(), Some("2025-06-18"));
        state.observe_message(&json!({"jsonrpc":"2.0","id":"srv-1","method":"ping"}));
        assert_eq!(state.protocol_version.as_deref(), Some("2025-06-18"), "a message without a version changes nothing");
    }

    #[test]
    fn messages_are_read_from_json_a_batch_sse_and_an_empty_body() {
        let one = messages_in(Some("application/json"), br#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
            .expect("one");
        assert_eq!(one.len(), 1);
        let batch = messages_in(Some("application/json; charset=utf-8"), br#"[{"id":1},{"id":2}]"#)
            .expect("batch");
        assert_eq!(batch.len(), 2);
        let sse = messages_in(
            Some("text/event-stream"),
            b"event: message\ndata: {\"id\":1}\n\nevent: message\ndata: {\"method\":\"n\"}\n\n",
        )
        .expect("sse");
        assert_eq!(sse.len(), 2);
        assert!(messages_in(None, b"").expect("empty").is_empty());
        assert!(messages_in(Some("application/json"), b"not json").is_err());
        assert!(messages_in(Some("text/event-stream"), b"data: not json\n\n").is_err());
    }

    #[test]
    fn a_notification_draws_no_reply_whatever_the_status() {
        let id = Value::Null;
        assert!(reply_lines(202, Ok(Vec::new()), b"", &id, true).is_empty());
        assert!(reply_lines(500, Ok(Vec::new()), b"boom", &id, true).is_empty());
        assert!(reply_lines(200, Err("x".into()), b"", &id, true).is_empty());
    }

    #[test]
    fn a_request_always_gets_exactly_one_answer_or_every_message_the_vendor_sent() {
        let id = json!(3);
        let refused = reply_lines(403, Ok(Vec::new()), b"{\"error\":\"nope\"}", &id, false);
        assert_eq!(refused.len(), 1);
        let refused: Value = serde_json::from_str(&refused[0]).unwrap();
        assert_eq!(refused["id"], json!(3));
        assert_eq!(refused["error"]["code"], json!(-32000));
        assert!(refused["error"]["message"].as_str().unwrap().contains("HTTP 403"));

        let empty = reply_lines(200, Ok(Vec::new()), b"", &id, false);
        assert_eq!(empty.len(), 1, "an empty 2xx must not leave the client waiting");
        assert!(empty[0].contains("-32000"));

        let unparsable = reply_lines(200, Err("bad".into()), b"?", &id, false);
        assert!(unparsable[0].contains("-32700"));

        let two = reply_lines(
            200,
            Ok(vec![json!({"jsonrpc":"2.0","method":"notifications/progress"}), json!({"jsonrpc":"2.0","id":3,"result":{}})]),
            b"",
            &id,
            false,
        );
        assert_eq!(two.len(), 2, "a server-side notification before the response is forwarded too");
    }

    #[test]
    fn the_session_carries_the_id_and_the_protocol_version_the_server_issued() {
        let mut state = SessionState::default();
        let first = state.request_headers("ph");
        assert!(first.iter().any(|(k, v)| k == "Authorization" && v == "Bearer ph"));
        assert!(first.iter().any(|(k, v)| k == "Accept" && v.contains("text/event-stream")));
        assert!(!first.iter().any(|(k, _)| k == "Mcp-Session-Id"));

        let mut headers = BTreeMap::new();
        headers.insert("mcp-session-id".to_string(), "sess-1".to_string());
        state.observe(
            &headers,
            &[json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}})],
        );
        let next = state.request_headers("ph");
        assert!(next.iter().any(|(k, v)| k == "Mcp-Session-Id" && v == "sess-1"));
        assert!(next.iter().any(|(k, v)| k == "MCP-Protocol-Version" && v == "2025-06-18"));

        // A later response without the header keeps the session; an empty header does too.
        let mut empty = BTreeMap::new();
        empty.insert("mcp-session-id".to_string(), "  ".to_string());
        state.observe(&empty, &[]);
        assert_eq!(state.session_id.as_deref(), Some("sess-1"));

        // No bearer header at all when the placeholder is empty.
        assert!(!SessionState::default()
            .request_headers("")
            .iter()
            .any(|(k, _)| k == "Authorization"));
    }
}
