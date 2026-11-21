//! Operator-facing error text: an error's whole `source()` chain, with URL credentials redacted,
//! and the startup warning for a credentialed proxy on this machine.
//!
//! reqwest's Display stops at "error sending request for url (..)": the cause (a proxy's 407, a
//! refused connect, a TLS failure) lives only in the source chain. A buyer daemon whose inherited
//! `HTTPS_PROXY` token had expired reported "private repository provisioning unavailable" and
//! nothing else, twice, before anyone found the 407.

use std::error::Error as StdError;
use std::ffi::OsString;
use std::net::IpAddr;

/// `error` followed by every `source()` in its chain, `": "`-separated, with any `user:pass@`
/// in a URL replaced by `***@`. A cause whose text the chain already ends with is skipped (some
/// wrappers repeat their inner error verbatim).
pub fn chain(error: &(dyn StdError + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let line = cause.to_string();
        if !line.is_empty() && !text.ends_with(&line) {
            text.push_str(": ");
            text.push_str(&line);
        }
        source = cause.source();
    }
    redact_userinfo(&text)
}

/// Replaces the userinfo of every `scheme://user:pass@host` in `text` with `***`, so a proxy or
/// remote URL that carries a token can be printed. The authority ends at the first `/ ? #`,
/// whitespace, quote, bracket-free delimiter (`( ) < > , ;`); a password holding a raw `/`
/// is not a valid URL and is not specially handled.
pub fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(c, '/' | '?' | '#' | '(' | ')' | '<' | '>' | '"' | '\'' | '`' | ',' | ';')
            })
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The proxy variables reqwest (and so every relay, git and mint request) reads from the
/// environment.
const PROXY_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// `host:port` of a proxy value that names a loopback host AND carries userinfo; `None` otherwise.
/// Never returns the userinfo.
fn credentialed_loopback(value: &str) -> Option<&str> {
    let value = value.trim();
    let after = value.split_once("://").map_or(value, |(_, rest)| rest);
    let authority = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
    let (userinfo, hostport) = authority.rsplit_once('@')?;
    if userinfo.is_empty() {
        return None;
    }
    let host = match hostport.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => hostport.rsplit_once(':').map_or(hostport, |(host, _)| host),
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    loopback.then_some(hostport)
}

/// One warning line when the environment routes requests through a proxy on this machine that
/// needs credentials, e.g. an agent sandbox's secret-egress proxy whose per-run token the daemon
/// inherited: once the token expires every request fails with 407. `None` when no such proxy is
/// set, or `NO_PROXY=*` bypasses every proxy. The credential is never part of the line.
pub fn local_proxy_warning(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Option<String> {
    let vars: Vec<(String, String)> = vars
        .into_iter()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    let get = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if [get("NO_PROXY"), get("no_proxy")]
        .into_iter()
        .flatten()
        .any(|v| v.trim() == "*")
    {
        return None;
    }
    let mut names = Vec::new();
    let mut at = None;
    for name in PROXY_VARS {
        if let Some(hostport) = get(name).and_then(credentialed_loopback) {
            names.push(name);
            at.get_or_insert(hostport.to_owned());
        }
    }
    let at = at?;
    Some(format!(
        "maxplayer: warning: {} points at a proxy on this machine ({at}) that requires \
         credentials. A credentialed local proxy may be short-lived (an agent's egress proxy \
         whose token ends with its run): once it expires, every relay and mint request fails \
         with HTTP 407 (proxy authorization required). To run without it, start this daemon \
         with the proxy variables unset (env -u HTTPS_PROXY -u https_proxy -u HTTP_PROXY \
         -u http_proxy -u ALL_PROXY -u all_proxy …), or list the relay and mint hosts in NO_PROXY.",
        names.join(" and ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_userinfo_never_shows_a_proxy_credential() {
        for (input, kept) in [
            ("http://user:secret@127.0.0.1:1", "http://***@127.0.0.1:1"),
            (
                "proxy http://openclaw:secret@127.0.0.1:41234/ refused",
                "proxy http://***@127.0.0.1:41234/ refused",
            ),
            (
                "error sending request for url (https://u:secret@relay.example/git/x)",
                "error sending request for url (https://***@relay.example/git/x)",
            ),
            ("a http://secret@[::1]:8080 b", "a http://***@[::1]:8080 b"),
        ] {
            let out = redact_userinfo(input);
            assert!(!out.contains("secret"), "{input:?} -> {out:?}");
            assert_eq!(out, kept);
        }
        let plain = "error sending request for url (https://relay.maxplayer.ai/git/a/b/info/refs)";
        assert_eq!(redact_userinfo(plain), plain, "a URL without userinfo is unchanged");
    }

    // The incident: a credentialed proxy answering CONNECT with 407. The chain must name the 407
    // (reqwest's own Display does not) and must never print the proxy credential.
    #[cfg(feature = "git-delivery")]
    #[test]
    fn chain_names_a_proxy_407_and_never_its_credential() {
        use std::io::{Read as _, Write as _};
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = proxy.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = proxy.accept().unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).unwrap();
            socket
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let error = reqwest::blocking::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://user:secret@{addr}")).unwrap())
            .build()
            .unwrap()
            .get("https://relay.example/git/o/r.git/info/refs?service=git-upload-pack")
            .send()
            .unwrap_err();
        server.join().unwrap();
        let text = chain(&error);
        assert!(text.starts_with("error sending request"), "{text}");
        assert!(text.contains("proxy authorization required"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs.iter().map(|(k, v)| (k.into(), v.into())).collect()
    }

    #[test]
    fn local_proxy_warning_names_a_credentialed_loopback_proxy_without_its_credential() {
        for (name, value, at) in [
            ("HTTPS_PROXY", "http://openclaw:secret@127.0.0.1:41234", "127.0.0.1:41234"),
            ("https_proxy", "http://openclaw:secret@localhost:3128/", "localhost:3128"),
            ("HTTP_PROXY", "openclaw:secret@127.0.0.2:9", "127.0.0.2:9"),
            ("all_proxy", "socks5://u:secret@[::1]:1080", "[::1]:1080"),
        ] {
            let line = local_proxy_warning(env(&[(name, value)])).unwrap_or_else(|| panic!("{name}={value}"));
            assert!(!line.contains("secret") && !line.contains("openclaw:"), "{line}");
            assert!(line.contains(name) && line.contains(at), "{line}");
            assert!(line.contains("407") && line.contains("NO_PROXY") && line.contains("env -u HTTPS_PROXY"), "{line}");
            assert_eq!(line.lines().count(), 1, "one line: {line}");
        }
        for quiet in [
            env(&[]),
            env(&[("HTTPS_PROXY", "http://proxy.corp.example:3128")]),
            env(&[("HTTPS_PROXY", "http://u:secret@proxy.corp.example:3128")]),
            env(&[("HTTPS_PROXY", "http://127.0.0.1:3128")]),
            env(&[("HTTPS_PROXY", "http://u:secret@127.0.0.1:1"), ("NO_PROXY", "*")]),
        ] {
            assert_eq!(local_proxy_warning(quiet.clone()), None, "{quiet:?}");
        }
    }
}
