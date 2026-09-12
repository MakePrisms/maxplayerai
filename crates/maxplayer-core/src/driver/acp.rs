use std::path::PathBuf;
// Tokio (not std) channels for the live stream: `next()` must YIELD while waiting for the agent's
// next update — a blocking receive would freeze every sibling job on the seller node's
// single-threaded LocalSet along with its run loop (issue #223).
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "acp")]
use std::time::Duration;
#[cfg(feature = "acp")]
use tokio::sync::mpsc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 2;
pub type SessionId = String;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Initialize {
    pub protocol_version: u32,
    pub client_capabilities: Caps,
}

impl Initialize {
    pub fn new(client_capabilities: Caps) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub agent_capabilities: Caps,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Caps {
    pub methods: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub mcp_servers: Vec<McpServer>,
    pub env: Vec<(String, String)>,
}

/// One MCP server for the session (ACP `session/new` → `mcpServers[]`). Two wire shapes, and the
/// difference between them is the `type` key:
///
/// * [`Self::Stdio`] carries NO `type` key. The adapter the sandbox image bakes (`claude-agent-acp`
///   0.67.0, `dist/acp-agent.js`, the `mcpServers` loop in `newSession`) treats an entry without
///   `type` as a stdio server, and an entry with any `type` other than `http`/`sse` as unknown — it
///   DROPS that entry. So a stdio entry must never say `type: "stdio"`; the absence is the tag.
/// * [`Self::Http`] carries `type: "http"`, a `url`, and `headers`.
///
/// Both shapes are read off that adapter's own mapping code, not guessed from the spec text. The
/// seller's job path attached no MCP server at all before the Proxy swap route, so the old
/// `{ name, command: [...] }` shape here was never on a wire; it did not match what any adapter
/// reads and is gone.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum McpServer {
    Stdio(McpServerStdio),
    Http(McpServerHttp),
}

impl McpServer {
    /// The server name the agent addresses tools under, whichever shape it is.
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio(server) => &server.name,
            Self::Http(server) => &server.name,
        }
    }
}

/// A stdio MCP server: the agent spawns `command args…` with `env` added to the child's
/// environment and speaks JSON-RPC over its stdin/stdout.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct McpServerStdio {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVariable>,
}

/// A Streamable-HTTP MCP server the agent's own MCP client connects to at `url`, sending `headers`
/// on every request.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct McpServerHttp {
    /// Always `"http"`. A field rather than a serde tag so the stdio variant can carry NO `type`
    /// key at all (see [`McpServer`]).
    #[serde(rename = "type")]
    pub transport: HttpTransport,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<EnvVariable>,
}

/// The one value [`McpServerHttp::transport`] takes.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpTransport {
    Http,
}

/// A `{ name, value }` pair, the ACP encoding of one environment variable or one HTTP header.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EnvVariable {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTurn {
    pub input: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "artifact")]
    Artifact(Artifact),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum SessionUpdate {
    #[serde(rename = "agent_message")]
    AgentMessage(Vec<ContentBlock>),
    #[serde(rename = "agent_message_chunk")]
    AgentMessageChunk(ContentBlock),
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult { id: String, output: Value },
    #[serde(rename = "plan")]
    Plan { entries: Vec<String> },
    #[serde(rename = "permission_request")]
    PermissionRequest(PermissionRequest),
    #[serde(rename = "ext")]
    Ext(ExtMethod),
    #[serde(rename = "turn_ended")]
    TurnEnded(StopReason),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PermissionRequest {
    pub tool: String,
    pub detail: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allow,
    AllowAlways,
    Deny,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Artifact {
    pub uri_or_path: String,
    pub mime: Option<String>,
    pub bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExtMethod {
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug)]
pub struct UpdateStream {
    inner: UpdateStreamInner,
}

#[derive(Debug)]
enum UpdateStreamInner {
    Scripted {
        updates: Vec<SessionUpdate>,
        next_index: usize,
        cancelled: Arc<AtomicBool>,
        emitted_cancelled: bool,
    },
    #[cfg(feature = "acp")]
    Live {
        receiver: mpsc::UnboundedReceiver<SessionUpdate>,
        idle_timeout: Duration,
    },
}

impl UpdateStream {
    pub(crate) fn new(updates: Vec<SessionUpdate>, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            inner: UpdateStreamInner::Scripted {
                updates,
                next_index: 0,
                cancelled,
                emitted_cancelled: false,
            },
        }
    }

    #[cfg(feature = "acp")]
    pub(crate) fn live(
        receiver: mpsc::UnboundedReceiver<SessionUpdate>,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            inner: UpdateStreamInner::Live {
                receiver,
                idle_timeout,
            },
        }
    }

    pub async fn next(&mut self) -> Option<SessionUpdate> {
        match &mut self.inner {
            UpdateStreamInner::Scripted {
                updates,
                next_index,
                cancelled,
                emitted_cancelled,
            } => {
                if cancelled.load(Ordering::SeqCst) {
                    if *emitted_cancelled {
                        None
                    } else {
                        *emitted_cancelled = true;
                        Some(SessionUpdate::TurnEnded(StopReason::Cancelled))
                    }
                } else if *next_index >= updates.len() {
                    None
                } else {
                    let update = updates[*next_index].clone();
                    *next_index += 1;
                    Some(update)
                }
            }
            #[cfg(feature = "acp")]
            UpdateStreamInner::Live {
                receiver,
                idle_timeout,
            } => tokio::time::timeout(*idle_timeout, receiver.recv())
                .await
                .ok()
                .flatten(),
        }
    }
}

