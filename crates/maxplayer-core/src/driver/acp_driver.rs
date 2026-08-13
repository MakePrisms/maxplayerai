use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// Tokio (not std) channels: every driver wait must YIELD to the runtime instead of blocking the
// thread. The seller node runs all awarded jobs as `spawn_local` tasks on ONE LocalSet thread, so
// a `std::sync::mpsc` blocking receive here — which used to span the ENTIRE `session/prompt` turn,
// i.e. the whole agent run — froze every other job and the run loop itself (issue #223). The
// reader THREAD stays a plain thread (blocking reads of the child's stdout belong off-runtime);
// only the receive side is async.
use tokio::sync::mpsc;

use serde_json::{Value, json};

use crate::driver::acp::{PROTOCOL_VERSION, UpdateStream, parse_acp_usage};
use crate::driver::{
    Artifact, Caps, ContentBlock, Driver, DriverError, Initialize, PermissionOutcome,
    PermissionRequest, PromptTurn, Readiness, RuntimeId, SessionConfig, SessionId, SessionUpdate,
    StopReason, UsageMetadata,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    program: String,
    args: Vec<String>,
}

impl AgentCommand {
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self { program, args }
    }

    fn runtime_id(&self) -> RuntimeId {
        RuntimeId(self.program.clone())
    }
}

pub struct AcpDriver {
    command: AgentCommand,
    permission_policy: PermissionOutcome,
    idle_timeout: Duration,
    child: Option<Child>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    responses: Option<mpsc::UnboundedReceiver<RpcResponse>>,
    updates: Option<mpsc::UnboundedReceiver<SessionUpdate>>,
    update_tx: Option<mpsc::UnboundedSender<SessionUpdate>>,
    next_request_id: AtomicU64,
    /// ACP-native usage captured from the most recent `session/prompt` result.
    /// `None` when the harness surfaced nothing (absent-stays-absent).
    last_usage: Option<UsageMetadata>,
    /// The harness-resolved model id (`models.currentModelId`) captured from the `session/new`
    /// response — the only ACP surface that carries it (the prompt result does not; see
    /// [`super::acp::parse_acp_usage`]). Folded into [`Self::usage`] so a run's exec-metadata carries
    /// the resolved model (#455). `None` when the harness reported no model.
    session_model: Option<String>,
}

