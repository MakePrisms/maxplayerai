//! The thin client half of the buyer boundary.
//!
//! A client (an MCP session, the CLI, a future seller surface) connects to
//! `$MAXPLAYER_HOME/buyer.sock`, writes one JSON request line, and reads one JSON
//! response line. It holds no wallet, no key, and no state — the daemon is the
//! single owner. This is a plain synchronous `UnixStream`, so a caller needs no
//! async runtime to talk to the buyer.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde_json::{Value, json};

use super::protocol::{Request, Response};

/// Client-side failure talking to the buyer.
#[derive(Debug)]
pub enum ClientError {
    /// No daemon is listening on the socket (not started, or wrong home).
    NotRunning(String),
    /// Transport / framing failure.
    Io(String),
    /// The response could not be decoded.
    Decode(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning(message) => write!(
                formatter,
                "no maxplayer buyer is listening ({message}); start it with `maxplayer buyer`"
            ),
            Self::Io(message) => write!(formatter, "buyer client io error: {message}"),
            Self::Decode(message) => write!(formatter, "buyer client decode error: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Send one request to the daemon at `sock_path` and return its response.
pub fn call(
    sock_path: impl AsRef<Path>,
    method: &str,
    params: Value,
) -> Result<Response, ClientError> {
    let stream = UnixStream::connect(sock_path.as_ref())
        .map_err(|error| ClientError::NotRunning(error.to_string()))?;
    let request = Request {
        method: method.to_owned(),
        params,
        id: json!(1),
    };
    let mut line = serde_json::to_string(&request).map_err(|error| ClientError::Io(error.to_string()))?;
    line.push('\n');

    let mut writer = &stream;
    writer
        .write_all(line.as_bytes())
        .map_err(|error| ClientError::Io(error.to_string()))?;
    writer
        .flush()
        .map_err(|error| ClientError::Io(error.to_string()))?;

    let mut reader = BufReader::new(&stream);
    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .map_err(|error| ClientError::Io(error.to_string()))?;
    if response_line.trim().is_empty() {
        return Err(ClientError::Io("empty response from buyer".into()));
    }
    serde_json::from_str(response_line.trim()).map_err(|error| ClientError::Decode(error.to_string()))
}

/// Convenience: call `status` and return the raw response.
pub fn status(sock_path: impl AsRef<Path>) -> Result<Response, ClientError> {
    call(sock_path, "status", json!({}))
}
