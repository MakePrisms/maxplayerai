//! `tool-mcp-bridge` — the MCP server an agent runs **inside a job container**.
//!
//! This is the piece an ACP `mcpServers` stdio entry names as its command: the agent spawns it,
//! speaks MCP JSON-RPC over stdio to it, and it forwards to the holder's per-job Unix socket.
//!
//! It is a byte-faithful proxy: the request line goes out as it arrived, ids and all, and the
//! holder's response line comes back unchanged. Nothing here validates, and nothing here holds
//! a credential — validation and custody are the holder's, on the other side of the socket.
//! Compromising this process gains exactly what the socket already allows.
//!
//! The socket comes from `--socket <path>`, else `HOLDER_JOB_SOCKET`, else `/run/holder/job.sock`.
//! The seller daemon passes the flag (`held_tool::JobToolEndpoint::attachments`), one bridge per
//! held tool, each at `/run/holder/<server name>/job.sock`: a stdio MCP server's arguments reach
//! the child on every harness, its environment only if the harness maps it. The environment form
//! stays for a hand-run bridge and the kit demo.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let socket = args
        .iter()
        .position(|a| a == "--socket")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| std::env::var("HOLDER_JOB_SOCKET").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/holder/job.sock"));

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // A notification carries no id and gets no reply — answering one would desynchronize
        // a strict MCP client.
        let is_notification = match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => v.get("id").is_none(),
            Err(_) => false,
        };

        match forward(&socket, trimmed) {
            Ok(response) => {
                if !is_notification {
                    let _ = writeln!(stdout, "{}", response.trim());
                    let _ = stdout.flush();
                }
            }
            Err(e) => {
                if !is_notification {
                    // Keep the caller's id so the error lands on the right request.
                    let id = serde_json::from_str::<serde_json::Value>(trimmed)
                        .ok()
                        .and_then(|v| v.get("id").cloned())
                        .unwrap_or(serde_json::Value::Null);
                    let err = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32603, "message": format!("holder endpoint unavailable: {e}")},
                    });
                    let _ = writeln!(stdout, "{err}");
                    let _ = stdout.flush();
                }
            }
        }
    }
}

fn forward(socket: &std::path::Path, line: &str) -> std::io::Result<String> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut writer = stream.try_clone()?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    if response.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "holder closed the connection without responding",
        ));
    }
    Ok(response)
}