impl AcpDriver {
    pub fn new(
        command: AgentCommand,
        permission_policy: PermissionOutcome,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            command,
            permission_policy,
            idle_timeout,
            child: None,
            stdin: None,
            responses: None,
            updates: None,
            update_tx: None,
            next_request_id: AtomicU64::new(1),
            last_usage: None,
            session_model: None,
        }
    }

    fn spawn(&mut self) -> Result<(), DriverError> {
        if self.child.is_some() {
            return Ok(());
        }

        let mut command = Command::new(&self.command.program);
        command
            .args(&self.command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|error| DriverError::Other(format!("failed to spawn ACP agent: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DriverError::Other("ACP child stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DriverError::Other("ACP child stdout unavailable".into()))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let (response_tx, response_rx) = mpsc::unbounded_channel();
        let (update_tx, update_rx) = mpsc::unbounded_channel();
        let update_tx_for_reader = update_tx.clone();
        let stdin_for_reader = stdin.clone();
        let permission_policy = self.permission_policy.clone();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let mut respond_permission = |id, result| {
                    let _ = write_wire_to_stdin(&stdin_for_reader, &response_value(id, result));
                };
                route_wire_message(
                    &value,
                    &response_tx,
                    &update_tx_for_reader,
                    &permission_policy,
                    &mut respond_permission,
                );
            }
        });

        self.stdin = Some(stdin);
        self.responses = Some(response_rx);
        self.updates = Some(update_rx);
        self.update_tx = Some(update_tx);
        self.child = Some(child);
        Ok(())
    }

    fn send_request(&self, method: &str, params: Value) -> Result<u64, DriverError> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_wire(&request)?;
        Ok(id)
    }

    fn write_wire(&self, value: &Value) -> Result<(), DriverError> {
        let stdin = self
            .stdin
            .as_ref()
            .ok_or_else(|| DriverError::Other("ACP child stdin unavailable".into()))?;
        let mut stdin = stdin
            .lock()
            .map_err(|_| DriverError::Other("ACP child stdin lock poisoned".into()))?;
        serde_json::to_writer(&mut *stdin, value).map_err(|error| {
            DriverError::Other(format!("failed to encode ACP JSON-RPC: {error}"))
        })?;
        stdin
            .write_all(b"\n")
            .and_then(|_| stdin.flush())
            .map_err(|error| DriverError::Other(format!("failed to write ACP JSON-RPC: {error}")))
    }

    /// Await the response to request `id`. This is where the driver spends the whole agent turn
    /// (`session/prompt` answers only when the turn ends), so it MUST yield to the runtime — a
    /// blocking receive here serializes every job on the seller node's single-threaded LocalSet
    /// and deafens its run loop (issue #223).
    async fn wait_response(&mut self, id: u64) -> Result<Value, DriverError> {
        let idle_timeout = self.idle_timeout;
        let responses = self
            .responses
            .as_mut()
            .ok_or_else(|| DriverError::Other("ACP response channel unavailable".into()))?;
        loop {
            let response = tokio::time::timeout(idle_timeout, responses.recv())
                .await
                .map_err(|_| DriverError::ResponseTimeout { request_id: id })?
                .ok_or_else(|| {
                    DriverError::Other(format!(
                        "ACP agent exited before responding to request {id}"
                    ))
                })?;
            if response.id != json!(id) {
                continue;
            }
            if let Some(error) = response.error {
                return Err(DriverError::Other(format!(
                    "ACP request {id} failed: {error}"
                )));
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }
}

impl Driver for AcpDriver {
    fn id(&self) -> RuntimeId {
        self.command.runtime_id()
    }

    async fn ready(&mut self) -> Result<Readiness, DriverError> {
        self.spawn()?;
        let initialize = Initialize::new(Caps::default());
        let id = self.send_request(
            "initialize",
            serde_json::to_value(initialize).map_err(|error| {
                DriverError::Other(format!("failed to encode initialize params: {error}"))
            })?,
        )?;
        let result = self.wait_response(id).await?;
        let protocol_version = result
            .get("protocol_version")
            .or_else(|| result.get("protocolVersion"))
            .and_then(Value::as_u64)
            .map(|value| value as u32)
            .unwrap_or(PROTOCOL_VERSION);
        if !supports_negotiated_protocol(protocol_version) {
            return Err(DriverError::Other(format!(
                "unsupported ACP protocol version {protocol_version}"
            )));
        }
        Ok(Readiness {
            runtime_id: self.command.runtime_id(),
            protocol_version,
        })
    }

    async fn start_session(&mut self, cfg: SessionConfig) -> Result<SessionId, DriverError> {
        let id = self.send_request(
            "session/new",
            serde_json::to_value(cfg).map_err(|error| {
                DriverError::Other(format!("failed to encode session params: {error}"))
            })?,
        )?;
        let result = self.wait_response(id).await?;
        // Capture the harness-resolved model (`models.currentModelId`) before the response is reduced
        // to a session id — this is the only ACP surface that carries it (#455). Absent-stays-absent:
        // a harness that reports no model leaves this `None`, and nothing downstream fabricates one.
        self.session_model = session_model_from_result(&result);
        session_id_from_result(&result)
    }

    async fn prompt(
        &mut self,
        session_id: &SessionId,
        turn: PromptTurn,
    ) -> Result<UpdateStream, DriverError> {
        let id = self.send_request("session/prompt", prompt_params(session_id, turn))?;
        let result = self.wait_response(id).await?;
        // Capture ACP-native usage off the prompt result before we reduce it to a stop reason.
        // Absent-stays-absent — `None` when the harness surfaced nothing.
        self.last_usage = parse_acp_usage(&result);
        if let Some(update_tx) = &self.update_tx {
            let _ = update_tx.send(SessionUpdate::TurnEnded(stop_reason_from_params(&result)));
        }
        let receiver = self
            .updates
            .take()
            .ok_or_else(|| DriverError::Other("ACP update channel already consumed".into()))?;
        Ok(UpdateStream::live(receiver, self.idle_timeout))
    }

    async fn on_permission(&mut self, _req: PermissionRequest) -> PermissionOutcome {
        self.permission_policy.clone()
    }

    async fn artifacts(&self, _session_id: &SessionId) -> Result<Vec<Artifact>, DriverError> {
        Ok(Vec::new())
    }

    async fn cancel(&mut self, session_id: &SessionId) -> Result<(), DriverError> {
        if self.stdin.is_some() {
            let id = self.send_request(
                "session/cancel",
                json!({
                    "session_id": session_id,
                    "sessionId": session_id,
                }),
            )?;
            let _ = self.wait_response(id).await;
        }
        Ok(())
    }

    fn usage(&self) -> Option<UsageMetadata> {
        merge_session_model(self.last_usage.clone(), self.session_model.as_deref())
    }

    async fn shutdown(&mut self) -> Result<(), DriverError> {
        if let Some(mut child) = self.child.take() {
            // Reaping the child (`wait`) and the group TERM are blocking syscalls/process spawns;
            // run them off the runtime so shutdown never stalls sibling jobs on the LocalSet.
            let _ = tokio::task::spawn_blocking(move || {
                #[cfg(unix)]
                {
                    let pid = child.id();
                    // Signal the GROUP only when this child actually leads it. `spawn` requests
                    // `process_group(0)`, but nothing here ever verified the request took effect —
                    // and a group TERM aimed at a group we do not own lands on processes that are
                    // not ours. On a CI runner that is the runner's own tree: the job dies at the
                    // instant of shutdown with exit 143 and reports "the runner has received a
                    // shutdown signal", which looks like infrastructure rather than like us.
                    match process_group_of(pid) {
                        Some(pgid) if pgid == pid => {
                            // `--` ends option parsing so an external `kill` cannot read the
                            // negative operand as a signal spec.
                            let _ = Command::new("kill")
                                .arg("-s")
                                .arg("TERM")
                                .arg("--")
                                .arg(format!("-{pgid}"))
                                .status();
                        }
                        // Not the group leader (or the group is unreadable): kill this process
                        // alone. Leaking a grandchild is recoverable; signalling a group we do not
                        // own is not.
                        _ => {}
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
            })
            .await;
        }
        Ok(())
    }
}

/// The process group id of `pid`, or `None` if it cannot be established.
///
/// `None` is the SAFE answer — a caller that cannot establish the group MUST NOT signal one.
///
/// Read from `/proc/<pid>/stat` rather than `libc::getpgid` deliberately: `libc` is an **optional**
/// dependency of this crate, enabled only by the `wallet` feature, while this module compiles under
/// `default = []`. A `libc` call here would break the default build — the one that is green.
#[cfg(unix)]
fn process_group_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_pgrp_from_stat(&stat)
}

/// Pull field 5 (`pgrp`) out of a `/proc/<pid>/stat` line.
///
/// ⚠ These fields CANNOT be split naively from the left. Field 2 is `comm`, a parenthesised command
/// name that may itself contain spaces and parentheses (`(my prog (v2))`), which mis-numbers every
/// field after it. Everything after the LAST `)` is parsed instead; there the fields are
/// `state ppid pgrp …`, so `pgrp` is the 3rd.
#[cfg(unix)]
fn parse_pgrp_from_stat(stat: &str) -> Option<u32> {
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(2)?.parse().ok()
}

#[cfg(all(test, unix))]
mod group_scope_tests {
    use super::parse_pgrp_from_stat;

    /// The ordinary shape.
    #[test]
    fn pgrp_is_field_five() {
        // pid comm state ppid pgrp …
        assert_eq!(parse_pgrp_from_stat("4242 (bash) S 4000 4242 4242 0"), Some(4242));
        assert_eq!(parse_pgrp_from_stat("4242 (bash) S 4000 999 4242 0"), Some(999));
    }

    /// ★ The reason this is parsed from the RIGHT. A left-to-right split would read `prog` as the
    /// state field and return a wrong pgrp — silently, since it still parses as a number.
    #[test]
    fn a_comm_containing_spaces_and_parens_does_not_shift_the_fields() {
        assert_eq!(
            parse_pgrp_from_stat("77 (my prog (v2)) S 1 4242 4242 0"),
            Some(4242)
        );
        assert_eq!(parse_pgrp_from_stat("77 (a b c d e) S 1 555 555 0"), Some(555));
    }

    /// Unreadable ⇒ `None`, and `None` must mean "never signal a group".
    #[test]
    fn unparseable_stat_yields_none_so_no_group_is_signalled() {
        assert_eq!(parse_pgrp_from_stat(""), None);
        assert_eq!(parse_pgrp_from_stat("no parens here at all"), None);
        assert_eq!(parse_pgrp_from_stat("77 (bash) S"), None);
        assert_eq!(parse_pgrp_from_stat("77 (bash) S 1 notanumber"), None);
    }

    /// Live control: this test's own process must be readable, and its pgrp non-zero. Without this,
    /// every assertion above is about a string literal and none about `/proc`.
    #[test]
    fn our_own_process_group_is_readable() {
        let pgid = super::process_group_of(std::process::id())
            .expect("our own /proc/<pid>/stat must be readable");
        assert!(pgid > 0, "a real pgrp is never 0");
    }
}

#[derive(Debug)]
struct RpcResponse {
    id: Value,
    result: Option<Value>,
    error: Option<Value>,
}

fn route_wire_message(
    value: &Value,
    response_tx: &mpsc::UnboundedSender<RpcResponse>,
    update_tx: &mpsc::UnboundedSender<SessionUpdate>,
    permission_policy: &PermissionOutcome,
    respond_permission: &mut impl FnMut(Value, Value),
) {
    if value.get("method").is_none() {
        if let Some(id) = value.get("id").cloned() {
            let _ = response_tx.send(RpcResponse {
                id,
                result: value.get("result").cloned(),
                error: value.get("error").cloned(),
            });
        }
        return;
    }

    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    if is_permission_method(method) {
        if let Some(id) = value.get("id").cloned()
            && let Some(result) = permission_response_result(&params, permission_policy)
        {
            respond_permission(id, result);
        }
        if let Some(request) = permission_request_from_params(&params) {
            let _ = update_tx.send(SessionUpdate::PermissionRequest(request));
        }
        return;
    }

    let update = session_update_from_method(method, params);
    let _ = update_tx.send(update);
}

fn response_value(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn write_wire_to_stdin(stdin: &Arc<Mutex<ChildStdin>>, value: &Value) -> Result<(), DriverError> {
    let mut stdin = stdin
        .lock()
        .map_err(|_| DriverError::Other("ACP child stdin lock poisoned".into()))?;
    serde_json::to_writer(&mut *stdin, value)
        .map_err(|error| DriverError::Other(format!("failed to encode ACP JSON-RPC: {error}")))?;
    stdin
        .write_all(b"\n")
        .and_then(|_| stdin.flush())
        .map_err(|error| DriverError::Other(format!("failed to write ACP JSON-RPC: {error}")))
}

fn prompt_params(session_id: &str, turn: PromptTurn) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": turn
            .input
            .into_iter()
            .map(prompt_content_block)
            .collect::<Vec<_>>(),
    })
}

