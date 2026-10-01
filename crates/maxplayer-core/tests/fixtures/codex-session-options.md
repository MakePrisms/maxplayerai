# Codex session-option captures

Measured 2026-10-01 using isolated npm installs of codex-acp 1.2.0 / Codex
0.147.0 and codex-acp 2.1.1 / Codex 0.159.3. Each ran with a fresh HOME and
CODEX_HOME. DEFAULT_AUTH_REQUEST configured a dummy gateway at
http://127.0.0.1:9/v1 with a non-secret placeholder. No inference request,
production relay, account login, or Docker runtime was used.

The JSON fixture projects only the `model` and `reasoning_effort` options from
real session/new and the subsequent set_config_option replies. Other options,
random session IDs, and the legacy model list are omitted, not fabricated.
Requests: initialize, session/new (temporary cwd, no MCP servers), then
session/set_config_option with configId=model,value=gpt-5.6-sol, followed by
configId=reasoning_effort,value=high. Both versions acknowledged the settings.
Model options contain bare IDs; bracketed `gpt-5.6-sol[high]` is a legacy
models.currentModelId identity, not the model config option's value.

A transparent stdio capture between ACP 1.2.0 and Codex 0.147.0 showed:
- thread/start params did not specify model or reasoning effort;
- thread/start result: model=gpt-5.6-sol, reasoningEffort=null;
- model/list result: gpt-5.6-sol defaultReasoningEffort=low;
- ACP createModelId falls back to that catalog default when the thread's
  reasoningEffort is null. Maxplayer does not hardcode low.
The newer catalog reported by an operator is not the catalog bundled with 0.147.0.
This reproduces the reported default locally; it is not a capture from their Mac.

Authentication note: the historical CODEX_CONFIG/MODEL_PROVIDER "too late" finding
concerned satisfying ACP's auth gate, not model options. DEFAULT_AUTH_REQUEST
continues to establish the fixed gateway. Our model selection runs after
session/new and successful authentication, so it does not participate in that gate.

Authenticated inference, Docker image contents and a signed advertising heartbeat
remain separate live checks; these captures do not claim any of them.
