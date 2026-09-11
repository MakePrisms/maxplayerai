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
//! standing in for it, exactly as `vendor-mcp` stands in for a real vendor MCP.

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
    fn spawn(proxy_url: &str, placeholder: &str) -> Bridge {
        let mut child = Command::new(BRIDGE)
            .env_clear()
            .env("PROXY_URL", proxy_url)
            .env("TOOL_PLACEHOLDER", placeholder)
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn bridge");
        let stdin = child.stdin.take().expect("bridge stdin");
        let stdout = BufReader::new(child.stdout.take().expect("bridge stdout"));
        Bridge { child, stdin, stdout, next_id: 1 }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        writeln!(self.stdin, "{line}").expect("write request");
        self.stdin.flush().expect("flush request");
        let mut resp = String::new();
        self.stdout.read_line(&mut resp).expect("read response");
        assert!(!resp.trim().is_empty(), "bridge returned an empty line for {method}");
        serde_json::from_str(resp.trim()).expect("response json")
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
    let vendor = spawn(VENDOR_MCP, &["--listen", "127.0.0.1:0", "--token", REAL, "--watch-token", PLACEHOLDER]);
    let vendor_url = format!("http://{}", vendor.addr);
    let proxy = spawn(
        SWAP_PROXY,
        &["--listen", "127.0.0.1:0", "--placeholder", PLACEHOLDER, "--real", REAL, "--upstream", &vendor_url, "--allow", &vendor.addr],
    );
    let proxy_url = format!("http://{}", proxy.addr);

    let mut bridge = Bridge::spawn(&proxy_url, PLACEHOLDER);
    let init = bridge.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}}));
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

/// A placeholder that skips the proxy is worthless: the vendor rejects it directly.
#[test]
fn a_placeholder_that_bypasses_the_proxy_is_worthless() {
    let vendor = spawn(VENDOR_MCP, &["--listen", "127.0.0.1:0", "--token", REAL, "--watch-token", PLACEHOLDER]);

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
    let vendor = spawn(VENDOR_MCP, &["--listen", "127.0.0.1:0", "--token", REAL]);
    let vendor_url = format!("http://{}", vendor.addr);
    let proxy = spawn(
        SWAP_PROXY,
        &["--listen", "127.0.0.1:0", "--placeholder", PLACEHOLDER, "--real", REAL, "--upstream", &vendor_url, "--allow", "127.0.0.1:1"],
    );
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
    let vendor = spawn(VENDOR_MCP, &["--listen", "127.0.0.1:0", "--token", REAL]);
    let vendor_url = format!("http://{}", vendor.addr);
    let proxy = spawn(
        SWAP_PROXY,
        &["--listen", "127.0.0.1:0", "--placeholder", PLACEHOLDER, "--real", REAL, "--upstream", &vendor_url, "--allow", &vendor.addr],
    );
    // The bridge carries a different token than the proxy knows.
    let mut bridge = Bridge::spawn(&format!("http://{}", proxy.addr), "some-other-token");
    let called = bridge.request("tools/call", json!({"name": "vendor-echo", "arguments": {"text": "hi"}}));
    assert!(called.get("error").is_some(), "an unknown placeholder must be refused: {called}");

    let s = vendor_stats(&vendor.addr);
    assert_eq!(s["auth_ok"], json!(0));
    assert_eq!(s["calls"], json!(0));
}
