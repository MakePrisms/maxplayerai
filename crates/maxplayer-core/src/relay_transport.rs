//! Relay websocket transport that honours the HTTP proxy variables.
//!
//! nostr-sdk's default transport always dials the relay directly. In a sandbox whose only way out
//! is an HTTP proxy (`HTTPS_PROXY`), that direct dial times out or hits a dead address, while HTTPS
//! calls to the very same host succeed because reqwest sends them through the proxy. This transport
//! opens the websocket through the proxy with HTTP CONNECT instead: the proxy resolves and dials the
//! relay host, and TLS plus the websocket handshake run end to end inside the tunnel, so the relay
//! certificate is still verified against the relay's name.
//!
//! The variables are read the way reqwest reads them (see `error_chain::PROXY_VARS`): the uppercase
//! name, the lowercase one only when the uppercase one is unset, and `ALL_PROXY` as the fallback. So a
//! relay websocket takes the same route as the HTTPS calls next to it. Loopback relays are always
//! dialled directly (a proxy cannot reach this machine's loopback), and so is any host listed in
//! `NO_PROXY`. A proxy that is not plain `http://` (SOCKS, or TLS to the proxy itself) keeps today's
//! direct dial.
//!
//! Every relay client in the product is built with [`client`] or [`builder`];
//! `no_relay_client_bypasses_the_proxy_transport` below fails if a new one calls `Client::new`.

use std::{
    fmt, io,
    net::IpAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use async_wsocket::{
    ConnectionMode, Message, WebSocket,
    futures_util::{Sink, SinkExt, StreamExt, TryStreamExt, stream::SplitSink},
};
use base64::Engine as _;
use nostr_sdk::{
    pool::transport::{
        error::TransportError,
        websocket::{
            DefaultWebsocketTransport, WebSocketSink, WebSocketStream, WebSocketTransport,
        },
    },
    prelude::*,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// A relay client for `keys` whose websockets go through the HTTP proxy when one is configured.
/// Use this instead of `Client::new`.
pub fn client(keys: Keys) -> Client {
    builder().signer(keys).build()
}

/// `Client::builder()` with the proxy-aware transport. Use this instead of `Client::builder()`.
pub fn builder() -> ClientBuilder {
    Client::builder().websocket_transport(RelayTransport::from_env())
}

/// An HTTP proxy that relay websockets are tunnelled through with CONNECT.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpProxy {
    host: String,
    port: u16,
    /// Decoded `user:password` from the proxy URL. Never printed.
    auth: Option<String>,
}

impl HttpProxy {
    /// Parse a proxy variable's value. `None` for anything that is not a plain HTTP proxy.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let rest = match value.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some(_) => return None,
            None => value,
        };
        let url = Url::parse(&format!("http://{rest}")).ok()?;
        let host = match url.host()? {
            url::Host::Domain(domain) => domain.to_owned(),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Ipv6(ip) => ip.to_string(),
        };
        let auth = (!url.username().is_empty()).then(|| {
            format!(
                "{}:{}",
                percent_decode(url.username()),
                percent_decode(url.password().unwrap_or(""))
            )
        });
        Some(Self {
            host,
            port: url.port_or_known_default()?,
            auth,
        })
    }

    fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Without the credential, so it is safe in errors and logs.
impl fmt::Display for HttpProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "http://{}", self.authority())
    }
}

impl fmt::Debug for HttpProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpProxy")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.auth.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Which relay websockets go through which proxy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxySettings {
    https: Option<HttpProxy>,
    http: Option<HttpProxy>,
    no_proxy: NoProxy,
}

impl ProxySettings {
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Self {
        let pick = |upper: &str, lower: &str| {
            get(upper)
                .filter(|v| !v.trim().is_empty())
                .or_else(|| get(lower).filter(|v| !v.trim().is_empty()))
        };
        let all = pick("ALL_PROXY", "all_proxy");
        let https = pick("HTTPS_PROXY", "https_proxy").or_else(|| all.clone());
        let http = pick("HTTP_PROXY", "http_proxy").or(all);
        Self {
            https: https.as_deref().and_then(HttpProxy::parse),
            http: http.as_deref().and_then(HttpProxy::parse),
            no_proxy: NoProxy::parse(pick("NO_PROXY", "no_proxy").as_deref().unwrap_or("")),
        }
    }

    /// The proxy a websocket to `url` goes through, or `None` to dial it directly.
    pub fn proxy_for(&self, url: &Url) -> Option<&HttpProxy> {
        let proxy = match url.scheme() {
            "wss" | "https" => self.https.as_ref(),
            "ws" | "http" => self.http.as_ref(),
            _ => None,
        }?;
        let host = bare_host(url.host_str()?);
        if is_loopback(&host) || self.no_proxy.matches(&host) {
            return None;
        }
        Some(proxy)
    }
}

