//! Thin MCP stdio surface: `mobee mcp`.
//!
//! Writes are newline-delimited JSON-RPC. Reads accept newline JSON *or* legacy
//! `Content-Length` framing (spike scar — Claude Code hung on LSP-only writes).
//!
//! Crash-class fix: relay-reading tools run as async work under one runtime with a
//! hard tool deadline (< Claude-Code client read-timeout ~60s). Slow/failed work
//! returns a graceful tool-error — the server never exits.

use std::io::{BufRead, Write};
use std::sync::Mutex;
use std::time::Duration;

use mobee_core::authorize_pay::{self, AuthorizePayRequest};
use mobee_core::budget::BudgetGate;
use mobee_core::home::{self, MobeeHome};
#[cfg(feature = "wallet")]
use mobee_core::job_lifecycle::{
    self, AcceptClaimRequest, AwardClaimRequest, ContributionSpec, GetJobRequest, JobKind,
    PostJobRequest, WaitFor,
};
use serde::Deserialize;
use serde_json::{Value, json};

const SUCCESS: i32 = 0;
const RUNTIME_ERROR: i32 = 2;

/// Hard cap per `tools/call`. Confirmed under Claude-Code MCP client default (~60s)
/// with margin (Scribe ★1). Cap-hit → graceful tool-error; server stays up.
const TOOL_DEADLINE_SECS: u64 = 15;
/// `setup_wallet` mint-fund is slower than relay reads; keep under ~60s client timeout
/// with margin, but allow enough wall time for testnut quote→pay→mint.
const SETUP_WALLET_DEADLINE_SECS: u64 = 45;

#[derive(Debug, Deserialize)]
struct McpRequest {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

struct McpState {
    home: MobeeHome,
    gate: Mutex<BudgetGate>,
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
        "mobee mcp ready (home={}, key_created={}, mint={}, relay={}, tool_deadline_secs={})",
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
        .thread_name("mobee-mcp")
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
    let gate = Mutex::new(BudgetGate::from_home(&home).map_err(|error| error.to_string())?);
    Ok(McpState { home, gate })
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
                    "name": "mobee",
                    "version": mobee_core::version(),
                },
            }),
        ),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools() })),
        "tools/call" => {
            let tool_name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let deadline_secs = if matches!(
                tool_name,
                "setup_wallet" | "wallet_mint" | "wallet_invoice" | "wallet_melt"
            ) {
                SETUP_WALLET_DEADLINE_SECS
            } else {
                TOOL_DEADLINE_SECS
            };
            match tokio::time::timeout(
                Duration::from_secs(deadline_secs),
                call_tool_async(state, &request.params),
            )
            .await
            {
                Ok(Ok(result)) => ok(id, result),
                Ok(Err(message)) => tool_error(id, message),
                Err(_) => tool_error(
                    id,
                    format!(
                        "tool deadline exceeded ({deadline_secs}s); server still alive — retry or narrow the call"
                    ),
                ),
            }
        }
        other => error_response(id, -32601, format!("method not found: {other}")),
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "setup_wallet",
            "description": "Bootstrap ~/.mobee (config + autogen key + wallet dir) and fund against the hard-pinned testnut mint (mint quote → auto-pay → mint). Returns status + balance; the secret key never appears in the response.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "set_profile",
            "description": "Set optional buyer identity: write [profile] name/about to ~/.mobee/config.toml and publish/replace the buyer kind-0 metadata event on the configured relay. Called with no args = re-publish from existing config. Never required — absent profile leaves the buyer as hex. Never echoes the secret key. Kind-0 names are untrusted display metadata only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Buyer display name published in kind-0" },
                    "about": { "type": "string", "description": "Buyer about text published in kind-0" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "post_job",
            "description": "Publish a real mobee job offer (OFFER kind) to the configured mobee relay. Targeted seller p-tag is the documented default (pass seller_pubkey); set untargeted=true for an open offer. Optional repo+branch attach git delivery tags. CONTRIBUTION (freelance-PR) mode: supply target_repo_owner + target_repo_url + base_branch + base_oid to post a job-class=contribution offer against a repo you own (seller forks it and delivers a PR); these four are ALL-OR-NOTHING (a partial set is refused). Omit all four ⇒ from-scratch job (unchanged). Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "output": { "type": "string", "description": "MIME / output type (e.g. text/plain)" },
                    "amount_sats": { "type": "integer", "minimum": 0 },
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
            "description": "Read job state from the relay (offer + claims + results). Surfaces claim created_at and flags the most-recent LIVE claim. Best-effort kind-0 display_name alongside each pubkey (claims[].display_name, results[].display_name, offer.seller_display_name) — cosmetic only; hex pubkey remains authoritative. Optional wait_for=claim|result long-poll. Local accept-bind attached if present. Never invents claims/results.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Offer event id (hex)" },
                    "wait_for": { "type": "string", "enum": ["claim", "result"] },
                    "timeout_secs": { "type": "integer", "minimum": 1 }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_result",
            "description": "Materialize the files of a PAID delivery into a local folder. Reads the delivered commit from the buyer store (retained during authorize_pay) for the accepted job — no network, no auth, no spend. Returns {path, commit, files}. Defaults to <home>/results/<job_id>; pass out for a different folder name under <home>/results.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Offer event id (hex) with a prior accept_claim bind" },
                    "out": { "type": "string", "description": "Optional folder NAME (no path separators) under <home>/results" }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "award_claim",
            "description": "Award a seller claim BEFORE work: publish the buyer AWARD (kind-3405, status=accepted) selecting one claim so that seller executes and every other claimant releases without spending compute. Verifies the claim is present, still processing, and (for a targeted offer) authored by the targeted seller. Returns quoted_mints — the mints the claim's creq will be paid at — so an incompatible award is visible before you commit. No pay-bind — settle after delivery with accept_claim. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "claim_id": { "type": "string" }
                },
                "required": ["job_id", "claim_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "accept_claim",
            "description": "Accept a seller claim: publish the buyer AWARD (status=accepted) and record local pay-bind {seller_pubkey, result_id, commit_oid, repo, branch, job_hash} for authorize_pay. Requires a matching git result on the relay. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "claim_id": { "type": "string" },
                    "result_id": {
                        "type": "string",
                        "description": "Optional; defaults to newest git result from the claim seller"
                    }
                },
                "required": ["job_id", "claim_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "stub_pay",
            "description": "Exercise the MCP budget gate over a mock pay (allow/deny). Does not call piece-6 run()/advance(). Caps bind from ~/.mobee config only — tool args that set per_job/total are ignored.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount_sats": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Spend amount in sats to authorize against config caps"
                    }
                },
                "required": ["amount_sats"],
                "additionalProperties": true
            }
        },
        {
            "name": "authorize_pay",
            "description": "Real testnut pay: BudgetGate → PayPathDeliveryVerifier → PaymentService::run(). Documented default: job_id form (job_id + amount_sats + buyer-supplied delivery_integrity_hash) binds fields from accept_claim — tip-match hash is NEVER auto-filled from the claim oid (D2). Explicit 9-field form kept for harness. If an accept-bind exists, seller/result/commit mismatches are REFUSED. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "amount_sats": { "type": "integer", "minimum": 0 },
                    "delivery_integrity_hash": {
                        "type": "string",
                        "description": "Buyer commitment — full lowercase git oid that must tip-match (required; never auto-filled)"
                    },
                    "result_id": { "type": "string" },
                    "job_hash": {
                        "type": "string",
                        "description": "Lowercase SHA-256 job digest (64 hex) — explicit form"
                    },
                    "seller_pubkey": { "type": "string" },
                    "repo": {
                        "type": "string",
                        "description": "Seller git locator (https / relay-git only; ext::/file/ssh refused)"
                    },
                    "branch": { "type": "string" },
                    "commit_oid": { "type": "string" }
                },
                "required": ["job_id", "amount_sats", "delivery_integrity_hash"],
                "additionalProperties": true
            }
        },
        {
            "name": "reconcile_wallet",
            "description": "Retire incomplete CDK Send(ProofsReserved) ops that have no confirmed attempt, a non-empty reserved set, and whose reserved proofs are all NUT-07 Unspent (non-mutating post_check_state). Pure cleanup (no receipt / no balance credit). Per-saga fail-closed: empty-reserved (migration-safe — Spent-deleted vs orphan indistinguishable), TokenCreated / RollingBack / Spent|Pending / check-state fail are refused (wedged-safer-than-double-spend). Idempotent. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "wallet_balance",
            "description": "Show ecash balance per configured mint (default testnut + opt-in extra_mints). Never echoes the secret key.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "wallet_mint",
            "description": "Flexible/repeatable mint-fund via bolt11. Two modes: (1) create — pass amount_sats to open a new quote (testnut FakeWallet auto-pays → status=funded; other mints → status=needs_payment + invoice, caller pays then completes). (2) complete — pass quote_id to finish a PAID quote from a prior create call; amount_sats optional (recovered from the local store) and used as an exact-fund guard when supplied. No already_funded hard-block. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount_sats": { "type": "integer", "minimum": 1 },
                    "quote_id": { "type": "string", "description": "Complete this paid quote instead of creating a new one" },
                    "mint": { "type": "string", "description": "Optional mint URL (must be configured)" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "wallet_send",
            "description": "Create an unlocked cashu token (ecash out). Returns the token string. Never echoes the wallet secret key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount_sats": { "type": "integer", "minimum": 1 },
                    "mint": { "type": "string" }
                },
                "required": ["amount_sats"],
                "additionalProperties": false
            }
        },
        {
            "name": "wallet_receive",
            "description": "Redeem a cashu token (ecash in). Token mint must already be configured. Response never echoes the raw token or secret key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token": { "type": "string" }
                },
                "required": ["token"],
                "additionalProperties": false
            }
        },
        {
            "name": "wallet_melt",
            "description": "Pay a lightning invoice from ecash (fail-closed on insufficient funds / unpaid). Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bolt11": { "type": "string" },
                    "mint": { "type": "string" }
                },
                "required": ["bolt11"],
                "additionalProperties": false
            }
        },
        {
            "name": "wallet_invoice",
            "description": "Create a bolt11 mint quote (invoice returned before wait). Testnut auto-pays → status=funded; extras → status=needs_payment + invoice. Flexible amount. Never echoes secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount_sats": { "type": "integer", "minimum": 1 },
                    "mint": { "type": "string" }
                },
                "required": ["amount_sats"],
                "additionalProperties": false
            }
        },
        {
            "name": "wallet_mints",
            "description": "Manage configured mints: action=list|add|remove. Default testnut stays pinned; add is opt-in. Never invents spendable credit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "add", "remove"] },
                    "mint": { "type": "string", "description": "Required for add/remove" }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }
    ])
}

