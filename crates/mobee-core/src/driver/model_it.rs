//! Acceptance legs for seat model config, driven through the REAL [`AcpDriver`] against a fake ACP
//! adapter running as a genuine child process.
//!
//! These legs exist because the unit tests in [`crate::driver::model`] check the classifier and the
//! request/confirm pair in isolation — they cannot show that `AcpDriver` actually sends the call, or
//! that a no-model run is untouched. The boundary being asserted here is the JSON-RPC the child
//! received, read back from a file the child wrote.
//!
//! ★ The fake adapter can LIE. `Mode::Liar` accepts the model and reports a different one, and
//! `Mode::Silent` accepts it and reports nothing — both measured behaviours of real adapters
//! (`claude-agent-acp` 0.45.1 resolves an unknown model through an alias matcher and answers
//! success while running `default`). A fixture that could only be honest would pass whether or not
//! the read-back gate existed, which is the fixture blindness that let a signerless client through
//! in the participation slice.
//!
//! The adapter is this test binary re-invoked ([`std::env::current_exe`]) with `MOBEE_FAKE_ACP` set,
//! running the `fake_acp_adapter_entry` test as its whole job. libtest's own stdout preamble is
//! harmless: the driver skips any line that is not JSON.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use crate::driver::{AcpDriver, AgentCommand, Driver, McpServer, PermissionOutcome, SessionConfig};

/// Env var naming which dialect the fake adapter speaks.
const MODE_VAR: &str = "MOBEE_FAKE_ACP";
/// Env var naming the file the fake adapter appends every received request to.
const WIRE_LOG_VAR: &str = "MOBEE_FAKE_ACP_WIRE";
/// The test that IS the fake adapter when [`MODE_VAR`] is set.
const ADAPTER_TEST: &str = "driver::model_it::fake_acp_adapter_entry";

/// What the fake adapter advertises and how it answers a model set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Advertises a `configOptions` model selector; honours the set truthfully.
    Modern,
    /// Advertises only the legacy `models` object; answers `session/set_model` with `{}`.
    Legacy,
    /// Advertises no model selector at all.
    None,
    /// Advertises modern, accepts any value, and reports a DIFFERENT model as current.
    Liar,
    /// Advertises modern, accepts any value, and reports no current value at all.
    Silent,
}

impl Mode {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "modern" => Self::Modern,
            "legacy" => Self::Legacy,
            "none" => Self::None,
            "liar" => Self::Liar,
            "silent" => Self::Silent,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Modern => "modern",
            Self::Legacy => "legacy",
            Self::None => "none",
            Self::Liar => "liar",
            Self::Silent => "silent",
        }
    }
}

/// The models the fake adapter advertises — shaped like `claude-agent-acp` 0.45.1's real list.
const OFFERED: [&str; 3] = ["default", "fable", "haiku"];
/// What a lying adapter claims is current, regardless of what was asked for.
const LIAR_REPORTS: &str = "default";

// ---------------------------------------------------------------------------------------------
// The fake adapter
// ---------------------------------------------------------------------------------------------

/// When `MOBEE_FAKE_ACP` is set this test IS the ACP adapter: it serves JSON-RPC on stdio until
/// stdin closes, then exits the process. With the var unset it does nothing, so a normal test run
/// simply passes it.
#[test]
fn fake_acp_adapter_entry() {
    let Ok(raw) = std::env::var(MODE_VAR) else {
        return;
    };
    let mode = Mode::parse(&raw).unwrap_or_else(|| panic!("unknown fake adapter mode {raw:?}"));
    let wire_log = std::env::var(WIRE_LOG_VAR).ok().map(PathBuf::from);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            continue;
        };
        // Record the request verbatim BEFORE answering, so a leg can assert what actually crossed
        // the boundary rather than what the driver believed it sent.
        if let Some(path) = &wire_log {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open fake adapter wire log");
            writeln!(file, "{line}").expect("append fake adapter wire log");
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let Some(result) = answer(mode, method, &params) else {
            continue;
        };
        let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        writeln!(stdout, "{response}").expect("write fake adapter response");
        stdout.flush().expect("flush fake adapter response");
    }
    // stdin closed: the client is gone. Exit the PROCESS — this test is the adapter, not a test.
    std::process::exit(0);
}

