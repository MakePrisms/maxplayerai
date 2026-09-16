//! Proxy swap route — synthetic mechanism proof.
//!
//! The job (the bridge) holds only a placeholder. The swap proxy substitutes the real credential in
//! the auth header, and only for an allowlisted destination. The vendor is the independent oracle:
//! it authenticates the real token, rejects the placeholder, and flags the placeholder if it ever
//! reaches it.
//!
//! What this proves, and its limit: the mechanism works against a fake vendor and a fake proxy, both
//! written for this contract, so it cannot falsify the contract. It is not third-party acceptance.
//! The real route extends the existing credential proxy (`#647`); `swap-proxy` here is a test double
//! standing in for it, exactly as `vendor-mcp` stands in for a real vendor MCP. The core-side proof
//! against the REAL proxy is `maxplayer-core`'s `seller_exec::mcp_tool_tests`.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const VENDOR_MCP: &str = env!("CARGO_BIN_EXE_vendor-mcp");
const SWAP_PROXY: &str = env!("CARGO_BIN_EXE_swap-proxy");
const BRIDGE: &str = env!("CARGO_BIN_EXE_mcp-http-bridge");

// Synthetic literals. The "real" token is what the vendor accepts; the placeholder is what the job
// holds. Neither is a real credential.
const REAL: &str = "real-vendor-token-synthetic";
const PLACEHOLDER: &str = "ph-synthetic-placeholder";

struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a server on an ephemeral port and read the address from its first stdout line.
fn spawn(bin: &str, args: &[&str]) -> Server {
    let mut child = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn server");
    let stdout = child.stdout.take().expect("server stdout");
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).expect("server banner");
    let addr = line
        .trim()
        .rsplit_once(' ')
        .map(|(_, a)| a.to_string())
        .expect("banner carries an address");
    Server { child, addr }
}

fn spawn_vendor(extra: &[&str]) -> Server {
    let mut args = vec!["--listen", "127.0.0.1:0", "--token", REAL, "--watch-token", PLACEHOLDER];
    args.extend_from_slice(extra);
    spawn(VENDOR_MCP, &args)
}

fn spawn_proxy(vendor: &Server, allow: &str) -> Server {
    let vendor_url = format!("http://{}", vendor.addr);
    spawn(
        SWAP_PROXY,
        &["--listen", "127.0.0.1:0", "--placeholder", PLACEHOLDER, "--real", REAL, "--upstream", &vendor_url, "--allow", allow],
    )
}

fn vendor_stats(addr: &str) -> Value {
    let resp = maxplayer_tool_kit::http::request(&format!("http://{addr}"), "GET", "/admin/stats", None, None)
        .expect("vendor stats");
    assert_eq!(resp.status, 200);
    serde_json::from_slice(&resp.body).expect("stats json")
}

/// Drive the stdio MCP bridge the way an agent in a job container would.
struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Bridge {
    /// The production shape: the three facts as flags, the environment empty of them.
    fn spawn(proxy_url: &str, placeholder: &str) -> Bridge {
        Self::spawn_with(
            &["--proxy-url", proxy_url, "--path", "/mcp", "--placeholder", placeholder],
            &[],
        )
    }

    fn spawn_with(args: &[&str], env: &[(&str, &str)]) -> Bridge {
        let mut command = Command::new(BRIDGE);
        command.args(args).env_clear().env("PATH", "/usr/bin:/bin");
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn bridge");
        let stdin = child.stdin.take().expect("bridge stdin");
        let stdout = BufReader::new(child.stdout.take().expect("bridge stdout"));
        Bridge { child, stdin, stdout, next_id: 1 }
    }

    fn write_line(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn read_line(&mut self) -> Value {
        let mut resp = String::new();
        self.stdout.read_line(&mut resp).expect("read response");
        assert!(!resp.trim().is_empty(), "bridge returned an empty line");
        serde_json::from_str(resp.trim()).expect("response json")
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write_line(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string());
        let reply = self.read_line();
        assert_eq!(reply["id"], json!(id), "the reply must answer the request just sent: {reply}");
        reply
    }

    /// A notification: no id, and the bridge must write nothing back for it.
    fn notify(&mut self, method: &str, params: Value) {
        self.write_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The happy path: the job holds only a placeholder, the proxy swaps in the real credential, and the
/// vendor authenticates and runs the tool. The placeholder never reaches the vendor.
#[test]
fn proxy_swap_runs_the_tool_without_the_job_holding_the_credential() {
    let vendor = spawn_vendor(&[]);
    let proxy = spawn_proxy(&vendor, &vendor.addr);
    let proxy_url = format!("http://{}", proxy.addr);

    let mut bridge = Bridge::spawn(&proxy_url, PLACEHOLDER);
    let init = bridge.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}}));
    assert_eq!(init["result"]["serverInfo"]["name"], json!("vendor-mcp"));

    let listed = bridge.request("tools/list", json!({}));
    assert_eq!(listed["result"]["tools"][0]["name"], json!("vendor-echo"));

    let called = bridge.request("tools/call", json!({"name": "vendor-echo", "arguments": {"text": "hello"}}));
    assert_eq!(called["result"]["isError"], json!(false), "the swapped call must succeed: {called}");
    assert_eq!(called["result"]["content"][0]["text"], json!("HELLO"));

    let s = vendor_stats(&vendor.addr);
    assert_eq!(s["auth_ok"], json!(3), "the real token authenticated initialize, tools/list, tools/call");
    assert_eq!(s["auth_fail"], json!(0));
    assert_eq!(s["calls"], json!(1), "the tool ran once");
    assert_eq!(s["saw_watch_token"], json!(false), "the placeholder never reached the vendor; the proxy swapped it");
}