fn prompt_content_block(block: crate::driver::ContentBlock) -> Value {
    match block {
        crate::driver::ContentBlock::Text { text } => json!({
            "type": "text",
            "text": text,
        }),
        crate::driver::ContentBlock::Artifact(artifact) => {
            let mut value = serde_json::to_value(artifact).unwrap_or(Value::Null);
            if let Value::Object(object) = &mut value {
                object.insert("type".into(), Value::String("artifact".into()));
            }
            value
        }
    }
}

fn session_id_from_result(result: &Value) -> Result<SessionId, DriverError> {
    result
        .get("session_id")
        .or_else(|| result.get("sessionId"))
        .or_else(|| result.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| result.as_str().map(str::to_owned))
        .ok_or_else(|| {
            DriverError::Other(format!("ACP session result missing session id: {result}"))
        })
}

/// The harness-resolved model id from a `session/new` result: ACP `models.currentModelId`
/// (camelCase on the wire). This is the resolved identity INCLUDING the harness's reasoning-effort
/// suffix, e.g. `gpt-5.6-terra[medium]`. `None` when the response carries no model block — the field
/// is optional, so absence stays absence and is never fabricated.
fn session_model_from_result(result: &Value) -> Option<String> {
    result
        .get("models")?
        .get("currentModelId")?
        .as_str()
        .map(str::to_owned)
}