async fn call_tool_async(state: &McpState, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call missing name".to_owned())?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "setup_wallet" => setup_wallet_fund_async(&state.home).await,
        "set_profile" => set_profile_tool_async(state, &arguments).await,
        "post_job" => {
            #[cfg(feature = "wallet")]
            {
                post_job_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                post_job_tool(state, &arguments)
            }
        }
        "get_job" => {
            #[cfg(feature = "wallet")]
            {
                get_job_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                get_job_tool(state, &arguments)
            }
        }
        "get_result" => {
            #[cfg(feature = "wallet")]
            {
                get_result_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                let _ = arguments;
                Err("get_result requires the wallet feature".into())
            }
        }
        "award_claim" => award_claim_tool_async(state, &arguments).await,
        "accept_claim" => {
            #[cfg(feature = "wallet")]
            {
                accept_claim_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                accept_claim_tool(state, &arguments)
            }
        }
        "stub_pay" => stub_pay(&state.gate, &arguments),
        "authorize_pay" => authorize_pay_tool_async(state, &arguments).await,
        "reconcile_wallet" => {
            #[cfg(feature = "wallet")]
            {
                reconcile_wallet_tool_async(state).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                let _ = arguments;
                Err("reconcile_wallet requires the wallet feature".into())
            }
        }
        "wallet_balance" => {
            #[cfg(feature = "wallet")]
            {
                wallet_balance_tool_async(state).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                Err("wallet_balance requires the wallet feature".into())
            }
        }
        "wallet_mint" => {
            #[cfg(feature = "wallet")]
            {
                wallet_mint_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                let _ = arguments;
                Err("wallet_mint requires the wallet feature".into())
            }
        }
        "wallet_send" => {
            #[cfg(feature = "wallet")]
            {
                wallet_send_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                let _ = arguments;
                Err("wallet_send requires the wallet feature".into())
            }
        }
        "wallet_receive" => {
            #[cfg(feature = "wallet")]
            {
                wallet_receive_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                let _ = arguments;
                Err("wallet_receive requires the wallet feature".into())
            }
        }
        "wallet_melt" => {
            #[cfg(feature = "wallet")]
            {
                wallet_melt_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                let _ = arguments;
                Err("wallet_melt requires the wallet feature".into())
            }
        }
        "wallet_invoice" => {
            #[cfg(feature = "wallet")]
            {
                wallet_invoice_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                let _ = arguments;
                Err("wallet_invoice requires the wallet feature".into())
            }
        }
        "wallet_mints" => {
            #[cfg(feature = "wallet")]
            {
                wallet_mints_tool_async(state, &arguments).await
            }
            #[cfg(not(feature = "wallet"))]
            {
                let _ = arguments;
                Err("wallet_mints requires the wallet feature".into())
            }
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Sync helper for unit tests (outermost runtime only — never call from MCP async path).
#[cfg(test)]
fn setup_wallet_fund(home: &MobeeHome) -> Result<Value, String> {
    mobee_core::runtime_guard::refuse_nested_block_on("setup_wallet_fund")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(setup_wallet_fund_async(home))
}

#[cfg(feature = "wallet")]
async fn wallet_balance_tool_async(state: &McpState) -> Result<Value, String> {
    use mobee_core::wallet_ops;
    let home = home::bootstrap(&state.home.root).map_err(|error| error.to_string())?;
    let rows = wallet_ops::balances_async(&home)
        .await
        .map_err(|error| error.to_string())?;
    let total_sats = rows.iter().fold(0u64, |acc, row| acc.saturating_add(row.balance_sats));
    let mints: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "mint_url": row.mint_url,
                "balance_sats": row.balance_sats,
                "role": if row.is_default { "default" } else { "extra" },
            })
        })
        .collect();
    Ok(tool_ok(json!({
        "mints": mints,
        "total_sats": total_sats,
    })))
}

#[cfg(feature = "wallet")]
async fn wallet_mint_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    use mobee_core::wallet_ops::{self, MintFlow};
    let mint = arguments.get("mint").and_then(Value::as_str);
    let home = home::bootstrap(&state.home.root).map_err(|error| error.to_string())?;
    // quote_id routes to completion of a prior quote — NOT a fresh mint. A bare
    // unknown arg used to be silently ignored and minted a new quote; route it
    // explicitly instead so a completion intent never opens a second quote.
    if let Some(quote_id) = arguments.get("quote_id").and_then(Value::as_str) {
        let amount = arguments.get("amount_sats").and_then(Value::as_u64);
        let outcome = wallet_ops::complete_mint_by_id_async(&home, quote_id, amount, mint)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(tool_ok(json!({
            "status": "funded",
            "mint_url": outcome.mint_url,
            "funded_sats": outcome.funded_sats,
            "balance_sats": outcome.balance_sats,
            "quote_id": outcome.quote_id,
        })));
    }
    let amount = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "wallet_mint requires amount_sats (to create) or quote_id (to complete)".to_owned())?;
    let flow = wallet_ops::mint_async(&home, amount, mint)
        .await
        .map_err(|error| error.to_string())?;
    match flow {
        MintFlow::Funded(outcome) => Ok(tool_ok(json!({
            "status": "funded",
            "mint_url": outcome.mint_url,
            "funded_sats": outcome.funded_sats,
            "balance_sats": outcome.balance_sats,
            "quote_id": outcome.quote_id,
            "invoice": outcome.invoice,
            "invoice_present": true,
        }))),
        MintFlow::NeedsPayment(quote) => Ok(tool_ok(json!({
            "status": "needs_payment",
            "mint_url": quote.mint_url,
            "amount_sats": quote.amount_sats,
            "quote_id": quote.quote_id,
            // bolt11 before poll — caller pays, then complete_mint
            "invoice": quote.invoice,
            "invoice_present": true,
        }))),
    }
}

