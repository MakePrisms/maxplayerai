//! A deliberately tiny HTTP/1.1 subset: enough for a fake vendor service and its CLI,
//! small enough to audit. Not a general-purpose server. No keep-alive, no chunking.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }

    /// Bearer token, if the Authorization header carries one.
    pub fn bearer(&self) -> Option<&str> {
        let raw = self.header("authorization")?;
        let rest = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
        let rest = rest.trim();
        if rest.is_empty() {
            None
        } else {
            Some(rest)
        }
    }
}

/// Read one request. `Ok(None)` means the peer closed without sending anything.
pub fn read_request<R: Read>(stream: R) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed request line"));
    }

    let mut headers = BTreeMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    // Cap the body: a fake vendor still refuses to be a memory sink.
    const MAX_BODY: usize = 1 << 20;
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(Request { method, path, headers, body }))
}

pub fn write_response<W: Write>(mut out: W, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    out.write_all(body)?;
    out.flush()
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Blocking single-shot client request.
pub fn request(
    base_url: &str,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&[u8]>,
) -> std::io::Result<Response> {
    let authority = base_url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "only http:// is supported"))?
        .trim_end_matches('/');
    let mut stream = TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;

    let empty: &[u8] = &[];
    let body = body.unwrap_or(empty);
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(token) = bearer {
        // The token goes in a header, never in a URL or an argv.
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    head.push_str("Content-Type: application/json\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no header terminator"))?;
    let head_txt = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head_txt
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no status"))?;
    Ok(Response { status, body: raw[split + 4..].to_vec() })
}