/// The fake adapter's answer to one request, or `None` for a notification it ignores.
fn answer(mode: Mode, method: &str, params: &Value) -> Option<Value> {
    match method {
        "initialize" => Some(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": false },
        })),
        "session/new" => {
            let mut result = json!({ "sessionId": "fake-session-1" });
            match mode {
                Mode::Modern | Mode::Liar | Mode::Silent => {
                    result["configOptions"] = json!([
                        { "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                          "currentValue": "default",
                          "options": [{ "value": "default", "name": "Default" }] },
                        { "id": "model", "name": "Model", "category": "model", "type": "select",
                          "currentValue": OFFERED[1],
                          "options": OFFERED.iter().map(|value| json!({ "value": value }))
                              .collect::<Vec<_>>() },
                    ]);
                }
                Mode::Legacy => {
                    result["models"] = json!({
                        "currentModelId": OFFERED[1],
                        "availableModels": OFFERED.iter()
                            .map(|value| json!({ "modelId": value })).collect::<Vec<_>>(),
                    });
                }
                Mode::None => {}
            }
            Some(result)
        }
        "session/set_config_option" => {
            let requested = params.get("value").and_then(Value::as_str).unwrap_or("");
            let current = match mode {
                // The lie: acknowledge, then report something else as current.
                Mode::Liar => LIAR_REPORTS,
                _ => requested,
            };
            let model_option = match mode {
                // The other lie: acknowledge, report no current value at all.
                Mode::Silent => json!({ "id": "model", "category": "model" }),
                _ => json!({ "id": "model", "category": "model", "currentValue": current }),
            };
            Some(json!({ "configOptions": [model_option] }))
        }
        // The legacy setter carries no state back — exactly as codex-acp and cursor-agent answer.
        "session/set_model" => Some(json!({})),
        "session/prompt" => Some(json!({ "stopReason": "end_turn" })),
        "session/cancel" => Some(json!({})),
        _ => Some(json!({})),
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// Serializes the legs' use of the process-global environment.
///
/// ★ The fake adapter is configured through env vars because `AcpDriver` sets NO per-child
/// environment — that is the same gap this slice's recon filed as issue #233, biting the test that
/// needed it. Env is process-global, so with the default multi-threaded runner the legs overwrite
/// each other's mode between `set_var` and `spawn` and a child answers in some other leg's dialect.
///
/// This was MEASURED, not anticipated: the legs passed inside the full 659-test suite (where they
/// were scheduled far enough apart to miss each other) and every one of them failed when run as a
/// filtered subset. The green was scheduling luck. A lock is the honest fix for a global resource —
/// held across set → spawn → session → unset, which is exactly the window that must not overlap.
static ADAPTER_ENV: Mutex<()> = Mutex::new(());

/// A wire log path unique to one leg, so concurrent legs never read each other's frames.
fn wire_log_path(leg: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "mobee-model-leg-{leg}-{}-{:?}.jsonl",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// Launch the fake adapter under the real [`AcpDriver`] and open one session.
///
/// Returns the driver (so `last_model` can be read) and every request the child received.
async fn start_session_against_fake(
    leg: &str,
    mode: Mode,
    model: Option<&str>,
) -> (
    Result<String, crate::driver::DriverError>,
    Vec<Value>,
    PathBuf,
) {
    // Held for the whole set-env → spawn → session window. Poisoning is ignored: a leg that panics
    // does so in its OWN assertions after this helper returns, and a poisoned lock must not turn
    // every later leg into an unrelated failure.
    let _env_guard = ADAPTER_ENV
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let wire = wire_log_path(leg);
    let exe = std::env::current_exe().expect("test binary path");
    let mut driver = AcpDriver::new(
        AgentCommand::with_model(
            exe.to_string_lossy().into_owned(),
            vec![
                "--exact".to_owned(),
                ADAPTER_TEST.to_owned(),
                "--nocapture".to_owned(),
            ],
            model.map(str::to_owned),
        ),
        PermissionOutcome::Allow,
        Duration::from_secs(20),
    );
    // Safety: the child reads these from its own environment at startup. Set for this process only
    // as the spawn channel; the fake adapter is the only reader.
    unsafe {
        std::env::set_var(MODE_VAR, mode.as_str());
        std::env::set_var(WIRE_LOG_VAR, &wire);
    }
    let readiness = driver.ready().await;
    let outcome = match readiness {
        Ok(_) => {
            driver
                .start_session(SessionConfig {
                    cwd: std::env::temp_dir(),
                    mcp_servers: Vec::<McpServer>::new(),
                    env: Vec::new(),
                })
                .await
        }
        Err(error) => Err(error),
    };
    let confirmed = driver.last_model().cloned();
    let _ = driver.shutdown().await;
    unsafe {
        std::env::remove_var(MODE_VAR);
        std::env::remove_var(WIRE_LOG_VAR);
    }
    let frames = read_frames(&wire);
    // Fold the confirmed model into the returned session id string so a leg can assert both.
    let outcome = outcome.map(|session_id| match confirmed {
        Some(outcome) => format!("{session_id}|{}|{}", outcome.model, outcome.confirmed),
        None => format!("{session_id}|<no-model>"),
    });
    (outcome, frames, wire)
}

/// Every JSON-RPC request the child recorded, in arrival order.
fn read_frames(wire: &PathBuf) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(wire) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// The requests whose method is `method`.
fn frames_for<'a>(frames: &'a [Value], method: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|frame| frame.get("method").and_then(Value::as_str) == Some(method))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Legs
// ---------------------------------------------------------------------------------------------

