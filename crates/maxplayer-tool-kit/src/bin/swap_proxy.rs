//! `swap-proxy` — a fake credential-swap proxy, for the proxy-swap demo.
//!
//! It stands in for the real host-side credential proxy (`maxplayer-core` `#647`). It does the one
//! property that route depends on: a request arriving with a per-job PLACEHOLDER in its bearer
//! header is forwarded to the vendor with the REAL credential swapped in, and only for an
//! allowlisted destination. The job never holds the real credential; a leaked placeholder is
//! worthless because the vendor rejects it directly.
//!
//! This is a test double. The production route does NOT add a second proxy: it EXTENDS `#647` by
//! registering the vendor as one more per-job credential (`[[sandbox.mcp_tools]]`), whose upstream
//! joins the proxy's destination allowlist. This binary exists only so the demo is self-contained.
//!
//! Header-only substitution, like the real proxy: the bearer header is swapped, every other request
//! header and the body are forwarded as they are, and the response comes back with its status,
//! headers and framing. Substituting inside the body would be a credential-recovery hole.
//!
//! The response is relayed as it arrives, like the real proxy relays a stream: each part the vendor
//! sends reaches the client now, not at the end of the response. One thread per connection, so a
//! response that the vendor holds open does not stop the next request. Both are what an SSE stream
//! that carries a server request needs.

use maxplayer_tool_kit::http::{
    self, read_request, write_chunk, write_last_chunk, write_response_head, write_response_with, BodyFraming, Request,
};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    chunked: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let listen = flag(&args, "--listen").unwrap_or_else(|| "127.0.0.1:0".to_string());
    let placeholder = flag(&args, "--placeholder").unwrap_or_else(|| fatal("--placeholder is required"));
    let real = flag(&args, "--real").unwrap_or_else(|| fatal("--real is required"));
    let upstream = flag(&args, "--upstream").unwrap_or_else(|| fatal("--upstream <url> is required"));
    // The one destination whose requests may receive the real credential. A request whose upstream
    // is not this is refused WITHOUT substitution.
    let allow = flag(&args, "--allow").unwrap_or_else(|| authority_of(&upstream));

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| fatal(&format!("bind {listen}: {e}")));
    match listener.local_addr() {
        Ok(a) => println!("swap-proxy listening on {a}"),
        Err(_) => println!("swap-proxy listening on {listen}"),
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let settings = Arc::new(Settings { placeholder, real, upstream, allow });
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let settings = Arc::clone(&settings);
        std::thread::spawn(move || serve(conn, &settings));
    }
}

/// The four facts the proxy is started with. Nothing here changes after start.
struct Settings {
    placeholder: String,
    real: String,
    upstream: String,
    allow: String,
}

/// Serve one connection: decide, then relay the vendor's response as it arrives.
fn serve(mut conn: TcpStream, settings: &Settings) {
    let Ok(peer) = conn.try_clone() else { return };
    let req = match read_request(peer) {
        Ok(Some(r)) => r,
        _ => return,
    };
    let headers = match decide(&req, settings) {
        Ok(headers) => headers,
        Err(reply) => {
            write_reply(&mut conn, &reply);
            return;
        }
    };
    // 3. Swap done. Relay the response with its status and its headers, and its body part by part:
    //    a client that must act on an event before the stream ends gets that event now.
    let mut resp = match http::request_streaming(
        &settings.upstream,
        &req.method,
        &req.path,
        &headers,
        Some(&req.body),
        Duration::from_secs(30),
    ) {
        Ok(resp) => resp,
        Err(e) => {
            write_reply(&mut conn, &refusal(502, format!("upstream unreachable: {e}")));
            return;
        }
    };
    let head: Vec<(&str, &str)> = resp.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    match resp.content_length() {
        Some(len) => {
            if write_response_head(&mut conn, resp.status, &head, BodyFraming::Length(len)).is_err() {
                return;
            }
            let _ = std::io::copy(&mut resp, &mut conn);
            let _ = conn.flush();
        }
        None => {
            if write_response_head(&mut conn, resp.status, &head, BodyFraming::Chunked).is_err() {
                return;
            }
            let mut buffer = [0u8; 8192];
            loop {
                match resp.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if write_chunk(&mut conn, &buffer[..n]).is_err() {
                            return;
                        }
                    }
                    // A broken upstream stream ends the relayed one without its last chunk, so
                    // the client sees a truncated body, not a complete one.
                    Err(_) => return,
                }
            }
            let _ = write_last_chunk(&mut conn);
        }
    }
}

/// Steps 1 and 2: identify the job and hold the destination to the allowlist. On success, the
/// headers to send upstream, with the real credential in place of the placeholder.
fn decide(req: &Request, settings: &Settings) -> Result<Vec<(String, String)>, Reply> {
    // 1. Identify the job by its placeholder. No known placeholder means no substitution.
    if req.bearer() != Some(settings.placeholder.as_str()) {
        return Err(refusal(
            403,
            "no known per-job placeholder in request; refusing without substitution".to_string(),
        ));
    }
    // 2. The destination must be on the allowlist before the real credential is substituted.
    if authority_of(&settings.upstream) != settings.allow {
        return Err(refusal(
            403,
            format!(
                "destination {} not on the credential-substitution allowlist",
                authority_of(&settings.upstream)
            ),
        ));
    }
    // Swap the bearer header for the real credential; forward every other header unchanged.
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    headers.push(("Authorization".to_string(), format!("Bearer {}", settings.real)));
    Ok(headers)
}

fn write_reply(conn: &mut TcpStream, reply: &Reply) {
    let headers: Vec<(&str, &str)> = reply.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let _ = write_response_with(conn, reply.status, &headers, &reply.body, reply.chunked);
}

fn refusal(status: u16, message: String) -> Reply {
    Reply {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: json!({"error": message}).to_string().into_bytes(),
        chunked: false,
    }
}

/// The `host:port` authority of an `http://host:port/...` URL.
fn authority_of(url: &str) -> String {
    url.strip_prefix("http://")
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn fatal(msg: &str) -> ! {
    eprintln!("swap-proxy: {msg}");
    std::process::exit(2)
}