/// `NO_PROXY`, matched as the locked reqwest (hyper-util 0.1.20 `NoProxy`) matches it: any `*` entry
/// bypasses every proxy; an IP address or CIDR range matches that address; a name matches itself and
/// its subdomains, with or without a leading `.`. Entries are taken as written (no port stripping),
/// so a websocket and an HTTPS call to the same host always make the same choice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NoProxy {
    all: bool,
    entries: Vec<String>,
}

impl NoProxy {
    fn parse(value: &str) -> Self {
        let mut out = Self::default();
        for raw in value.split(',') {
            let entry = raw.trim().to_ascii_lowercase();
            if entry == "*" {
                out.all = true;
            } else if !entry.is_empty() {
                out.entries.push(entry);
            }
        }
        out
    }

    fn matches(&self, host: &str) -> bool {
        if self.all {
            return true;
        }
        let host = host.to_ascii_lowercase();
        let ip = host.parse::<IpAddr>().ok();
        self.entries.iter().any(|entry| {
            if let Some((net, bits)) = entry.split_once('/') {
                return match (ip, net.parse::<IpAddr>(), bits.parse::<u8>()) {
                    (Some(ip), Ok(net), Ok(bits)) => in_cidr(ip, net, bits),
                    _ => false,
                };
            }
            if let (Some(ip), Ok(entry_ip)) = (ip, entry.parse::<IpAddr>()) {
                return ip == entry_ip;
            }
            let name = entry.strip_prefix('.').unwrap_or(entry);
            host == name || host.ends_with(&format!(".{name}"))
        })
    }
}

fn in_cidr(ip: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(ip), IpAddr::V4(net)) if bits <= 32 => {
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            u32::from(ip) & mask == u32::from(net) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) if bits <= 128 => {
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            u128::from(ip) & mask == u128::from(net) & mask
        }
        _ => false,
    }
}

fn bare_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// nostr-sdk websocket transport: CONNECT through the configured HTTP proxy, else the SDK default.
#[derive(Clone, Debug, Default)]
pub struct RelayTransport {
    proxy: ProxySettings,
}

impl RelayTransport {
    pub fn from_env() -> Self {
        Self::with_settings(ProxySettings::from_env())
    }

    pub fn with_settings(proxy: ProxySettings) -> Self {
        Self { proxy }
    }
}

impl WebSocketTransport for RelayTransport {
    fn support_ping(&self) -> bool {
        DefaultWebsocketTransport.support_ping()
    }

    fn connect<'a>(
        &'a self,
        url: &'a Url,
        mode: &'a ConnectionMode,
        timeout: Duration,
    ) -> BoxedFuture<'a, Result<(WebSocketSink, WebSocketStream), TransportError>> {
        Box::pin(async move {
            let proxy = match mode {
                ConnectionMode::Direct => self.proxy.proxy_for(url),
                #[allow(unreachable_patterns)]
                _ => None,
            };
            let Some(proxy) = proxy else {
                return DefaultWebsocketTransport.connect(url, mode, timeout).await;
            };
            let socket = connect_via_proxy(url, proxy, timeout)
                .await
                .map_err(TransportError::backend)?;
            let (tx, rx) = socket.split();
            let sink: WebSocketSink = Box::new(TunnelSink(tx));
            let stream: WebSocketStream = Box::pin(rx.map_err(TransportError::backend));
            Ok((sink, stream))
        })
    }
}

/// Open a websocket to `url` through `proxy` with HTTP CONNECT, all within `timeout`.
async fn connect_via_proxy(
    url: &Url,
    proxy: &HttpProxy,
    timeout: Duration,
) -> io::Result<WebSocket> {
    let host = url
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "relay URL has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "relay URL has no port"))?;
    // host_str already brackets an IPv6 literal.
    let target = format!("{host}:{port}");
    let attempt = async {
        let mut tcp = TcpStream::connect((proxy.host.as_str(), proxy.port))
            .await
            .map_err(|e| io::Error::new(e.kind(), format!("could not reach proxy {proxy}: {e}")))?;
        let _ = tcp.set_nodelay(true);
        let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
        if let Some(auth) = &proxy.auth {
            let token = base64::engine::general_purpose::STANDARD.encode(auth);
            request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
        }
        request.push_str("\r\n");
        tcp.write_all(request.as_bytes()).await?;
        let (code, status) = read_connect_response(&mut tcp).await.map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("proxy {proxy} gave no CONNECT answer for {target}: {e}"),
            )
        })?;
        if !(200..300).contains(&code) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("proxy {proxy} refused CONNECT {target}: {status}"),
            ));
        }
        // NOTE: Box::pin, as the SDK does: the handshake future is large.
        let (socket, _) = Box::pin(tokio_tungstenite::client_async_tls(url.as_str(), tcp))
            .await
            .map_err(|e| {
                io::Error::other(format!(
                    "websocket handshake with {target} through proxy {proxy} failed: {e}"
                ))
            })?;
        Ok(WebSocket::Tokio(socket))
    };
    tokio::time::timeout(timeout, attempt).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out after {}s connecting to {target} through proxy {proxy}",
                timeout.as_secs()
            ),
        )
    })?
}