/// LEG 2 — a configured model reaches the harness boundary with the RIGHT VALUE.
///
/// Asserts the `session/set_config_option` frame the child actually received: its `configId` and
/// `value`, not merely that a session was created.
#[tokio::test]
async fn leg2_configured_model_reaches_the_harness_with_the_configured_value() {
    let (outcome, frames, _) =
        start_session_against_fake("leg2", Mode::Modern, Some("haiku")).await;
    let outcome = outcome.expect("session with a supported model must start");
    assert_eq!(
        outcome, "fake-session-1|haiku|true",
        "the driver must report the confirmed model"
    );

    let sets = frames_for(&frames, "session/set_config_option");
    assert_eq!(
        sets.len(),
        1,
        "exactly one model call belongs on the wire, got {frames:#?}"
    );
    let params = &sets[0]["params"];
    assert_eq!(params["configId"], json!("model"));
    assert_eq!(
        params["value"],
        json!("haiku"),
        "the VALUE is the whole point of this leg"
    );
    assert_eq!(params["sessionId"], json!("fake-session-1"));

    // Ordering is load-bearing: the model must be pinned BEFORE any prompt could run.
    let set_index = frames
        .iter()
        .position(|f| f.get("method").and_then(Value::as_str) == Some("session/set_config_option"))
        .expect("set frame present");
    assert!(
        frames
            .iter()
            .skip(set_index)
            .all(|f| f.get("method").and_then(Value::as_str) != Some("session/prompt")),
        "no prompt may precede or race the model call"
    );
}

/// LEG 2 (legacy dialect) — an adapter advertising only `models` is driven with `session/set_model`.
#[tokio::test]
async fn leg2_legacy_dialect_uses_set_model_and_reports_it_unconfirmed() {
    let (outcome, frames, _) =
        start_session_against_fake("leg2legacy", Mode::Legacy, Some("haiku")).await;
    let outcome = outcome.expect("legacy dialect must start");
    assert_eq!(
        outcome, "fake-session-1|haiku|false",
        "the legacy dialect cannot confirm, and must say so rather than claim it did"
    );

    let sets = frames_for(&frames, "session/set_model");
    assert_eq!(sets.len(), 1, "got {frames:#?}");
    assert_eq!(sets[0]["params"]["modelId"], json!("haiku"));
    assert!(
        frames_for(&frames, "session/set_config_option").is_empty(),
        "an adapter that never advertised config options must not be sent one"
    );
}

