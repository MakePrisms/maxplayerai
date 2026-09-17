//! A deliberately tiny HTTP/1.1 subset: enough for a fake vendor service, its CLI, and the Proxy
//! swap transport shim; small enough to audit. Not a general-purpose client or server. One request
//! per connection (`Connection: close`), no keep-alive.
//!
//! Chunked transfer coding is DECODED on the client side, and that is not optional: the real
//! credential proxy (`maxplayer-core` `#647`) is hyper-based and drops the upstream's framing
//! headers, so it re-frames every streamed response as chunked — a JSON reply and an SSE stream
//! alike. A client that cannot decode chunks reads framing bytes as body. On the server side a
//! caller chooses chunked framing explicitly, so a test can put the decoder on the path.
//!
//! The client reads a body as it arrives. [`request_streaming`] returns the status and the headers
//! as soon as the head is complete, and a [`BodyReader`] that yields decoded bytes chunk by chunk.
//! That is what lets the bridge forward a server request from an open SSE stream before the vendor
//! closes the response. [`request_with`] is the same path read to its end. On the server side,
//! [`write_response_head`], [`write_chunk`] and [`write_last_chunk`] write a body in parts, so a
//! fake vendor can hold a response open, and a relay can forward bytes as it reads them.

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
    if chunked {
        write_response_head(&mut out, status, headers, BodyFraming::Chunked)?;
        write_chunk(&mut out, body)?;
        write_last_chunk(&mut out)?;
    } else {
        write_response_head(&mut out, status, headers, BodyFraming::Length(body.len()))?;
        out.write_all(body)?;
    }
    out.flush()
}

/// How a response body is framed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFraming {
    /// A `Content-Length` header with this many body bytes to follow.
    Length(usize),
    /// `Transfer-Encoding: chunked`: the body follows as chunks, see [`write_chunk`].
    Chunked,
}

/// Write the status line and the headers, then the framing headers this module owns. The body
/// follows: written by the caller for [`BodyFraming::Length`], or as chunks for
/// [`BodyFraming::Chunked`]. Framing headers in `headers` are ignored.
pub fn write_response_head<W: Write>(
    mut out: W,
    status: u16,
    headers: &[(&str, &str)],
    framing: BodyFraming,
) -> std::io::Result<()> {
    write!(out, "HTTP/1.1 {status} {}\r\n", reason_phrase(status))?;
    for (name, value) in headers {
        if is_framing_header(name) {
            continue;
        }
        write!(out, "{name}: {value}\r\n")?;
    }
    match framing {
        BodyFraming::Chunked => write!(out, "Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?,
        BodyFraming::Length(len) => write!(out, "Content-Length: {len}\r\nConnection: close\r\n\r\n")?,
    }
    out.flush()
}

/// Write one chunk of a chunked body and flush it, so the peer reads it now. An empty `data`
/// writes nothing: a zero-size chunk would end the body.
pub fn write_chunk<W: Write>(mut out: W, data: &[u8]) -> std::io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(out, "{:x}\r\n", data.len())?;
    out.write_all(data)?;
    out.write_all(b"\r\n")?;
    out.flush()
}

