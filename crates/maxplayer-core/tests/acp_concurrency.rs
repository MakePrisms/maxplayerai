//! Regression test for MakePrisms/maxplayerai#223 (multi-slot execution): N awarded jobs must run
//! CONCURRENTLY on the seller node's single-threaded LocalSet, and the event loop must keep
//! breathing while a job is in flight.
//!
//! The seller node runs every awarded job as a `spawn_local` task on ONE thread (see
//! `seller_node/run.rs`): tasks interleave only at await points, so any blocking (non-yielding)
//! section inside the agent-run path freezes every other job AND the run loop. The blocking
//! sections live in the ACP driver: `AcpDriver::wait_response` (a synchronous channel receive that
//! spans the ENTIRE `session/prompt` turn — the whole agent run) and `UpdateStream::next`'s live
//! arm. This test drives the REAL `AcpDriver` + `engine::run_job` path against a stub ACP agent
//! that sleeps during its prompt turn, exactly the layer `run_agent_job` (the seller's execute
//! path) wraps.
//!
//! Observed live (petar's rig, PR #223 review): slots=7, 7 awarded jobs — strictly serial
//! execution, never more than one agent process, and new awards piled up unprocessed until the
//! running job finished.

#![cfg(feature = "acp")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use maxplayer_core::driver::{
    AcpDriver, AgentCommand, ContentBlock, PermissionOutcome, PromptTurn, SessionConfig,
};
use maxplayer_core::engine::{RunParams, run_job};
use maxplayer_core::event::{JobExecutionStatus, JobId};
use maxplayer_core::log::EventLog;

/// How long the stub agent "works" inside its `session/prompt` turn.
const AGENT_SLEEP: Duration = Duration::from_secs(2);
/// Two concurrent jobs must finish in ~1× the sleep; two SERIAL jobs take ~2×. The threshold sits
/// between (1.75×) so a serial head fails and a concurrent fix passes with slack on slow runners.
const CONCURRENT_DEADLINE: Duration = Duration::from_millis(3500);

/// Write a stub ACP agent: a `/bin/sh` script speaking just enough line-delimited JSON-RPC for one
/// engine run — answer `initialize` (id 1) and `session/new` (id 2) immediately, then SLEEP through
/// the `session/prompt` turn (id 3) before reporting `end_turn`, like a real harness doing real
/// work. Request ids are deterministic: the driver numbers requests from 1 per process.
fn write_stub_agent(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("stub-acp-agent.sh");
    let body = format!(
        "#!/bin/sh\n\
         read _req\n\
         printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":2}}}}'\n\
         read _req\n\
         printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"sessionId\":\"stub\"}}}}'\n\
         read _req\n\
         sleep {}\n\
         printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"stopReason\":\"end_turn\"}}}}'\n",
        AGENT_SLEEP.as_secs()
    );
    std::fs::write(&script, body).expect("write stub agent");
    script
}

/// One end-to-end engine run against the stub agent — the same driver + engine path the seller
/// node's `execute_job` takes through `run_agent_job`.
async fn run_stub_job(
    script: std::path::PathBuf,
    dir: std::path::PathBuf,
    label: &str,
) -> JobExecutionStatus {
    let mut driver = AcpDriver::new(
        AgentCommand::new(
            "/bin/sh".into(),
            vec![script.to_string_lossy().into_owned()],
        ),
        PermissionOutcome::Allow,
        Duration::from_secs(30),
    );
    let mut log = EventLog::open(dir.join(format!("{label}.jsonl"))).expect("open log");
    let params = RunParams {
        session_config: SessionConfig {
            cwd: dir,
            mcp_servers: Vec::new(),
            env: Vec::new(),
        },
        // This test is about concurrency, not model selection: no request, so no binding step runs
        // and the harness default applies exactly as before.
        requested_model: None,
        prompt: PromptTurn {
            input: vec![ContentBlock::Text {
                text: "do the work".into(),
            }],
        },
    };
    run_job(
        &mut driver,
        &mut log,
        &JobId(label.into()),
        params,
        &mut |_| {},
    )
    .await
    .expect("stub job runs")
    .terminal
}

// Two sleeping jobs on one thread (the seller node's LocalSet model) must overlap — completing in
// ~1× the sleep, not 2× — and a peer task (standing in for the run loop's offer/award/payment
// servicing) must keep ticking while the jobs are in flight. On a head where the ACP driver blocks
// the thread for the whole prompt turn, execution is strictly serial AND the ticker starves.
#[test]
fn two_jobs_execute_concurrently_and_the_loop_keeps_ticking_on_one_thread() {
    let dir = std::env::temp_dir().join(format!("maxplayer-acp-concurrency-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    let script = write_stub_agent(&dir);

    // Single-threaded runtime + LocalSet: the exact execution model of the seller node's run loop
    // (`seller_node/run.rs` — jobs are `spawn_local` tasks, `!Send` futures, one thread).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let local = tokio::task::LocalSet::new();

    let (elapsed, ticks) = runtime.block_on(local.run_until(async {
        // The "event loop stays live" probe: ticks every 50ms only when the thread actually yields.
        let ticks = Arc::new(AtomicU32::new(0));
        let ticker = {
            let ticks = Arc::clone(&ticks);
            tokio::task::spawn_local(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
            })
        };

        let started = Instant::now();
        let first = tokio::task::spawn_local(run_stub_job(script.clone(), dir.clone(), "job-a"));
        let second = tokio::task::spawn_local(run_stub_job(script.clone(), dir.clone(), "job-b"));
        let first = first.await.expect("job-a joins");
        let second = second.await.expect("job-b joins");
        let elapsed = started.elapsed();
        ticker.abort();

        assert_eq!(first, JobExecutionStatus::Completed, "job-a completes");
        assert_eq!(second, JobExecutionStatus::Completed, "job-b completes");
        (elapsed, ticks.load(Ordering::SeqCst))
    }));

    assert!(
        elapsed < CONCURRENT_DEADLINE,
        "two {AGENT_SLEEP:?}-sleep jobs must OVERLAP on one thread (expected ~1x sleep, \
         got {elapsed:?} — serial execution is the #223 bug)"
    );
    assert!(
        ticks >= 10,
        "the event loop must keep servicing events while jobs run (expected >=10 ticks of 50ms, \
         got {ticks} — a deaf loop is the #223 bug)"
    );
}