/// The Streamable HTTP shape a real vendor answers with: SSE bodies under chunked framing, a session
/// id issued on `initialize` and required after, and the negotiated protocol version echoed back.
/// A notification travels through and draws no reply line.
#[test]
fn the_bridge_speaks_streamable_http_with_sse_a_session_and_chunked_framing() {
    let vendor = spawn_vendor(&["--sse", "--session"]);
    let proxy = spawn_proxy(&vendor, &vendor.addr);
    let proxy_url = format!("http://{}", proxy.addr);

    let mut bridge = Bridge::spawn(&proxy_url, PLACEHOLDER);
    let init = bridge.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}}));
    assert_eq!(init["result"]["protocolVersion"], json!("2025-06-18"), "{init}");

    // The client's `notifications/initialized`, as every MCP client sends it. No reply line: the
    // next line read back must be the answer to the NEXT request.
    bridge.notify("notifications/initialized", json!({}));

    let listed = bridge.request("tools/list", json!({}));
    assert_eq!(listed["result"]["tools"][0]["name"], json!("vendor-echo"), "{listed}");

    let called = bridge.request("tools/call", json!({"name": "vendor-echo", "arguments": {"text": "stream"}}));
    assert_eq!(called["result"]["content"][0]["text"], json!("STREAM"), "{called}");

    let s = vendor_stats(&vendor.addr);
    assert_eq!(s["auth_ok"], json!(4), "initialize, the notification, tools/list, tools/call");
    assert_eq!(s["notifications"], json!(1), "the notification reached the vendor");
    assert_eq!(s["sessions_issued"], json!(1));
    assert_eq!(s["session_missing"], json!(0), "every request after initialize echoed the session id");
    assert_eq!(
        s["protocol_header_ok"],
        json!(3),
        "the notification, tools/list and tools/call carried MCP-Protocol-Version"
    );
    assert_eq!(s["calls"], json!(1));
    assert_eq!(s["saw_watch_token"], json!(false));
}

