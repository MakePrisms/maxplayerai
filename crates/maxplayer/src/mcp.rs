//! Thin MCP stdio surface: `maxplayer mcp`.
//!
//! Writes are newline-delimited JSON-RPC. Reads accept newline JSON *or* legacy
//! `Content-Length` framing (spike scar — Claude Code hung on LSP-only writes).
//!
//! Crash-class fix: relay-reading tools run as async work under one runtime with a
//! hard tool deadline (< Claude-Code client read-timeout ~60s). Slow/failed work
//! returns a graceful tool-error — the server never exits.

use std::io::{BufRead, Write};
use std::time::Duration;

use maxplayer_core::home::{self, MaxplayerHome};
use maxplayer_core::job_lifecycle;
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(feature = "wallet")]
use crate::daemon;

const SUCCESS: i32 = 0;
const RUNTIME_ERROR: i32 = 2;

/// Hard cap per `tools/call`. Confirmed under Claude-Code MCP client default (~60s)
/// with margin (Scribe ★1). Cap-hit → graceful tool-error; server stays up.
const TOOL_DEADLINE_SECS: u64 = 15;

#[derive(Debug, Deserialize)]
struct McpRequest {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// The MCP owns no money authority — the buyer daemon does. The MCP only needs the home to resolve
/// the daemon socket (connect-or-spawn) and to run the never-echo-secret guard over daemon replies.
struct McpState {
    home: MaxplayerHome,
}

/// Run the MCP server on the provided stdio handles until stdin EOF.
pub fn run(out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let state = match bootstrap_state() {
        Ok(state) => state,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let _ = writeln!(
        err,
        "maxplayer mcp ready (home={}, key_created={}, mint={}, relay={}, tool_deadline_secs={})",
        state.home.root.display(),
        state.home.key_created,
        state.home.config.default_mint(),
        state.home.config.relay_url,
        TOOL_DEADLINE_SECS
    );

    // Multi-thread so sync verify/pay inside authorize_pay_async does not starve
    // the runtime while still honoring the outer tool deadline.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("maxplayer-mcp")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "mcp runtime: {error}");
            return RUNTIME_ERROR;
        }
    };

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    loop {
        let request = match read_mcp_request(&mut input) {
            Ok(Some(request)) => request,
            Ok(None) => return SUCCESS,
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return RUNTIME_ERROR;
            }
        };
        // Notifications (no id) get no response.
        if request.id.is_none() {
            if request.method == "notifications/initialized" {
                continue;
            }
            let _ = writeln!(err, "ignoring MCP notification {}", request.method);
            continue;
        }
        let response = runtime.block_on(dispatch_async(&state, &request));
        if let Err(error) = write_mcp_response(out, &response) {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    }
}

fn bootstrap_state() -> Result<McpState, String> {
    let root = home::default_home_dir().map_err(|error| error.to_string())?;
    let home = home::bootstrap(root).map_err(|error| error.to_string())?;
    Ok(McpState { home })
}

#[cfg(test)]
fn dispatch(state: &McpState, request: &McpRequest) -> Value {
    // Sync entry for unit tests that don't hold a runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test mcp runtime");
    runtime.block_on(dispatch_async(state, request))
}

async fn dispatch_async(state: &McpState, request: &McpRequest) -> Value {
    let id = request.id.clone().unwrap_or(Value::Null);
    match request.method.as_str() {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "maxplayer",
                    "version": maxplayer_core::version(),
                },
            }),
        ),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools() })),
        "tools/call" => {
            match tokio::time::timeout(
                Duration::from_secs(TOOL_DEADLINE_SECS),
                call_tool_async(state, &request.params),
            )
            .await
            {
                Ok(Ok(result)) => ok(id, result),
                Ok(Err(message)) => tool_error(id, message),
                Err(_) => tool_error(
                    id,
                    format!(
                        "tool deadline exceeded ({TOOL_DEADLINE_SECS}s); server still alive — retry or narrow the call"
                    ),
                ),
            }
        }
        other => error_response(id, -32601, format!("method not found: {other}")),
    }
}