use super::UsageMetadata;

/// Extract execution usage from an ACP `session/prompt` JSON-RPC result.
///
/// The prompt result is the only ACP-native usage surface the driver has: whatever the harness
/// reports under its `usage` object is captured here. **Absent-stays-absent** — a result with no
/// recognizable usage returns `None` and nothing is emitted downstream, so a missing number is
/// never rendered as a fabricated zero.
///
/// Token components are read only from a real `usage` object, never guessed off unrelated root
/// fields.
pub fn parse_acp_usage(result: &Value) -> Option<UsageMetadata> {
    // The prompt result IS the ACP `PromptResponse`. `usage` is the spec's usage surface
    // (the unstable `unstable_end_turn_token_usage` capability); `_meta.usage` is the
    // spec-sanctioned extension point. Every field name below is verified against either the
    // ACP `Usage` wire shape (rename_all = "camelCase") or a maintained harness's real output
    // — none are guessed:
    //   - inputTokens / outputTokens / cachedReadTokens / cachedWriteTokens
    //         ACP `Usage` (camelCase) AND claude-code-acp `PromptResponse.usage`.
    //   - input_tokens / output_tokens
    //         Anthropic-usage snake case (claude-code-acp raw snapshot) AND codex TokenUsage.
    //   - reasoning: ACP `Usage.thoughtTokens`; codex `reasoning_output_tokens`.
    //   - cache read: Anthropic `cache_read_input_tokens`; codex `cached_input_tokens`.
    //   - cache write: Anthropic `cache_creation_input_tokens`.
    let usage_obj = result
        .get("usage")
        .or_else(|| result.get("_meta").and_then(|m| m.get("usage")));

    let (input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens) =
        match usage_obj {
            Some(u) => (
                first_u64(u, &["inputTokens", "input_tokens"]),
                first_u64(u, &["outputTokens", "output_tokens"]),
                first_u64(u, &["thoughtTokens", "reasoning_output_tokens"]),
                first_u64(
                    u,
                    &[
                        "cachedReadTokens",
                        "cache_read_input_tokens",
                        "cached_input_tokens",
                    ],
                ),
                first_u64(u, &["cachedWriteTokens", "cache_creation_input_tokens"]),
            ),
            None => (None, None, None, None, None),
        };

    let meta = UsageMetadata {
        // No maintained ACP harness (claude-code-acp, codex, cursor) and no ACP spec field carries a
        // model id or a monetary cost in the `session/prompt` result, so neither is read here. The
        // resolved model IS on the wire — but in the `session/new` response, in either of the two
        // shapes `session_model_from_result` reads (#896), captured separately and OR-filled into
        // usage by the driver (see `AcpDriver::start_session` / `merge_session_model`, #455). Cost
        // stays absent (no harness surfaces one).
        model: None,
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost: None,
    };
    if meta.is_empty() {
        return None;
    }
    Some(meta)
}