/// End a chunked body.
pub fn write_last_chunk<W: Write>(mut out: W) -> std::io::Result<()> {
    out.write_all(b"0\r\n\r\n")?;
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
/// `headers` are dropped; this function writes its own. A chunked response body is decoded. This is
/// [`request_streaming`] read to the end of its body.
pub fn request_with(
    base_url: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<Response> {
    request_streaming(base_url, method, path, headers, body, timeout)?.into_response()
}

/// Send one request and return as soon as the response head is complete. The body is read from the
/// returned [`StreamingResponse`] as it arrives: a chunked body is decoded chunk by chunk, a
/// `Content-Length` body ends at its length, and any other body ends at the close of the connection.
/// `timeout` bounds each write and each read, not the whole response, so a long stream stays open
/// while the peer keeps sending.
pub fn request_streaming(
    base_url: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<StreamingResponse> {
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

    let mut reader = BufReader::new(stream);
    let (status, headers) = read_response_head(&mut reader)?;
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let framing = if chunked {
        Framing::Chunked { remaining: 0, started: false, done: false }
    } else if let Some(len) = headers.get("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
        Framing::Length { remaining: len }
    } else {
        Framing::ToEnd
    };
    Ok(StreamingResponse { status, headers, body: BodyReader { inner: reader, framing } })
}

/// The status line and the headers of one response, read line by line. Header names are lowercased;
/// a repeated header keeps its last value.
fn read_response_head<R: BufRead>(reader: &mut R) -> std::io::Result<(u16, BTreeMap<String, String>)> {
    let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(invalid("no status"));
    }
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| invalid("no status"))?;
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(invalid("no header terminator"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Ok((status, headers));
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
}

/// A response whose body is still on the wire. Read it through [`std::io::Read`]; each read returns
/// the decoded bytes that have arrived, so a caller can act on a part of the body before the rest.
pub struct StreamingResponse {
    pub status: u16,
    /// Header names lowercased. A repeated header keeps its last value.
    pub headers: BTreeMap<String, String>,
    body: BodyReader<BufReader<TcpStream>>,
}

impl StreamingResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }

    /// The `Content-Length` the peer declared, when it declared one. A relay that keeps the framing
    /// of the upstream reads it here.
    pub fn content_length(&self) -> Option<usize> {
        match self.body.framing {
            Framing::Length { remaining } => Some(remaining),
            _ => None,
        }
    }

    /// Read the rest of the body and return the whole response.
    pub fn into_response(mut self) -> std::io::Result<Response> {
        let mut body = Vec::new();
        self.body.read_to_end(&mut body)?;
        Ok(Response { status: self.status, headers: self.headers, body })
    }
}

impl Read for StreamingResponse {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.body.read(buf)
    }
}

/// Where one body ends, and how to find its bytes on the wire.
#[derive(Debug, Clone, Copy)]
enum Framing {
    /// `Content-Length`: this many bytes remain.
    Length { remaining: usize },
    /// `Transfer-Encoding: chunked`: `remaining` bytes of the current chunk are unread; `started`
    /// says a chunk was read before (so a CRLF precedes the next size line); `done` says the
    /// last chunk was read.
    Chunked { remaining: usize, started: bool, done: bool },
    /// No framing: the body ends when the peer closes.
    ToEnd,
}

/// A body decoded as it is read. A read returns the bytes that have arrived, never more than one
/// chunk at a time, so a chunk the peer flushes is available to the caller at once. Malformed or
/// truncated chunked framing is an error, never a silently shortened body.
pub struct BodyReader<R: BufRead> {
    inner: R,
    framing: Framing,
}

impl<R: BufRead> Read for BodyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Self { inner, framing } = self;
        match framing {
            Framing::ToEnd => inner.read(buf),
            Framing::Length { remaining } => {
                if *remaining == 0 {
                    return Ok(0);
                }
                let want = buf.len().min(*remaining);
                let n = inner.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "body shorter than its Content-Length",
                    ));
                }
                *remaining -= n;
                Ok(n)
            }
            Framing::Chunked { remaining, started, done } => {
                if *done {
                    return Ok(0);
                }
                if *remaining == 0 {
                    if *started {
                        expect_crlf(inner)?;
                    }
                    *started = true;
                    let size = read_chunk_size(inner)?;
                    if size == 0 {
                        skip_trailers(inner)?;
                        *done = true;
                        return Ok(0);
                    }
                    *remaining = size;
                }
                let want = buf.len().min(*remaining);
                let n = inner.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "chunk truncated"));
                }
                *remaining -= n;
                Ok(n)
            }
        }
    }
}

/// One chunk-size line: hex, an optional extension after `;`, CRLF.
fn read_chunk_size<R: BufRead>(inner: &mut R) -> std::io::Result<usize> {
    let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
    let mut line = String::new();
    if inner.read_line(&mut line)? == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "chunk size line not terminated"));
    }
    let Some(line) = line.strip_suffix("\r\n") else {
        return Err(invalid("chunk size line not terminated"));
    };
    let size_text = line.split(';').next().unwrap_or("").trim();
    usize::from_str_radix(size_text, 16).map_err(|_| invalid("chunk size not hex"))
}

/// The CRLF that ends a chunk's data.
fn expect_crlf<R: BufRead>(inner: &mut R) -> std::io::Result<()> {
    let mut crlf = [0u8; 2];
    inner
        .read_exact(&mut crlf)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "chunk truncated"))?;
    if crlf != *b"\r\n" {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk not terminated"));
    }
    Ok(())
}

/// Trailers run to a blank line. Nothing here reads them; the end of input ends them too.
fn skip_trailers<R: BufRead>(inner: &mut R) -> std::io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if inner.read_line(&mut line)? == 0 || line.trim_end_matches(['\r', '\n']).is_empty() {
            return Ok(());
        }
    }
}