/// A placeholder that skips the proxy is worthless: the vendor rejects it directly.
#[test]
fn a_placeholder_that_bypasses_the_proxy_is_worthless() {
    let vendor = spawn_vendor(&[]);

    let mut bridge = Bridge::spawn(&format!("http://{}", vendor.addr), PLACEHOLDER);
    let called = bridge.request("tools/call", json!({"name": "vendor-echo", "arguments": {"text": "hello"}}));
    assert!(called.get("error").is_some(), "the vendor must reject a bare placeholder: {called}");

    let s = vendor_stats(&vendor.addr);
    assert_eq!(s["auth_ok"], json!(0), "the placeholder never authenticated");
    assert!(s["auth_fail"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(s["saw_watch_token"], json!(true), "the placeholder reached the vendor here and was refused");
    assert_eq!(s["calls"], json!(0), "no tool ran");
}

/// The proxy substitutes only for an allowlisted destination.
#[test]
fn the_proxy_refuses_a_destination_not_on_the_allowlist() {
    let vendor = spawn_vendor(&[]);
    let proxy = spawn_proxy(&vendor, "127.0.0.1:1");
    let mut bridge = Bridge::spawn(&format!("http://{}", proxy.addr), PLACEHOLDER);
    let called = bridge.request("tools/call", json!({"name": "vendor-echo", "arguments": {"text": "hi"}}));
    assert!(called.get("error").is_some(), "a non-allowlisted destination must be refused: {called}");

    let s = vendor_stats(&vendor.addr);
    assert_eq!(s["auth_ok"], json!(0), "nothing reached the vendor with the real token");
    assert_eq!(s["calls"], json!(0));
}

/// The proxy substitutes only for the placeholder it was told about.
#[test]
fn the_proxy_refuses_an_unknown_placeholder() {
    let vendor = spawn_vendor(&[]);
    let proxy = spawn_proxy(&vendor, &vendor.addr);
    // The bridge carries a different token than the proxy knows.
    let mut bridge = Bridge::spawn(&format!("http://{}", proxy.addr), "some-other-token");
    let called = bridge.request("tools/call", json!({"name": "vendor-echo", "arguments": {"text": "hi"}}));
    assert!(called.get("error").is_some(), "an unknown placeholder must be refused: {called}");

    let s = vendor_stats(&vendor.addr);
    assert_eq!(s["auth_ok"], json!(0));
    assert_eq!(s["calls"], json!(0));
}

/// A server may ask the client something in the middle of a response and wait for the answer
/// before it finishes the stream (the Streamable HTTP transport permits it). The bridge must
/// forward the server's request as soon as its event arrives, post the client's answer while the
/// stream is still open, and only then read the tool result. A bridge that waits for the end of
/// the stream before it reads stdin deadlocks here: the vendor's call times out unanswered.
#[test]
fn the_bridge_relays_a_server_request_mid_stream_and_posts_the_answer_while_the_stream_is_open() {
    let vendor = spawn_vendor(&["--sse", "--session", "--server-request"]);
    let proxy = spawn_proxy(&vendor, &vendor.addr);
    let proxy_url = format!("http://{}", proxy.addr);

    let mut bridge = Bridge::spawn(&proxy_url, PLACEHOLDER);
    let init = bridge.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}}));
    assert_eq!(init["result"]["protocolVersion"], json!("2025-06-18"), "{init}");
    bridge.notify("notifications/initialized", json!({}));

    // The call. The FIRST line back is the server's own request, not the result.
    bridge.write_line(&json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "vendor-echo", "arguments": {"text": "interactive"}}}).to_string());
    let server_request = bridge.read_line();
    assert_eq!(server_request["method"], json!("ping"), "the server's request arrives before the stream ends: {server_request}");
    assert_eq!(server_request["id"], json!("srv-1"));

    // The client answers, as an agent would: a JSON-RPC RESPONSE on stdin, while the stream is open.
    bridge.write_line(&json!({"jsonrpc": "2.0", "id": "srv-1", "result": {}}).to_string());

    // Only now does the vendor finish the call.
    let called = bridge.read_line();
    assert_eq!(called["id"], json!(7), "the next line is the tool result, not a reply to the response: {called}");
    assert_eq!(called["result"]["content"][0]["text"], json!("INTERACTIVE"), "{called}");

    // The response POST drew no line: the next request reads its own reply first.
    let listed = bridge.request("tools/list", json!({}));
    assert_eq!(listed["result"]["tools"][0]["name"], json!("vendor-echo"), "{listed}");

    let s = vendor_stats(&vendor.addr);
    assert_eq!(s["server_requests_answered"], json!(1), "the vendor received the client's answer while it waited: {s}");
    assert_eq!(s["server_requests_unanswered"], json!(0), "{s}");
    assert_eq!(s["stray_responses"], json!(0), "{s}");
    assert_eq!(s["calls"], json!(1));
    assert_eq!(s["auth_ok"], json!(5), "initialize, the notification, tools/call, the response, tools/list");
    assert_eq!(s["session_missing"], json!(0), "the response carried the session id too");
    assert_eq!(s["saw_watch_token"], json!(false));
}

/// Two requests in flight at once: the bridge posts the second while the first stream is still
/// open, and each reply lands on its own id. The vendor holds the first call open until its
/// server request is answered, so the second request's reply is read first.
#[test]
fn the_bridge_serves_two_requests_at_once_and_each_reply_carries_its_own_id() {
    let vendor = spawn_vendor(&["--sse", "--server-request"]);
    let proxy = spawn_proxy(&vendor, &vendor.addr);
    let proxy_url = format!("http://{}", proxy.addr);

    let mut bridge = Bridge::spawn(&proxy_url, PLACEHOLDER);
    bridge.write_line(&json!({"jsonrpc": "2.0", "id": 100, "method": "tools/call", "params": {"name": "vendor-echo", "arguments": {"text": "first"}}}).to_string());
    let server_request = bridge.read_line();
    assert_eq!(server_request["id"], json!("srv-1"), "{server_request}");
    // While the first call waits on its answer, a second request goes through.
    let listed = bridge.request("tools/list", json!({}));
    assert_eq!(listed["result"]["tools"][0]["name"], json!("vendor-echo"), "{listed}");
    // Now answer the server, and the first call completes.
    bridge.write_line(&json!({"jsonrpc": "2.0", "id": "srv-1", "result": {}}).to_string());
    let first = bridge.read_line();
    assert_eq!(first["id"], json!(100), "{first}");
    assert_eq!(first["result"]["content"][0]["text"], json!("FIRST"));

    let s = vendor_stats(&vendor.addr);
    assert_eq!(s["server_requests_answered"], json!(1), "{s}");
    assert_eq!(s["calls"], json!(1));
}