fn first_u64(v: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        let Some(raw) = v.get(*key) else { continue };
        if let Some(n) = raw.as_u64() {
            return Some(n);
        }
        if let Some(n) = raw.as_str().and_then(|s| s.trim().parse::<u64>().ok()) {
            return Some(n);
        }
    }
    None
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_usage_stays_absent_never_fabricated() {
        // A bare stop-reason result (today's real claude-agent-acp shape) → no usage at all.
        assert_eq!(parse_acp_usage(&json!({"stopReason": "end_turn"})), None);
        assert_eq!(parse_acp_usage(&json!({})), None);
        // A usage object with no recognizable fields is still nothing.
        assert_eq!(parse_acp_usage(&json!({"usage": {"unrelated": 5}})), None);
    }

    #[test]
    fn acp_native_usage_is_captured_from_the_prompt_result() {
        // Real claude-code-acp `session/prompt` result.usage shape (camelCase, matches the ACP
        // spec `Usage`). No model or cost is present in an ACP prompt result.
        let usage = parse_acp_usage(&json!({
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 100,
                "outputTokens": 40,
                "cachedReadTokens": 4096,
                "cachedWriteTokens": 512,
                "totalTokens": 4748
            }
        }))
        .expect("usage present");

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(40));
        // reasoning absent = unknown, NOT zero.
        assert_eq!(usage.reasoning_tokens, None);
        assert_eq!(usage.cache_read_tokens, Some(4096));
        assert_eq!(usage.cache_write_tokens, Some(512));
        // total = input + output (+ reasoning if present); cache siblings NEVER folded in.
        assert_eq!(usage.total_tokens(), Some(140));
        // Neither is carried on the ACP wire, so neither is fabricated.
        assert_eq!(usage.model, None);
        assert_eq!(usage.cost, None);
    }

    #[test]
    fn reasoning_is_summed_into_total_when_present() {
        // ACP spec `Usage.thoughtTokens`.
        let usage = parse_acp_usage(&json!({
            "usage": {"inputTokens": 10, "outputTokens": 5, "thoughtTokens": 3}
        }))
        .expect("usage present");
        assert_eq!(usage.reasoning_tokens, Some(3));
        assert_eq!(usage.total_tokens(), Some(18));
    }

    #[test]
    fn partial_capture_never_reports_a_total() {
        // Output known, input unknown → no total (a partial must not masquerade as complete).
        let usage = parse_acp_usage(&json!({"usage": {"outputTokens": 40}})).expect("some");
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.total_tokens(), None);
    }

    // --- Compatibility with the maintained ACP harnesses ---------------------------------------
    //
    // The maxplayer repo carries no captured ACP-usage fixtures, so these payloads are constructed
    // from verified sources (cited per test), not from a live capture:
    //   - ACP `Usage` schema: agent-client-protocol-schema/src/v1/agent.rs (PromptResponse.usage,
    //     serde rename_all = "camelCase").
    //   - claude-code-acp: zed-industries/claude-code-acp src/acp-agent.ts `sessionUsage()`.
    //   - codex: openai/codex codex-rs/protocol/src/protocol.rs `TokenUsage`.
    // They pin the field names the parser must keep reading so a rename in either the spec or a
    // maintained harness surfaces here as a failing test rather than a silent usage dropout.

    #[test]
    fn compat_claude_code_acp_prompt_result_usage() {
        // claude-code-acp `PromptResponse.usage` from sessionUsage(): camelCase, matches the ACP
        // spec Usage; no model or cost is present on the wire.
        let usage = parse_acp_usage(&json!({
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 1200,
                "outputTokens": 340,
                "cachedReadTokens": 8192,
                "cachedWriteTokens": 256,
                "totalTokens": 9988
            }
        }))
        .expect("claude-code-acp usage present");
        assert_eq!(usage.input_tokens, Some(1200));
        assert_eq!(usage.output_tokens, Some(340));
        assert_eq!(usage.cache_read_tokens, Some(8192));
        assert_eq!(usage.cache_write_tokens, Some(256));
        assert_eq!(usage.reasoning_tokens, None);
        assert_eq!(usage.total_tokens(), Some(1540));
        assert_eq!(usage.model, None);
        assert_eq!(usage.cost, None);
    }

    #[test]
    fn compat_codex_token_usage_shape() {
        // codex `TokenUsage`: snake_case input_tokens/output_tokens/reasoning_output_tokens/
        // cached_input_tokens/total_tokens. Codex has no cache-write counterpart.
        let usage = parse_acp_usage(&json!({
            "usage": {
                "input_tokens": 900,
                "cached_input_tokens": 512,
                "output_tokens": 210,
                "reasoning_output_tokens": 64,
                "total_tokens": 1686
            }
        }))
        .expect("codex usage present");
        assert_eq!(usage.input_tokens, Some(900));
        assert_eq!(usage.output_tokens, Some(210));
        assert_eq!(usage.reasoning_tokens, Some(64));
        assert_eq!(usage.cache_read_tokens, Some(512));
        assert_eq!(usage.cache_write_tokens, None);
        // total = input + output + reasoning (cache read is a sibling, never folded in).
        assert_eq!(usage.total_tokens(), Some(1174));
        assert_eq!(usage.model, None);
        assert_eq!(usage.cost, None);
    }

    #[test]
    fn compat_acp_spec_usage_shape_incl_cursor_baseline() {
        // Canonical ACP `Usage` (camelCase, incl the spec-only `thoughtTokens`). cursor-agent's
        // ACP usage shape is not publicly documented; this spec-conformant object is the compat
        // baseline the parser holds for cursor and any other harness that emits ACP-native usage.
        let usage = parse_acp_usage(&json!({
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 50,
                "outputTokens": 20,
                "thoughtTokens": 7,
                "cachedReadTokens": 1024,
                "cachedWriteTokens": 128,
                "totalTokens": 1222
            }
        }))
        .expect("acp-spec usage present");
        assert_eq!(usage.input_tokens, Some(50));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.reasoning_tokens, Some(7));
        assert_eq!(usage.cache_read_tokens, Some(1024));
        assert_eq!(usage.cache_write_tokens, Some(128));
        assert_eq!(usage.total_tokens(), Some(77));
    }

    #[test]
    fn compat_usage_under_meta_extension_point() {
        // `_meta` is the ACP spec's sanctioned extension point; a harness that nests usage there
        // is still captured.
        let usage = parse_acp_usage(&json!({
            "stopReason": "end_turn",
            "_meta": {"usage": {"inputTokens": 5, "outputTokens": 3}}
        }))
        .expect("_meta usage present");
        assert_eq!(usage.input_tokens, Some(5));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.total_tokens(), Some(8));
    }
}