/// Fold the `session/new` model into a run's captured usage. The `session/prompt` result never
/// carries a model (see [`super::acp::parse_acp_usage`]), so the resolved model is OR-filled from the
/// session-start capture — but only when the prompt usage did not itself surface one (a real wire
/// model always wins). When there is no token usage at all yet a model IS known, the model alone is
/// surfaced: a known model with unknown token counts is honest, not empty.
fn merge_session_model(
    last_usage: Option<UsageMetadata>,
    session_model: Option<&str>,
) -> Option<UsageMetadata> {
    match (last_usage, session_model) {
        (Some(mut usage), model) => {
            if usage.model.is_none() {
                usage.model = model.map(str::to_owned);
            }
            Some(usage)
        }
        (None, Some(model)) => Some(UsageMetadata {
            model: Some(model.to_owned()),
            ..UsageMetadata::default()
        }),
        (None, None) => None,
    }
}

fn is_permission_method(method: &str) -> bool {
    method.contains("permission")
}

fn permission_request_from_params(params: &Value) -> Option<PermissionRequest> {
    serde_json::from_value(params.clone())
        .ok()
        .or_else(|| serde_json::from_value(params.get("request")?.clone()).ok())
}

fn permission_response_result(params: &Value, policy: &PermissionOutcome) -> Option<Value> {
    let options = params
        .get("options")
        .or_else(|| params.get("request")?.get("options"))?
        .as_array()?;

    // ACP `PermissionOption.optionId` is an ARBITRARY, agent-chosen string; the SEMANTIC
    // category lives in `kind` (allow_once | allow_always | reject_once | reject_always).
    // Select by `kind` so the choice is harness-agnostic: claude-agent-acp names its
    // allow-once option `optionId: "allow"` while codex-acp names it `optionId: "allow_once"`,
    // but BOTH set `kind: "allow_once"`. The previous code matched the raw `optionId` against a
    // literal "allow"/"reject", which silently found nothing for codex — so no response was
    // ever written and the agent blocked the whole `session/prompt` turn (the "ACP request 3"
    // timeout). The legacy optionId-string match is kept as a fallback ONLY for agents that
    // omit `kind`.
    let wanted_kinds: &[&str] = match policy {
        PermissionOutcome::Allow => &["allow_once", "allow_always"],
        PermissionOutcome::AllowAlways => &["allow_always", "allow_once"],
        PermissionOutcome::Deny => &["reject_once", "reject_always"],
    };
    let legacy_id = match policy {
        PermissionOutcome::Allow => "allow",
        PermissionOutcome::AllowAlways => "allow_always",
        PermissionOutcome::Deny => "reject",
    };

    let option_id = wanted_kinds
        .iter()
        .find_map(|wanted_kind| {
            options.iter().find_map(|option| {
                let kind = option.get("kind").and_then(Value::as_str)?;
                (kind == *wanted_kind)
                    .then(|| permission_option_id(option))
                    .flatten()
            })
        })
        .or_else(|| {
            options
                .iter()
                .filter_map(permission_option_id)
                .find(|option_id| option_id == legacy_id)
        })?;

    Some(json!({
        "outcome": {
            "outcome": "selected",
            "optionId": option_id,
        }
    }))
}