#[cfg(feature = "wallet")]
async fn wallet_send_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    use mobee_core::wallet_ops;
    let amount = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "wallet_send requires amount_sats".to_owned())?;
    let mint = arguments.get("mint").and_then(Value::as_str);
    let home = home::bootstrap(&state.home.root).map_err(|error| error.to_string())?;
    let outcome = wallet_ops::send_async(&home, amount, mint)
        .await
        .map_err(|error| error.to_string())?;
    Ok(tool_ok(json!({
        "mint_url": outcome.mint_url,
        "sent_sats": outcome.sent_sats,
        "balance_sats": outcome.balance_sats,
        "token": outcome.token,
    })))
}

#[cfg(feature = "wallet")]
async fn wallet_receive_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    use mobee_core::wallet_ops;
    let token = arguments
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| "wallet_receive requires token".to_owned())?;
    let home = home::bootstrap(&state.home.root).map_err(|error| error.to_string())?;
    let outcome = wallet_ops::receive_async(&home, token)
        .await
        .map_err(|error| error.to_string())?;
    // Never echo the raw token back.
    Ok(tool_ok(json!({
        "mint_url": outcome.mint_url,
        "received_sats": outcome.received_sats,
        "balance_sats": outcome.balance_sats,
    })))
}

#[cfg(feature = "wallet")]
async fn wallet_melt_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    use mobee_core::wallet_ops;
    let bolt11 = arguments
        .get("bolt11")
        .and_then(Value::as_str)
        .ok_or_else(|| "wallet_melt requires bolt11".to_owned())?;
    let mint = arguments.get("mint").and_then(Value::as_str);
    let home = home::bootstrap(&state.home.root).map_err(|error| error.to_string())?;
    let outcome = wallet_ops::melt_async(&home, bolt11, mint)
        .await
        .map_err(|error| error.to_string())?;
    Ok(tool_ok(json!({
        "mint_url": outcome.mint_url,
        "paid_sats": outcome.paid_sats,
        "fee_sats": outcome.fee_sats,
        "balance_sats": outcome.balance_sats,
    })))
}

#[cfg(feature = "wallet")]
async fn wallet_invoice_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    use mobee_core::wallet_ops::{self, MintFlow};
    let amount = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "wallet_invoice requires amount_sats".to_owned())?;
    let mint = arguments.get("mint").and_then(Value::as_str);
    let home = home::bootstrap(&state.home.root).map_err(|error| error.to_string())?;
    let flow = wallet_ops::invoice_async(&home, amount, mint)
        .await
        .map_err(|error| error.to_string())?;
    match flow {
        MintFlow::Funded(outcome) => Ok(tool_ok(json!({
            "status": "funded",
            "mint_url": outcome.mint_url,
            "amount_sats": amount,
            "funded_sats": outcome.funded_sats,
            "balance_sats": outcome.balance_sats,
            "quote_id": outcome.quote_id,
            "invoice": outcome.invoice,
        }))),
        MintFlow::NeedsPayment(quote) => Ok(tool_ok(json!({
            "status": "needs_payment",
            "mint_url": quote.mint_url,
            "amount_sats": quote.amount_sats,
            "quote_id": quote.quote_id,
            "invoice": quote.invoice,
        }))),
    }
}

#[cfg(feature = "wallet")]
async fn wallet_mints_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    use mobee_core::wallet_ops;
    let action = arguments
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "wallet_mints requires action".to_owned())?;
    let mut home = home::bootstrap(&state.home.root).map_err(|error| error.to_string())?;
    match action {
        "list" => {
            let rows = wallet_ops::list_mints(&home).map_err(|error| error.to_string())?;
            let mints: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "mint_url": row.mint_url,
                        "role": if row.is_default { "default" } else { "extra" },
                    })
                })
                .collect();
            Ok(tool_ok(json!({ "mints": mints })))
        }
        "add" => {
            let mint = arguments
                .get("mint")
                .and_then(Value::as_str)
                .ok_or_else(|| "wallet_mints add requires mint".to_owned())?;
            let normalized = wallet_ops::add_mint(&mut home, mint).map_err(|error| error.to_string())?;
            Ok(tool_ok(json!({ "added": normalized })))
        }
        "remove" => {
            let mint = arguments
                .get("mint")
                .and_then(Value::as_str)
                .ok_or_else(|| "wallet_mints remove requires mint".to_owned())?;
            wallet_ops::remove_mint(&mut home, mint).map_err(|error| error.to_string())?;
            Ok(tool_ok(json!({ "removed": mint })))
        }
        other => Err(format!("unknown wallet_mints action: {other}")),
    }
}

/// First-run setup fund amount (sats). Kept local to the `mobee` bin — mobee-core's
/// mint-fund path takes an explicit amount, so the default lives with the caller.
#[cfg(feature = "wallet")]
const SETUP_WALLET_FUND_SATS: u64 = 21;

async fn setup_wallet_fund_async(home: &MobeeHome) -> Result<Value, String> {
    // Re-bootstrap so a long-lived MCP process still converges if files were removed.
    let home = home::bootstrap(&home.root).map_err(|error| error.to_string())?;
    #[cfg(feature = "wallet")]
    {
        use mobee_core::wallet_ops::{self, MintFlow};
        // Mint-fund via the invoice-up-front path: testnut FakeWallet auto-pays (immediate
        // Funded), any other configured mint returns the bolt11 to pay before completion.
        let flow = wallet_ops::mint_async(&home, SETUP_WALLET_FUND_SATS, None)
            .await
            .map_err(|error| error.to_string())?;
        let home_path = home.root.display().to_string();
        let wallet_dir = home.wallet_dir.display().to_string();
        let key_present = home::key_file_present(&home);
        let body = match flow {
            MintFlow::Funded(outcome) => json!({
                "home": home_path,
                "key_created": home.key_created,
                "key_present": key_present,
                "wallet_dir": wallet_dir,
                "relay_url": home.config.relay_url,
                "per_job_budget_sats": home.config.per_job_budget_sats,
                "total_budget_sats": home.config.total_budget_sats,
                "status": "funded",
                "mint_url": outcome.mint_url,
                "invoice": outcome.invoice,
                "funded_sats": outcome.funded_sats,
                "balance_sats": outcome.balance_sats,
                "next": "wallet funded on testnut — use stub_pay to exercise budget caps",
            }),
            MintFlow::NeedsPayment(quote) => json!({
                "home": home_path,
                "key_created": home.key_created,
                "key_present": key_present,
                "wallet_dir": wallet_dir,
                "relay_url": home.config.relay_url,
                "per_job_budget_sats": home.config.per_job_budget_sats,
                "total_budget_sats": home.config.total_budget_sats,
                "status": "needs_payment",
                "mint_url": quote.mint_url,
                "invoice": quote.invoice,
                "quote_id": quote.quote_id,
                "amount_sats": quote.amount_sats,
                "next": "pay the bolt11 invoice, then wallet_mint(quote_id) to complete the fund",
            }),
        };
        Ok(tool_ok(body))
    }
    #[cfg(not(feature = "wallet"))]
    {
        let _ = home;
        Err("setup_wallet fund path requires the wallet feature".into())
    }
}

#[cfg(all(test, feature = "wallet"))]
fn set_profile_tool(state: &McpState, arguments: &Value) -> Result<Value, String> {
    mobee_core::runtime_guard::refuse_nested_block_on("set_profile_tool")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(set_profile_tool_async(state, arguments))
}

#[cfg(feature = "wallet")]
async fn set_profile_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    use mobee_core::profile::{self, SetProfileRequest};

    let name = arguments
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let about = arguments
        .get("about")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // Reload from disk so long-lived MCP sees the latest config.toml.
    let mut home = home::bootstrap(&state.home.root).map_err(|error| error.to_string())?;
    let outcome = profile::set_profile_async(
        &mut home,
        SetProfileRequest { name, about },
    )
    .await
    .map_err(|error| error.to_string())?;
    let body = json!({
        "ok": outcome.ok,
        "pubkey": outcome.pubkey,
        "name": outcome.name,
        "about": outcome.about,
        "event_id": outcome.event_id,
        "relay_url": outcome.relay_url,
    });
    let rendered = body.to_string();
    if let Ok(secret) = home::read_secret_key_hex(&home) {
        if rendered.contains(&secret) {
            return Err("set_profile refused: response would echo secret key".into());
        }
    }
    Ok(tool_ok(body))
}

