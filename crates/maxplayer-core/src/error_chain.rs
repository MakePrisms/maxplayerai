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

/// `error` followed by every `source()` in its chain, `": "`-separated, with every credential
/// redacted by [`redact_userinfo`]. A cause whose text the chain already ends with is skipped (some
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

/// Replaces every URL credential in `text` with `***`, so a proxy or remote URL that carries a
/// token can be printed:
///
/// - `scheme://<userinfo>@host` keeps only `***@host`. The authority ends at whitespace or at a
///   character that is never unencoded in a URL's userinfo (`/ ? # \ < > " ` { }`) and never
///   runs past the next `://`, so a second URL glued to the first (`&`, `]`, `,`) is still
///   redacted. `, ' ( ) ; = & [ ]` are legal in a password and do NOT end it, so a URL directly
///   followed by an `@`-word is over-redacted rather than leaked.
/// - Scheme-less `user:pass@host:port` (how a proxy variable can be written) fails closed: in a
///   whitespace-separated word with no URL, everything before the last `@` that is followed by a
///   host becomes `***` (an e-mail address is redacted the same way).
///
/// A credential inside a URL's path or query is not userinfo and is not specially handled.
pub fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for piece in text.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        match word.find("://") {
            None => redact_bare(word, &mut out),
            Some(i) => {
                let scheme = word[..i]
                    .char_indices()
                    .rev()
                    .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
                    .map_or(0, |(j, c)| j + c.len_utf8());
                redact_bare(&word[..scheme], &mut out);
                redact_urls(&word[scheme..], &mut out);
            }
        }
        out.push_str(&piece[word.len()..]);
    }
    out
}

/// Characters that end a URL authority: none of them is ever unencoded in userinfo.
fn ends_authority(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '/' | '?' | '#' | '\\' | '<' | '>' | '"' | '`' | '{' | '}'
        )
}

fn redact_urls(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        // Never past the next URL's `://`: its scheme stays outside this authority.
        let next = tail.find("://").unwrap_or(tail.len());
        let end = tail[..next].find(ends_authority).unwrap_or(next);
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
}

/// A word without a URL: `user:pass@host` (or `token@host`) loses everything before its last
/// `@`, keeping only leading opening punctuation.
fn redact_bare(word: &str, out: &mut String) {
    let lead = word.len()
        - word
            .trim_start_matches(['(', '[', '<', '"', '\'', '`'])
            .len();
    match word.rfind('@') {
        Some(at)
            if at > lead
                && word[at + 1..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '[') =>
        {
            out.push_str(&word[..lead]);
            out.push_str("***");
            out.push_str(&word[at..]);
        }
        _ => out.push_str(word),
    }
}

/// The proxy variables reqwest (and so every relay, git and mint request) reads, as reqwest reads
/// them: the uppercase name, and the lowercase one only when the uppercase one is unset.
const PROXY_VARS: [(&str, &str); 3] = [
    ("HTTPS_PROXY", "https_proxy"),
    ("HTTP_PROXY", "http_proxy"),
    ("ALL_PROXY", "all_proxy"),
];

/// `host[:port]` of a proxy value that names a loopback host AND carries userinfo; `None`
/// otherwise. Rebuilt from the parsed host and port, so it never carries the userinfo, control
/// characters or any other suffix of the value.
fn credentialed_loopback(value: &str) -> Option<String> {
    let value = value.trim();
    let after = value.split_once("://").map_or(value, |(_, rest)| rest);
    let authority = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
    let (userinfo, hostport) = authority.rsplit_once('@')?;
    if userinfo.is_empty() {
        return None;
    }
    let (host, port) = split_hostport(hostport)?;
    let shown = if host.trim_end_matches('.').eq_ignore_ascii_case("localhost") {
        "localhost".to_owned()
    } else {
        match host.parse::<IpAddr>().ok()? {
            IpAddr::V4(v4) if v4.is_loopback() => v4.to_string(),
            IpAddr::V6(v6)
                if v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()) =>
            {
                format!("[{v6}]")
            }
            _ => return None,
        }
    };
    Some(match port {
        Some(port) => format!("{shown}:{port}"),
        None => shown,
    })
}