/// LEG 3 — a harness that advertises no model selector fails LOUDLY, naming the adapter.
#[tokio::test]
async fn leg3_unsupported_harness_with_a_configured_model_fails_naming_the_adapter() {
    let (outcome, frames, _) = start_session_against_fake("leg3", Mode::None, Some("haiku")).await;
    let error = outcome.expect_err("an unsupported harness must refuse a pinned model");
    let message = error.to_string();
    assert!(message.contains("haiku"), "{message}");
    assert!(
        message.contains("does not support model selection"),
        "{message}"
    );
    // The adapter is named by the binary it launched — what the operator sees in their config.
    let exe = std::env::current_exe().expect("test binary path");
    let basename = exe
        .file_name()
        .and_then(|name| name.to_str())
        .expect("exe basename");
    assert!(
        message.contains(basename),
        "the error must name the adapter: {message}"
    );
    assert!(
        frames_for(&frames, "session/set_config_option").is_empty()
            && frames_for(&frames, "session/set_model").is_empty(),
        "nothing may be sent to a harness that advertised no selector: {frames:#?}"
    );
}

/// LEG 3b — a model the harness never offered is refused BEFORE anything is sent.
#[tokio::test]
async fn leg3b_a_model_outside_the_offered_list_is_refused_without_a_call() {
    let (outcome, frames, _) =
        start_session_against_fake("leg3b", Mode::Modern, Some("claude-haiku-4-5")).await;
    let error = outcome.expect_err("a model the adapter never offered must be refused");
    let message = error.to_string();
    assert!(message.contains("claude-haiku-4-5"), "{message}");
    for offered in OFFERED {
        assert!(
            message.contains(offered),
            "the error must list what IS accepted, missing {offered}: {message}"
        );
    }
    assert!(
        frames_for(&frames, "session/set_config_option").is_empty(),
        "a model known-bad from the advertisement must not be sent: {frames:#?}"
    );
}

/// LEG 3c — ★ THE LIE. The adapter accepts the model and reports a different one.
///
/// This is the measured behaviour of a real adapter, and the only reason the read-back exists. If
/// this leg is deleted, every other leg still passes while the feature silently does nothing.
#[tokio::test]
async fn leg3c_an_adapter_that_reports_a_different_model_fails_the_session() {
    let (outcome, frames, _) = start_session_against_fake("leg3c", Mode::Liar, Some("haiku")).await;
    let error = outcome.expect_err("a substituted model must fail the session, not warn");
    let message = error.to_string();
    assert!(
        message.contains("haiku") && message.contains(LIAR_REPORTS),
        "the error must name BOTH what was asked and what the harness reports: {message}"
    );
    // The call WAS made — this is a failure of confirmation, not of sending.
    assert_eq!(frames_for(&frames, "session/set_config_option").len(), 1);
}

/// LEG 3d — an adapter that accepts the model and reports NO current value also fails.
///
/// "Accepted but unreadable" leaves the running model unknown; treating that as success is how a
/// check whose failure mode is silence gets built.
#[tokio::test]
async fn leg3d_an_adapter_that_reports_no_current_value_fails_the_session() {
    let (outcome, _, _) = start_session_against_fake("leg3d", Mode::Silent, Some("haiku")).await;
    let error = outcome.expect_err("an unreadable confirmation must fail");
    assert!(error.to_string().contains("unknown"), "{error}");
}

/// LEG 4 — with no model configured, the session is created and NOTHING else is sent.
///
/// The comparison is the captured frame list, not an eyeballed diff: byte-identical means the child
/// receives exactly `initialize` + `session/new` and no model call of either dialect.
#[tokio::test]
async fn leg4_no_model_configured_sends_no_model_call_at_all() {
    let (outcome, frames, _) = start_session_against_fake("leg4", Mode::Modern, None).await;
    let outcome = outcome.expect("a session with no model configured must start");
    assert_eq!(
        outcome, "fake-session-1|<no-model>",
        "no model configured ⇒ nothing recorded, not a record of the harness default"
    );
    let methods: Vec<&str> = frames
        .iter()
        .filter_map(|frame| frame.get("method").and_then(Value::as_str))
        .collect();
    assert_eq!(
        methods,
        vec!["initialize", "session/new"],
        "an unconfigured seat must reach the harness exactly as it did before this feature"
    );
}