#[cfg(not(feature = "wallet"))]
async fn set_profile_tool_async(_state: &McpState, _arguments: &Value) -> Result<Value, String> {
    Err("set_profile requires the wallet feature".into())
}

#[cfg(not(feature = "wallet"))]
fn set_profile_tool(_state: &McpState, _arguments: &Value) -> Result<Value, String> {
    Err("set_profile requires the wallet feature".into())
}

/// Real pay: BudgetGate → PaymentService::run(&mut PayPathDeliveryVerifier) only.
/// Tool args that set per_job/total caps are ignored. Never echoes the secret key.
///
/// Forms:
/// - **job_id (documented default):** job_id + amount_sats + delivery_integrity_hash
///   (buyer tip-match). Other fields filled from accept_claim bind. D2: integrity hash
///   is never auto-filled from the claim/result oid.
/// - **explicit:** all nine fields (harness / stub path). If an accept-bind exists for
///   job_id, seller/result/commit must match (Gate D).
async fn authorize_pay_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let require_str = |key: &str| -> Result<String, String> {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("authorize_pay requires {key} (string)"))
    };
    let amount_sats = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "authorize_pay requires amount_sats (integer)".to_owned())?;
    let delivery_integrity_hash = require_str("delivery_integrity_hash")?;
    let job_id = require_str("job_id")?;

    let explicit = [
        "result_id",
        "job_hash",
        "seller_pubkey",
        "repo",
        "branch",
        "commit_oid",
    ]
    .iter()
    .all(|key| arguments.get(key).and_then(Value::as_str).is_some());

    // Optional explicit co-signature (piece-9). The bind form carries it automatically; the
    // explicit form may pass it, else it is filled from the accept-bind when one exists.
    let seller_signature_arg = arguments
        .get("seller_signature")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();

    #[cfg(feature = "wallet")]
    let request = if explicit {
        let mut request = AuthorizePayRequest {
            job_id: job_id.clone(),
            result_id: require_str("result_id")?,
            // FromScratch by default; overwritten from the accept-bind by
            // fill_explicit_request_from_bind, and re-derived + fail-closed for a no-bind
            // contribution offer by the GAP-1 guard below.
            job_class: authorize_pay::JobClass::FromScratch,
            delivery_integrity_hash,
            job_hash: require_str("job_hash")?,
            seller_pubkey: require_str("seller_pubkey")?,
            amount_sats,
            repo: require_str("repo")?,
            branch: require_str("branch")?,
            commit_oid: require_str("commit_oid")?,
            seller_signature: seller_signature_arg.clone(),
            // Piece-14: filled from the accept-bind below when one exists (like seller_signature).
            creq_hash: None,
            accepted_mints: Vec::new(),
            // Filled from the sealed accept-bind when one exists; None falls back to the live
            // config default (legacy) in authorize_pay.
            realized_mint: None,
            contribution: None,
        };
        let accept_bind = job_lifecycle::load_accepted_bind(&state.home, &job_id)
            .map_err(|error| error.to_string())?;
        if let Some(bind) = &accept_bind {
            job_lifecycle::assert_authorize_matches_bind(
                bind,
                &request.seller_pubkey,
                &request.result_id,
                &request.commit_oid,
            )
            .map_err(|error| error.to_string())?;
            // Issue #53: fill the preimage-critical + auth fields from the bind so the explicit
            // form builds the SAME co-signed receipt preimage as the accept-first path (job_hash is
            // authoritative from the bind; seller_signature/creq_hash/accepted_mints/contribution
            // fill when the caller left them empty).
            job_lifecycle::fill_explicit_request_from_bind(&mut request, bind);
        }
        // GAP-1 money-gate: with NO accept-bind, the paid offer's job-class was never resolved
        // locally, so an explicit-form pay for a `job-class=contribution` offer would reach
        // authorize_pay with `contribution: None` and SKIP all four contribution gates + the
        // authorship-tuple seam (a non-descendant / empty / out-of-scope commit would be payable).
        // Re-derive the offer class from the relay and REFUSE fail-closed for a contribution offer
        // (or when the class cannot be determined). An accept-bind already resolved the class at
        // accept time (contribution binds threaded above for a contribution offer, or proven
        // from-scratch), so the accept-first path is untouched — no fetch, no new refusal.
        if accept_bind.is_none() {
            let job_class = derive_offer_job_class(&state.home, &job_id).await?;
            guard_explicit_contribution_pay(&job_id, job_class.as_deref())?;
        }
        request
    } else {
        let bind = job_lifecycle::load_accepted_bind(&state.home, &job_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                "authorize_pay(job_id) requires a prior accept_claim bind for this job (or pass the explicit 9-field form)".to_owned()
            })?;
        job_lifecycle::authorize_request_from_bind(&bind, amount_sats, delivery_integrity_hash)
            .map_err(|error| error.to_string())?
    };

    #[cfg(not(feature = "wallet"))]
    let request = {
        let _ = explicit;
        AuthorizePayRequest {
            job_id,
            result_id: require_str("result_id")?,
            job_class: authorize_pay::JobClass::FromScratch,
            delivery_integrity_hash,
            job_hash: require_str("job_hash")?,
            seller_pubkey: require_str("seller_pubkey")?,
            amount_sats,
            repo: require_str("repo")?,
            branch: require_str("branch")?,
            commit_oid: require_str("commit_oid")?,
            seller_signature: seller_signature_arg.clone(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        }
    };

    // #98: the paid commit + where the delivery is retained on disk, so the buyer can locate the
    // result (materialize its files with get_result). The verify path retains the delivered commit
    // in the buyer store under refs/mobee/deliveries/<oid>; capture the locator before `request`
    // moves into the pay call.
    let paid_commit = request.commit_oid.clone();
    let store_path = delivery_store_path(&state.home).display().to_string();
    let store_ref = mobee_core::delivery_git::PayPathDeliveryVerifier::store_ref_for(&paid_commit);

    let mut gate = state
        .gate
        .lock()
        .map_err(|_| "budget gate lock poisoned".to_owned())?;

    let outcome = authorize_pay::authorize_pay_async(&state.home, &mut gate, request)
        .await
        .map_err(|error| error.to_string())?;

    let body = json!({
        "ok": true,
        "amount_sats": outcome.amount_sats,
        "attempt_id": outcome.attempt_id,
        "spent_total_sats": outcome.spent_total_sats,
        "remaining_sats": outcome.remaining_sats,
        "per_job_cap_sats": gate.per_job_cap(),
        "total_cap_sats": gate.total_cap(),
        "state": outcome.state,
        // Delivered-result pointer: the buyer store repo + the ref/commit the paid delivery is
        // retained under. Use get_result(job_id) to materialize the files.
        "commit_oid": paid_commit,
        "store_path": store_path,
        "store_ref": store_ref,
    });
    Ok(tool_ok(body))
}

/// The buyer store: the local bare repository the pay path retains verified delivery objects in.
/// Mirrors the path `authorize_pay` opens the [`PayPathDeliveryVerifier`] against.
fn delivery_store_path(home: &MobeeHome) -> std::path::PathBuf {
    home.root.join("store")
}

/// #98: materialize a PAID delivery's files. The accepted job's delivered commit is retained in the
/// buyer store (during authorize_pay); this checks it out READ-ONLY into a folder under
/// `<home>/results` so the buyer can find the result on disk. No network, no auth, no spend gate.
#[cfg(feature = "wallet")]
async fn get_result_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let job_id = arguments
        .get("job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "get_result requires job_id".to_owned())?
        .to_owned();

    // The delivered commit comes from the local accept-bind (recorded by accept_claim) — never
    // caller input, so get_result always points at the accepted, paid-for delivery.
    let bind = job_lifecycle::load_accepted_bind(&state.home, &job_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!("get_result: no accept-bind for job {job_id} (run accept_claim first)")
        })?;
    let commit_oid = bind.commit_oid.clone();

    // Output folder: a simple name under <home>/results (default = the job id). Reject path
    // separators / traversal so a caller can never write outside the results directory.
    let dest = match arguments.get("out").and_then(Value::as_str) {
        Some(name) => {
            if name.is_empty()
                || name.contains('/')
                || name.contains('\\')
                || name.contains("..")
            {
                return Err(
                    "get_result 'out' must be a simple folder name (no path separators or '..')"
                        .into(),
                );
            }
            state.home.root.join("results").join(name)
        }
        None => state.home.root.join("results").join(&job_id),
    };

    let store = delivery_store_path(&state.home);
    let store_ref = mobee_core::delivery_git::PayPathDeliveryVerifier::store_ref_for(&commit_oid);
    let files = materialize_delivery(&store, &store_ref, &commit_oid, &dest)?;

    Ok(tool_ok(json!({
        "job_id": job_id,
        "commit": commit_oid,
        "path": dest.display().to_string(),
        "files": files,
    })))
}