#[cfg(test)]
mod mcp_server_wire_tests {
    use super::*;
    use serde_json::json;

    // The adapter drops a stdio entry that carries a `type` key (`claude-agent-acp` 0.67.0 reads
    // `"type" in server` before anything else), so the stdio shape must serialize with none.
    #[test]
    fn a_stdio_server_serializes_with_no_type_key() {
        let server = McpServer::Stdio(McpServerStdio {
            name: "github".into(),
            command: "/usr/local/bin/mcp-http-bridge".into(),
            args: vec!["--proxy-url".into(), "http://host.docker.internal:9100".into()],
            env: vec![EnvVariable {
                name: "TOOL_PLACEHOLDER".into(),
                value: "ph".into(),
            }],
        });
        let wire = serde_json::to_value(&server).expect("encode");
        assert_eq!(
            wire,
            json!({
                "name": "github",
                "command": "/usr/local/bin/mcp-http-bridge",
                "args": ["--proxy-url", "http://host.docker.internal:9100"],
                "env": [{"name": "TOOL_PLACEHOLDER", "value": "ph"}]
            })
        );
        assert!(wire.get("type").is_none(), "a type key makes the adapter drop the entry");
        let back: McpServer = serde_json::from_value(wire).expect("decode");
        assert_eq!(back, server);
    }

    #[test]
    fn an_http_server_serializes_with_type_http() {
        let server = McpServer::Http(McpServerHttp {
            transport: HttpTransport::Http,
            name: "github".into(),
            url: "http://host.docker.internal:9100/mcp/".into(),
            headers: vec![EnvVariable {
                name: "Authorization".into(),
                value: "Bearer ph".into(),
            }],
        });
        let wire = serde_json::to_value(&server).expect("encode");
        assert_eq!(
            wire,
            json!({
                "type": "http",
                "name": "github",
                "url": "http://host.docker.internal:9100/mcp/",
                "headers": [{"name": "Authorization", "value": "Bearer ph"}]
            })
        );
        let back: McpServer = serde_json::from_value(wire).expect("decode");
        assert_eq!(back, server);
    }

    // The session config puts the list under the camelCase key the adapter reads.
    #[test]
    fn the_session_config_carries_the_servers_under_mcp_servers() {
        let cfg = SessionConfig {
            cwd: "/work".into(),
            mcp_servers: vec![McpServer::Stdio(McpServerStdio {
                name: "t".into(),
                command: "c".into(),
                args: Vec::new(),
                env: Vec::new(),
            })],
            env: Vec::new(),
        };
        let wire = serde_json::to_value(&cfg).expect("encode");
        assert_eq!(wire["mcpServers"][0]["name"], json!("t"));
        assert_eq!(wire["mcpServers"][0]["args"], json!([]));
    }
}