/// The slimmed MCP surface is the buyer TRADE LOOP only: post_job → get_job → award_claim →
/// collect. Wallet management (setup / balance / mint / send / receive / melt / invoice / mints /
/// reconcile), profile, stub-pay, and the lower-level accept/authorize_pay primitives moved to the
/// `maxplayer` CLI. A kept tool that needs a missing prerequisite returns an actionable error naming
/// the CLI command to run (see [`missing_prereq_hint`]).
fn tools() -> Value {
    json!([
        {
            "name": "post_job",
            "description": "Publish a real maxplayer job offer (OFFER kind) to the configured maxplayer relay, then let the buyer daemon drive the award: once a payable seller claim appears the daemon auto-awards it under the hood, so the normal flow is just post_job then collect (two calls). max_sats caps what the daemon will commit to (defaults to amount_sats); it never auto-awards a claim it cannot pay. harness is a hard award filter (only a seller advertising it can be awarded); model is a recorded auto-award preference. Targeted seller p-tag is the documented default (pass seller_pubkey); set untargeted=true for an open offer. Optional repo+branch attach git delivery tags. CONTRIBUTION (freelance-PR) mode: supply target_repo_owner + target_repo_url + base_branch + base_oid to post a job-class=contribution offer against a repo you own (seller forks it and delivers a PR); these four are ALL-OR-NOTHING (a partial set is refused). Omit all four ⇒ from-scratch job. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "output": { "type": "string", "description": "MIME / output type (e.g. text/plain)" },
                    "amount_sats": { "type": "integer", "minimum": 0 },
                    "max_sats": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Per-job spend ceiling for the daemon's auto-award (defaults to amount_sats). A claim priced above it, or that the buyer cannot pay, is never auto-awarded."
                    },
                    "harness": {
                        "type": "string",
                        "description": "Request a specific seller harness (e.g. claude|cursor|codex). Posted on the offer as [\"param\",\"agent\",<name>] and enforced as a HARD award filter: only a seller advertising that harness on its claim can be awarded, and a seller that cannot run it will not claim at all. Omit it (or pass \"any\") for no preference. Also recorded as the auto-award preference it always was. NOTE: this is the REQUEST vocabulary (preset labels); attribution surfaces (get_job results[].harness, collect agent_used) return the RESOLVED harness id (e.g. claude → claude-agent-acp) — relate the two semantically, never by string equality."
                    },
                    "model": {
                        "type": "string",
                        "description": "Preferred seller model. Recorded as an auto-award preference; not yet a hard filter."
                    },
                    "seller_pubkey": {
                        "type": "string",
                        "description": "Targeted seller hex pubkey (documented default)"
                    },
                    "untargeted": {
                        "type": "boolean",
                        "description": "When true, omit p-tag (open offer). Default false."
                    },
                    "deadline_unix": { "type": "integer", "minimum": 0 },
                    "repo": { "type": "string", "description": "Optional https git repo for delivery bind" },
                    "branch": { "type": "string" },
                    "target_repo_owner": {
                        "type": "string",
                        "description": "Contribution mode: owner pubkey (64 hex) of the target repo you own. Requires target_repo_url + base_branch + base_oid."
                    },
                    "target_repo_url": {
                        "type": "string",
                        "description": "Contribution mode: https/relay-git clone URL of the target repo (ext::/file/ssh refused). Requires target_repo_owner + base_branch + base_oid."
                    },
                    "base_branch": {
                        "type": "string",
                        "description": "Contribution mode: base branch the contribution must descend from. Requires target_repo_owner + target_repo_url + base_oid."
                    },
                    "base_oid": {
                        "type": "string",
                        "description": "Contribution mode: exact base commit oid (40 lowercase hex, a git sha1 commit oid) the contribution must descend from. Requires target_repo_owner + target_repo_url + base_branch."
                    },
                    "accepts": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Contribution mode (optional): accepted delivery forms. Defaults to [\"fork\"] and must include \"fork\" (v1 fork-only)."
                    }
                },
                "required": ["task", "output", "amount_sats"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_job",
            "description": format!("Read job state from the relay (offer + claims + results). Surfaces claim created_at and flags the most-recent LIVE claim. Optional include_display_names=true adds best-effort cosmetic kind-0 names; the default skips that extra network fetch and hex pubkeys remain authoritative. Optional wait_for=claim|result long-poll; timeout_secs bounds the wait, capped internally at {cap}s (values above {cap} are refused, not silently shortened) — omit timeout_secs to use the {cap}s default. Local accept-bind attached if present. Results may carry seller-claimed exec-metadata attribution (harness, model): harness is the RESOLVED id (e.g. claude-agent-acp) — a DIFFERENT vocabulary from post_job's harness labels (claude), so never string-compare the two — and every such value is an attribution, not a verification. Never invents claims/results.", cap = job_lifecycle::WAIT_FOR_CAP_SECS),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Offer event id (hex)" },
                    "wait_for": { "type": "string", "enum": ["claim", "result"] },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": job_lifecycle::WAIT_FOR_CAP_SECS },
                    "include_display_names": { "type": "boolean", "description": "Opt in to an additional kind-0 profile fetch for cosmetic display names (default false)." }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "collect",
            "description": "Single-call buyer collect: if no accept-bind exists yet, accept the delivered claim itself (fetch the seller's result from the relay and record the co-signed pay-bind — the same accept path `maxplayer accept` runs), verify the delivery integrity (the delivered branch must tip at the accepted commit — the PayPathDeliveryVerifier tip-match), auto-pay the seller through the sealed money path (BudgetGate → PaymentService::run, single-redeem + mint-compat intact), then materialize the paid files into <home>/results/<job_id>. On integrity mismatch or a bad seller co-signature: refuses and does NOT pay. Idempotent: re-collecting an already-paid job re-materializes without a second payment. If the wallet holds no funds it refuses with a message pointing at `maxplayer wallet setup`. Returns {pay: {state, attempt_id, amount_sats, spent_total_sats}, commit_oid, path, files, agent_used, model_used} — agent_used/model_used are the seller-claimed harness/model that produced the paid result (null = the seller reported nothing; an attribution, never a verification). agent_used is the RESOLVED harness id (e.g. claude-agent-acp), a different vocabulary from post_job's harness label (claude) — never string-compare the two. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Offer event id (hex)" },
                    "out": { "type": "string", "description": "Optional folder NAME (no path separators) under <home>/results" }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "award_claim",
            "description": "Manually award a specific seller claim (the fine-grain override of the daemon's auto-award): reserve the funds and publish the buyer AWARD (kind-3405, status=accepted) selecting that claim so the seller executes and every other claimant releases without spending compute. The daemon refuses to award a claim it cannot pay or whose price exceeds max_sats (defaults to the offer amount). Awards are WRITE-ONCE per job: the first call pins one signed award event (sealing the claim AND the amount — max_sats applies to the first call only), and every retry re-sends that exact event — a retry can never award a different claim or publish a duplicate, so retrying after an ambiguous error (e.g. 'relay gave no verdict') is always safe and is the way to converge. A claim_id that contradicts an already-pinned attempt is refused. No pay-bind — settle after delivery with collect. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "claim_id": { "type": "string" },
                    "max_sats": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Spend ceiling for this manual award (defaults to the offer amount). A claim priced above it is refused."
                    }
                },
                "required": ["job_id", "claim_id"],
                "additionalProperties": false
            }
        },
    ])
}