/// Check out the tree of `commit_oid` from the buyer store (bare repo) into `dest`, writing each
/// blob to its path and returning the sorted relative file list. Fail-closed: any read/write error
/// aborts (never a partial-but-reported materialization). Resolves the retention ref first, then the
/// raw oid as a fallback.
#[cfg(feature = "wallet")]
fn materialize_delivery(
    store: &std::path::Path,
    store_ref: &str,
    commit_oid: &str,
    dest: &std::path::Path,
) -> Result<Vec<String>, String> {
    let repo = git2::Repository::open_bare(store)
        .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
    let commit = repo
        .revparse_single(store_ref)
        .or_else(|_| repo.revparse_single(commit_oid))
        .map_err(|error| {
            format!("delivery {commit_oid} not found in buyer store (ref {store_ref}): {error}")
        })?
        .peel_to_commit()
        .map_err(|error| error.to_string())?;
    let tree = commit.tree().map_err(|error| error.to_string())?;

    std::fs::create_dir_all(dest).map_err(|error| error.to_string())?;
    let mut files: Vec<String> = Vec::new();
    let mut walk_err: Option<String> = None;
    tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return git2::TreeWalkResult::Ok;
        }
        let name = match entry.name() {
            Some(name) => name,
            None => {
                walk_err = Some("non-UTF-8 tree entry name".into());
                return git2::TreeWalkResult::Abort;
            }
        };
        let rel = format!("{root}{name}");
        let object = match entry.to_object(&repo) {
            Ok(object) => object,
            Err(error) => {
                walk_err = Some(error.to_string());
                return git2::TreeWalkResult::Abort;
            }
        };
        let Some(blob) = object.as_blob() else {
            return git2::TreeWalkResult::Ok;
        };
        let path = dest.join(&rel);
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                walk_err = Some(error.to_string());
                return git2::TreeWalkResult::Abort;
            }
        }
        if let Err(error) = std::fs::write(&path, blob.content()) {
            walk_err = Some(error.to_string());
            return git2::TreeWalkResult::Abort;
        }
        files.push(rel);
        git2::TreeWalkResult::Ok
    })
    .map_err(|error| error.to_string())?;
    if let Some(error) = walk_err {
        return Err(format!("get_result materialize failed: {error}"));
    }
    files.sort();
    Ok(files)
}

/// GAP-1: re-derive a paid job's OFFER `job-class` from the relay, reusing the job-view machinery
/// (no bespoke fetch). `Ok(Some("contribution"))` ⇒ a contribution offer; `Ok(Some(other))` /
/// `Ok(None)` ⇒ a non-contribution (from-scratch) offer was found. `Err(..)` ⇒ the class could
/// NOT be determined (relay read failed, or no offer on the relay) — the caller MUST fail closed
/// (never treat an undeterminable class as from-scratch).
#[cfg(feature = "wallet")]
async fn derive_offer_job_class(home: &MobeeHome, job_id: &str) -> Result<Option<String>, String> {
    let view = job_lifecycle::get_job_async(
        home,
        GetJobRequest {
            job_id: job_id.to_owned(),
            wait_for: None,
            timeout_secs: None,
        },
    )
    .await
    .map_err(|error| {
        format!(
            "authorize_pay refused: cannot determine job-class for job {job_id} — relay read failed \
             ({error}); refusing contribution-less explicit pay fail-closed (accept_claim first, or \
             retry when the relay is reachable)"
        )
    })?;
    let offer = view.offer.ok_or_else(|| {
        format!(
            "authorize_pay refused: cannot determine job-class for job {job_id} — no offer found on \
             the relay; refusing contribution-less explicit pay fail-closed (accept_claim first)"
        )
    })?;
    Ok(offer.job_class)
}

/// GAP-1 pure decision: an explicit-form pay carrying NO contribution binds must be refused when
/// its offer is `job-class=contribution` — otherwise authorize_pay skips the four contribution
/// gates + the authorship-tuple seam and a non-descendant / empty / out-of-scope commit is payable.
/// Non-contribution classes (incl. `None` from-scratch) proceed unchanged.
#[cfg(feature = "wallet")]
fn guard_explicit_contribution_pay(job_id: &str, job_class: Option<&str>) -> Result<(), String> {
    if job_class == Some(mobee_core::contribution::JOB_CLASS_CONTRIBUTION) {
        return Err(format!(
            "authorize_pay refused: job {job_id} is job-class=contribution but the explicit pay form \
             carries no contribution binds — accept_claim this job first so the contribution gates \
             (base-from-pin, descendant, content-policy, authorship-tuple) run before any spend"
        ));
    }
    Ok(())
}

#[cfg(feature = "wallet")]
async fn reconcile_wallet_tool_async(state: &McpState) -> Result<Value, String> {
    use mobee_core::buyer_fund;
    use mobee_core::payment_wallet;

    let wallet = buyer_fund::open_wallet_async(&state.home)
        .await
        .map_err(|error| error.to_string())?;
    let report = payment_wallet::retire_eligible_incomplete_sagas(&wallet)
        .await
        .map_err(|error| error.to_string())?;
    let body = json!({
        "ok": true,
        "retired": report.retired,
        "mapped_token_created": report.mapped_token_created,
        "swap_rolled_back": report.swap_rolled_back,
        "swap_recovered": report.swap_recovered,
    });
    let rendered = body.to_string();
    let secret = home::read_secret_key_hex(&state.home).unwrap_or_default();
    if !secret.is_empty() && rendered.contains(&secret) {
        return Err("reconcile_wallet refused: response would echo secret key".into());
    }
    Ok(tool_ok(body))
}

#[cfg(feature = "wallet")]
async fn post_job_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let require_str = |key: &str| -> Result<String, String> {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("post_job requires {key} (string)"))
    };
    let amount_sats = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "post_job requires amount_sats (integer)".to_owned())?;
    let untargeted = arguments
        .get("untargeted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let seller_pubkey = arguments
        .get("seller_pubkey")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let deadline_unix = match arguments.get("deadline_unix") {
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| "post_job requires deadline_unix (integer >= 0)".to_owned())?,
        ),
        None => None,
    };
    let repo = arguments
        .get("repo")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let branch = arguments
        .get("branch")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let opt_str = |key: &str| -> Option<String> {
        arguments.get(key).and_then(Value::as_str).map(str::to_owned)
    };
    let accepts = arguments.get("accepts").and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect::<Vec<String>>()
    });

    // The four target/base pins are ALL-OR-NOTHING: all four ⇒ a contribution offer; none ⇒
    // from-scratch; a partial set is refused so the core never sees a half-specified contribution.
    let job = match (
        opt_str("target_repo_owner"),
        opt_str("target_repo_url"),
        opt_str("base_branch"),
        opt_str("base_oid"),
    ) {
        (None, None, None, None) => JobKind::FromScratch,
        (Some(target_repo_owner), Some(target_repo_url), Some(base_branch), Some(base_oid)) => {
            JobKind::Contribution(ContributionSpec {
                target_repo_owner,
                target_repo_url,
                base_branch,
                base_oid,
                accepts,
            })
        }
        _ => {
            return Err(
                "post_job contribution mode requires ALL of target_repo_owner, target_repo_url, \
                 base_branch, base_oid (a partial set is refused)"
                    .into(),
            )
        }
    };

    let outcome = job_lifecycle::post_job_async(
        &state.home,
        PostJobRequest {
            task: require_str("task")?,
            output: require_str("output")?,
            amount_sats,
            seller_pubkey,
            untargeted,
            deadline_unix,
            repo,
            branch,
            job,
        },
    )
    .await
    .map_err(|error| error.to_string())?;

    let body = json!({
        "ok": true,
        "job_id": outcome.job_id,
        "job_hash": outcome.job_hash,
        "offer_kind": outcome.offer_kind,
        "targeted": outcome.targeted,
        "seller_pubkey": outcome.seller_pubkey,
        "amount_sats": outcome.amount_sats,
        "relay_url": outcome.relay_url,
        "task": outcome.task,
        "output": outcome.output,
    });
    // never-echo: secret key must not appear
    let rendered = body.to_string();
    if let Ok(secret) = home::read_secret_key_hex(&state.home) {
        if rendered.contains(&secret) {
            return Err("post_job refused: response would echo secret key".into());
        }
    }
    Ok(tool_ok(body))
}