/// Read the proxy's answer to CONNECT up to the blank line, one byte at a time so nothing that
/// belongs to the tunnel is consumed. Returns the status code and a printable status line.
async fn read_connect_response(tcp: &mut TcpStream) -> io::Result<(u16, String)> {
    const MAX: usize = 16 * 1024;
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response header too long",
            ));
        }
        if tcp.read(&mut byte).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        head.push(byte[0]);
    }
    let line: String = String::from_utf8_lossy(&head)
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    let code = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|_| line.starts_with("HTTP/"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("not an HTTP response: {line}"),
            )
        })?;
    Ok((code, line))
}

struct TunnelSink(SplitSink<WebSocket, Message>);

impl Sink<Message> for TunnelSink {
    type Error = TransportError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_ready_unpin(cx)
            .map_err(TransportError::backend)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        Pin::new(&mut self.0)
            .start_send_unpin(item)
            .map_err(TransportError::backend)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_flush_unpin(cx)
            .map_err(TransportError::backend)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_close_unpin(cx)
            .map_err(TransportError::backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
    use std::{
        collections::HashMap,
        path::Path,
        sync::{Arc, Mutex},
    };
    use tokio::net::TcpListener;

    fn settings(vars: &[(&str, &str)]) -> ProxySettings {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ProxySettings::from_vars(|name| map.get(name).cloned())
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn proxied(s: &ProxySettings, u: &str) -> Option<String> {
        s.proxy_for(&url(u)).map(|p| p.to_string())
    }

    #[test]
    fn wss_takes_the_https_proxy_ws_the_http_proxy_and_all_proxy_is_the_fallback() {
        let s = settings(&[
            ("HTTPS_PROXY", "http://sec:3128"),
            ("HTTP_PROXY", "plain:8080"),
        ]);
        assert_eq!(
            proxied(&s, "wss://relay.maxplayer.ai"),
            Some("http://sec:3128".into())
        );
        assert_eq!(
            proxied(&s, "ws://relay.example"),
            Some("http://plain:8080".into())
        );
        let s = settings(&[("ALL_PROXY", "http://all:1")]);
        assert_eq!(
            proxied(&s, "wss://relay.maxplayer.ai"),
            Some("http://all:1".into())
        );
        assert_eq!(
            proxied(&s, "ws://relay.example"),
            Some("http://all:1".into())
        );
        assert_eq!(proxied(&settings(&[]), "wss://relay.maxplayer.ai"), None);
    }

    #[test]
    fn the_uppercase_variable_wins_and_lowercase_is_the_fallback_as_in_reqwest() {
        let s = settings(&[
            ("HTTPS_PROXY", "http://upper:1"),
            ("https_proxy", "http://lower:2"),
        ]);
        assert_eq!(
            proxied(&s, "wss://r.example"),
            Some("http://upper:1".into())
        );
        let s = settings(&[("https_proxy", "http://lower:2")]);
        assert_eq!(
            proxied(&s, "wss://r.example"),
            Some("http://lower:2".into())
        );
        let s = settings(&[("HTTPS_PROXY", "  "), ("https_proxy", "http://lower:2")]);
        assert_eq!(
            proxied(&s, "wss://r.example"),
            Some("http://lower:2".into())
        );
    }

    #[test]
    fn only_plain_http_proxies_are_tunnelled_and_others_keep_the_direct_dial() {
        for value in ["socks5://p:1080", "socks5h://p:1080", "https://p:443", ""] {
            let s = settings(&[("HTTPS_PROXY", value)]);
            assert_eq!(proxied(&s, "wss://r.example"), None, "{value}");
        }
        assert_eq!(HttpProxy::parse("p").unwrap().to_string(), "http://p:80");
        assert_eq!(
            HttpProxy::parse("HTTP://[::1]:3128/").unwrap().to_string(),
            "http://[::1]:3128"
        );
    }

    #[test]
    fn the_proxy_credential_is_decoded_for_the_header_and_never_printed() {
        let proxy = HttpProxy::parse("http://us%40er:p%3Ass%20w@proxy.corp:3128").unwrap();
        assert_eq!(proxy.auth.as_deref(), Some("us@er:p:ss w"));
        let shown = format!("{proxy} {proxy:?}");
        assert!(
            !shown.contains("us@er")
                && !shown.contains("us%40er")
                && !shown.contains("ss w")
                && !shown.contains("p%3Ass"),
            "{shown}"
        );
    }

    #[test]
    fn no_proxy_hosts_and_loopback_are_dialled_directly() {
        let s = settings(&[
            ("HTTPS_PROXY", "http://p:1"),
            ("HTTP_PROXY", "http://p:1"),
            (
                "NO_PROXY",
                " .corp.example, Relay.Maxplayer.ai ,10.0.0.0/8,fd00::1",
            ),
        ]);
        for direct in [
            "wss://relay.maxplayer.ai",
            "wss://a.b.corp.example",
            "wss://corp.example",
            "ws://10.2.3.4:7777",
            "wss://[fd00::1]",
            "ws://127.0.0.1:9",
            "ws://localhost:9",
            "ws://[::1]:9",
        ] {
            assert_eq!(proxied(&s, direct), None, "{direct}");
        }
        for via in [
            "wss://maxplayer.ai",
            "wss://notcorp.example",
            "ws://11.0.0.1",
            "wss://relay.maxplayer.ai.evil",
        ] {
            assert!(proxied(&s, via).is_some(), "{via}");
        }
        let star = settings(&[("HTTPS_PROXY", "http://p:1"), ("no_proxy", "*")]);
        assert_eq!(proxied(&star, "wss://r.example"), None);
    }

    /// A proxy like the sandbox's: CONNECT only (anything else gets 405), optionally a required
    /// credential, optionally refusing every tunnel. Each CONNECT target's port is dialled on
    /// 127.0.0.1, so the target's NAME never has to resolve: as in the sandbox, only the proxy can
    /// reach the relay. Records each request line.
    async fn connect_only_proxy(
        credential: Option<&'static str>,
        refuse: bool,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if client.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let first = head.lines().next().unwrap_or("").to_owned();
                    log.lock().unwrap().push(first.clone());
                    let Some(target) = first
                        .strip_prefix("CONNECT ")
                        .and_then(|rest| rest.split_whitespace().next())
                        .map(str::to_owned)
                    else {
                        let _ = client
                            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
                            .await;
                        return;
                    };
                    if refuse {
                        let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
                        return;
                    }
                    if let Some(credential) = credential {
                        let want = format!(
                            "Basic {}",
                            base64::engine::general_purpose::STANDARD.encode(credential)
                        );
                        let sent = head.lines().any(|line| {
                            line.split_once(':').is_some_and(|(name, value)| {
                                name.eq_ignore_ascii_case("proxy-authorization")
                                    && value.trim() == want
                            })
                        });
                        if !sent {
                            let _ = client
                                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                                .await;
                            return;
                        }
                    }
                    let port = target.rsplit(':').next().unwrap_or("0").to_owned();
                    let Ok(mut upstream) = TcpStream::connect(format!("127.0.0.1:{port}")).await
                    else {
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                        return;
                    };
                    let _ = client
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await;
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        (port, seen)
    }

    async fn local_relay() -> (LocalRelay, u16) {
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.unwrap();
        let port = url(&relay.url().await.to_string()).port().unwrap();
        (relay, port)
    }

    fn transport_via(proxy_port: u16, userinfo: &str) -> RelayTransport {
        let proxy = format!("http://{userinfo}127.0.0.1:{proxy_port}");
        RelayTransport::with_settings(settings(&[("HTTP_PROXY", proxy.as_str())]))
    }

    async fn dial(
        transport: &RelayTransport,
        target: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        transport
            .connect(&url(target), &ConnectionMode::Direct, timeout)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn a_relay_only_the_proxy_can_reach_works_end_to_end_through_connect() {
        let (_relay, relay_port) = local_relay().await;
        let (proxy_port, seen) = connect_only_proxy(None, false).await;
        // .invalid never resolves (RFC 6761): a direct dial cannot succeed.
        let relay_url = format!("ws://relay.invalid:{relay_port}");
        let client = Client::builder()
            .signer(Keys::generate())
            .websocket_transport(transport_via(proxy_port, ""))
            .build();
        client.add_relay(&relay_url).await.unwrap();
        client
            .relay(&relay_url)
            .await
            .unwrap()
            .try_connect(Duration::from_secs(5))
            .await
            .expect("connect through the proxy");
        let sent = client
            .send_event_builder(EventBuilder::text_note("through the tunnel"))
            .await
            .expect("publish through the proxy");
        let got = client
            .fetch_events(Filter::new().id(sent.val), Duration::from_secs(5))
            .await
            .expect("read back through the proxy");
        assert_eq!(got.len(), 1);
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|line| *line == format!("CONNECT relay.invalid:{relay_port} HTTP/1.1")),
            "{seen:?}"
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn the_proxy_credential_is_sent_and_a_rejected_one_is_named_without_leaking_it() {
        let (_relay, relay_port) = local_relay().await;
        let (proxy_port, _) = connect_only_proxy(Some("us@er:p:ss w"), false).await;
        let target = format!("ws://relay.invalid:{relay_port}");
        let good = transport_via(proxy_port, "us%40er:p%3Ass%20w@");
        dial(&good, &target, Duration::from_secs(5))
            .await
            .expect("right credential");
        let bad = transport_via(proxy_port, "us%40er:hunter2@");
        let err = dial(&bad, &target, Duration::from_secs(5))
            .await
            .expect_err("wrong credential");
        assert!(
            err.contains("407") && err.contains(&format!("http://127.0.0.1:{proxy_port}")),
            "{err}"
        );
        assert!(
            !err.contains("hunter2") && !err.contains("us%40er"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_refused_tunnel_and_a_silent_proxy_are_reported_with_the_proxy_named() {
        let (proxy_port, _) = connect_only_proxy(None, true).await;
        let err = dial(
            &transport_via(proxy_port, ""),
            "ws://relay.invalid:1",
            Duration::from_secs(5),
        )
        .await
        .expect_err("refused");
        assert!(
            err.contains("refused CONNECT relay.invalid:1") && err.contains("403"),
            "{err}"
        );

        let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent_port = silent.local_addr().unwrap().port();
        let _hold = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = silent.accept().await {
                held.push(stream);
            }
        });
        let err = dial(
            &transport_via(silent_port, ""),
            "ws://relay.invalid:1",
            Duration::from_millis(300),
        )
        .await
        .expect_err("timeout");
        assert!(
            err.contains("timed out")
                && err.contains(&format!("through proxy http://127.0.0.1:{silent_port}")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_loopback_relay_is_dialled_directly_even_with_a_proxy_set() {
        let (_relay, relay_port) = local_relay().await;
        let (proxy_port, seen) = connect_only_proxy(None, true).await;
        dial(
            &transport_via(proxy_port, ""),
            &format!("ws://127.0.0.1:{relay_port}"),
            Duration::from_secs(5),
        )
        .await
        .expect("direct");
        assert!(seen.lock().unwrap().is_empty());
    }

    /// Every relay client the product builds must use [`client`]/[`builder`], or it dials the relay
    /// directly and fails behind an HTTP-only proxy. Test code is exempt.
    #[test]
    fn no_relay_client_bypasses_the_proxy_transport() {
        fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n != "tests") {
                        walk(&path, out);
                    }
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut files = Vec::new();
        for krate in [
            "maxplayer",
            "maxplayer-core",
            "maxplayer-chat",
            "maxplayer-team",
            "maxplayer-trade",
            "maxplayer-mint",
        ] {
            walk(&crates.join(krate).join("src"), &mut files);
        }
        assert!(
            files.len() > 50,
            "source walk found only {} files",
            files.len()
        );
        let mut offenders = Vec::new();
        for file in files {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            if name == "relay_transport.rs"
                || name == "tests.rs"
                || name.ends_with("_tests.rs")
                || name.ends_with("_it.rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let cut = ["#[cfg(test)]", "#[cfg(all(test"]
                .iter()
                .filter_map(|marker| text.find(marker))
                .min()
                .unwrap_or(text.len());
            let product = &text[..cut];
            for (n, line) in product.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                for pattern in ["Client::new(", "Client::builder()"] {
                    for (at, _) in code.match_indices(pattern) {
                        let before = &code[..at];
                        let qualified = before.ends_with(|c: char| c.is_alphanumeric() || c == '_')
                            || before.ends_with("reqwest::")
                            || before.ends_with("blocking::");
                        if !qualified {
                            offenders.push(format!(
                                "{}:{}: {}",
                                file.display(),
                                n + 1,
                                line.trim()
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "relay clients built without crate::relay_transport (use relay_transport::client/builder):\n{}",
            offenders.join("\n")
        );
    }
}