fn permission_option_id(option: &Value) -> Option<String> {
    option
        .get("optionId")
        .or_else(|| option.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn session_update_from_method(method: &str, params: Value) -> SessionUpdate {
    if let Some(update) = session_update_from_params(&params) {
        return update;
    }

    match method {
        "session/update" | "session.update" | "session_update" => {
            SessionUpdate::Ext(crate::driver::ExtMethod {
                method: method.into(),
                params,
            })
        }
        method if method.contains("turn") && method.contains("end") => {
            SessionUpdate::TurnEnded(stop_reason_from_params(&params))
        }
        _ => SessionUpdate::Ext(crate::driver::ExtMethod {
            method: method.into(),
            params,
        }),
    }
}

fn session_update_from_params(params: &Value) -> Option<SessionUpdate> {
    params
        .get("update")
        .and_then(session_update_from_wire_update)
        .or_else(|| session_update_from_wire_update(params))
        .or_else(|| serde_json::from_value(params.clone()).ok())
        .or_else(|| serde_json::from_value(params.get("update")?.clone()).ok())
}

fn session_update_from_wire_update(update: &Value) -> Option<SessionUpdate> {
    match update.get("sessionUpdate")?.as_str()? {
        "agent_message_chunk" => {
            wire_content_block(update.get("content")?).map(SessionUpdate::AgentMessageChunk)
        }
        _ => None,
    }
}

fn wire_content_block(value: &Value) -> Option<ContentBlock> {
    match value.get("type").and_then(Value::as_str) {
        Some("text") | None => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| ContentBlock::Text { text: text.into() }),
        Some("artifact") => serde_json::from_value(value.clone()).ok(),
        _ => None,
    }
}

fn stop_reason_from_params(params: &Value) -> StopReason {
    params
        .get("reason")
        .or_else(|| params.get("stop_reason"))
        .or_else(|| params.get("stopReason"))
        .and_then(Value::as_str)
        .and_then(|reason| match reason {
            "completed" | "end_turn" => Some(StopReason::Completed),
            "cancelled" | "canceled" => Some(StopReason::Cancelled),
            "failed" => Some(StopReason::Failed),
            _ => None,
        })
        .unwrap_or(StopReason::Failed)
}

fn supports_negotiated_protocol(protocol_version: u32) -> bool {
    (1..=PROTOCOL_VERSION).contains(&protocol_version)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::driver::{ContentBlock, ExtMethod, PermissionOutcome};

    #[test]
    fn session_model_read_from_the_new_session_response() {
        // Ground-truthed against a captured codex-acp `session/new` result: the resolved model is
        // `models.currentModelId`, carrying the reasoning-effort suffix (e.g. `[medium]`).
        let result = json!({
            "sessionId": "019f61bd-89be-7230-b67b-717871387cea",
            "models": {
                "currentModelId": "gpt-5.6-terra[medium]",
                "availableModels": [
                    {"modelId": "gpt-5.6-terra[medium]", "name": "GPT-5.6-Terra (medium)"}
                ]
            }
        });
        assert_eq!(
            session_model_from_result(&result).as_deref(),
            Some("gpt-5.6-terra[medium]")
        );
    }

    #[test]
    fn session_model_absent_stays_absent() {
        // No model block, or a block without `currentModelId` → None (opportunistic, never fabricated).
        assert_eq!(session_model_from_result(&json!({"sessionId": "abc"})), None);
        assert_eq!(
            session_model_from_result(
                &json!({"sessionId": "abc", "models": {"availableModels": []}})
            ),
            None
        );
    }

    #[test]
    fn merge_session_model_or_fills_only_a_missing_model() {
        // Prompt usage carries tokens but never a model (parse_acp_usage); the session-start model
        // fills it and the tokens are preserved — the #455 fix at the driver's usage seam.
        let prompt = UsageMetadata {
            input_tokens: Some(3162),
            output_tokens: Some(10),
            ..UsageMetadata::default()
        };
        let merged =
            merge_session_model(Some(prompt), Some("gpt-5.6-terra[medium]")).expect("some");
        assert_eq!(merged.model.as_deref(), Some("gpt-5.6-terra[medium]"));
        assert_eq!(merged.input_tokens, Some(3162));
        assert_eq!(merged.output_tokens, Some(10));

        // A model already present on the usage is never clobbered by the session model.
        let wired = UsageMetadata {
            model: Some("wire-model".into()),
            ..UsageMetadata::default()
        };
        assert_eq!(
            merge_session_model(Some(wired), Some("gpt-5.6-terra[medium]"))
                .expect("some")
                .model
                .as_deref(),
            Some("wire-model")
        );

        // No token usage at all but a known model → surface the model alone (not empty/None).
        let model_only = merge_session_model(None, Some("gpt-5.6-terra[medium]")).expect("some");
        assert_eq!(model_only.model.as_deref(), Some("gpt-5.6-terra[medium]"));
        assert!(model_only.input_tokens.is_none());

        // Nothing known stays None.
        assert!(merge_session_model(None, None).is_none());
    }

    #[test]
    fn driver_usage_surfaces_the_captured_session_model() {
        // Pins the wiring (not just the pure helper): the trait `usage()` folds the captured
        // session model into the run usage, so a regression that stops merging is caught here.
        use crate::driver::Driver;
        let mut driver = AcpDriver::new(
            AgentCommand::new("codex".into(), Vec::new()),
            PermissionOutcome::Allow,
            Duration::from_secs(1),
        );
        driver.session_model = Some("gpt-5.6-terra[medium]".into());
        driver.last_usage = Some(UsageMetadata {
            input_tokens: Some(3162),
            ..UsageMetadata::default()
        });
        let usage = driver.usage().expect("usage present");
        assert_eq!(usage.model.as_deref(), Some("gpt-5.6-terra[medium]"));
        assert_eq!(usage.input_tokens, Some(3162));
    }

    #[test]
    fn request_side_wire_uses_real_acp_camel_case() {
        let initialize =
            serde_json::to_value(Initialize::new(Caps::default())).expect("serialize initialize");
        assert_eq!(
            initialize,
            json!({
                "protocolVersion": 2,
                "clientCapabilities": {
                    "methods": []
                }
            })
        );

        let session = serde_json::to_value(SessionConfig {
            cwd: "/tmp/maxplayer".into(),
            mcp_servers: Vec::new(),
            env: Vec::new(),
        })
        .expect("serialize session config");
        assert_eq!(
            session,
            json!({
                "cwd": "/tmp/maxplayer",
                "mcpServers": [],
                "env": []
            })
        );

        let turn = PromptTurn {
            input: vec![ContentBlock::Text { text: "hi".into() }],
        };
        assert_eq!(
            prompt_params("session-1", turn),
            json!({
                "sessionId": "session-1",
                "prompt": [
                    {
                        "type": "text",
                        "text": "hi"
                    }
                ]
            })
        );
    }

    #[test]
    fn negotiated_protocol_accepts_real_acp_v1() {
        assert!(supports_negotiated_protocol(1));
        assert!(supports_negotiated_protocol(PROTOCOL_VERSION));
        assert!(!supports_negotiated_protocol(0));
        assert!(!supports_negotiated_protocol(PROTOCOL_VERSION + 1));
    }

    #[test]
    fn real_prompt_response_stop_reason_becomes_terminal_update() {
        assert_eq!(
            stop_reason_from_params(&json!({
                "stopReason": "end_turn",
                "usage": {"inputTokens": 1, "outputTokens": 1}
            })),
            StopReason::Completed
        );
        assert_eq!(
            stop_reason_from_params(&json!({"stopReason": "cancelled"})),
            StopReason::Cancelled
        );
        assert_eq!(
            stop_reason_from_params(&json!({"stopReason": "unrecognized"})),
            StopReason::Failed
        );
        assert_eq!(stop_reason_from_params(&json!({})), StopReason::Failed);
    }

    #[test]
    fn fixture_lines_translate_to_updates() {
        let (response_tx, _response_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let mut permission_responses = Vec::new();

        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "type": "agent_message",
                    "data": [{"type": "text", "data": {"text": "hello"}}]
                }
            }),
            &response_tx,
            &update_tx,
            &PermissionOutcome::Allow,
            &mut |id, result| permission_responses.push(response_value(id, result)),
        );
        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "session/turn/end",
                "params": {"reason": "completed"}
            }),
            &response_tx,
            &update_tx,
            &PermissionOutcome::Allow,
            &mut |id, result| permission_responses.push(response_value(id, result)),
        );

        assert_eq!(
            update_rx.try_recv().expect("first update"),
            SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                text: "hello".into()
            }])
        );
        assert_eq!(
            update_rx.try_recv().expect("terminal"),
            SessionUpdate::TurnEnded(StopReason::Completed)
        );
        assert!(permission_responses.is_empty());
    }

    #[test]
    fn real_agent_message_chunks_translate_to_updates() {
        let (response_tx, _response_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let mut permission_responses = Vec::new();

        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "hello "}
                    }
                }
            }),
            &response_tx,
            &update_tx,
            &PermissionOutcome::Allow,
            &mut |id, result| permission_responses.push(response_value(id, result)),
        );

        assert_eq!(
            update_rx.try_recv().expect("chunk update"),
            SessionUpdate::AgentMessageChunk(ContentBlock::Text {
                text: "hello ".into()
            })
        );
        assert!(permission_responses.is_empty());
    }

    #[test]
    fn unknown_methods_surface_as_ext() {
        let (response_tx, _response_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let mut permission_responses = Vec::new();
        let params = json!({"x": 1});

        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "cursor/ask_question",
                "params": params
            }),
            &response_tx,
            &update_tx,
            &PermissionOutcome::Allow,
            &mut |id, result| permission_responses.push(response_value(id, result)),
        );

        assert_eq!(
            update_rx.try_recv().expect("ext update"),
            SessionUpdate::Ext(ExtMethod {
                method: "cursor/ask_question".into(),
                params,
            })
        );
        assert!(permission_responses.is_empty());
    }

    #[test]
    fn permission_request_replies_immediately_and_emits_observer_update() {
        let (response_tx, _response_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let mut permission_responses = Vec::new();

        route_wire_message(
            &json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "session/request_permission",
                "params": {
                    "tool": "shell",
                    "detail": {"cmd": "true"},
                    "options": [
                        {"id": "allow_always", "kind": "allow_always"},
                        {"id": "allow", "kind": "allow_once"},
                        {"id": "reject", "kind": "reject"}
                    ]
                }
            }),
            &response_tx,
            &update_tx,
            &PermissionOutcome::Allow,
            &mut |id, result| permission_responses.push(response_value(id, result)),
        );

        assert_eq!(
            permission_responses,
            vec![json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow"
                    }
                }
            })]
        );
        assert_eq!(
            update_rx.try_recv().expect("permission update"),
            SessionUpdate::PermissionRequest(PermissionRequest {
                tool: "shell".into(),
                detail: json!({"cmd": "true"}),
            })
        );
        assert_eq!(
            permission_response_result(
                &json!({
                    "options": [
                        {"optionId": "allow_always"},
                        {"optionId": "allow"},
                        {"optionId": "reject"}
                    ]
                }),
                &PermissionOutcome::AllowAlways
            ),
            Some(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_always"
                }
            }))
        );
        assert_eq!(
            permission_response_result(
                &json!({
                    "options": [
                        {"optionId": "allow_always"},
                        {"optionId": "allow"},
                        {"optionId": "reject"}
                    ]
                }),
                &PermissionOutcome::Deny
            ),
            Some(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "reject"
                }
            }))
        );

        let mut driver = AcpDriver::new(
            AgentCommand::new("fake".into(), Vec::new()),
            PermissionOutcome::Allow,
            Duration::from_secs(1),
        );
        assert_eq!(
            futures_free_on_permission(
                &mut driver,
                PermissionRequest {
                    tool: "shell".into(),
                    detail: json!({"cmd": "true"}),
                }
            ),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn codex_permission_options_are_answered_by_kind_not_hung() {
        // Regression for the codex-harness "ACP request 3 timeout": codex-acp names its
        // permission options by `kind` (optionId "allow_once"/"allow_always"/"reject_once"),
        // NEVER the literal "allow"/"reject" that claude-agent-acp happens to use. The reader
        // thread must select by the spec `kind` and write a permission RESPONSE — otherwise
        // codex blocks awaiting a decision that never comes and the whole `session/prompt`
        // turn hangs until the job deadline. Exercised through `route_wire_message` so the test
        // covers the exact auto-answer path the live hang takes.
        let codex_request = |policy| {
            let (response_tx, _response_rx) = mpsc::unbounded_channel();
            let (update_tx, _update_rx) = mpsc::unbounded_channel();
            let mut permission_responses = Vec::new();
            route_wire_message(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": "codex-session",
                        "options": [
                            {"optionId": "allow_once", "name": "Allow Once", "kind": "allow_once"},
                            {"optionId": "allow_always", "name": "Allow for Session", "kind": "allow_always"},
                            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"}
                        ]
                    }
                }),
                &response_tx,
                &update_tx,
                policy,
                &mut |id, result| permission_responses.push(response_value(id, result)),
            );
            permission_responses
        };

        let selected = |option_id: &str| {
            vec![json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {"outcome": {"outcome": "selected", "optionId": option_id}}
            })]
        };

        // The live default (the seller node: PermissionOutcome::Allow) MUST resolve — this is the
        // case that hung. Deny/AllowAlways selected by kind too.
        assert_eq!(
            codex_request(&PermissionOutcome::Allow),
            selected("allow_once"),
            "codex allow must be answered, not dropped (the ACP request 3 hang)"
        );
        assert_eq!(
            codex_request(&PermissionOutcome::AllowAlways),
            selected("allow_always")
        );
        assert_eq!(
            codex_request(&PermissionOutcome::Deny),
            selected("reject_once")
        );
    }

    /// #729 end-to-end, on a live child: `cat` echoes the `initialize` request back verbatim and
    /// never answers it, so `ready()` times out and `run_job` exits through its very first `?`.
    /// That failure exit must still shut the driver down AND reap the child — the measured leak
    /// was a healthy ACP child parented to the seller daemon 80 minutes past `execute fail`, and
    /// a zombie (kill without `wait`) once killed by hand.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_run_still_reaps_the_acp_child() {
        use crate::engine::{RunParams, run_job};
        use crate::event::JobId;
        use crate::log::EventLog;

        let mut driver = AcpDriver::new(
            AgentCommand::new("cat".into(), Vec::new()),
            PermissionOutcome::Allow,
            Duration::from_millis(200),
        );
        // Spawn first so the child pid is observable before the run consumes the driver;
        // `spawn` is idempotent, so `ready()` reuses this same child.
        driver.spawn().expect("spawn cat");
        let pid = driver.child.as_ref().expect("child present").id();

        let log_path = std::env::temp_dir().join(format!(
            "maxplayer-acp-reap-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&log_path);
        let mut log = EventLog::open(&log_path).expect("open log");

        let result = run_job(
            &mut driver,
            &mut log,
            &JobId("job-reap".into()),
            RunParams::mock_defaults(),
            &mut |_| {},
        )
        .await;
        assert!(result.is_err(), "initialize must time out under cat");

        // `shutdown()` ran on the failure exit and took the child…
        assert!(
            driver.child.is_none(),
            "the failure exit must shut the driver down (the #729 leak)"
        );
        // …and `wait()` reaped it: `run_job` awaited the off-runtime kill+wait, so by now the
        // pid is gone from the process table (or reused — a stranger's ppid). A `Z`-state child
        // of THIS process is the exact absent-`wait` signature from the live seat.
        if let Some((state, ppid)) = state_and_ppid(pid) {
            assert!(
                !(state == 'Z' && ppid == std::process::id()),
                "ACP child {pid} is a zombie of this process — killed but never waited"
            );
        }
    }

    /// `(state, ppid)` from `/proc/<pid>/stat`, parsed from the right of `comm` like
    /// [`super::parse_pgrp_from_stat`]; `None` when the pid is gone (fully reaped).
    #[cfg(unix)]
    fn state_and_ppid(pid: u32) -> Option<(char, u32)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        let mut fields = after_comm.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let ppid = fields.next()?.parse().ok()?;
        Some((state, ppid))
    }

    fn futures_free_on_permission(
        driver: &mut AcpDriver,
        request: PermissionRequest,
    ) -> PermissionOutcome {
        let future = driver.on_permission(request);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(outcome) => outcome,
            std::task::Poll::Pending => panic!("permission future should not pend"),
        }
    }

    #[tokio::test]
    async fn response_timeout_keeps_a_typed_driver_error() {
        // Keep the sender alive so this waits for the timer instead of observing a closed channel.
        let (_response_tx, response_rx) = mpsc::unbounded_channel();
        let mut driver = AcpDriver::new(
            AgentCommand::new("fake".into(), Vec::new()),
            PermissionOutcome::Allow,
            Duration::from_millis(1),
        );
        driver.responses = Some(response_rx);

        assert_eq!(
            driver.wait_response(3).await.expect_err("must time out"),
            DriverError::ResponseTimeout { request_id: 3 },
            "the timer's classification must not be flattened into message text"
        );
    }
}