#[cfg(not(feature = "wallet"))]
fn post_job_tool(_state: &McpState, _arguments: &Value) -> Result<Value, String> {
    Err("post_job requires the wallet feature".into())
}

#[cfg(feature = "wallet")]
async fn get_job_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let job_id = arguments
        .get("job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "get_job requires job_id (string)".to_owned())?
        .to_owned();
    let wait_for = match arguments.get("wait_for").and_then(Value::as_str) {
        Some(raw) => Some(WaitFor::parse(raw).map_err(|error| error.to_string())?),
        None => None,
    };
    let timeout_secs = arguments.get("timeout_secs").and_then(Value::as_u64);
    let view = job_lifecycle::get_job_async(
        &state.home,
        GetJobRequest {
            job_id,
            wait_for,
            timeout_secs,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let body = serde_json::to_value(&view).map_err(|error| error.to_string())?;
    let mut body = body;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("ok".into(), json!(true));
        obj.insert("source".into(), json!("relay"));
        if view.pending {
            obj.insert("status".into(), json!("pending"));
        }
    }
    Ok(tool_ok(body))
}

#[cfg(not(feature = "wallet"))]
fn get_job_tool(_state: &McpState, _arguments: &Value) -> Result<Value, String> {
    Err("get_job requires the wallet feature".into())
}

#[cfg(feature = "wallet")]
async fn award_claim_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let require_str = |key: &str| -> Result<String, String> {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("award_claim requires {key} (string)"))
    };
    let outcome = job_lifecycle::award_claim_async(
        &state.home,
        AwardClaimRequest {
            job_id: require_str("job_id")?,
            claim_id: require_str("claim_id")?,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(tool_ok(json!({
        "ok": true,
        "award_event_id": outcome.award_event_id,
        "job_id": outcome.job_id,
        "claim_id": outcome.claim_id,
        "seller_pubkey": outcome.seller_pubkey,
        "quoted_mints": outcome.quoted_mints,
    })))
}

async fn accept_claim_tool_async(state: &McpState, arguments: &Value) -> Result<Value, String> {
    let require_str = |key: &str| -> Result<String, String> {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("accept_claim requires {key} (string)"))
    };
    let result_id = arguments
        .get("result_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let outcome = job_lifecycle::accept_claim_async(
        &state.home,
        AcceptClaimRequest {
            job_id: require_str("job_id")?,
            claim_id: require_str("claim_id")?,
            result_id,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let body = json!({
        "ok": true,
        "accept_event_id": outcome.accept_event_id,
        "bind": outcome.bind,
    });
    let rendered = body.to_string();
    if let Ok(secret) = home::read_secret_key_hex(&state.home) {
        if rendered.contains(&secret) {
            return Err("accept_claim refused: response would echo secret key".into());
        }
    }
    Ok(tool_ok(body))
}

#[cfg(not(feature = "wallet"))]
fn accept_claim_tool(_state: &McpState, _arguments: &Value) -> Result<Value, String> {
    Err("accept_claim requires the wallet feature".into())
}

/// Gate-wrapped mock pay. Caps from the in-process gate (config-bound at MCP start).
/// Tool args `per_job` / `total` / `per_job_budget_sats` / `total_budget_sats` are ignored.
fn stub_pay(gate: &Mutex<BudgetGate>, arguments: &Value) -> Result<Value, String> {
    let amount = arguments
        .get("amount_sats")
        .and_then(Value::as_u64)
        .ok_or_else(|| "stub_pay requires amount_sats (integer)".to_owned())?;

    let mut guard = gate
        .lock()
        .map_err(|_| "budget gate lock poisoned".to_owned())?;

    // Capture attempted overrides for the response audit trail (values are NOT applied).
    let ignored_cap_overrides = json!({
        "per_job": arguments.get("per_job"),
        "total": arguments.get("total"),
        "per_job_budget_sats": arguments.get("per_job_budget_sats"),
        "total_budget_sats": arguments.get("total_budget_sats"),
    });

    let mut effect_fired = false;
    match guard.authorize_then(amount, || {
        effect_fired = true;
        "stub_ok"
    }) {
        Ok(stub) => {
            debug_assert!(effect_fired);
            let body = json!({
                "ok": true,
                "stub": stub,
                "amount_sats": amount,
                "spent_total_sats": guard.spent(),
                "remaining_sats": guard.remaining(),
                "per_job_cap_sats": guard.per_job_cap(),
                "total_cap_sats": guard.total_cap(),
                "ignored_cap_overrides": ignored_cap_overrides,
                "piece6": "not_called",
            });
            Ok(tool_ok(body))
        }
        Err(refuse) => {
            debug_assert!(!effect_fired);
            Err(refuse.to_string())
        }
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

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-mcp-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn state_at(root: &std::path::Path) -> McpState {
        let home = home::bootstrap(root).expect("bootstrap");
        let gate = Mutex::new(BudgetGate::from_home(&home).expect("gate"));
        McpState { home, gate }
    }

    // #98: get_result materializes a PAID delivery's files to <home>/results/<job_id> and returns
    // {path, commit, files}. Builds a delivered-job fixture — a buyer store bare repo holding the
    // commit under its retention ref + an accept-bind pinning that commit — then proves get_result
    // writes the exact tree to the exact on-disk location.
    #[cfg(feature = "wallet")]
    #[tokio::test(flavor = "current_thread")]
    async fn get_result_materializes_delivered_job_to_disk() {
        let root = temp_home("get-result");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);

        // Buyer store (bare) with a delivered commit: README.md + src/lib.rs.
        let store = root.join("store");
        let repo = git2::Repository::init_bare(&store).expect("init store");
        let readme = repo.blob(b"# delivered\n").expect("blob readme");
        let lib = repo.blob(b"pub fn delivered() {}\n").expect("blob lib");
        let mut sub = repo.treebuilder(None).expect("subtree");
        sub.insert("lib.rs", lib, 0o100644).expect("insert lib");
        let sub_oid = sub.write().expect("write subtree");
        let mut top = repo.treebuilder(None).expect("tree");
        top.insert("README.md", readme, 0o100644).expect("insert readme");
        top.insert("src", sub_oid, 0o040000).expect("insert src");
        let tree_oid = top.write().expect("write tree");
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo
            .commit(None, &sig, &sig, "delivery", &tree, &[])
            .expect("commit");
        let commit_hex = commit_oid.to_string();
        repo.reference(
            &mobee_core::delivery_git::PayPathDeliveryVerifier::store_ref_for(&commit_hex),
            commit_oid,
            true,
            "retain",
        )
        .expect("retain ref");

        // Accept-bind pinning the delivered commit (what accept_claim would have written).
        let job_id = "a".repeat(64);
        let bind = mobee_core::job_lifecycle::AcceptedBind {
            job_id: job_id.clone(),
            claim_id: "b".repeat(64),
            result_id: "c".repeat(64),
            seller_pubkey: "d".repeat(64),
            commit_oid: commit_hex.clone(),
            repo: "https://example.invalid/repo.git".into(),
            branch: "master".into(),
            job_hash: "e".repeat(64),
            amount_sats: 7,
            accept_event_id: "f".repeat(64),
            accepted_at: 1,
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let jobs_dir = root.join("jobs");
        std::fs::create_dir_all(&jobs_dir).expect("jobs dir");
        std::fs::write(
            jobs_dir.join(format!("{job_id}.json")),
            serde_json::to_string(&bind).expect("serialize bind"),
        )
        .expect("write bind");

        let result = get_result_tool_async(&state, &json!({ "job_id": job_id }))
            .await
            .expect("get_result");
        let payload = &result["structuredContent"];

        let expected_path = root.join("results").join(&job_id);
        assert_eq!(payload["commit"], commit_hex, "returns the delivered commit");
        assert_eq!(
            payload["path"],
            expected_path.display().to_string(),
            "materializes under <home>/results/<job_id>"
        );
        assert_eq!(
            payload["files"],
            json!(["README.md", "src/lib.rs"]),
            "returns the sorted delivered file list"
        );
        // Files actually on disk with the delivered content.
        assert_eq!(
            std::fs::read_to_string(expected_path.join("README.md")).expect("readme on disk"),
            "# delivered\n"
        );
        assert_eq!(
            std::fs::read_to_string(expected_path.join("src/lib.rs")).expect("lib on disk"),
            "pub fn delivered() {}\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

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

    #[test]
    fn tools_list_exposes_job_lifecycle_and_pay_tools() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .expect("array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            vec![
                "setup_wallet",
                "set_profile",
                "post_job",
                "get_job",
                "get_result",
                "accept_claim",
                "stub_pay",
                "authorize_pay",
                "reconcile_wallet",
                "wallet_balance",
                "wallet_mint",
                "wallet_send",
                "wallet_receive",
                "wallet_melt",
                "wallet_invoice",
                "wallet_mints",
            ]
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn setup_wallet_funds_testnut_and_never_emits_secret_key() {
        let root = temp_home("setup-fund");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let result = setup_wallet_fund(&state.home).expect("fund");
        let rendered = result.to_string();
        assert!(!rendered.contains(&secret), "secret leaked in setup_wallet");
        assert_eq!(result["isError"], false);
        assert_eq!(
            result["structuredContent"]["mint_url"],
            home::DEFAULT_MINT_URL
        );
        let balance = result["structuredContent"]["balance_sats"]
            .as_u64()
            .expect("balance");
        assert!(balance > 0, "balance={balance}");
        assert!(
            result["structuredContent"]["invoice"].is_string()
                || result["structuredContent"]["already_funded"] == true
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn set_profile_publishes_kind0_writes_config_never_echoes_secret() {
        let root = temp_home("set-profile");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let result = set_profile_tool(
            &state,
            &json!({ "name": "anvil-kind0", "about": "testnut only" }),
        )
        .expect("set_profile");
        let rendered = result.to_string();
        assert!(!rendered.contains(&secret), "secret leaked in set_profile");
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["ok"], true);
        assert_eq!(result["structuredContent"]["name"], "anvil-kind0");
        assert_eq!(result["structuredContent"]["about"], "testnut only");
        let event_id = result["structuredContent"]["event_id"]
            .as_str()
            .expect("event_id");
        assert_eq!(event_id.len(), 64);

        let reloaded = home::bootstrap(&root).expect("reload");
        let profile = reloaded.config.profile.expect("profile in config");
        assert_eq!(profile.name.as_deref(), Some("anvil-kind0"));
        assert_eq!(profile.about.as_deref(), Some("testnut only"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Live MCP smoke via async `tools/call` dispatch (same path as Claude-Code MCP).
    /// Closes Temper MEDIUM: set_profile was suite-attested on the sync twin only.
    #[cfg(feature = "wallet")]
    #[test]
    fn set_profile_async_mcp_dispatch_publishes_kind0_never_echoes_secret() {
        let root = temp_home("set-profile-async-mcp");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(77)),
                method: "tools/call".into(),
                params: json!({
                    "name": "set_profile",
                    "arguments": {
                        "name": "anvil-async-mcp",
                        "about": "gate9 fast-follow"
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(
            !rendered.contains(&secret),
            "secret leaked on async MCP set_profile"
        );
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(response["result"]["structuredContent"]["ok"], true);
        assert_eq!(
            response["result"]["structuredContent"]["name"],
            "anvil-async-mcp"
        );
        let event_id = response["result"]["structuredContent"]["event_id"]
            .as_str()
            .expect("event_id");
        assert_eq!(event_id.len(), 64);
        let reloaded = home::bootstrap(&root).expect("reload");
        let profile = reloaded.config.profile.expect("profile in config");
        assert_eq!(profile.name.as_deref(), Some("anvil-async-mcp"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `quote_id` argument must route to quote-completion, NOT silently mint a
    /// fresh quote (the field-failure bug shape). With an unknown quote and no
    /// amount, completion refuses with a "pass --amount" message — a create call
    /// would instead demand `amount_sats`, so the message discriminates the path.
    #[cfg(feature = "wallet")]
    #[test]
    fn wallet_mint_quote_id_routes_to_completion_not_fresh_mint() {
        let root = temp_home("wallet-mint-quoteid");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(91)),
                method: "tools/call".into(),
                params: json!({
                    "name": "wallet_mint",
                    "arguments": { "quote_id": "quote-not-in-store" }
                }),
            },
        );
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("error text");
        assert!(text.contains("pass --amount"), "unexpected error: {text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn wallet_mint_without_amount_or_quote_id_refused() {
        let root = temp_home("wallet-mint-neither");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(92)),
                method: "tools/call".into(),
                params: json!({ "name": "wallet_mint", "arguments": {} }),
            },
        );
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("error text");
        assert!(text.contains("amount_sats") && text.contains("quote_id"), "unexpected error: {text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn set_profile_empty_name_refused_never_echoes_secret() {
        let root = temp_home("set-profile-empty");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let err = set_profile_tool(&state, &json!({ "name": "   " })).expect_err("refuse");
        assert!(err.contains("name"), "got: {err}");
        assert!(!err.contains(&secret));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stub_pay_within_caps_passes() {
        let root = temp_home("stub-ok");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let result = stub_pay(&state.gate, &json!({ "amount_sats": 1 })).expect("allow");
        let rendered = result.to_string();
        assert!(!rendered.contains(&secret));
        assert_eq!(result["structuredContent"]["ok"], true);
        assert_eq!(result["structuredContent"]["piece6"], "not_called");
    }

    #[test]
    fn stub_pay_exceed_per_job_refused_as_error() {
        let root = temp_home("stub-job");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let per_job = state.home.config.per_job_budget_sats;
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let err = stub_pay(&state.gate, &json!({ "amount_sats": per_job + 1 })).expect_err("refuse");
        assert!(err.contains("per-job"));
        assert!(!err.contains(&secret));
        assert_eq!(state.gate.lock().expect("lock").spent(), 0);
    }

    #[test]
    fn stub_pay_exceed_remaining_total_refused_as_error() {
        let root = temp_home("stub-total");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let per_job = state.home.config.per_job_budget_sats;
        let total = state.home.config.total_budget_sats;
        // Drain close to total with per-job-sized chunks.
        let mut remaining = total;
        while remaining > 0 {
            let chunk = per_job.min(remaining);
            if chunk == 0 {
                break;
            }
            // If next full per_job would exceed, use remaining if ≤ per_job.
            stub_pay(&state.gate, &json!({ "amount_sats": chunk })).expect("drain");
            remaining = remaining.saturating_sub(chunk);
        }
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let err = stub_pay(&state.gate, &json!({ "amount_sats": 1 })).expect_err("total");
        assert!(err.contains("remaining total"));
        assert!(!err.contains(&secret));
    }

    #[test]
    fn stub_pay_ignores_cap_mutation_via_tool_args() {
        let root = temp_home("stub-mutate");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let per_job = state.home.config.per_job_budget_sats;
        // Attempt to raise caps via tool args — must still refuse per_job+1.
        let err = stub_pay(
            &state.gate,
            &json!({
                "amount_sats": per_job + 1,
                "per_job": 1_000_000,
                "total": 1_000_000,
                "per_job_budget_sats": 1_000_000,
                "total_budget_sats": 1_000_000,
            }),
        )
        .expect_err("mutation ignored");
        assert!(err.contains("per-job"));
        assert_eq!(
            state.gate.lock().expect("lock").per_job_cap(),
            per_job
        );
    }

    #[test]
    fn stub_pay_boundary_exact_per_job_passes() {
        let root = temp_home("stub-boundary");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let per_job = state.home.config.per_job_budget_sats;
        stub_pay(&state.gate, &json!({ "amount_sats": per_job })).expect("exact");
        stub_pay(&state.gate, &json!({ "amount_sats": per_job + 1 })).expect_err("plus one");
    }

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
                    "arguments": { "amount_sats": state.home.config.per_job_budget_sats + 1 }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret));
        assert_eq!(response["result"]["isError"], true);
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn authorize_pay_ext_refused_never_echoes_secret() {
        let root = temp_home("authorize-ext");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let seller = home::public_key_hex(&state.home).expect("pubkey");
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(9)),
                method: "tools/call".into(),
                params: json!({
                    "name": "authorize_pay",
                    "arguments": {
                        "job_id": "job",
                        "result_id": "result",
                        "delivery_integrity_hash": "aa".repeat(20),
                        "job_hash": "bb".repeat(32),
                        "seller_pubkey": seller,
                        "amount_sats": 2,
                        "repo": "ext::sh -c evil",
                        "branch": "main",
                        "commit_oid": "aa".repeat(20),
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret), "secret leaked on authorize_pay error");
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            message.contains("ext") || message.contains("refused") || message.contains("transport"),
            "message={message}"
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn post_job_error_path_never_echoes_secret() {
        let root = temp_home("post-echo");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(41)),
                method: "tools/call".into(),
                params: json!({
                    "name": "post_job",
                    "arguments": {
                        "task": "gate-e",
                        "output": "text/plain",
                        "amount_sats": 1
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret), "secret leaked on post_job error");
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            message.contains("seller_pubkey") || message.contains("untargeted"),
            "message={message}"
        );
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn post_job_mcp_refuses_zero_deadline_before_publish() {
        let root = temp_home("post-zero-deadline");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(42)),
                method: "tools/call".into(),
                params: json!({
                    "name": "post_job",
                    "arguments": {
                        "task": "deadline-gate",
                        "output": "text/plain",
                        "amount_sats": 1,
                        "seller_pubkey": "aa".repeat(32),
                        "deadline_unix": 0
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret), "secret leaked on post_job error");
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert!(message.contains("deadline_unix"), "message={message}");
        assert!(message.contains("given=0"), "message={message}");
        assert!(message.contains("current="), "message={message}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "wallet")]
    #[test]
    fn accept_claim_error_path_never_echoes_secret() {
        let root = temp_home("accept-echo");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(42)),
                method: "tools/call".into(),
                params: json!({
                    "name": "accept_claim",
                    "arguments": {
                        "job_id": "aa".repeat(32),
                        "claim_id": "bb".repeat(32)
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret), "secret leaked on accept_claim error");
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            !message.is_empty(),
            "expected refuse message, got empty"
        );
    }

    #[test]
    fn stub_pay_spent_survives_mcp_state_reload() {
        let root = temp_home("spent-reload");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        stub_pay(&state.gate, &json!({ "amount_sats": 7 })).expect("pay");
        assert_eq!(state.gate.lock().expect("lock").spent(), 7);

        // Simulate MCP process restart: new gate from same home.
        let reloaded = state_at(&root);
        assert_eq!(reloaded.gate.lock().expect("lock").spent(), 7);
        assert_eq!(
            reloaded.gate.lock().expect("lock").remaining(),
            reloaded.home.config.total_budget_sats - 7
        );
    }

    // --- GAP-1: explicit-form contribution money-gate ------------------------------------
    // The explicit 9-field pay form must NOT let a `job-class=contribution` offer be paid with
    // `contribution: None` — that would SKIP the four contribution gates + the authorship-tuple
    // seam in authorize_pay (a non-descendant / empty / out-of-scope commit would be payable).
    // The offer class is re-derived from the relay; the load-bearing decision is asserted here.

    // THE GUARD: a contribution offer paid via the explicit form with no threaded binds refuses,
    // naming the accept_claim remedy. Red-on-revert anchor: neutering the refuse flips this to
    // an unexpected Ok (rc101).
    #[cfg(feature = "wallet")]
    #[test]
    fn gap1_contribution_offer_without_binds_is_refused() {
        let err = guard_explicit_contribution_pay("job-contrib", Some("contribution"))
            .expect_err("contribution offer paid contribution-less must refuse");
        assert!(err.contains("job-class=contribution"), "err={err}");
        assert!(err.contains("accept_claim"), "remedy must be named: {err}");
        assert!(err.contains("no contribution binds"), "err={err}");
    }

    // From-scratch (no job-class tag) and any non-`contribution` class proceed unchanged — the
    // guard gates ONLY `job-class=contribution`.
    #[cfg(feature = "wallet")]
    #[test]
    fn gap1_from_scratch_and_other_classes_pass() {
        guard_explicit_contribution_pay("job-scratch", None).expect("from-scratch must pass");
        guard_explicit_contribution_pay("job-other", Some("bounty"))
            .expect("non-contribution class passes");
    }

    // Class-derivation failure is FAIL-CLOSED with a distinct reason (never fail-open to
    // from-scratch). A non-hex job_id makes the offer un-fetchable (EventId parse fails before any
    // relay I/O), so the explicit contribution-less pay refuses at the class guard.
    #[cfg(feature = "wallet")]
    #[test]
    fn gap1_explicit_pay_fails_closed_when_class_underivable() {
        let root = temp_home("gap1-failclosed");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let secret = home::read_secret_key_hex(&state.home).expect("secret");
        let seller = home::public_key_hex(&state.home).expect("pubkey");
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(51)),
                method: "tools/call".into(),
                params: json!({
                    "name": "authorize_pay",
                    "arguments": {
                        "job_id": "contribution-job-not-on-relay",
                        "result_id": "result",
                        "delivery_integrity_hash": "aa".repeat(20),
                        "job_hash": "bb".repeat(32),
                        "seller_pubkey": seller,
                        "amount_sats": 2,
                        "repo": "https://example.invalid/repo.git",
                        "branch": "main",
                        "commit_oid": "aa".repeat(20),
                    }
                }),
            },
        );
        let rendered = response.to_string();
        assert!(!rendered.contains(&secret), "secret leaked on fail-closed refuse");
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            message.contains("cannot determine job-class"),
            "must be the fail-closed class-derivation refusal, got: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The accept-first path is UNTOUCHED: when an accept-bind exists (here from-scratch), the class
    // was already resolved at accept time, so the explicit pay SKIPS the class guard entirely (no
    // relay fetch, no guard refusal) and reaches today's core pre-pay path — proven by refusing at
    // the seller co-signature tooth, NOT at the GAP-1 guard.
    #[cfg(feature = "wallet")]
    #[test]
    fn gap1_accept_bind_present_skips_class_guard() {
        let root = temp_home("gap1-acceptbind");
        let _ = std::fs::remove_dir_all(&root);
        let state = state_at(&root);
        let seller = home::public_key_hex(&state.home).expect("pubkey");
        let job_id = "gap1-accept-skip";
        let bind = job_lifecycle::AcceptedBind {
            job_id: job_id.to_owned(),
            claim_id: "claim-x".into(),
            result_id: "result".into(),
            seller_pubkey: seller.clone(),
            commit_oid: "aa".repeat(20),
            repo: "https://example.invalid/repo.git".into(),
            branch: "main".into(),
            job_hash: "bb".repeat(32),
            amount_sats: 2,
            accept_event_id: "accept-x".into(),
            accepted_at: 0,
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        let jobs_dir = state.home.root.join("jobs");
        std::fs::create_dir_all(&jobs_dir).expect("jobs dir");
        std::fs::write(
            jobs_dir.join(format!("{job_id}.json")),
            serde_json::to_string(&bind).expect("bind json"),
        )
        .expect("write bind");
        let response = dispatch(
            &state,
            &McpRequest {
                id: Some(json!(52)),
                method: "tools/call".into(),
                params: json!({
                    "name": "authorize_pay",
                    "arguments": {
                        "job_id": job_id,
                        "result_id": "result",
                        "delivery_integrity_hash": "aa".repeat(20),
                        "job_hash": "bb".repeat(32),
                        "seller_pubkey": seller,
                        "amount_sats": 2,
                        "repo": "https://example.invalid/repo.git",
                        "branch": "main",
                        "commit_oid": "aa".repeat(20),
                    }
                }),
            },
        );
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        // Reached today's core pre-pay path (guard skipped): the refusal is the seller-cosig tooth,
        // NOT the GAP-1 class guard nor the fail-closed derivation error.
        assert!(
            message.contains("co-signature"),
            "accept-bind path must reach the core cosig tooth, got: {message}"
        );
        assert!(
            !message.contains("no contribution binds")
                && !message.contains("cannot determine job-class"),
            "accept-bind path must NOT hit the GAP-1 class guard, got: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