async fn call_tool_async(state: &McpState, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call missing name".to_owned())?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    // The MCP is a thin client of the buyer daemon: the trade-loop tools route over the daemon
    // socket (connect-or-spawn), which owns the wallet, budget ledger, and reservation ledger — the
    // single money authority. Everything else moved to the `maxplayer` CLI; a stale client calling a
    // moved tool gets a pointer to the command that replaced it. `award_claim` maps to the daemon's
    // `award` RPC (manual, claim_id-named award).
    match name {
        "post_job" => route_tool(state, "post_job", "post_job", arguments).await,
        "get_job" => route_tool(state, "get_job", "get_job", arguments).await,
        "collect" => route_tool(state, "collect", "collect", arguments).await,
        "award_claim" => route_tool(state, "award_claim", "award", arguments).await,
        moved => Err(moved_tool_error(moved)),
    }
}

/// Route a trade-loop tool over the buyer daemon socket (connect-or-spawn). `tool` is the MCP tool
/// name (for errors/hints); `method` is the daemon RPC. The tool arguments are forwarded verbatim as
/// RPC params — the daemon is the sole authority for validation and money, so the MCP adds no
/// second-guessing. The daemon never returns the secret key; the never-echo guard is defense in
/// depth over its reply.
#[cfg(feature = "wallet")]
async fn route_tool(
    state: &McpState,
    tool: &str,
    method: &str,
    arguments: Value,
) -> Result<Value, String> {
    let home = state.home.clone();
    let rpc = method.to_owned();
    // The daemon client is synchronous (a plain UnixStream) and may spawn+poll a daemon on cold
    // start, so run it off the async reactor under the outer tool deadline.
    let body = tokio::task::spawn_blocking(move || daemon::ensure_then_call(&home, &rpc, arguments))
        .await
        .map_err(|error| format!("buyer daemon call task failed: {error}"))?
        .map_err(|error| with_prereq_hint(tool, error))?;
    guard_never_echo(state, tool, &body)?;
    Ok(tool_ok(with_ok(body)))
}

