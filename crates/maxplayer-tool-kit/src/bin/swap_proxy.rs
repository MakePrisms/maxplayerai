//! `swap-proxy` — a fake credential-swap proxy, for the proxy-swap demo.
//!
//! It stands in for the real host-side credential proxy (`maxplayer-core` `#647`). It does the one
//! property that route depends on: a request arriving with a per-job PLACEHOLDER in its bearer
//! header is forwarded to the vendor with the REAL credential swapped in, and only for an
//! allowlisted destination. The job never holds the real credential; a leaked placeholder is
//! worthless because the vendor rejects it directly.
//!
//! This is a test double. The production route does NOT add a second proxy: it EXTENDS `#647` by
//! registering the vendor as one more credential (the `FileCredential` shape), whose `upstream`
//! joins the proxy's destination allowlist. This binary exists only so the demo is self-contained.
//!
//! Header-only substitution, like the real proxy: the bearer header is swapped, the body is
//! forwarded byte for byte. Substituting inside the body would be a credential-recovery hole.

use maxplayer_tool_kit::http::{self, read_request, write_response, Request};
use serde_json::json;
use std::net::TcpListener;

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

    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let Ok(peer) = conn.try_clone() else { continue };
        let req = match read_request(peer) {
            Ok(Some(r)) => r,
            _ => continue,
        };
        let (status, body) = handle(&req, &placeholder, &real, &upstream, &allow);
        let _ = write_response(&mut conn, status, &body);
    }
}

fn handle(req: &Request, placeholder: &str, real: &str, upstream: &str, allow: &str) -> (u16, Vec<u8>) {
    // 1. Identify the job by its placeholder. No known placeholder means no substitution.
    if req.bearer() != Some(placeholder) {
        return (
            403,
            json!({"error": "no known per-job placeholder in request; refusing without substitution"})
                .to_string()
                .into_bytes(),
        );
    }
    // 2. The destination must be on the allowlist before the real credential is substituted.
    if authority_of(upstream) != allow {
        return (
            403,
            json!({"error": format!("destination {} not on the credential-substitution allowlist", authority_of(upstream))})
                .to_string()
                .into_bytes(),
        );
    }
    // 3. Swap the bearer header for the real credential and forward the body unchanged.
    match http::request(upstream, &req.method, &req.path, Some(real), Some(&req.body)) {
        Ok(resp) => (resp.status, resp.body),
        Err(e) => (
            502,
            json!({"error": format!("upstream unreachable: {e}")}).to_string().into_bytes(),
        ),
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