/// Decode a complete chunked transfer-coded body. Chunk extensions and trailers are dropped.
/// Malformed or truncated framing is an error, never a silently shortened body. The same decoder
/// as [`BodyReader`], run over bytes already in memory.
pub fn decode_chunked(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut reader = BodyReader { inner: raw, framing: Framing::Chunked { remaining: 0, started: false, done: false } };
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
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
    fn a_chunk_is_readable_before_the_body_ends() {
        // Only the first chunk is on the wire. The reader hands it over now, and reports the
        // missing rest as an error on the NEXT read, not as a short body.
        let wire: &[u8] = b"5\r\nhello\r\n";
        let mut reader = BodyReader { inner: wire, framing: Framing::Chunked { remaining: 0, started: false, done: false } };
        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).expect("the first chunk");
        assert_eq!(&buf[..n], b"hello");
        let error = reader.read(&mut buf).expect_err("the stream ended inside the framing");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_read_never_crosses_a_chunk_boundary_and_the_last_chunk_ends_the_body() {
        let wire: &[u8] = b"3\r\nabc\r\n2\r\nde\r\n0\r\nx-trailer: 1\r\n\r\nafter";
        let mut reader = BodyReader { inner: wire, framing: Framing::Chunked { remaining: 0, started: false, done: false } };
        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).expect("chunk one");
        assert_eq!(&buf[..n], b"abc");
        let n = reader.read(&mut buf).expect("chunk two");
        assert_eq!(&buf[..n], b"de");
        assert_eq!(reader.read(&mut buf).expect("the end"), 0);
        assert_eq!(reader.read(&mut buf).expect("still the end"), 0);
    }

    #[test]
    fn a_content_length_body_ends_at_its_length_and_a_short_one_is_an_error() {
        let wire: &[u8] = b"abcdef";
        let mut reader = BodyReader { inner: wire, framing: Framing::Length { remaining: 4 } };
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read");
        assert_eq!(out, b"abcd");

        let wire: &[u8] = b"ab";
        let mut reader = BodyReader { inner: wire, framing: Framing::Length { remaining: 4 } };
        let error = reader.read_to_end(&mut Vec::new()).expect_err("short body");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn the_response_head_parses_status_and_lowercased_headers() {
        let wire: &[u8] = b"HTTP/1.1 202 Accepted\r\nContent-Type: text/plain\r\nMcp-Session-Id: s-1\r\n\r\nbody";
        let mut reader = BufReader::new(wire);
        let (status, headers) = read_response_head(&mut reader).expect("head");
        assert_eq!(status, 202);
        assert_eq!(headers.get("mcp-session-id").map(String::as_str), Some("s-1"));
        assert_eq!(headers.get("content-type").map(String::as_str), Some("text/plain"));
        let mut rest = String::new();
        reader.read_to_string(&mut rest).expect("rest");
        assert_eq!(rest, "body", "the head parser must not consume body bytes");

        let wire: &[u8] = b"HTTP/1.1 200 OK\r\nX: 1\r\n";
        let error = read_response_head(&mut BufReader::new(wire)).expect_err("no terminator");
        assert_eq!(error.to_string(), "no header terminator");
    }

    #[test]
    fn the_streaming_head_writer_and_the_chunk_writers_compose_into_one_body() {
        let mut wire = Vec::new();
        write_response_head(&mut wire, 200, &[("Content-Type", "text/event-stream"), ("Content-Length", "9")], BodyFraming::Chunked)
            .expect("head");
        write_chunk(&mut wire, b"data: 1\n\n").expect("chunk");
        write_chunk(&mut wire, b"").expect("an empty chunk writes nothing");
        write_chunk(&mut wire, b"data: 2\n\n").expect("chunk");
        write_last_chunk(&mut wire).expect("end");
        let mut reader = BufReader::new(wire.as_slice());
        let (status, headers) = read_response_head(&mut reader).expect("head");
        assert_eq!(status, 200);
        assert!(!headers.contains_key("content-length"), "a caller's framing header is dropped");
        assert_eq!(headers.get("transfer-encoding").map(String::as_str), Some("chunked"));
        let mut body = BodyReader { inner: reader, framing: Framing::Chunked { remaining: 0, started: false, done: false } };
        let mut out = Vec::new();
        body.read_to_end(&mut out).expect("body");
        assert_eq!(out, b"data: 1\n\ndata: 2\n\n");
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