#[cfg(not(feature = "wallet"))]
async fn route_tool(
    _state: &McpState,
    tool: &str,
    _method: &str,
    _arguments: Value,
) -> Result<Value, String> {
    Err(format!(
        "{tool} requires the wallet feature (rebuild with --features wallet, on by default)"
    ))
}

/// Tag a daemon result object with `ok: true` for the MCP surface (a non-object result is wrapped).
fn with_ok(body: Value) -> Value {
    match body {
        Value::Object(mut map) => {
            map.insert("ok".into(), json!(true));
            Value::Object(map)
        }
        other => json!({ "ok": true, "result": other }),
    }
}

/// Defense in depth: refuse a daemon reply that would echo the buyer secret key. The daemon never
/// includes it, so this only ever fires on a bug.
#[cfg(feature = "wallet")]
fn guard_never_echo(state: &McpState, tool: &str, body: &Value) -> Result<(), String> {
    if let Ok(secret) = home::read_secret_key_hex(&state.home) {
        if !secret.is_empty() && body.to_string().contains(&secret) {
            return Err(format!("{tool} refused: response would echo secret key"));
        }
    }
    Ok(())
}

/// Actionable error for a tool that moved off the MCP surface to the `maxplayer` CLI, or an unknown
/// tool. Names the exact CLI command a stale caller should run instead.
fn moved_tool_error(name: &str) -> String {
    let cli = match name {
        "setup_wallet" => "maxplayer wallet setup",
        "wallet_balance" => "maxplayer wallet balance",
        "wallet_mint" | "wallet_invoice" => "maxplayer wallet mint / maxplayer wallet invoice",
        "wallet_send" => "maxplayer wallet send",
        "wallet_receive" => "maxplayer wallet receive",
        "wallet_melt" => "maxplayer wallet melt",
        "wallet_mints" => "maxplayer wallet mints",
        "reconcile_wallet" => "maxplayer wallet reconcile",
        "set_profile" => "maxplayer profile set",
        "stub_pay" => "maxplayer stub-pay",
        "accept_claim" => "maxplayer accept",
        "authorize_pay" | "get_result" => "maxplayer collect",
        other => {
            return format!(
                "unknown tool: {other} (MCP surface is post_job, get_job, award_claim, collect)"
            )
        }
    };
    format!(
        "tool `{name}` moved to the maxplayer CLI — run `{cli}`. The MCP surface is the trade loop only \
         (post_job, get_job, award_claim, collect)."
    )
}


/// Wallet bootstrap/funding moved off the MCP surface to `maxplayer wallet setup`, so a kept trade tool
/// that fails because the wallet is unfunded / its mint is unreachable appends the actionable CLI
/// remedy to the underlying error. Pure over the message so it stays testable without fixtures;
/// non-prerequisite errors pass through unchanged.
fn with_prereq_hint(tool: &str, error: String) -> String {
    let lower = error.to_lowercase();
    let funds_prereq = lower.contains("no balance at any accepted mint")
        || lower.contains("insufficient")
        || lower.contains("mint_unreachable")
        || lower.contains("real-mint fence");
    if funds_prereq {
        format!(
            "{error} — {tool} prerequisite: fund your wallet with `maxplayer wallet setup` or \
             `maxplayer wallet mint <sats>`. Both invoice the configured mint; you pay that invoice \
             to fund the wallet."
        )
    } else {
        error
    }
}

fn tool_ok(body: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": body.to_string() }],
        "structuredContent": body,
        "isError": false
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i32, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn tool_error(id: Value, message: String) -> Value {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true
        }),
    )
}

