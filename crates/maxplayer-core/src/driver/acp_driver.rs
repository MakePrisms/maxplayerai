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
    /// The harness-resolved model id captured from the `session/new` response — the only ACP surface
    /// that carries it (the prompt result does not; see [`super::acp::parse_acp_usage`]). Read from
    /// either wire shape by [`session_model_from_result`]. Folded into [`Self::usage`] so a run's
    /// exec-metadata carries the resolved model (#455). `None` when the harness reported no model.
    session_model: Option<String>,
    /// The CONCRETE model id the Claude adapter announced for the turn, captured off its private
    /// `_claude/sdkMessage` `system`/`init` frame. Shared with the stdout reader thread because a
    /// notification is only ever seen there. Stays `None` for every other harness: nothing else
    /// sends that method, and [`Self::is_claude_adapter`] means nothing else is asked to.
    claude_turn_model: Arc<Mutex<Option<String>>>,
    /// Set when `initialize` came back from claude-agent-acp ITSELF. Gates the adapter-private
    /// opt-in below off the generic ACP path.
    ///
    /// Written by [`Driver::ready`], read by [`Driver::start_session`], and that ORDER is what makes
    /// the opt-in reach the wire — `run_job` calls them in exactly that sequence
    /// (`engine.rs:133`, then `:141`). A caller that started a session without readying the driver
    /// first would send no opt-in and resolve no model; it would fail closed to absent, not wrong.
    is_claude_adapter: bool,
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
            claude_turn_model: Arc::new(Mutex::new(None)),
            is_claude_adapter: false,
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
        let claude_turn_model = self.claude_turn_model.clone();

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
                handle_wire_message(
                    &value,
                    &claude_turn_model,
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
    /// Forget any turn-resolved model, because a new session has resolved none yet.
    ///
    /// Every current call site builds a fresh driver per run, so this clears nothing today. A REUSED
    /// driver would otherwise report the previous session's concrete model for a session whose init
    /// frame never arrives — a WRONG model where absence was the truth, which is the failure mode
    /// this whole path exists to remove. A poisoned lock leaves the stale value in place and is not
    /// worth failing a session over; the model is the only thing at risk and it is already
    /// best-effort.
    fn reset_turn_model(&self) {
        if let Ok(mut resolved) = self.claude_turn_model.lock() {
            *resolved = None;
        }
    }

    /// The model this run is attributed with.
    ///
    /// The TURN-resolved id wins over the `session/new` one. `session/new` reports what the picker is
    /// set to, which on a Claude seat is a selector a human chose (`sonnet`, `opus[1m]`, or
    /// `default`); the init frame reports the decorated id the turn actually ran on. When no turn
    /// resolved one, every non-Claude harness reports byte-identically to what the driver reported
    /// before (#896 follow-on), and a Claude run whose frame never arrived reports absence.
    fn resolved_model(&self) -> Option<String> {
        if let Ok(resolved) = self.claude_turn_model.lock()
            && let Some(model) = resolved.clone()
        {
            return Some(model);
        }
        // EVERY value of the Claude picker is a SELECTOR, so none of them identifies a model — not
        // just `default`. Each resolves to whatever that name currently means for the signed-in
        // account, so two sellers reporting `sonnet` can be running different models and a buyer
        // filtering on it is awarding against ad copy. That is the same false advertisement this path
        // exists to remove, arriving by a second door.
        //
        // No shape test separates the rows, and the committed capture is why. Its picker offers
        // `opus[1m]`, while the init frame for that same model reads `claude-opus-5[1m]` — one model,
        // two strings. So passing the rows that LOOK like ids would publish `opus[1m]` to a buyer
        // filtering on `claude-opus-5[1m]` and miss. The rows that look concrete are selectors too.
        //
        // Hence: on claude-agent-acp the init frame is the ONLY identity and its absence is absence.
        // There is no allow-list or deny-list here to fall behind when the picker gains a row.
        //
        // Every other harness keeps the #896 behaviour byte-for-byte: `session_model` is still the
        // `session/new` value, and codex-acp's legacy `models.currentModelId` is untouched.
        if self.is_claude_adapter {
            return None;
        }
        self.session_model.clone()
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
        // Which adapter answered, by its own account. Everything adapter-private this driver does is
        // gated on this and on nothing else — not on the program name we spawned, which an operator
        // may alias, wrap or rename in `[agents]`.
        self.is_claude_adapter = is_claude_agent_acp(&result);
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
            session_new_params(cfg, self.is_claude_adapter)?,
        )?;
        let result = self.wait_response(id).await?;
        // Capture the harness-resolved model before the response is reduced to a session id — this is
        // the only ACP surface that carries it (#455), in either of the two shapes
        // `session_model_from_result` reads (#896). Absent-stays-absent: a harness that reports no
        // model leaves this `None`, and nothing downstream fabricates one.
        self.session_model = session_model_from_result(&result);
        self.reset_turn_model();
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
        merge_session_model(self.last_usage.clone(), self.resolved_model().as_deref())
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
    ///
    /// Linux-only (#662): `process_group_of` reads `/proc/<pid>/stat`, which darwin lacks. A
    /// portable `libc::getpgid` probe was not worth it — `libc` is wallet-feature-only and this
    /// module builds under `default = []`; pulling it in just so a live control runs on macOS
    /// would break the default build. The string-parse tests above already cover the parser on
    /// every unix; this gate is the narrowest fix for the darwin phantom red.
    #[cfg(target_os = "linux")]
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

/// One decoded stdout line: capture what only the driver's own state can hold, then route it.
///
/// This exists as a function rather than as the reader thread's body so the capture is TESTABLE. The
/// thread's body is reachable only by spawning a real child process, and a seam that no test can
/// drive is how a model read stays green while publishing the wrong value — which is exactly the
/// defect this path was written to fix (#896 follow-on).
///
/// The adapter-private read happens HERE and not inside [`route_wire_message`], which is the generic
/// ACP path and stays free of any one adapter's extension. Routing is unchanged either way: the
/// notification still surfaces as an `Ext` update, exactly as any unknown method does.
///
/// A poisoned lock is not worth failing a run over — the model stays unresolved and the run reports
/// what it would have reported without this path.
fn handle_wire_message(
    value: &Value,
    claude_turn_model: &Mutex<Option<String>>,
    response_tx: &mpsc::UnboundedSender<RpcResponse>,
    update_tx: &mpsc::UnboundedSender<SessionUpdate>,
    permission_policy: &PermissionOutcome,
    respond_permission: &mut impl FnMut(Value, Value),
) {
    if let Some(model) = claude_sdk_init_model(value)
        && let Ok(mut resolved) = claude_turn_model.lock()
    {
        *resolved = Some(model);
    }
    route_wire_message(
        value,
        response_tx,
        update_tx,
        permission_policy,
        respond_permission,
    );
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

/// The ACP Session Config Options `category` this crate treats as the session's model selector.
///
/// This is a Maxplayer POLICY, not an ACP wire contract, and the distinction is load-bearing. ACP
/// defines `category` as optional semantic metadata that MUST NOT be required for correctness, and
/// requires clients to handle a missing or unknown category gracefully. So an option carrying no
/// category is VALID ACP — not malformed — and an adapter is entitled to publish its model selector
/// without one.
///
/// The policy: we read a model only from an option that declares `category == "model"`, and where
/// the category is absent we DECLINE TO INFER model semantics rather than guess from `id` or
/// position. Returning no model is the graceful handling ACP asks for, because in this crate an
/// absent `harness_model` is honest while a wrong one is a false advertisement a buyer can be
/// awarded against.
///
/// Deliberately not keyed on `id == "model"`: ACP documents `id` as the agent's own identifier and
/// does not standardise it — the spec's own example names the model selector `models`, not `model` —
/// so an id match would bind this to one adapter's naming while also matching a selector that is
/// semantically something else. One of the two, never both loosely.
const MODEL_CONFIG_CATEGORY: &str = "model";

/// The one model-picker value that is NOT a model id.
///
/// Captured on the wire from claude-agent-acp 0.70.0: the model selector's `currentValue` is
/// `default`, and `default` is itself `options[0]`, named "Default (recommended)". It names a
/// PREFERENCE — whatever the signed-in account resolves to — so it identifies nothing: the same
/// string is a different model on a different account, and a buyer that filters on it is awarding
/// against ad copy. Every Claude seat in the field advertised it as its model (#896 follow-on).
///
/// Refused as an EXACT string and only in the model category. `sonnet`, `haiku` and `opus[1m]` are
/// aliases too, and they all still pass through verbatim — narrowing those is a separate decision
/// with a separate blast radius. It is not refused in the legacy `models.currentModelId` shape
/// either: that shape is codex-acp's, it has never carried this string, and widening the refusal to
/// reach it would change a harness this defect does not touch.
///
/// Refusing it is the TRIGGER to resolve, not the answer. [`AcpDriver::resolved_model`] reads the
/// concrete id off the turn; absence is only what is left when that fails.
const CLAUDE_PICKER_DEFAULT: &str = "default";

/// The harness-resolved model id from a `session/new` result, as the resolved identity INCLUDING any
/// reasoning-effort suffix (e.g. `gpt-5.6-terra[medium]`).
///
/// Two wire shapes carry it, tried in this order:
///
/// 1. `models.currentModelId` — a top-level `models` object (codex-acp, and any adapter still
///    returning that shape).
/// 2. The FIRST `configOptions` entry whose `category` is [`MODEL_CONFIG_CATEGORY`], with the
///    resolved id in that entry's `currentValue`. Current Claude ACP adapters return only
///    `sessionId`, `modes` and `configOptions`, publishing the model here (#896). Only that first
///    entry is consulted, and an unusable value there yields `None` rather than promoting a later
///    model-category entry — see [`config_option_session_model`].
///
/// Legacy is preferred when it carries a usable value: an adapter still returning the top-level
/// object has not changed what it means, so reading it first keeps every already-working harness
/// byte-identical. The test `session_model_prefers_legacy_over_config_option` fails if that order
/// flips.
///
/// `None` when neither shape yields a non-blank string. Absence stays absence — a missing, blank,
/// non-string or malformed value is never repaired into a fabricated model, and no value is borrowed
/// from a neighbouring option: the sibling `thought_level` selector carries values like `medium`,
/// which is an effort level and not a model.
fn session_model_from_result(result: &Value) -> Option<String> {
    legacy_session_model(result).or_else(|| config_option_session_model(result))
}

/// Shape 1: the legacy top-level `models.currentModelId` (camelCase on the wire).
fn legacy_session_model(result: &Value) -> Option<String> {
    non_blank_model(result.get("models")?.get("currentModelId")?)
}

/// Shape 2: the FIRST model-category `configOptions` entry, value in its `currentValue`.
///
/// Takes the first entry declaring the model category, validates ONLY that entry, and FAILS CLOSED
/// to `None` when its value is unusable — it does not scan past it for a later entry that happens to
/// carry a usable string.
///
/// That choice is the conservative one and it is the point of this function. A later model-category
/// entry is a DIFFERENT selector; promoting its value because the first one was blank would advertise
/// a model the session's primary selector does not report, which is the same
/// read-an-adjacent-value defect as reading the neighbouring `thought_level` option, just one level
/// in. An absent `harness_model` is honest; a wrong one is a false advertisement a buyer can be
/// awarded against. It also matches ACP, which asks clients to use `configOptions` order as the
/// primary way to establish priority and resolve ties — but the conservatism above is the reason,
/// and it would stand on its own if that guidance changed.
///
/// A `configOptions` that is not an array, entries that are not objects, and entries of any other
/// category never yield a value.
fn config_option_session_model(result: &Value) -> Option<String> {
    let first_model_option = result
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|option| {
            option.get("category").and_then(Value::as_str) == Some(MODEL_CONFIG_CATEGORY)
        })?;
    concrete_model(first_model_option.get("currentValue")?)
}

/// A model id that also is not the reserved picker alias — see [`CLAUDE_PICKER_DEFAULT`].
fn concrete_model(value: &Value) -> Option<String> {
    non_blank_model(value).filter(|model| model != CLAUDE_PICKER_DEFAULT)
}

/// A model id is a non-blank JSON string, taken verbatim.
///
/// Anything else — a blank or whitespace-only string, a number, bool, null, array or object — is not
/// a model id and yields `None` rather than a coerced, trimmed or fabricated value. This also
/// applies to the legacy shape: a present-but-blank `currentModelId` is not a model, so it falls
/// through to the config-option shape rather than advertising an empty string.
fn non_blank_model(value: &Value) -> Option<String> {
    let model = value.as_str()?;
    (!model.trim().is_empty()).then(|| model.to_owned())
}

/// How claude-agent-acp names ITSELF in its `initialize` result's `agentInfo.name`.
const CLAUDE_ADAPTER_NAME: &str = "@agentclientprotocol/claude-agent-acp";

/// The adapter-private notification carrying raw SDK frames. The `_`-prefixed namespace is ACP's
/// marker for exactly this: a method no other agent implements.
const CLAUDE_SDK_MESSAGE_METHOD: &str = "_claude/sdkMessage";

/// Did claude-agent-acp itself answer `initialize`?
///
/// Read from the adapter's own `agentInfo.name` rather than from the program we spawned: an operator
/// names the binary in `[agents]` and may alias, wrap or rename it, so the spawned path says what we
/// asked for and this says what answered. Anything else — including a fork under another name — is
/// not this adapter and takes the generic path, which fails closed to an absent model.
fn is_claude_agent_acp(initialize_result: &Value) -> bool {
    initialize_result
        .get("agentInfo")
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        == Some(CLAUDE_ADAPTER_NAME)
}

/// `session/new` params, plus the raw-SDK opt-in when — and only when — claude-agent-acp answered
/// `initialize`.
///
/// COUPLING, stated deliberately: `_meta.claudeCode.emitRawSDKMessages` is claude-agent-acp's own
/// extension and is no part of ACP. It is here because the concrete model id is reachable NOWHERE
/// else. `session/new` publishes only the picker alias, there is no `session/set_model`, and
/// `session/update` usage carries no model at all — measured against 0.70.0, the version
/// `tools/fold:108` gives every Claude seat. This is the only surface that names what actually ran.
///
/// The FILTER is the cost control, and the adapter honours it: `dist/acp-agent.js:5183` defaults the
/// flag to `false`, and `:1829` gates every frame on `shouldEmitRawMessage` (`:5241`) before building
/// a notification. Measured on 0.70.0, one prompt turn: unfiltered `true` emits 15 notifications,
/// this filter emits exactly 1 (2472 bytes), and without the opt-in the gate is false and nothing is
/// sent. Any other harness gets byte-identical params to the ones it got before.
fn session_new_params(cfg: SessionConfig, is_claude_adapter: bool) -> Result<Value, DriverError> {
    let mut params = serde_json::to_value(cfg)
        .map_err(|error| DriverError::Other(format!("failed to encode session params: {error}")))?;
    if is_claude_adapter && let Some(object) = params.as_object_mut() {
        object.insert(
            "_meta".into(),
            json!({
                "claudeCode": {
                    "emitRawSDKMessages": [{"type": "system", "subtype": "init"}]
                }
            }),
        );
    }
    Ok(params)
}

/// The concrete model id off a `_claude/sdkMessage` `system`/`init` notification, or `None` for every
/// other wire message.
///
/// `system`/`init` is the ONLY frame read for identity, and the choice is not arbitrary — the same
/// turn names the model on three surfaces and they disagree (measured, 0.70.0):
///
/// - `system`/`init` — `claude-opus-5[1m]`, decorated, and it arrives BEFORE any agent output.
/// - `assistant` and `stream_event` — `claude-opus-5`, the same model missing its context decoration.
/// - `result.modelUsage` KEYS — carry `claude-opus-5[1m]` and `claude-haiku-4-5-20251001` together.
///   The second is a side model the turn also billed. `modelUsage` is a billing surface, not an
///   identity one, and reading a key off it would advertise whichever model happened to be enumerated
///   first.
///
/// The alias refusal applies here too: a frame that somehow named `default` has still named no model.
fn claude_sdk_init_model(notification: &Value) -> Option<String> {
    if notification.get("method").and_then(Value::as_str)? != CLAUDE_SDK_MESSAGE_METHOD {
        return None;
    }
    let message = notification.get("params")?.get("message")?;
    if message.get("type").and_then(Value::as_str)? != "system"
        || message.get("subtype").and_then(Value::as_str)? != "init"
    {
        return None;
    }
    concrete_model(message.get("model")?)
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

    /// A `session/new` result CAPTURED from `claude-agent-acp` v0.70.0, with only the model
    /// option's `currentValue` parameterised.
    ///
    /// Captured, not read. The fixture this replaces was ground-truthed by READING the adapter's
    /// source, and so it invented `opus[1m]` — a string the real wire never sends — and had
    /// no `default` row at all. Source-reading cannot falsify an invented fixture, which is why
    /// every test here was green while a Claude seat advertised `default` to the market (#896).
    /// Captured from `$HOME/forge/npm/bin/claude-agent-acp`, which is the binary `tools/fold:108`
    /// gives every Claude seat — NOT whatever `claude-agent-acp` resolves to on a PATH, which on this
    /// host is a different install at a different version. A capture taken against a binary the fleet
    /// does not run is an invented fixture with a timestamp. `initialize.agentInfo.version` in the
    /// capture reports 0.70.0.
    ///
    /// Verbatim from the wire: `sessionId`, every `configOptions` entry's `id`/`name`/`description`/
    /// `category`/`type`/`currentValue`, and the model entry's FIVE options in wire order. Elided to
    /// `[]`: the option lists of the non-model selectors and `modes.availableModes`, none of which
    /// any model read touches.
    ///
    /// Three properties of the real wire are load-bearing here and were all absent before:
    ///
    /// 1. `default` is a real, selectable option — `options[0]`, named "Default (recommended)". It
    ///    is the picker's own alias for "whatever this account resolves to", NOT a model id.
    /// 2. The `mode` and `thought_level` siblings ALSO sit at `default`, so a matcher loose enough
    ///    to read a neighbouring option cannot be caught by a differing value — only by category.
    /// 3. The `default` row's `description` all but names what it resolves to — v0.70.0 puts the
    ///    literal string "Opus (1M context)" there, which is `opus[1m]`'s `name`. Do not read it.
    ///    v0.64.0 of the same adapter instead gave `default` a byte-identical COPY of `opus[1m]`'s
    ///    description. One display field, two meanings, two adjacent releases — it is ad copy for a
    ///    human, and it is never a model read's input.
    fn claude_session_new_result(current_value: Value) -> Value {
        json!({
            "sessionId": "cac8fd7e-bcb9-44c6-8c44-68a7c7c6343b",
            "modes": {
                "currentModeId": "default",
                "availableModes": []
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "description": "Session permission mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "default",
                    "options": []
                },
                {
                    "id": "model",
                    "name": "Model",
                    "description": "AI model to use",
                    "category": "model",
                    "type": "select",
                    "currentValue": current_value,
                    "options": [
                        {
                            "value": "default",
                            "name": "Default (recommended)",
                            "description": "Opus (1M context)"
                        },
                        {
                            "value": "opus[1m]",
                            "name": "Opus (1M context)",
                            "description": "Opus 5 with 1M context \u{b7} Best for everyday, complex tasks"
                        },
                        {
                            "value": "claude-fable-5[1m]",
                            "name": "Fable",
                            "description": "Fable 5 \u{b7} Most capable for your hardest and longest-running tasks"
                        },
                        {
                            "value": "sonnet",
                            "name": "Sonnet",
                            "description": "Sonnet 5 \u{b7} Efficient for routine tasks"
                        },
                        {
                            "value": "haiku",
                            "name": "Haiku",
                            "description": "Haiku 4.5 \u{b7} Fastest for quick answers"
                        }
                    ]
                },
                {
                    "id": "effort",
                    "name": "Effort",
                    "description": "Available effort levels for this model",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "default",
                    "options": []
                },
                {
                    "id": "fast",
                    "name": "Fast mode",
                    "description": "Faster responses on supported models",
                    "category": "model_config",
                    "type": "select",
                    "currentValue": "off",
                    "options": []
                }
            ]
        })
    }

    #[test]
    fn the_claude_picker_default_alias_is_not_a_model() {
        // THE defect (#896 follow-on): the captured wire sits at `default`, and every Claude seat in
        // the field advertised that string as its model. `default` names a PREFERENCE — "whatever
        // this account resolves to" — and resolves to a different id on a different account, so it
        // cannot identify what served a job. It is the trigger to resolve, never a value to publish.
        assert_eq!(
            session_model_from_result(&claude_session_new_result(json!("default"))),
            None
        );
    }

    #[test]
    fn only_the_exact_default_alias_is_refused() {
        // The SESSION-START read stays one exact string in the model category: every other picker
        // value passes through verbatim, unparsed and unnormalised, so `session_model` keeps
        // reporting what the field said.
        //
        // The narrowing lives one level up, at the attribution chokepoint — see
        // `AcpDriver::resolved_model` and `no_claude_picker_value_is_published_as_a_model`. Splitting
        // it that way keeps this read honest about the wire while nothing publishes a selector as a
        // model.
        for value in [
            "opus[1m]",
            "claude-fable-5[1m]",
            "sonnet",
            "haiku",
            "Default",
            "default-x",
            "defaults",
        ] {
            assert_eq!(
                session_model_from_result(&claude_session_new_result(json!(value))).as_deref(),
                Some(value),
                "{value} is not the reserved alias and must pass through verbatim"
            );
        }
    }

    #[test]
    fn session_model_read_from_a_claude_session_config_option() {
        // #896: the Claude shape carries no legacy object at all — the resolved model is the
        // model-category config option's `currentValue`.
        let result = claude_session_new_result(json!("opus[1m]"));
        assert!(
            result.get("models").is_none(),
            "fixture must not carry the legacy shape, or it proves nothing about the new one"
        );
        assert_eq!(
            session_model_from_result(&result).as_deref(),
            Some("opus[1m]")
        );
    }

    #[test]
    fn session_model_from_a_config_option_is_verbatim() {
        // The value is the resolved identity as the harness stated it, suffix and all — not parsed,
        // trimmed or normalised.
        let result = claude_session_new_result(json!("claude-fable-5[1m]"));
        assert_eq!(
            session_model_from_result(&result).as_deref(),
            Some("claude-fable-5[1m]")
        );
    }

    #[test]
    fn session_model_never_reads_a_neighbouring_config_option() {
        // Remove ONLY the model entry. All three non-model selectors survive with perfectly usable
        // string values — and the answer must be None, not a neighbour's value.
        //
        // `thought_level` is moved off `default` to `medium` first, which is a value its own captured
        // options list offers. On the captured wire every selector sits at `default`, so once the
        // model read refuses that string a loose matcher reading a neighbour would return None too —
        // and this test would pass for the wrong reason, proving category discipline it never
        // exercised. One neighbour must carry a value the read would otherwise HAPPILY return.
        let mut result = claude_session_new_result(json!("opus[1m]"));
        let options = result["configOptions"].as_array_mut().expect("array");
        for option in options.iter_mut() {
            if option.get("category").and_then(Value::as_str) == Some("thought_level") {
                option["currentValue"] = json!("medium");
            }
        }
        options.retain(|option| {
            option.get("category").and_then(Value::as_str) != Some(MODEL_CONFIG_CATEGORY)
        });
        assert_eq!(
            options.len(),
            3,
            "every non-model selector must remain, or this proves nothing"
        );
        assert!(
            options
                .iter()
                .any(|option| option.get("currentValue") == Some(&json!("medium"))),
            "a neighbour must carry a value the model read would return if it looked"
        );
        assert_eq!(session_model_from_result(&result), None);
    }

    #[test]
    fn session_model_prefers_legacy_over_config_option() {
        // Both shapes present, both usable → legacy wins. Named in `session_model_from_result`'s
        // docs as the test that fails if that precedence flips.
        let mut result = claude_session_new_result(json!("config-option-model"));
        result["models"] = json!({"currentModelId": "legacy-model"});
        assert_eq!(
            session_model_from_result(&result).as_deref(),
            Some("legacy-model"),
            "legacy models.currentModelId must win when both shapes carry a usable value"
        );
    }

    #[test]
    fn session_model_falls_through_when_legacy_is_present_but_unusable() {
        // A legacy key that is not a usable model id must not shadow a good config option, and must
        // not be advertised as an empty or coerced model either.
        for unusable in [json!(""), json!("   "), json!(7), json!(null), json!({})] {
            let mut result = claude_session_new_result(json!("opus[1m]"));
            result["models"] = json!({"currentModelId": unusable.clone()});
            assert_eq!(
                session_model_from_result(&result).as_deref(),
                Some("opus[1m]"),
                "unusable legacy value {unusable} must fall through to the config option"
            );
        }
    }

    #[test]
    fn session_model_rejects_hostile_config_option_values_without_fabricating() {
        // Each of these means "no model". None may become a coerced, stringified or invented one —
        // and with the `thought_level` sibling still in the fixture, none may borrow its value.
        for hostile in [
            json!(""),
            json!("   "),
            json!("\t\n"),
            json!(0),
            json!(4.8),
            json!(true),
            json!(null),
            json!(["opus[1m]"]),
            json!({"currentModelId": "opus[1m]"}),
        ] {
            let result = claude_session_new_result(hostile.clone());
            assert_eq!(
                session_model_from_result(&result),
                None,
                "hostile currentValue {hostile} must yield None"
            );
        }
    }

    #[test]
    fn session_model_rejects_malformed_config_options_without_fabricating() {
        for malformed in [
            // `configOptions` itself the wrong type, or empty.
            json!({"sessionId": "abc", "configOptions": "model"}),
            json!({"sessionId": "abc", "configOptions": {}}),
            json!({"sessionId": "abc", "configOptions": null}),
            json!({"sessionId": "abc", "configOptions": []}),
            // Entries that are not objects.
            json!({"sessionId": "abc", "configOptions": ["model", 7, null]}),
            // Right category, no value key at all.
            json!({"sessionId": "abc", "configOptions": [{"id": "model", "category": "model"}]}),
            // A usable value under the WRONG category — an `id` of "model" is not a match, because
            // this reads the category and never the id.
            json!({"sessionId": "abc", "configOptions": [
                {"id": "model", "category": "mode", "currentValue": "opus[1m]"}
            ]}),
            // A non-string category is malformed rather than merely absent: the field is present and
            // the wrong type. Absent and unknown categories are VALID ACP and are covered by
            // `session_model_declines_to_infer_when_the_optional_category_is_absent`, deliberately
            // not filed here.
            json!({"sessionId": "abc", "configOptions": [
                {"id": "model", "category": 7, "currentValue": "opus[1m]"}
            ]}),
        ] {
            assert_eq!(
                session_model_from_result(&malformed),
                None,
                "malformed input {malformed} must yield None"
            );
        }
    }

    #[test]
    fn session_model_fails_closed_when_the_first_model_option_is_unusable() {
        // ORDER IS THE CONTRACT. The first model-category entry is the session's model selector; a
        // later one is a DIFFERENT selector. When the first carries no usable value the answer is
        // None — we do NOT scan ahead and advertise the second entry's model, because that would
        // report a model the primary selector does not, which is the same read-an-adjacent-value
        // defect as reading the neighbouring `thought_level` option one level in.
        //
        // A usable value sits behind the blank one deliberately: skip-ahead behaviour returns
        // Some("opus[1m]") here, so this test goes red the moment the fail-closed rule
        // regresses.
        let result = json!({
            "sessionId": "abc",
            "configOptions": [
                {"id": "model", "category": "model", "currentValue": ""},
                {"id": "model-fallback", "category": "model", "currentValue": "opus[1m]"}
            ]
        });
        assert_eq!(session_model_from_result(&result), None);
    }

    #[test]
    fn session_model_reads_the_first_model_option_not_a_later_one() {
        // The positive direction of the same rule: with BOTH entries usable, the first wins. Without
        // this, a fail-closed implementation that read the LAST entry would pass the test above.
        let result = json!({
            "sessionId": "abc",
            "configOptions": [
                {"id": "model", "category": "model", "currentValue": "first-selector-model"},
                {"id": "model-fallback", "category": "model", "currentValue": "second-selector-model"}
            ]
        });
        assert_eq!(
            session_model_from_result(&result).as_deref(),
            Some("first-selector-model")
        );
    }

    #[test]
    fn session_model_declines_to_infer_when_the_optional_category_is_absent() {
        // NOT a malformed-input test. ACP makes `category` optional semantic metadata that MUST NOT
        // be required for correctness, and requires clients to handle its absence gracefully — so an
        // option with no category is VALID ACP. This pins a Maxplayer POLICY choice: we decline to
        // infer model semantics from `id` or position, and returning no model IS the graceful
        // handling, because an absent harness_model is honest where a guessed one is a false
        // advertisement.
        let no_category = json!({
            "sessionId": "abc",
            "configOptions": [{"id": "model", "currentValue": "opus[1m]"}]
        });
        assert_eq!(session_model_from_result(&no_category), None);

        // Same for a category we do not recognise: handled gracefully, never guessed at.
        let unknown_category = json!({
            "sessionId": "abc",
            "configOptions": [
                {"id": "model", "category": "_vendor_model", "currentValue": "opus[1m]"}
            ]
        });
        assert_eq!(session_model_from_result(&unknown_category), None);
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

    /// The `initialize` result CAPTURED from `$HOME/forge/npm/bin/claude-agent-acp` 0.70.0 —
    /// `tools/fold:108`'s binary. Trimmed to `agentInfo`, the only part any gate here reads.
    fn claude_initialize_result() -> Value {
        json!({
            "protocolVersion": 1,
            "agentInfo": {
                "name": "@agentclientprotocol/claude-agent-acp",
                "title": "Claude Agent",
                "version": "0.70.0"
            },
            "authMethods": []
        })
    }

    /// A `_claude/sdkMessage` notification CAPTURED from the same binary, one prompt turn, with the
    /// opt-in filtered to `system`/`init`. The message's list-valued fields (`tools`, `skills`,
    /// `slash_commands`, `agents`, `mcp_servers`, `plugins`) are elided to `[]`; no model read looks
    /// at them. Every scalar is verbatim.
    ///
    /// `permissionMode: "default"` is kept and is load-bearing: the refused picker alias sits in the
    /// SAME object as the concrete model, so a read loose enough to take the wrong field would
    /// publish `default` all over again.
    fn claude_sdk_init_notification(model: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "_claude/sdkMessage",
            "params": {
                "sessionId": "cac8fd7e-bcb9-44c6-8c44-68a7c7c6343b",
                "message": {
                    "type": "system",
                    "subtype": "init",
                    "model": model,
                    "permissionMode": "default",
                    "apiKeySource": "none",
                    "output_style": "Concise",
                    "fast_mode_state": "off",
                    "claude_code_version": "2.1.220",
                    "session_id": "cac8fd7e-bcb9-44c6-8c44-68a7c7c6343b",
                    "uuid": "8b676ce0-64dd-41b8-a8d3-c298e7dfd3ea",
                    "tools": [],
                    "skills": [],
                    "slash_commands": [],
                    "agents": [],
                    "mcp_servers": [],
                    "plugins": []
                }
            }
        })
    }

    #[test]
    fn the_init_frame_names_the_model_the_turn_actually_ran_on() {
        // The whole point of the resolution path: the picker said `default`, the turn ran on this.
        assert_eq!(
            claude_sdk_init_model(&claude_sdk_init_notification(json!("claude-opus-5[1m]")))
                .as_deref(),
            Some("claude-opus-5[1m]")
        );
    }

    #[test]
    fn no_other_frame_or_method_is_read_for_identity() {
        // Measured on 0.70.0: the same turn names the model on three surfaces and they disagree.
        // Only `system`/`init` is an identity surface, so everything else here must read as nothing —
        // including the two frames that carry a perfectly plausible model string.
        let mut assistant = claude_sdk_init_notification(json!("claude-opus-5"));
        assistant["params"]["message"]["type"] = json!("assistant");
        assistant["params"]["message"]["subtype"] = Value::Null;
        assert_eq!(claude_sdk_init_model(&assistant), None);

        let mut stream_event = claude_sdk_init_notification(json!("claude-opus-5"));
        stream_event["params"]["message"]["type"] = json!("stream_event");
        assert_eq!(claude_sdk_init_model(&stream_event), None);

        // `result.modelUsage` keys carried `claude-opus-5[1m]` AND `claude-haiku-4-5-20251001` on the
        // same turn — a billing surface, never an identity one.
        let mut result = claude_sdk_init_notification(json!("claude-opus-5[1m]"));
        result["params"]["message"]["type"] = json!("result");
        result["params"]["message"]["subtype"] = json!("success");
        result["params"]["message"]["modelUsage"] =
            json!({"claude-opus-5[1m]": {}, "claude-haiku-4-5-20251001": {}});
        assert_eq!(claude_sdk_init_model(&result), None);

        // A different method with a byte-identical payload is still not this notification.
        let mut other_method = claude_sdk_init_notification(json!("claude-opus-5[1m]"));
        other_method["method"] = json!("session/update");
        assert_eq!(claude_sdk_init_model(&other_method), None);

        // And an init frame that named the alias has still named no model.
        assert_eq!(
            claude_sdk_init_model(&claude_sdk_init_notification(json!("default"))),
            None
        );
        for unusable in [json!(""), json!("  "), json!(7), json!(null), json!({})] {
            assert_eq!(
                claude_sdk_init_model(&claude_sdk_init_notification(unusable.clone())),
                None,
                "unusable init model {unusable} must yield None"
            );
        }
    }

    #[test]
    fn the_raw_sdk_opt_in_is_asked_of_claude_and_of_nothing_else() {
        // Rider: the adapter-private extension stays OFF the generic ACP path. A harness that is not
        // claude-agent-acp must receive the params it received before this existed, byte for byte.
        let cfg = || SessionConfig {
            cwd: "/tmp/maxplayer".into(),
            mcp_servers: Vec::new(),
            env: Vec::new(),
        };
        let generic = session_new_params(cfg(), false).expect("encode");
        assert_eq!(
            generic,
            serde_json::to_value(cfg()).expect("encode"),
            "a non-Claude harness must see no _meta at all"
        );

        assert_eq!(
            session_new_params(cfg(), true).expect("encode"),
            json!({
                "cwd": "/tmp/maxplayer",
                "mcpServers": [],
                "env": [],
                "_meta": {
                    "claudeCode": {
                        "emitRawSDKMessages": [{"type": "system", "subtype": "init"}]
                    }
                }
            }),
            "the opt-in must be FILTERED — an unfiltered `true` emitted 15 notifications per turn"
        );
    }

    #[test]
    fn the_opt_in_follows_the_adapter_that_answered_not_the_binary_we_spawned() {
        assert!(is_claude_agent_acp(&claude_initialize_result()));
        for other in [
            json!({"protocolVersion": 1}),
            json!({"agentInfo": {"name": "codex-acp", "version": "0.1.0"}}),
            json!({"agentInfo": {"name": "@agentclientprotocol/claude-agent-acp-fork"}}),
            json!({"agentInfo": {"name": 7}}),
            json!({"agentInfo": "@agentclientprotocol/claude-agent-acp"}),
        ] {
            assert!(
                !is_claude_agent_acp(&other),
                "{other} is not claude-agent-acp naming itself"
            );
        }
    }

    #[test]
    fn the_reader_seam_captures_the_model_and_still_routes_the_notification() {
        // The wiring, not the parser: drive the exact function the stdout reader calls, with the
        // captured notification, and assert BOTH halves. A capture that swallowed the message would
        // change what the rest of the driver sees; a route that skipped the capture would leave the
        // run attributed to nothing.
        let (response_tx, _response_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let claude_turn_model = Mutex::new(None);
        let notification = claude_sdk_init_notification(json!("claude-opus-5[1m]"));

        handle_wire_message(
            &notification,
            &claude_turn_model,
            &response_tx,
            &update_tx,
            &PermissionOutcome::Allow,
            &mut |_, _| panic!("an init frame is not a permission request"),
        );

        assert_eq!(
            claude_turn_model.lock().expect("lock").as_deref(),
            Some("claude-opus-5[1m]")
        );
        assert_eq!(
            update_rx.try_recv().expect("ext update"),
            SessionUpdate::Ext(ExtMethod {
                method: CLAUDE_SDK_MESSAGE_METHOD.into(),
                params: notification["params"].clone(),
            }),
            "the notification must still reach the stream unmodified"
        );

        // A message that is not the init frame leaves the slot exactly as it was.
        let mut assistant = claude_sdk_init_notification(json!("claude-opus-5"));
        assistant["params"]["message"]["type"] = json!("assistant");
        handle_wire_message(
            &assistant,
            &claude_turn_model,
            &response_tx,
            &update_tx,
            &PermissionOutcome::Allow,
            &mut |_, _| panic!("not a permission request"),
        );
        assert_eq!(
            claude_turn_model.lock().expect("lock").as_deref(),
            Some("claude-opus-5[1m]"),
            "a bare id off a later frame must not overwrite the decorated one"
        );
    }

    #[test]
    fn a_claude_run_is_attributed_to_the_concrete_model_not_the_picker_alias() {
        // End to end through the field the reader thread writes, so this fails if the capture is
        // wired to a slot `usage()` does not read.
        use crate::driver::Driver;
        let mut driver = AcpDriver::new(
            AgentCommand::new("claude-agent-acp".into(), Vec::new()),
            PermissionOutcome::Allow,
            Duration::from_secs(1),
        );
        // What `session/new` gave us on the captured wire: nothing, because the alias is refused.
        driver.session_model =
            session_model_from_result(&claude_session_new_result(json!("default")));
        assert_eq!(driver.session_model, None);
        assert_eq!(driver.usage().and_then(|usage| usage.model), None);

        // Then the turn's init frame lands.
        *driver.claude_turn_model.lock().expect("lock") =
            claude_sdk_init_model(&claude_sdk_init_notification(json!("claude-opus-5[1m]")));
        assert_eq!(
            driver.usage().and_then(|usage| usage.model).as_deref(),
            Some("claude-opus-5[1m]"),
            "the run must be attributed to what actually served it"
        );
    }

    #[test]
    fn the_turn_resolved_model_outranks_the_picker_and_absence_outranks_neither() {
        // A human who pins the picker still gets the decorated id the turn ran on — the alias names a
        // family, the init frame names the build. And with no frame at all the driver reports exactly
        // what it reported before this path existed, which is what keeps codex byte-identical.
        let mut driver = AcpDriver::new(
            AgentCommand::new("codex".into(), Vec::new()),
            PermissionOutcome::Allow,
            Duration::from_secs(1),
        );
        driver.session_model = Some("gpt-5.6-terra[medium]".into());
        assert_eq!(
            driver.resolved_model().as_deref(),
            Some("gpt-5.6-terra[medium]")
        );

        driver.session_model = Some("sonnet".into());
        *driver.claude_turn_model.lock().expect("lock") = Some("claude-sonnet-5[1m]".into());
        assert_eq!(
            driver.resolved_model().as_deref(),
            Some("claude-sonnet-5[1m]")
        );

        driver.session_model = None;
        *driver.claude_turn_model.lock().expect("lock") = None;
        assert_eq!(driver.resolved_model(), None);
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

    // ---------------------------------------------------------------------------------------------
    // THE CAPTURED WIRE (#916 review, C3/C16).
    //
    // Every test above builds its input with `json!`, so all of them pass if the scalars were
    // invented. That is not a hypothetical: the #896 fixture was ground-truthed by READING this
    // adapter's source, and it invented a `currentValue` the adapter never sends. Source-reading
    // cannot falsify an invented scalar.
    //
    // The tests below take their load-bearing input from a COMMITTED CAPTURE instead. `CAPTURE_SHA256`
    // stops the capture being edited into agreement with the code;
    // `the_committed_capture_still_matches_the_live_adapter` is what proves the adapter ever sent it.
    // ---------------------------------------------------------------------------------------------

    /// The committed capture. Provenance, redaction and the reproduction recipe live in
    /// `tests/fixtures/claude-agent-acp-0.70.0/README.md`.
    const CAPTURE_JSON: &str =
        include_str!("../../tests/fixtures/claude-agent-acp-0.70.0/session-new-and-init-frame.json");

    /// sha256 of [`CAPTURE_JSON`]. A DRIFT guard and not an authenticity proof — it catches the file
    /// being quietly changed, and says nothing about whether the adapter ever sent those bytes.
    const CAPTURE_SHA256: &str =
        "4dc4c23645f0ddd6b05f65ac34b1e891e36b53046b7b79a91a919535f194a1ec";

    /// The binary the capture came from, by ABSOLUTE PATH. A bare `claude-agent-acp` names the
    /// reader's `PATH`, not a version: the capturing host carries a SECOND install of this same
    /// package at `/opt/homebrew/bin/claude-agent-acp`, at 0.64.0.
    const CAPTURE_BIN: &str = "/Users/forge/forge/npm/bin/claude-agent-acp";
    const CAPTURE_ADAPTER_VERSION: &str = "0.70.0";

    /// Keys deleted from the capture before committing it: the capturing workstation's environment,
    /// not adapter wire. Redaction is DELETION of whole keys and never alteration of a value, because
    /// a deletion cannot fabricate a scalar. The live test applies this same list before comparing,
    /// which is what keeps the redaction part of the reproducible procedure rather than a one-off
    /// edit.
    const REDACTED_KEYS: &[&str] = &[
        "skills",
        "slash_commands",
        "terminal_slash_commands",
        "memory_paths",
        "messaging_socket_path",
        "cwd",
        "agents",
        "tools",
        "stderr",
    ];

    /// A driver with no child process, for the pure-logic reads. The runtime id is the only thing
    /// the command contributes here.
    fn test_driver() -> AcpDriver {
        AcpDriver::new(
            AgentCommand::new("claude-agent-acp".into(), Vec::new()),
            PermissionOutcome::Allow,
            Duration::from_secs(1),
        )
    }

    /// The keys the model read actually depends on.
    ///
    /// [`REDACTED_KEYS`] on its own is an ABSENCE gate, and the same list redacts the committed
    /// fixture AND the live re-capture. So a load-bearing key that ever entered that list would
    /// disappear from both sides together, every comparison would still pass, and the test would stop
    /// testing while staying green. This list is the presence half that stops it: every name here
    /// must be absent from `REDACTED_KEYS` and present in the committed capture.
    const LOAD_BEARING_KEYS: &[&str] = &[
        // provenance
        "bin",
        "initialize",
        "agentInfo",
        "name",
        "version",
        // the session-start picker read
        "sessionNew",
        "configOptions",
        "category",
        "currentValue",
        "options",
        "value",
        // the init-frame read, envelope included
        "rawSdkNotifications",
        "method",
        "params",
        "message",
        "type",
        "subtype",
        "model",
    ];

    /// Every object key anywhere in `value`, so presence is checked against the parsed structure
    /// rather than against the file's spelling.
    fn all_keys(value: &Value, into: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    into.insert(key.clone());
                    all_keys(child, into);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| all_keys(item, into)),
            _ => {}
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    fn capture() -> Value {
        serde_json::from_str(CAPTURE_JSON).expect("the committed capture parses as JSON")
    }

    /// The `initialize` RESULT, verbatim from the capture.
    fn captured_initialize_result() -> Value {
        capture()["initialize"].clone()
    }

    /// The `session/new` RESULT, verbatim from the capture.
    fn captured_session_new_result() -> Value {
        capture()["sessionNew"].clone()
    }

    /// The `_claude/sdkMessage` notification, verbatim from the capture — JSON-RPC envelope included,
    /// because the envelope is part of what the reader has to match.
    fn captured_init_notification() -> Value {
        capture()["rawSdkNotifications"][0].clone()
    }

    /// Delete every [`REDACTED_KEYS`] entry, at any depth. Used on a LIVE capture so it can be
    /// compared with the committed one.
    fn redact(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.retain(|key, _| !REDACTED_KEYS.contains(&key.as_str()));
                for child in map.values_mut() {
                    redact(child);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(redact),
            _ => {}
        }
    }

    #[test]
    fn the_committed_capture_is_the_file_that_was_measured() {
        assert_eq!(
            sha256_hex(CAPTURE_JSON.as_bytes()),
            CAPTURE_SHA256,
            "the committed capture changed; re-capture and update the hash, do not edit the file"
        );
        let capture = capture();
        assert_eq!(
            capture["bin"].as_str(),
            Some(CAPTURE_BIN),
            "the capture must name the binary it came from, absolute"
        );
        assert_eq!(
            capture["initialize"]["agentInfo"]["version"].as_str(),
            Some(CAPTURE_ADAPTER_VERSION),
            "the capture must carry the adapter's OWN version, not one we assert about it"
        );
        // The redaction is verified in-repo, so nobody has to trust that it ran.
        for key in REDACTED_KEYS {
            assert!(
                !CAPTURE_JSON.contains(&format!("\"{key}\"")),
                "redacted key {key} is still in the committed capture"
            );
        }
    }

    #[test]
    fn the_redaction_list_cannot_delete_the_thing_under_test() {
        // The deletion list is applied to BOTH the committed fixture and the live re-capture, so a
        // load-bearing key inside it would vanish from both and every comparison would still pass.
        for key in LOAD_BEARING_KEYS {
            assert!(
                !REDACTED_KEYS.contains(key),
                "{key} is load-bearing and must never be redacted"
            );
        }
        let mut present = std::collections::BTreeSet::new();
        all_keys(&capture(), &mut present);
        let missing: Vec<_> = LOAD_BEARING_KEYS
            .iter()
            .filter(|key| !present.contains(**key))
            .collect();
        assert!(
            missing.is_empty(),
            "the committed capture is missing load-bearing keys: {missing:?}"
        );
        // Names alone would pass on a key that exists somewhere irrelevant, so require the exact
        // paths the reads walk to resolve to real values.
        let capture = capture();
        for path in [
            &capture["bin"],
            &capture["initialize"]["agentInfo"]["name"],
            &capture["initialize"]["agentInfo"]["version"],
            &capture["sessionNew"]["configOptions"][0]["category"],
            &capture["rawSdkNotifications"][0]["method"],
            &capture["rawSdkNotifications"][0]["params"]["message"]["type"],
            &capture["rawSdkNotifications"][0]["params"]["message"]["subtype"],
            &capture["rawSdkNotifications"][0]["params"]["message"]["model"],
        ] {
            assert!(
                path.is_string(),
                "a load-bearing path resolved to {path:?} instead of a string"
            );
        }
        // POSITIVE CONTROL: the redaction did run, so this is not a fixture nothing was removed from.
        assert!(
            !present.contains("skills") && !present.contains("memory_paths"),
            "POSITIVE CONTROL: the redaction must actually have removed keys"
        );
    }

    #[test]
    fn the_captured_picker_is_the_string_the_field_reported() {
        // The whole defect in one assertion, driven by real wire: the adapter's model-category
        // config option really does report `default`, and that string reaches us unchanged. Nothing
        // here is typed as input — the value comes out of the capture.
        let result = captured_session_new_result();
        assert_eq!(
            result.as_object().map(|object| {
                let mut keys: Vec<_> = object.keys().map(String::as_str).collect();
                keys.sort_unstable();
                keys
            }),
            Some(vec!["configOptions", "modes", "sessionId"]),
            "0.70.0 returns exactly these three keys, so the legacy `models` shape cannot fire"
        );
        assert!(
            result.get("models").is_none(),
            "POSITIVE CONTROL: if a top-level `models` appeared, the read below would be testing the \
             legacy shape and would prove nothing about the Claude one"
        );
        let model_option = result["configOptions"]
            .as_array()
            .expect("configOptions is an array")
            .iter()
            .find(|option| option["category"] == json!(MODEL_CONFIG_CATEGORY))
            .expect("a model-category option");
        assert_eq!(
            model_option["currentValue"].as_str(),
            Some(CLAUDE_PICKER_DEFAULT),
            "the captured picker value IS the refused alias — this is the field the field reported"
        );
        assert_eq!(
            session_model_from_result(&result),
            None,
            "so the session-start read must yield absence, not the alias"
        );
    }

    #[test]
    fn the_captured_init_frame_names_the_concrete_model() {
        let notification = captured_init_notification();
        // Assert against the capture's OWN field, so this cannot pass by agreeing with a constant
        // somebody typed. The literal below then records WHICH model was measured.
        let on_the_wire = notification["params"]["message"]["model"]
            .as_str()
            .expect("the captured frame carries a model");
        assert_eq!(
            claude_sdk_init_model(&notification).as_deref(),
            Some(on_the_wire),
            "the reader must return the frame's own model field, not a neighbour"
        );
        assert_eq!(
            on_the_wire, "claude-opus-5[1m]",
            "and this is the id 0.70.0 reported for a `default` picker on the captured account"
        );
        assert_ne!(
            on_the_wire, CLAUDE_PICKER_DEFAULT,
            "the whole point: the frame carries an identity where the picker carried a preference"
        );
    }

    #[test]
    fn the_captured_initialize_result_is_what_gates_the_opt_in() {
        assert!(
            is_claude_agent_acp(&captured_initialize_result()),
            "the gate must fire on the adapter's real `initialize` result, not only on a hand-built one"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // FAIL-CLOSED (#916 review, C6). The picker is a preference at EVERY value, so no picker value
    // may be published as an identity on this adapter.
    // ---------------------------------------------------------------------------------------------

    /// Every value the 0.70.0 picker offers, read out of the committed capture rather than listed
    /// here — a hand-written list would go stale silently and would not be wire.
    fn captured_picker_values() -> Vec<String> {
        captured_session_new_result()["configOptions"]
            .as_array()
            .expect("configOptions is an array")
            .iter()
            .find(|option| option["category"] == json!(MODEL_CONFIG_CATEGORY))
            .expect("a model-category option")["options"]
            .as_array()
            .expect("the model option has an options array")
            .iter()
            .map(|option| {
                option["value"]
                    .as_str()
                    .expect("every picker option has a string value")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn no_claude_picker_value_is_published_as_a_model() {
        // C6: refusing only `default` left `sonnet`, `haiku` and `opus[1m]` publishable. Each of
        // those is a preference too — the same string is a different concrete model on a different
        // account — so a buyer filtering on it is awarding against ad copy.
        let values = captured_picker_values();
        assert!(
            values.len() >= 4,
            "POSITIVE CONTROL: the capture must actually offer several picker values, or this test \
             proves nothing: {values:?}"
        );
        assert!(
            values.iter().any(|value| value != CLAUDE_PICKER_DEFAULT),
            "POSITIVE CONTROL: at least one value must be something OTHER than the alias the old \
             code already refused: {values:?}"
        );
        for value in values {
            let mut driver = test_driver();
            driver.is_claude_adapter = true;
            driver.session_model = Some(value.clone());
            assert_eq!(
                driver.resolved_model(),
                None,
                "picker value {value} was published as this run's model"
            );
        }
    }

    #[test]
    fn the_picker_still_reports_for_every_other_harness() {
        // Rider 4: codex-acp must not regress. The refusal is scoped to the adapter that has the
        // preference picker, so a legacy-shape harness reports exactly what it reported before.
        let mut driver = test_driver();
        driver.is_claude_adapter = false;
        driver.session_model = Some("gpt-5.6-terra[medium]".into());
        assert_eq!(
            driver.resolved_model().as_deref(),
            Some("gpt-5.6-terra[medium]"),
            "narrowing the Claude picker must not touch any other harness"
        );
    }

    #[test]
    fn the_init_frame_outranks_the_picker_on_the_claude_path() {
        let mut driver = test_driver();
        driver.is_claude_adapter = true;
        driver.session_model = Some("sonnet".into());
        *driver.claude_turn_model.lock().expect("lock") = Some("claude-sonnet-5[1m]".into());
        assert_eq!(
            driver.resolved_model().as_deref(),
            Some("claude-sonnet-5[1m]"),
            "the turn-resolved id wins; the picker never supplies a fallback here"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // EVERY FAIL-CLOSED BRANCH, EXECUTED (#916 review, C8/C12). One case per branch of
    // `claude_sdk_init_model`, plus both poisoned-lock branches.
    // ---------------------------------------------------------------------------------------------

    #[test]
    fn every_malformed_init_frame_branch_yields_absence() {
        // Each case names the branch it drives. A branch with no case here is a branch nothing runs.
        let cases: Vec<(&str, Value)> = vec![
            ("method absent", json!({"params": {"message": {}}})),
            ("method not a string", json!({"method": 7, "params": {"message": {}}})),
            (
                "method is another notification",
                json!({"method": "session/update", "params": {"message": {
                    "type": "system", "subtype": "init", "model": "claude-opus-5[1m]"}}}),
            ),
            ("params absent", json!({"method": CLAUDE_SDK_MESSAGE_METHOD})),
            (
                "params not an object",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": []}),
            ),
            (
                "message absent",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"sessionId": "s"}}),
            ),
            (
                "message not an object",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": "system"}}),
            ),
            (
                "type absent",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "subtype": "init", "model": "claude-opus-5[1m]"}}}),
            ),
            (
                "type not a string",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": ["system"], "subtype": "init", "model": "claude-opus-5[1m]"}}}),
            ),
            (
                "type is another frame kind",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": "assistant", "subtype": "init", "model": "claude-opus-5"}}}),
            ),
            (
                "subtype absent",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": "system", "model": "claude-opus-5[1m]"}}}),
            ),
            (
                "subtype not a string",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": "system", "subtype": 1, "model": "claude-opus-5[1m]"}}}),
            ),
            (
                "subtype is another system frame",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": "system", "subtype": "status", "model": "claude-opus-5[1m]"}}}),
            ),
            (
                "model absent",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": "system", "subtype": "init"}}}),
            ),
            (
                "model not a string",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": "system", "subtype": "init", "model": {"id": "claude-opus-5"}}}}),
            ),
            (
                "model blank",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": "system", "subtype": "init", "model": "   "}}}),
            ),
            (
                "model is the refused picker alias",
                json!({"method": CLAUDE_SDK_MESSAGE_METHOD, "params": {"message": {
                    "type": "system", "subtype": "init", "model": CLAUDE_PICKER_DEFAULT}}}),
            ),
        ];
        assert_eq!(cases.len(), 17, "keep the branch list and the case list in step");
        for (branch, notification) in cases {
            assert_eq!(
                claude_sdk_init_model(&notification),
                None,
                "branch `{branch}` must fail closed to absence, never to a fabricated model"
            );
        }
        // POSITIVE CONTROL: the same reader, on real wire, does produce a model — so the seventeen
        // absences above are the branches doing their job and not a reader that always returns None.
        assert!(
            claude_sdk_init_model(&captured_init_notification()).is_some(),
            "the reader must still read the captured frame"
        );
    }

    #[test]
    fn a_poisoned_model_lock_reports_absence_and_never_panics() {
        // C12: `resolved_model`'s `Ok(..)` guard had no executing test. Poison the lock for real.
        let mut driver = test_driver();
        driver.is_claude_adapter = true;
        driver.session_model = Some("sonnet".into());
        *driver.claude_turn_model.lock().expect("lock") = Some("claude-opus-5[1m]".into());
        let lock = driver.claude_turn_model.clone();
        let poisoned = std::panic::catch_unwind(move || {
            let _guard = lock.lock().expect("lock");
            panic!("poison the model lock on purpose");
        });
        assert!(poisoned.is_err(), "the helper must actually have panicked");
        assert!(
            driver.claude_turn_model.is_poisoned(),
            "POSITIVE CONTROL: the lock must really be poisoned, or the branch below is not reached"
        );
        assert_eq!(
            driver.resolved_model(),
            None,
            "a poisoned lock must fail closed to absence, not fall back to the picker alias"
        );
    }

    #[test]
    fn a_poisoned_model_lock_still_routes_the_notification() {
        // C12: `handle_wire_message`'s capture is best-effort. A poisoned lock must cost the model,
        // never the update — the notification still has to reach the generic ACP path.
        let claude_turn_model = Arc::new(Mutex::new(None));
        let lock = claude_turn_model.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = lock.lock().expect("lock");
            panic!("poison the model lock on purpose");
        });
        assert!(
            claude_turn_model.is_poisoned(),
            "POSITIVE CONTROL: the lock must really be poisoned"
        );
        let (response_tx, _response_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        handle_wire_message(
            &captured_init_notification(),
            &claude_turn_model,
            &response_tx,
            &update_tx,
            &PermissionOutcome::Deny,
            &mut |_, _| panic!("a notification must not answer a permission request"),
        );
        let update = update_rx.try_recv().expect("the notification must still route");
        assert!(
            matches!(update, SessionUpdate::Ext(ExtMethod { ref method, .. })
                if method == CLAUDE_SDK_MESSAGE_METHOD),
            "routing must be unchanged by the adapter-private read: {update:?}"
        );
    }

    #[test]
    fn a_new_session_does_not_inherit_the_previous_turns_model() {
        // Advisor's non-gating note, taken. Safe today because every call site builds a fresh
        // driver; a reused one would otherwise report a stale concrete model as this session's.
        let mut driver = test_driver();
        driver.is_claude_adapter = true;
        *driver.claude_turn_model.lock().expect("lock") = Some("claude-opus-5[1m]".into());
        assert_eq!(
            driver.resolved_model().as_deref(),
            Some("claude-opus-5[1m]"),
            "POSITIVE CONTROL: the stale value must be readable first, or the clear proves nothing"
        );
        // `start_session` calls exactly this, which is why it is a named method and not three inline
        // lines: a clear no test can reach is how a stale model would survive unnoticed.
        driver.reset_turn_model();
        assert_eq!(
            driver.resolved_model(),
            None,
            "the next session must start with no turn-resolved model"
        );
    }


    /// A `/bin/sh` agent that speaks just enough ACP to drive `ready` -> `start_session` -> `prompt`,
    /// announcing itself as claude-agent-acp and emitting the init frame DURING the prompt turn,
    /// which is where a real one arrives.
    ///
    /// One `sed` over the raw request line and never `grep`: `grep` on some hosts is `ugrep`, and a
    /// stub whose behaviour depends on which `grep` is first on `PATH` fails for a reason having
    /// nothing to do with the code under test.
    fn claude_ordering_stub() -> AgentCommand {
        let script = concat!(
            "while IFS= read -r line; do\n",
            "  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n",
            "  case \"$line\" in\n",
            "    *'\"initialize\"'*)\n",
            "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"protocolVersion\":1,",
            "\"agentInfo\":{\"name\":\"@agentclientprotocol/claude-agent-acp\",\"version\":\"0.70.0\"},",
            "\"authMethods\":[]}}\\n' \"$id\" ;;\n",
            "    *'\"session/new\"'*)\n",
            "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"sessionId\":\"s1\",",
            "\"modes\":{\"currentModeId\":\"default\",\"availableModes\":[]},",
            "\"configOptions\":[{\"id\":\"model\",\"category\":\"model\",\"type\":\"select\",",
            "\"currentValue\":\"default\",\"options\":[]}]}}\\n' \"$id\" ;;\n",
            "    *'\"session/prompt\"'*)\n",
            "      printf '{\"jsonrpc\":\"2.0\",\"method\":\"_claude/sdkMessage\",\"params\":",
            "{\"sessionId\":\"s1\",\"message\":{\"type\":\"system\",\"subtype\":\"init\",",
            "\"model\":\"claude-opus-5[1m]\"}}}\\n'\n",
            "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"stopReason\":\"end_turn\"}}\\n' \"$id\" ;;\n",
            "  esac\n",
            "done\n",
        );
        AgentCommand::new("/bin/sh".into(), vec!["-c".into(), script.into()])
    }

    #[tokio::test]
    async fn the_session_reset_lands_before_any_init_frame_can() {
        // The clear in `start_session` is a one-liner, and one-liners that clear state are where
        // ORDERING bugs hide. The safety is not the line, it is where the line sits: the reset runs
        // while `session/new` is settling, and an init frame can only arrive once a PROMPT is under
        // way. Move the reset any later — into `prompt`, or after the first frame is handled — and it
        // wipes a live value, turning a known model into absence.
        //
        // This drives the real driver against a stub that emits the frame during the prompt turn, so
        // it goes red if the reset ever moves past it.
        use crate::driver::Driver;
        let mut driver = AcpDriver::new(
            claude_ordering_stub(),
            PermissionOutcome::Allow,
            Duration::from_secs(5),
        );
        driver.ready().await.expect("the stub answers initialize");
        assert!(
            driver.is_claude_adapter,
            "POSITIVE CONTROL: the stub must be taken for claude-agent-acp, or the reset under test \
             is not even on the path"
        );

        // Seed a value from a PREVIOUS session. The reset exists for exactly this.
        *driver.claude_turn_model.lock().expect("lock") = Some("claude-haiku-4-5-20251001".into());
        let session = driver
            .start_session(SessionConfig {
                cwd: std::env::temp_dir(),
                mcp_servers: Vec::new(),
                env: Vec::new(),
            })
            .await
            .expect("the stub answers session/new");
        assert_eq!(
            driver.claude_turn_model.lock().expect("lock").clone(),
            None,
            "the stale model from the previous session must be gone once the session is open"
        );
        assert_eq!(
            driver.usage().and_then(|usage| usage.model),
            None,
            "and the picker must supply no fallback, so the run is unattributed until a frame lands"
        );

        driver
            .prompt(&session, PromptTurn { input: Vec::new() })
            .await
            .expect("the stub answers session/prompt");
        assert_eq!(
            driver.usage().and_then(|usage| usage.model).as_deref(),
            Some("claude-opus-5[1m]"),
            "the init frame arrived DURING the turn, so the reset must already have happened — a \
             reset that ran any later would have wiped this"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // THE AUTHENTICITY LEG (#916 review, C3). This is the only test that can fail because the
    // committed capture is fabricated. It spends a real turn, so it is env-gated.
    // ---------------------------------------------------------------------------------------------

    /// Re-capture from a live adapter and compare the load-bearing fields with the committed file.
    ///
    /// Ignored by default: it needs the adapter binary, working credentials, `node`, and one real
    /// prompt turn. Run it with the binary you want to prove against:
    ///
    /// ```text
    /// LIVE_ACP_CAPTURE_BIN=/Users/forge/forge/npm/bin/claude-agent-acp \
    ///   cargo test -p maxplayer-core --features acp \
    ///   the_committed_capture_still_matches_the_live_adapter -- --ignored --nocapture
    /// ```
    ///
    /// Session ids, uuids and token counts differ every run, so this compares the fields the model
    /// read actually depends on — not the whole file.
    ///
    /// ⚠ CI DOES NOT RUN THIS TEST, and cannot. It needs Claude credentials and spends a real turn,
    /// and this repo's CI is additionally held pending a maintainer click because branches arrive
    /// from a fork. So this is the only leg that can fail on a fabricated fixture, and it is the leg
    /// no automated run exercises — which is why the PR that adds it records the command, its output
    /// and the UTC date it was run by hand. An unrun proof is not a proof. `CAPTURE_SHA256` catches
    /// drift on every ordinary `cargo test`; authenticity is only ever as fresh as the last manual
    /// run recorded in the PR.
    ///
    /// RE-CAPTURE WHEN — the obligation `#[ignore]` creates. Moving authenticity out of automation
    /// makes it an operating duty, and a duty with no clock is not owned by anyone. Run this test,
    /// by hand, and record the result in the change that causes it:
    ///
    /// - the fleet's adapter binary changes version or digest
    /// - this fixture, [`REDACTED_KEYS`], [`LOAD_BEARING_KEYS`] or the parser's keys change
    /// - the account's model default moves
    /// - before any release that republishes the claim this fixture makes
    #[test]
    #[ignore = "spends a real Claude turn; needs a live adapter binary and credentials"]
    fn the_committed_capture_still_matches_the_live_adapter() {
        let Ok(bin) = std::env::var("LIVE_ACP_CAPTURE_BIN") else {
            panic!("set LIVE_ACP_CAPTURE_BIN to the adapter binary to prove against");
        };
        let client = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/claude-agent-acp-0.70.0/capture.mjs"
        );
        // The capture is UNREDACTED on disk: the raw init frame carries the workstation's skills,
        // memory paths, cwd and an IPC socket path. Redaction happens in memory, below.
        //
        // So it gets a private directory of its own, per run, removed when the test ends.
        //
        // Unique per run because a fixed name in a shared temp dir lets two hand-runs overwrite
        // each other's capture, and the loser then compares the winner's bytes while reporting its
        // own binary — the same class as the fixed-path mis-send this repo's authors have already
        // paid for. But a unique name alone only converts a COLLISION into an ACCUMULATION: one
        // sensitive file left behind per run, forever. Hence the mode and the cleanup.
        //
        // `DirBuilder::mode` applies at creation, so there is no window where the directory exists
        // world-readable. `CaptureDir` removes it on the way out INCLUDING on panic, which is what
        // a failing assertion below does. Note the bound: a `panic = "abort"` profile skips Drop,
        // and then the directory survives the run.
        struct CaptureDir(std::path::PathBuf);
        impl Drop for CaptureDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = std::env::temp_dir().join(format!(
            "live-acp-capture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ));
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .create(&dir)
            .expect("the capture directory must be creatable");
        let dir = CaptureDir(dir);
        let out = dir.0.join("capture.json");
        let status = std::process::Command::new("node")
            .arg(client)
            .arg(&bin)
            .arg(&out)
            .status()
            .expect("node must be on PATH to run the capture client");
        assert!(status.success(), "the capture client failed: {status:?}");
        // The enclosing directory is already 0700; this is for the case where it is not, because
        // the file is the unredacted frame and one line is cheaper than reasoning about `TMPDIR`.
        std::fs::set_permissions(&out, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("the capture must be readable only by its owner");

        let mut live: Value = serde_json::from_str(
            &std::fs::read_to_string(&out).expect("the live capture must be readable"),
        )
        .expect("the live capture must parse");
        redact(&mut live);
        let committed = capture();

        assert_eq!(
            live["initialize"]["agentInfo"]["name"],
            committed["initialize"]["agentInfo"]["name"],
            "a different adapter answered"
        );
        assert_eq!(
            live["initialize"]["agentInfo"]["version"],
            committed["initialize"]["agentInfo"]["version"],
            "adapter version moved; re-capture the fixture rather than loosening this test"
        );
        let live_option = live["sessionNew"]["configOptions"]
            .as_array()
            .expect("live configOptions")
            .iter()
            .find(|option| option["category"] == json!(MODEL_CONFIG_CATEGORY))
            .expect("a live model-category option")
            .clone();
        let committed_option = committed["sessionNew"]["configOptions"]
            .as_array()
            .expect("committed configOptions")
            .iter()
            .find(|option| option["category"] == json!(MODEL_CONFIG_CATEGORY))
            .expect("a committed model-category option")
            .clone();
        assert_eq!(
            live_option["currentValue"], committed_option["currentValue"],
            "the picker value the committed capture claims is not what the adapter sends"
        );
        assert_eq!(
            live_option["options"], committed_option["options"],
            "the picker vocabulary the committed capture claims is not what the adapter sends"
        );
        let live_frame = live["rawSdkNotifications"][0].clone();
        let committed_frame = committed["rawSdkNotifications"][0].clone();
        for field in ["method"] {
            assert_eq!(
                live_frame[field], committed_frame[field],
                "the committed capture's `{field}` is not what the adapter sends"
            );
        }
        for field in ["type", "subtype"] {
            assert_eq!(
                live_frame["params"]["message"][field], committed_frame["params"]["message"][field],
                "the committed capture's frame `{field}` is not what the adapter sends"
            );
        }
        // THE BINDING THAT MATTERS. Everything else in this file can be satisfied by a fabrication
        // made consistently in one commit: `CAPTURE_SHA256` is a const beside the file it hashes and
        // moves with an edit, `LOAD_BEARING_KEYS` checks presence and not value, and the typed literal
        // in `the_captured_init_frame_names_the_concrete_model` is one more editable string. Only a
        // comparison against a LIVE adapter is outside the author's reach, and the model id is the
        // scalar this whole change exists to publish. An earlier revision of this test asserted only
        // that the live model was not the picker alias — a SHAPE check, which the invented value
        // `claude-opus-4-8` also passes. Equality or nothing.
        let live_model = claude_sdk_init_model(&live_frame)
            .expect("the live init frame must carry a model the reader accepts");
        let committed_model = claude_sdk_init_model(&committed_frame)
            .expect("the committed capture must carry a model the reader accepts");
        assert_eq!(
            live_model, committed_model,
            "the committed capture claims this session runs on `{committed_model}` and the adapter \
             reports `{live_model}`; re-capture the fixture rather than loosening this test. A \
             grader on another account, or after a model default moves, sees this too — that is \
             fixture STALENESS, not a false alarm, and a RED test saying re-capture is the correct \
             outcome. There is deliberately no environment variable to skip it."
        );
        assert_ne!(
            live_model, CLAUDE_PICKER_DEFAULT,
            "the live adapter published the picker alias as its model"
        );
        println!("LIVE bin={bin} model={live_model} committed={committed_model}");
    }
}
