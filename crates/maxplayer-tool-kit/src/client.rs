//! Client for the holder's seller-only Unix socket.

use crate::proto;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// One request, one response. Returns the JSON-RPC `result`, or a formatted error carrying the
/// application code so callers can distinguish "refused" from "unhealthy" from "unreachable".
pub fn call(socket: &Path, method: &str, params: Value) -> Result<Value, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("holder endpoint {} unreachable: {e}", socket.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("set timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("set timeout: {e}"))?;

    let mut writer = stream.try_clone().map_err(|e| format!("clone socket: {e}"))?;
    writer
        .write_all(proto::request_line(1, method, params).as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|e| format!("write request: {e}"))?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;
    if line.trim().is_empty() {
        return Err("holder closed the connection without responding".into());
    }

    let parsed: proto::RpcResponse =
        serde_json::from_str(line.trim()).map_err(|e| format!("malformed response: {e}"))?;
    if let Some(err) = parsed.error {
        return Err(format!("[{}] {}", err.code, err.message));
    }
    parsed.result.ok_or_else(|| "response carried neither result nor error".to_string())
}