fn read_mcp_request(input: &mut dyn BufRead) -> Result<Option<McpRequest>, String> {
    let mut first = String::new();
    let bytes = input
        .read_line(&mut first)
        .map_err(|error| format!("failed to read MCP request: {error}"))?;
    if bytes == 0 {
        return Ok(None);
    }
    if first.trim().is_empty() {
        return read_mcp_request(input);
    }
    if !first.to_ascii_lowercase().starts_with("content-length:") {
        return serde_json::from_str(first.trim_end())
            .map(Some)
            .map_err(|error| format!("invalid MCP JSON line: {error}"));
    }
    let length = first
        .split_once(':')
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .ok_or_else(|| "invalid MCP Content-Length header".to_string())?;
    loop {
        let mut header = String::new();
        let bytes = input
            .read_line(&mut header)
            .map_err(|error| format!("failed to read MCP header: {error}"))?;
        if bytes == 0 {
            return Err("MCP stream ended inside headers".into());
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
    }
    let mut body = vec![0; length];
    std::io::Read::read_exact(input, &mut body)
        .map_err(|error| format!("failed to read MCP body: {error}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| format!("invalid MCP JSON body: {error}"))
}

fn write_mcp_response(out: &mut dyn Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *out, value)
        .map_err(|error| format!("failed to encode MCP: {error}"))?;
    out.write_all(b"\n")
        .map_err(|error| format!("failed to write MCP newline: {error}"))?;
    out.flush()
        .map_err(|error| format!("failed to flush MCP response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    // The post_job schema promises that harness constrains award selection while model does not.
    // Keep those caller-facing claims tied to the real award predicate: removing the harness
    // predicate or weakening either description makes this test fail. If AwardFilters grows a
    // model filter, update both the model description and this test together.
    #[cfg(feature = "wallet")]
    #[test]
    fn post_job_award_filter_descriptions_match_enforcement() {
        use maxplayer_core::buyer::lifecycle::{select_awardable_claim, AwardFilters};
        use maxplayer_core::gateway::creq::build_seller_creq;
        use maxplayer_core::home::DEFAULT_MINT_URL;
        use maxplayer_core::job_lifecycle::{ClaimView, JobView};

        let listed_tools = tools();
        let post_job = listed_tools
            .as_array()
            .expect("tools array")
            .iter()
            .find(|tool| tool["name"] == "post_job")
            .expect("post_job tool");
        let properties = &post_job["inputSchema"]["properties"];
        let harness_description = properties["harness"]["description"]
            .as_str()
            .expect("harness description")
            .to_ascii_lowercase();
        let model_description = properties["model"]["description"]
            .as_str()
            .expect("model description")
            .to_ascii_lowercase();

        assert!(harness_description.contains("hard award filter"));
        assert!(harness_description.contains("only a seller advertising"));
        assert!(model_description.contains("recorded as an auto-award preference"));
        assert!(model_description.contains("not yet a hard filter"));

        let job_id = "a".repeat(64);
        let seller_pubkey = "aa1e5f8c9d3b6a2f4e7c1d0b8a5f3e2c1d0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f";
        let creq = build_seller_creq(
            &job_id,
            10,
            "sat",
            &[DEFAULT_MINT_URL.to_owned()],
            seller_pubkey,
        )
        .expect("payable creq");
        let view = JobView {
            job_id,
            offer: None,
            claims: vec![ClaimView {
                claim_id: "c".repeat(64),
                created_at: 1,
                seller_pubkey: seller_pubkey.to_owned(),
                display_name: None,
                status: "processing".to_owned(),
                live: true,
                creq: Some(creq),
                agents: vec!["codex".to_owned()],
            }],
            results: Vec::new(),
            live_claim_id: None,
            accepted: None,
            pending: false,
            read_confirmed: true,
        };
        let filters = AwardFilters {
            offer_amount_sats: 10,
            max_sats: 10,
            buyer_mint: DEFAULT_MINT_URL,
            allow_real_mints: false,
            requested_agent: Some("claude"),
        };

        assert_eq!(
            select_awardable_claim(&view, &filters),
            None,
            "a payable codex-only claim must not win a job requesting claude"
        );
    }

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-mcp-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn state_at(root: &std::path::Path) -> McpState {
        let home = home::bootstrap(root).expect("bootstrap");
        McpState { home }
    }

    // #98: get_result materializes a PAID delivery's files to <home>/results/<job_id> and returns
    // {path, commit, files}. Builds a delivered-job fixture — a buyer store bare repo holding the
    // commit under its retention ref + an accept-bind pinning that commit — then proves get_result
    // writes the exact tree to the exact on-disk location.
    #[test]
    fn response_uses_newline_delimited_json_rpc() {
        let mut output = Vec::new();
        write_mcp_response(
            &mut output,
            &json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } }),
        )
        .expect("write");
        let response = String::from_utf8(output).expect("utf8");
        assert!(!response.starts_with("Content-Length:"));
        assert!(response.ends_with('\n'));
        let decoded: Value = serde_json::from_str(response.trim_end()).expect("json");
        assert_eq!(decoded["result"]["ok"], true);
    }

    #[test]
    fn read_accepts_newline_and_content_length() {
        let newline = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
        let request = read_mcp_request(&mut Cursor::new(&newline[..]))
            .expect("read newline")
            .expect("present");
        assert_eq!(request.method, "ping");

        let body = br#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = framed.into_bytes();
        bytes.extend_from_slice(body);
        let request = read_mcp_request(&mut Cursor::new(bytes))
            .expect("read framed")
            .expect("present");
        assert_eq!(request.method, "ping");
        assert_eq!(request.id, Some(json!(2)));
    }

    // The slimmed MCP surface is EXACTLY the buyer trade loop — nothing else advertised. Wallet,
    // profile, stub-pay, accept, authorize_pay, get_result moved to the CLI.
    #[test]
    fn tools_list_is_slimmed_to_the_trade_loop() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .expect("array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["post_job", "get_job", "collect", "award_claim"]);
    }

    #[test]
    fn get_job_timeout_schema_maximum_tracks_lifecycle_cap() {
        let tools = tools();
        let get_job = tools
            .as_array()
            .expect("tools array")
            .iter()
            .find(|tool| tool["name"] == "get_job")
            .expect("get_job tool");
        assert_eq!(
            get_job["inputSchema"]["properties"]["timeout_secs"]["maximum"],
            json!(job_lifecycle::WAIT_FOR_CAP_SECS)
        );
        let description = get_job["description"]
            .as_str()
            .expect("get_job description");
        assert!(description.contains(&format!(
            "values above {} are refused, not silently shortened",
            job_lifecycle::WAIT_FOR_CAP_SECS
        )));
        assert!(description.contains(&format!(
            "omit timeout_secs to use the {}s default",
            job_lifecycle::WAIT_FOR_CAP_SECS
        )));
    }

    // A moved tool called by a stale client returns an actionable error naming the CLI command.
    #[test]
    fn moved_tools_point_at_their_cli_command() {
        assert!(moved_tool_error("setup_wallet").contains("maxplayer wallet setup"));
        assert!(moved_tool_error("wallet_balance").contains("maxplayer wallet balance"));
        assert!(moved_tool_error("reconcile_wallet").contains("maxplayer wallet reconcile"));
        assert!(moved_tool_error("set_profile").contains("maxplayer profile set"));
        assert!(moved_tool_error("stub_pay").contains("maxplayer stub-pay"));
        assert!(moved_tool_error("accept_claim").contains("maxplayer accept"));
        assert!(moved_tool_error("authorize_pay").contains("maxplayer collect"));
        assert!(moved_tool_error("get_result").contains("maxplayer collect"));
        assert!(moved_tool_error("bogus").contains("unknown tool"));
    }

    // A kept trade tool that fails on a missing funds prerequisite appends the actionable CLI
    // remedy; a non-prerequisite error passes through unchanged.
    #[test]
    fn prereq_hint_names_wallet_setup_on_funds_failure() {
        let mapped = with_prereq_hint(
            "collect",
            "authorize_pay refused: the single-mint buyer wallet holds no balance at any accepted mint"
                .to_owned(),
        );
        assert!(mapped.contains("maxplayer wallet setup"), "message: {mapped}");
        assert!(mapped.contains("collect prerequisite"), "message: {mapped}");
        // #595 (mints-are-mints): the hint must not fork mint classes or say "real".
        // RED-ON-REVERT: re-adding "a REAL mint" / "real sats" reds this.
        assert!(
            !mapped.contains("REAL") && !mapped.to_lowercase().contains("real sats"),
            "prereq hint must not fork mint classes / say 'real': {mapped}"
        );

        let untouched = with_prereq_hint("post_job", "task must be non-empty".to_owned());
        assert_eq!(untouched, "task must be non-empty");
    }

    // A tool error flows through dispatch as isError=true and never echoes the secret. Uses a
    // moved tool (stub_pay) — its actionable "moved to CLI" refusal is a representative error path.
    #[test]
    fn tools_call_error_path_never_echoes_secret() {
        let root = temp_home("never-echo-err");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(1)),
                method: "tools/call".into(),
                params: json!({
                    "name": "stub_pay",
                    "arguments": { "amount_sats": 1 }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret));
        assert_eq!(response["result"]["isError"], true);
    }

    // post_job/collect/award business validation (targeting, zero/past deadline, contribution pins,
    // over-budget, integrity+pay) now lives in the buyer daemon and maxplayer-core, tested there
    // (job_lifecycle::post_job_* , buyer::* , collect_integrity). The MCP is a thin router; its own
    // tests cover the transport surface (framing, tool list, moved-tool pointers, never-echo).
}