/// LIVE — the same gate against a REAL adapter, on demand.
///
/// The legs above prove mobee's response to each dialect against a fake adapter; they cannot prove a
/// real adapter honours a model, because neither fixture is one. This closes that half and makes the
/// evidence RE-RUNNABLE from the repo instead of living in whoever-measured-it's notes.
///
/// Ignored by default (it needs an installed, authenticated harness). Two modes:
///
/// - `MOBEE_MODEL_LIVE_MODEL` set ⇒ requires the adapter to confirm that exact model.
/// - unset ⇒ requests a deliberately impossible model and requires the refusal to enumerate what the
///   adapter DOES offer. That prints the adapter's matrix row, and it is a real assertion: an adapter
///   advertising no selector fails with a different message and no list.
///
/// ```text
/// MOBEE_MODEL_LIVE_ADAPTER=/path/to/claude-agent-acp \
/// MOBEE_MODEL_LIVE_MODEL=haiku \
/// CLAUDE_CODE_EXECUTABLE=$(command -v claude) \
///   cargo test -p mobee-core --features acp,gateway,git-delivery,wallet --lib \
///   -- live_model_round_trip_against_a_real_adapter --ignored --nocapture
/// ```
/// ⚠ On NixOS the claude adapter needs `CLAUDE_CODE_EXECUTABLE`, or it times out with no child
/// stderr and the failure looks like a protocol fault rather than a missing binary.
#[tokio::test]
#[ignore = "needs a real installed ACP adapter; set MOBEE_MODEL_LIVE_ADAPTER"]
async fn live_model_round_trip_against_a_real_adapter() {
    let Ok(raw) = std::env::var("MOBEE_MODEL_LIVE_ADAPTER") else {
        panic!("set MOBEE_MODEL_LIVE_ADAPTER to the adapter argv (space-separated)");
    };
    let mut argv = raw.split_whitespace().map(str::to_owned);
    let program = argv.next().expect("adapter argv must name a program");
    let args: Vec<String> = argv.collect();
    let wanted = std::env::var("MOBEE_MODEL_LIVE_MODEL").ok();
    // With no model named, ask for one no adapter can have: the refusal must enumerate the real
    // offered list, which is both the assertion and the matrix row.
    const IMPOSSIBLE: &str = "mobee-live-probe-not-a-model-9c1f";
    let requested = wanted.clone().unwrap_or_else(|| IMPOSSIBLE.to_owned());

    let mut driver = AcpDriver::new(
        AgentCommand::with_model(program.clone(), args, Some(requested.clone())),
        PermissionOutcome::Allow,
        Duration::from_secs(60),
    );
    driver.ready().await.expect("real adapter must initialize");
    let outcome = driver
        .start_session(SessionConfig {
            cwd: std::env::temp_dir(),
            mcp_servers: Vec::<McpServer>::new(),
            env: Vec::new(),
        })
        .await;
    let confirmed = driver.last_model().cloned();
    let _ = driver.shutdown().await;

    match wanted {
        Some(model) => {
            outcome.unwrap_or_else(|error| {
                panic!("{program} refused model {model:?}: {error}");
            });
            let confirmed = confirmed.expect("a configured model must be recorded");
            assert_eq!(confirmed.model, model);
            assert!(
                confirmed.confirmed,
                "{program} did not CONFIRM {model:?} — on the legacy dialect it cannot, and that is \
                 the point: a model there is applied but unverifiable"
            );
            println!("LIVE OK {program} model={model} confirmed=true");
        }
        None => {
            let error = outcome.expect_err("an impossible model must be refused");
            let message = error.to_string();
            assert!(
                message.contains(IMPOSSIBLE),
                "the refusal must name what was asked: {message}"
            );
            assert!(
                message.contains("it accepts:"),
                "{program} advertised no model selector, so there is no offered list to print — a \
                 real adapter with a selector always enumerates one: {message}"
            );
            println!("LIVE MATRIX {program}: {message}");
        }
    }
}

/// LEG 4b — the same, against an adapter that advertises NO selector.
///
/// A seat with no model must not care what the harness supports: an unsupported harness is only an
/// error for a seat that pinned something.
#[tokio::test]
async fn leg4b_no_model_configured_is_fine_on_a_harness_that_supports_none() {
    let (outcome, frames, _) = start_session_against_fake("leg4b", Mode::None, None).await;
    assert_eq!(
        outcome.expect("no model + no support must still start"),
        "fake-session-1|<no-model>"
    );
    let methods: Vec<&str> = frames
        .iter()
        .filter_map(|frame| frame.get("method").and_then(Value::as_str))
        .collect();
    assert_eq!(methods, vec!["initialize", "session/new"]);
}
