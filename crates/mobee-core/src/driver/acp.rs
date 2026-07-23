use std::path::PathBuf;
#[cfg(feature = "acp")]
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "acp")]
use std::time::Duration;

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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct McpServer {
    pub name: String,
    pub command: Vec<String>,
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
        receiver: mpsc::Receiver<SessionUpdate>,
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
    pub(crate) fn live(receiver: mpsc::Receiver<SessionUpdate>, idle_timeout: Duration) -> Self {
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
            } => receiver.recv_timeout(*idle_timeout).ok(),
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
                first_u64(u, &["cachedReadTokens", "cache_read_input_tokens", "cached_input_tokens"]),
                first_u64(u, &["cachedWriteTokens", "cache_creation_input_tokens"]),
            ),
            None => (None, None, None, None, None),
        };

    let meta = UsageMetadata {
        // No maintained ACP harness (claude-code-acp, codex, cursor) and no ACP spec field
        // carries a model id or a monetary cost in the `session/prompt` result, so neither is
        // read here — a model, when known, comes from the launch preset, not the wire.
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
    // The mobee repo carries no captured ACP-usage fixtures, so these payloads are constructed
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
