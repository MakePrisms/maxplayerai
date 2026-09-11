//! A deliberately tiny HTTP/1.1 subset: enough for a fake vendor service, its CLI, and the Proxy
//! swap transport shim; small enough to audit. Not a general-purpose client or server. One request
//! per connection (`Connection: close`), no keep-alive.
//!
//! Chunked transfer coding is DECODED on the client side, and that is not optional: the real
//! credential proxy (`maxplayer-core` `#647`) is hyper-based and drops the upstream's framing
//! headers, so it re-frames every streamed response as chunked — a JSON reply and an SSE stream
//! alike. A client that cannot decode chunks reads framing bytes as body. On the server side a
//! caller chooses chunked framing explicitly, so a test can put the decoder on the path.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

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

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Write one JSON response with a `Content-Length`.
pub fn write_response<W: Write>(out: W, status: u16, body: &[u8]) -> std::io::Result<()> {
    write_response_with(out, status, &[("Content-Type", "application/json")], body, false)
}

/// Write one response with the given headers. `chunked` frames the body as one chunk plus the
/// terminator, the way a streaming server does, so a client's chunked decoder is on the path.
/// Framing headers (`Content-Length`, `Transfer-Encoding`, `Connection`) are this function's to
/// write; a caller's copy of them is ignored.
pub fn write_response_with<W: Write>(
    mut out: W,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
    chunked: bool,
) -> std::io::Result<()> {
    write!(out, "HTTP/1.1 {status} {}\r\n", reason_phrase(status))?;
    for (name, value) in headers {
        if is_framing_header(name) {
            continue;
        }
        write!(out, "{name}: {value}\r\n")?;
    }
    if chunked {
        write!(out, "Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
        if !body.is_empty() {
            write!(out, "{:x}\r\n", body.len())?;
            out.write_all(body)?;
            out.write_all(b"\r\n")?;
        }
        out.write_all(b"0\r\n\r\n")?;
    } else {
        write!(out, "Content-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
        out.write_all(body)?;
    }
    out.flush()
}

fn is_framing_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("host")
}

pub struct Response {
    pub status: u16,
    /// Header names lowercased. A repeated header keeps its last value.
    pub headers: BTreeMap<String, String>,
    /// The body with any chunked framing already removed.
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }
}

/// Blocking single-shot client request: JSON content type, an optional bearer, a 10 s read timeout.
pub fn request(
    base_url: &str,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&[u8]>,
) -> std::io::Result<Response> {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(token) = bearer {
        // The token goes in a header, never in a URL or an argv.
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    request_with(base_url, method, path, &headers, body, Duration::from_secs(10))
}

/// Blocking single-shot client request with explicit headers and a read timeout. Framing headers in
/// `headers` are dropped; this function writes its own. A chunked response body is decoded.
pub fn request_with(
    base_url: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<Response> {
    let authority = base_url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "only http:// is supported"))?
        .trim_end_matches('/');
    let mut stream = TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let empty: &[u8] = &[];
    let body = body.unwrap_or(empty);
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        if is_framing_header(name) {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let split = find(&raw, b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no header terminator"))?;
    let head_txt = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head_txt.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no status"))?;
    let mut response_headers = BTreeMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            response_headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let payload = &raw[split + 4..];
    let chunked = response_headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked { decode_chunked(payload)? } else { payload.to_vec() };
    Ok(Response { status, headers: response_headers, body })
}

/// Decode a complete chunked transfer-coded body. Chunk extensions and trailers are dropped.
/// Malformed or truncated framing is an error, never a silently shortened body.
pub fn decode_chunked(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
    let mut out = Vec::new();
    let mut rest = raw;
    loop {
        let line_end = find(rest, b"\r\n").ok_or_else(|| invalid("chunk size line not terminated"))?;
        let size_text = std::str::from_utf8(&rest[..line_end]).map_err(|_| invalid("chunk size not text"))?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid("chunk size not hex"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            // Trailers, if any, run to the final blank line; nothing here reads them.
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err(invalid("chunk truncated"));
        }
        out.extend_from_slice(&rest[..size]);
        if &rest[size..size + 2] != b"\r\n" {
            return Err(invalid("chunk not terminated"));
        }
        rest = &rest[size + 2..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunked_body_decodes_to_its_payload() {
        let raw = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw).expect("decode"), b"hello world");
    }

    #[test]
    fn a_truncated_chunk_is_an_error_not_a_short_body() {
        let raw = b"5\r\nhel";
        assert!(decode_chunked(raw).is_err());
        let raw = b"5\r\nhelloXX0\r\n\r\n";
        assert!(decode_chunked(raw).is_err(), "a chunk without its CRLF is malformed");
    }

    #[test]
    fn the_chunked_writer_and_the_decoder_agree() {
        let mut wire = Vec::new();
        write_response_with(&mut wire, 200, &[("Content-Type", "text/event-stream")], b"data: {}\n\n", true)
            .expect("write");
        let split = find(&wire, b"\r\n\r\n").expect("head");
        let head = String::from_utf8_lossy(&wire[..split]).to_ascii_lowercase();
        assert!(head.contains("transfer-encoding: chunked"));
        assert!(!head.contains("content-length"));
        assert_eq!(decode_chunked(&wire[split + 4..]).expect("decode"), b"data: {}\n\n");
    }

    #[test]
    fn a_caller_cannot_override_the_framing_headers() {
        let mut wire = Vec::new();
        write_response_with(&mut wire, 200, &[("Content-Length", "999"), ("X-Ok", "1")], b"ab", false)
            .expect("write");
        let text = String::from_utf8_lossy(&wire).to_string();
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(!text.contains("999"));
        assert!(text.contains("X-Ok: 1\r\n"));
    }
}