/// A vendor that keeps its stream open after the result (keepalive comments, no close) must not
/// keep a bridge worker and a connection alive per request. Each reply arrives at once, and once
/// the answer is written the bridge closes its side: the vendor's count of held streams returns
/// to zero.
#[test]
fn a_request_worker_ends_when_its_answer_arrives_even_if_the_vendor_holds_the_stream() {
    let vendor = spawn_vendor(&["--sse", "--hold-stream"]);
    let proxy = spawn_proxy(&vendor, &vendor.addr);
    let proxy_url = format!("http://{}", proxy.addr);

    let mut bridge = Bridge::spawn(&proxy_url, PLACEHOLDER);
    for i in 0..24u64 {
        let started = std::time::Instant::now();
        let called = bridge.request("tools/call", json!({"name": "vendor-echo", "arguments": {"text": format!("held-{i}")}}));
        assert_eq!(called["result"]["content"][0]["text"], json!(format!("HELD-{i}")), "{called}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "a reply must not wait for the stream's end");
    }

    // The vendor opened 24 held streams; the bridge closed every one of them once it had its answer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let s = loop {
        let s = vendor_stats(&vendor.addr);
        if s["streams_open"] == json!(0) || std::time::Instant::now() >= deadline {
            break s;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(s["streams_held_total"], json!(24), "{s}");
    assert_eq!(s["streams_open"], json!(0), "the bridge must close a stream once its answer is out: {s}");
    assert_eq!(s["calls"], json!(24));
}

/// The in-flight bound: forty requests at once, the vendor answering each after 1.5 s. Exactly
/// thirty-two are served; the eight over the bound get one error line each, at once, on their own
/// ids. Every id comes back exactly once.
#[test]
fn requests_over_the_in_flight_bound_get_one_error_line_and_the_rest_are_served() {
    let vendor = spawn_vendor(&["--delay-ms", "1500"]);
    let proxy = spawn_proxy(&vendor, &vendor.addr);
    let proxy_url = format!("http://{}", proxy.addr);

    let mut bridge = Bridge::spawn(&proxy_url, PLACEHOLDER);
    for id in 1..=40u64 {
        bridge.write_line(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "vendor-echo", "arguments": {"text": format!("n{id}")}}}).to_string());
    }

    let started = std::time::Instant::now();
    let mut refused_at = Vec::new();
    let mut served = std::collections::BTreeSet::new();
    let mut refused = std::collections::BTreeSet::new();
    for _ in 0..40 {
        let reply = bridge.read_line();
        let id = reply["id"].as_u64().expect("every reply carries a numeric id");
        if reply.get("error").is_some() {
            assert_eq!(reply["error"]["code"], json!(-32000), "{reply}");
            assert!(reply["error"]["message"].as_str().unwrap_or("").contains("too many requests in flight"), "{reply}");
            refused_at.push(started.elapsed());
            assert!(refused.insert(id), "id {id} refused twice");
        } else {
            assert_eq!(reply["result"]["content"][0]["text"], json!(format!("N{id}").to_uppercase()), "{reply}");
            assert!(served.insert(id), "id {id} served twice");
        }
    }
    assert_eq!(refused.len(), 8, "eight requests over the bound: {refused:?}");
    assert_eq!(served.len(), 32, "thirty-two requests served: {served:?}");
    assert!(served.is_disjoint(&refused));
    for at in &refused_at {
        assert!(*at < std::time::Duration::from_millis(1200), "a refusal is immediate, not after the vendor's delay: {at:?}");
    }
    assert_eq!(vendor_stats(&vendor.addr)["calls"], json!(32), "the refused requests never reached the vendor");
}

/// The environment shape still works for a hand-run bridge, and a line that is not JSON is answered
/// with a parse error instead of being forwarded.
#[test]
fn the_bridge_also_reads_the_environment_and_answers_a_malformed_line_locally() {
    let vendor = spawn_vendor(&[]);
    let proxy = spawn_proxy(&vendor, &vendor.addr);
    let proxy_url = format!("http://{}", proxy.addr);

    let mut bridge = Bridge::spawn_with(
        &[],
        &[("PROXY_URL", proxy_url.as_str()), ("TOOL_PLACEHOLDER", PLACEHOLDER), ("MCP_PATH", "/mcp")],
    );
    bridge.write_line("this is not json");
    let parse_error = bridge.read_line();
    assert_eq!(parse_error["error"]["code"], json!(-32700), "{parse_error}");
    assert_eq!(vendor_stats(&vendor.addr)["auth_ok"], json!(0), "a malformed line is never forwarded");

    let listed = bridge.request("tools/list", json!({}));
    assert_eq!(listed["result"]["tools"][0]["name"], json!("vendor-echo"));
}
