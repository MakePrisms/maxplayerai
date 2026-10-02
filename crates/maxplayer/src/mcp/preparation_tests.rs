//! Exercise the MCP -> daemon socket boundary, including the new pending shape.
use super::*;
use std::os::unix::net::UnixListener;

#[test]
fn pending_post_and_poll_cross_the_mcp_daemon_boundary() {
    struct TempHome(std::path::PathBuf);
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = TempHome(
        std::env::temp_dir().join(format!(
            "maxplayer-mcp-preparation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )),
    );
    let home = home::bootstrap(&dir.0).unwrap();
    let listener = UnixListener::bind(daemon::socket_path(&home)).unwrap();
    let server = std::thread::spawn(move || {
        for expected in ["status", "prepare_post_job", "status", "get_job"] {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(socket.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
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
