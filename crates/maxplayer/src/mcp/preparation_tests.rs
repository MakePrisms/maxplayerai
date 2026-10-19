//! Exercise the MCP -> daemon socket boundary, including the new pending shape.
use super::*;
use std::os::unix::net::{UnixListener, UnixStream};

/// Accept until a connection sends a request line. `daemon::ensure` probes liveness with a bare
/// connect that sends nothing; those connections are skipped, never answered.
fn next_request(listener: &UnixListener) -> (UnixStream, String) {
    loop {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(socket.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        if !line.trim().is_empty() {
            return (socket, line);
        }
    }
}

/// A fake daemon: hold the home's exclusive lock (as `buyer serve` does before it binds) and listen
/// on its socket. `daemon::ensure` counts a listener as serving only while that lock is held.
fn fake_daemon(home: &MaxplayerHome) -> (maxplayer_core::buyer::lock::HomeLock, UnixListener) {
    let lock = maxplayer_core::buyer::lock::HomeLock::acquire(
        home.root.join(maxplayer_core::buyer::LOCK_FILE),
    )
    .unwrap();
    (lock, UnixListener::bind(daemon::socket_path(home)).unwrap())
}

struct TempHome(std::path::PathBuf);
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn temp_home(label: &str) -> TempHome {
    TempHome(std::env::temp_dir().join(format!(
        "maxplayer-mcp-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )))
}

#[test]
fn pending_post_and_poll_cross_the_mcp_daemon_boundary() {
    let dir = temp_home("preparation");
    let home = home::bootstrap(&dir.0).unwrap();
    let (_lock, listener) = fake_daemon(&home);
    let server = std::thread::spawn(move || {
        for expected in ["prepare_post_job", "get_job"] {
            let (mut socket, line) = next_request(&listener);
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], expected);
            let result = match expected {
                "prepare_post_job" => {
                    assert_eq!(request["params"]["request_id"], "one-intent");
                    json!({"status":"preparing","preparation_id":"preparation:fixture"})
                }
                "get_job" => {
                    assert_eq!(request["params"]["job_id"], "preparation:fixture");
                    json!({"status":"posted","job_id":"real-job","preparation_id":"preparation:fixture"})
                }
                _ => json!({"pid":1234}),
            };
            writeln!(socket, "{}", json!({"id":request["id"],"result":result})).unwrap();
        }
    });
    let state = McpState {
        home,
        instructions: String::new(),
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let posted = runtime
        .block_on(call_tool_async(
            &state,
            &json!({"name":"post_job","arguments":{"task":"fixture","request_id":"one-intent"}}),
        ))
        .unwrap();
    assert_eq!(posted["structuredContent"]["status"], "preparing");
    assert!(posted["structuredContent"].get("job_id").is_none());
    let polled = runtime
        .block_on(call_tool_async(
            &state,
            &json!({"name":"get_job","arguments":{"job_id":"preparation:fixture"}}),
        ))
        .unwrap();
    assert_eq!(polled["structuredContent"]["job_id"], "real-job");
    assert_eq!(polled["isError"], false);
    server.join().unwrap();
}

/// R4-2 regression: a loaded buyer can take longer than the general 15s tool deadline to answer
/// post_job (queueing before the daemon dispatches it). The post must come back as a result, not a
/// deadline error the caller has to retry, and the MCP must forward exactly one posting request.
/// An identical retry carries the identical request_id, so the daemon's preparation key (tested in
/// `buyer::preparations`) maps it to the same job.
#[test]
fn slow_post_job_beats_the_general_deadline_without_a_duplicate_post() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = temp_home("slow-post");
    let home = home::bootstrap(&dir.0).unwrap();
    let (_lock, listener) = fake_daemon(&home);
    let posts = Arc::new(AtomicUsize::new(0));
    let counted = posts.clone();
    let stall = Duration::from_secs(TOOL_DEADLINE_SECS + 2);
    let server = std::thread::spawn(move || {
        for attempt in 0..2 {
            let (mut socket, line) = next_request(&listener);
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "prepare_post_job");
            assert_eq!(request["params"]["request_id"], "slow-intent");
            counted.fetch_add(1, Ordering::SeqCst);
            let result = if attempt == 0 {
                std::thread::sleep(stall);
                json!({"status":"preparing","preparation_id":"preparation:slow"})
            } else {
                json!({"status":"posted","job_id":"only-job","preparation_id":"preparation:slow","deduplicated":true})
            };
            writeln!(socket, "{}", json!({"id":request["id"],"result":result})).unwrap();
        }
    });
    let state = McpState {
        home,
        instructions: String::new(),
    };
    let call = McpRequest {
        id: Some(json!(7)),
        method: "tools/call".into(),
        params: json!({"name":"post_job","arguments":{"task":"fixture","request_id":"slow-intent"}}),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let started = std::time::Instant::now();
    let slow = runtime.block_on(dispatch_async(&state, &call));
    assert!(
        started.elapsed() > Duration::from_secs(TOOL_DEADLINE_SECS),
        "premise: the daemon answered after the general deadline"
    );
    assert_eq!(
        slow["result"]["isError"], false,
        "no deadline error: {slow}"
    );
    assert_eq!(slow["result"]["structuredContent"]["status"], "preparing");
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "one call, one posting request"
    );
    let retry = runtime.block_on(dispatch_async(&state, &call));
    assert_eq!(retry["result"]["isError"], false);
    assert_eq!(retry["result"]["structuredContent"]["job_id"], "only-job");
    assert_eq!(retry["result"]["structuredContent"]["deduplicated"], true);
    server.join().unwrap();
    assert_eq!(posts.load(Ordering::SeqCst), 2);
}