/// `[v6]`, `[v6]:port`, a bare IP (unbracketed `::1` included), `host` or `host:port`; the port
/// must be all digits and fit a u16.
fn split_hostport(hostport: &str) -> Option<(&str, Option<u16>)> {
    let port = |p: &str| {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u16>().ok())
            .flatten()
    };
    if let Some(v6) = hostport.strip_prefix('[') {
        let (host, rest) = v6.split_once(']')?;
        return match rest {
            "" => Some((host, None)),
            _ => Some((host, Some(port(rest.strip_prefix(':')?)?))),
        };
    }
    if hostport.parse::<IpAddr>().is_ok() {
        return Some((hostport, None));
    }
    match hostport.rsplit_once(':') {
        Some((host, p)) => Some((host, Some(port(p)?))),
        None => Some((hostport, None)),
    }
}

/// `a`, `a and b`, `a, b and c`.
fn join_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// One warning line when the environment routes requests through a proxy on this machine that
/// needs credentials, e.g. an agent sandbox's secret-egress proxy whose per-run token the daemon
/// inherited: once the token expires every request fails with 407. `None` when no such proxy is
/// set, or a `*` entry in NO_PROXY bypasses every proxy. The credential is never part of the line.
pub fn local_proxy_warning(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Option<String> {
    let vars: Vec<(String, String)> = vars
        .into_iter()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    let get = |name: &str| {
        vars.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let pick = |upper: &'static str, lower: &'static str| {
        get(upper)
            .map(|v| (upper, v))
            .or_else(|| get(lower).map(|v| (lower, v)))
    };
    if pick("NO_PROXY", "no_proxy").is_some_and(|(_, v)| v.split(',').any(|e| e.trim() == "*")) {
        return None;
    }
    let found: Vec<(&str, String)> = PROXY_VARS
        .iter()
        .filter_map(|(upper, lower)| {
            let (name, value) = pick(upper, lower)?;
            Some((name, credentialed_loopback(value)?))
        })
        .collect();
    let (_, first) = found.first()?;
    let subject = if found.iter().all(|(_, at)| at == first) {
        let names: Vec<String> = found.iter().map(|(name, _)| name.to_string()).collect();
        let verb = if names.len() == 1 { "points" } else { "point" };
        format!(
            "{} {verb} at a proxy on this machine ({first}) that requires credentials",
            join_list(&names)
        )
    } else {
        let pairs: Vec<String> = found
            .iter()
            .map(|(name, at)| format!("{name} ({at})"))
            .collect();
        format!(
            "{} point at proxies on this machine that require credentials",
            join_list(&pairs)
        )
    };
    Some(format!(
        "maxplayer: warning: {subject}. A credentialed local proxy may be short-lived (an agent's \
         egress proxy whose token ends with its run): once it expires, every relay and mint \
         request not exempted by NO_PROXY fails with HTTP 407 (proxy authorization required). To \
         run without it, start maxplayer with the proxy variables unset (env -u HTTPS_PROXY \
         -u https_proxy -u HTTP_PROXY -u http_proxy -u ALL_PROXY -u all_proxy …), or list the \
         relay and mint hosts in NO_PROXY."
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
        assert_eq!(
            redact_userinfo(plain),
            plain,
            "a URL without userinfo is unchanged"
        );
    }

    // `, ' ( ) ;` are legal unencoded in a URL's userinfo (url/reqwest keep `, ' ( )` literal),
    // so none of them may end the credential; a URL glued to another one is still redacted.
    #[test]
    fn redact_userinfo_covers_every_userinfo_character_and_adjacent_urls() {
        for (input, kept) in [
            (
                "for url (http://user:tok,secret@127.0.0.1:8080/x)",
                "for url (http://***@127.0.0.1:8080/x)",
            ),
            ("http://u:o'secret@127.0.0.1:1", "http://***@127.0.0.1:1"),
            ("http://u:a(secret)@127.0.0.1:1", "http://***@127.0.0.1:1"),
            ("http://u:a)secret@127.0.0.1:1", "http://***@127.0.0.1:1"),
            ("http://u:a;secret=1@127.0.0.1:1", "http://***@127.0.0.1:1"),
            (
                "http://u%40x:secret%2F%3A@relay.example/p",
                "http://***@relay.example/p",
            ),
            (
                "https://secret@relay.example/git",
                "https://***@relay.example/git",
            ),
            ("https://:secret@relay.example", "https://***@relay.example"),
            (
                "http://u:a@secret@relay.example",
                "http://***@relay.example",
            ),
            (
                "for url (https://u:secret@host)",
                "for url (https://***@host)",
            ),
            (
                "for url (https://u:secret@host).",
                "for url (https://***@host).",
            ),
            (
                "http://a:secret1@h1 http://b:secret2@h2",
                "http://***@h1 http://***@h2",
            ),
            (
                "http://a:secret1@h1,http://b:secret2@h2",
                "http://***@h1,http://***@h2",
            ),
            (
                "url=http://host1&proxy=http://u:secret@127.0.0.1:8080",
                "url=http://host1&proxy=http://***@127.0.0.1:8080",
            ),
            (
                "[http://host1][http://u:secret@127.0.0.1:8080]",
                "[http://host1][http://***@127.0.0.1:8080]",
            ),
            ("x\thttp://u:secret@h\nnext", "x\thttp://***@h\nnext"),
            ("→http://u:secret@h", "→http://***@h"),
        ] {
            let out = redact_userinfo(input);
            assert!(!out.contains("secret"), "{input:?} -> {out:?}");
            assert_eq!(out, kept, "{input:?}");
        }
    }

    // A scheme-less proxy value (`user:pass@host:port`, as HTTP_PROXY may be written) fails
    // closed: everything before the host is dropped.
    #[test]
    fn redact_userinfo_redacts_a_scheme_less_credential() {
        for (input, kept) in [
            (
                "proxy openclaw:secret@127.0.0.1:9 refused",
                "proxy ***@127.0.0.1:9 refused",
            ),
            ("(:secret@localhost:3128)", "(***@localhost:3128)"),
            ("u:secret@[::1]:1080,", "***@[::1]:1080,"),
        ] {
            let out = redact_userinfo(input);
            assert!(!out.contains("secret"), "{input:?} -> {out:?}");
            assert_eq!(out, kept, "{input:?}");
        }
        for unchanged in ["HEAD@{0}", "@mention", "no credential here", "a @ b"] {
            assert_eq!(redact_userinfo(unchanged), unchanged);
        }
    }

    #[test]
    fn chain_redacts_every_cause_in_the_chain() {
        #[derive(Debug)]
        struct E(&'static str, Option<Box<E>>);
        impl std::fmt::Display for E {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl StdError for E {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                self.1.as_deref().map(|e| e as _)
            }
        }
        let error = E(
            "io error: http request",
            Some(Box::new(E(
                "proxy u:tok,secret@127.0.0.1:1 (http://a:b'secret@h)",
                None,
            ))),
        );
        let text = chain(&error);
        assert_eq!(
            text,
            "io error: http request: proxy ***@127.0.0.1:1 (http://***@h)"
        );
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
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
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
            (
                "HTTPS_PROXY",
                "http://openclaw:secret@127.0.0.1:41234",
                "127.0.0.1:41234",
            ),
            (
                "https_proxy",
                "http://openclaw:secret@localhost:3128/",
                "localhost:3128",
            ),
            ("HTTP_PROXY", "openclaw:secret@127.0.0.2:9", "127.0.0.2:9"),
            ("all_proxy", "socks5://u:secret@[::1]:1080", "[::1]:1080"),
        ] {
            let line = local_proxy_warning(env(&[(name, value)]))
                .unwrap_or_else(|| panic!("{name}={value}"));
            assert!(
                !line.contains("secret") && !line.contains("openclaw:"),
                "{line}"
            );
            assert!(line.contains(name) && line.contains(at), "{line}");
            assert!(
                line.contains("407")
                    && line.contains("NO_PROXY")
                    && line.contains("env -u HTTPS_PROXY"),
                "{line}"
            );
            assert_eq!(line.lines().count(), 1, "one line: {line}");
        }
        for quiet in [
            env(&[]),
            env(&[("HTTPS_PROXY", "http://proxy.corp.example:3128")]),
            env(&[("HTTPS_PROXY", "http://u:secret@proxy.corp.example:3128")]),
            env(&[("HTTPS_PROXY", "http://127.0.0.1:3128")]),
            env(&[
                ("HTTPS_PROXY", "http://u:secret@127.0.0.1:1"),
                ("NO_PROXY", "*"),
            ]),
            env(&[
                ("HTTPS_PROXY", "http://u:secret@127.0.0.1:1"),
                ("NO_PROXY", "localhost,*"),
            ]),
            env(&[
                ("HTTPS_PROXY", "http://u:secret@127.0.0.1:1"),
                ("no_proxy", "127.0.0.1, *"),
            ]),
            // Not a loopback host, or not a parsable port: no warning, never a raw suffix.
            env(&[("HTTPS_PROXY", "http://u:secret@[::1]extra")]),
            env(&[("HTTPS_PROXY", "http://u:secret@127.0.0.1:80\ninjected")]),
            env(&[("HTTPS_PROXY", "http://u:secret@127.0.0.1:99999")]),
            env(&[("HTTPS_PROXY", "http://u:secret@[::ffff:10.0.0.1]:1")]),
        ] {
            assert_eq!(local_proxy_warning(quiet.clone()), None, "{quiet:?}");
        }
    }

    // Loopback spelled every way reqwest accepts it; the shown endpoint is rebuilt from the parsed
    // host and port.
    #[test]
    fn local_proxy_warning_parses_every_loopback_spelling() {
        for (value, at) in [
            ("http://u:secret@::1", "([::1])"),
            ("u:secret@::1", "([::1])"),
            ("http://u:secret@[::1]", "([::1])"),
            (
                "http://u:secret@[::ffff:127.0.0.1]:3128",
                "([::ffff:127.0.0.1]:3128)",
            ),
            ("http://u:secret@localhost.:3128", "(localhost:3128)"),
            ("http://u:secret@LOCALHOST", "(localhost)"),
            ("http://u:secret@127.1.2.3", "(127.1.2.3)"),
        ] {
            let line = local_proxy_warning(env(&[("HTTPS_PROXY", value)]))
                .unwrap_or_else(|| panic!("{value}"));
            assert!(
                line.contains(at) && !line.contains("secret"),
                "{value}: {line}"
            );
        }
    }

    // reqwest (hyper-util 0.1.20) reads the uppercase variable first and the lowercase one only
    // when the uppercase one is unset; the warning follows the same rule for NO_PROXY and for each
    // proxy variable.
    #[test]
    fn local_proxy_warning_follows_reqwest_variable_precedence() {
        let proxy = ("HTTPS_PROXY", "http://u:secret@127.0.0.1:1");
        assert_eq!(
            local_proxy_warning(env(&[proxy, ("NO_PROXY", "*"), ("no_proxy", "")])),
            None
        );
        assert!(local_proxy_warning(env(&[proxy, ("NO_PROXY", ""), ("no_proxy", "*")])).is_some());
        // The effective HTTPS proxy is the uppercase one: a credentialed lowercase one is unused.
        assert_eq!(
            local_proxy_warning(env(&[
                ("HTTPS_PROXY", "http://proxy.corp.example:3128"),
                ("https_proxy", "http://u:secret@127.0.0.1:1"),
            ])),
            None
        );
        let line =
            local_proxy_warning(env(&[("https_proxy", "http://u:secret@127.0.0.1:1")])).unwrap();
        assert!(
            line.contains("https_proxy points at a proxy on this machine (127.0.0.1:1)"),
            "{line}"
        );
        assert!(line.contains("not exempted by NO_PROXY"), "{line}");
    }

    #[test]
    fn local_proxy_warning_grammar_with_several_variables() {
        let same = local_proxy_warning(env(&[
            ("HTTPS_PROXY", "http://u:secret@127.0.0.1:1"),
            ("HTTP_PROXY", "http://u:secret@127.0.0.1:1"),
            ("ALL_PROXY", "socks5://u:secret@127.0.0.1:1"),
        ]))
        .unwrap();
        assert!(
            same.contains("HTTPS_PROXY, HTTP_PROXY and ALL_PROXY point at a proxy on this machine (127.0.0.1:1) that requires credentials."),
            "{same}"
        );
        let different = local_proxy_warning(env(&[
            ("HTTPS_PROXY", "http://u:secret@127.0.0.1:1111"),
            ("HTTP_PROXY", "http://u:secret@[::1]:2222"),
        ]))
        .unwrap();
        assert!(
            different.contains("HTTPS_PROXY (127.0.0.1:1111) and HTTP_PROXY ([::1]:2222) point at proxies on this machine that require credentials."),
            "{different}"
        );
        assert!(!same.contains("secret") && !different.contains("secret"));
    }
}
