//! Relay websocket transport that honours the HTTP proxy variables.
//!
//! nostr-sdk's default transport always dials the relay directly. In a sandbox whose only way out
//! is an HTTP proxy (`HTTPS_PROXY`), that direct dial times out or hits a dead address, while HTTPS
//! calls to the very same host succeed because reqwest sends them through the proxy. This transport
//! opens the websocket through the proxy with HTTP CONNECT instead: the proxy resolves and dials the
//! relay host, and TLS plus the websocket handshake run end to end inside the tunnel, so the relay
//! certificate is still verified against the relay's name.
//!
//! Which proxy (if any) a relay takes is decided by hyper-util's proxy matcher, the same code the
//! locked reqwest runs for its HTTPS calls (`Matcher::from_system`), asked about the matching
//! `https://` (for `wss://`) or `http://` (for `ws://`) URL. So variable precedence, `ALL_PROXY`,
//! `NO_PROXY`, proxy URL parsing, credentials and the CGI rule (`REQUEST_METHOD` set: no env proxy)
//! are reqwest's by construction, not a copy of it. Two deliberate exceptions, both keeping today's
//! direct dial: loopback relays (a proxy cannot reach this machine's loopback), and a proxy that is
//! not plain `http://` (SOCKS, or TLS to the proxy itself), which this transport does not speak.
//!
//! Every relay client in the product is built with [`client`] or [`builder`];
//! `no_relay_client_bypasses_the_proxy_transport` below fails if a product path builds one any
//! other way.

use std::time::Duration;

use async_wsocket::ConnectionMode;
use nostr_sdk::{
    pool::transport::{
        error::TransportError,
        websocket::{
            DefaultWebsocketTransport, WebSocketSink, WebSocketStream, WebSocketTransport,
        },
    },
    prelude::*,
};

#[cfg(not(target_arch = "wasm32"))]
pub use tunnel::{HttpProxy, ProxySettings};

/// A relay client for `keys` whose websockets go through the HTTP proxy when one is configured.
/// Use this instead of `Client::new`.
pub fn client(keys: Keys) -> Client {
    builder().signer(keys).build()
}

/// `Client::builder()` with the proxy-aware transport. Use this instead of `Client::builder()`.
pub fn builder() -> ClientBuilder {
    Client::builder().websocket_transport(RelayTransport::from_env())
}

/// nostr-sdk websocket transport: CONNECT through the configured HTTP proxy, else the SDK default.
/// The proxy settings are read once, when the transport is built, and reused for every connect and
/// reconnect of the clients that share it.
#[derive(Clone, Debug)]
pub struct RelayTransport {
    #[cfg(not(target_arch = "wasm32"))]
    proxy: ProxySettings,
}

impl RelayTransport {
    /// The proxy settings of this process's environment, read the way reqwest reads them.
    pub fn from_env() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            proxy: ProxySettings::from_env(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
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
        #[cfg(not(target_arch = "wasm32"))]
        {
            Box::pin(async move {
                let proxy = match mode {
                    ConnectionMode::Direct => self.proxy.proxy_for(url),
                    #[allow(unreachable_patterns)]
                    _ => None,
                };
                let Some(proxy) = proxy else {
                    return DefaultWebsocketTransport.connect(url, mode, timeout).await;
                };
                tunnel::connect(url, &proxy, timeout).await
            })
        }
        // The browser has no sockets of its own to tunnel; its websocket honours the browser's
        // proxy configuration already.
        #[cfg(target_arch = "wasm32")]
        {
            DefaultWebsocketTransport.connect(url, mode, timeout)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod tunnel {
    use std::{
        fmt, io,
        net::{IpAddr, SocketAddr},
        pin::Pin,
        sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };

    use async_wsocket::{
        Message, WebSocket,
        futures_util::{Sink, SinkExt, StreamExt, TryStreamExt, stream::SplitSink},
    };
    use hyper_util::client::proxy::matcher::Matcher;
    use nostr_sdk::{
        pool::transport::{
            error::TransportError,
            websocket::{WebSocketSink, WebSocketStream},
        },
        prelude::{Url, url},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        task::JoinSet,
    };

    /// Which relay websockets go through which proxy: hyper-util's matcher, as reqwest uses it.
    #[derive(Clone)]
    pub struct ProxySettings {
        matcher: Arc<Matcher>,
    }

    impl fmt::Debug for ProxySettings {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // hyper-util's own Debug redacts the proxy credentials.
            f.debug_tuple("ProxySettings").field(&self.matcher).finish()
        }
    }

    impl ProxySettings {
        /// Exactly what reqwest's default client reads (`Matcher::from_system`).
        pub fn from_env() -> Self {
            Self::with_matcher(Matcher::from_system())
        }

        pub fn with_matcher(matcher: Matcher) -> Self {
            Self {
                matcher: Arc::new(matcher),
            }
        }

        /// The proxy a websocket to `url` goes through, or `None` to dial it directly.
        pub fn proxy_for(&self, url: &Url) -> Option<HttpProxy> {
            let scheme = match url.scheme() {
                "wss" | "https" => "https",
                "ws" | "http" => "http",
                _ => return None,
            };
            if is_loopback(&url.host()?) {
                return None;
            }
            // The URL reqwest would ask the matcher about for an HTTPS call to the same host.
            let host = url.host_str()?;
            let dst = match url.port() {
                Some(port) => format!("{scheme}://{host}:{port}/"),
                None => format!("{scheme}://{host}/"),
            };
            let intercept = self.matcher.intercept(&dst.parse::<http::Uri>().ok()?)?;
            let proxy = intercept.uri();
            if proxy.scheme() != Some(&http::uri::Scheme::HTTP) {
                return None;
            }
            let host = proxy.host()?;
            Some(HttpProxy {
                host: host
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .unwrap_or(host)
                    .to_owned(),
                port: proxy.port_u16().unwrap_or(80),
                auth: intercept.basic_auth().cloned(),
            })
        }
    }

    /// An HTTP proxy that relay websockets are tunnelled through with CONNECT.
    #[derive(Clone, PartialEq, Eq)]
    pub struct HttpProxy {
        host: String,
        port: u16,
        /// The `Proxy-Authorization` value hyper-util built from the proxy URL. Never printed.
        auth: Option<http::HeaderValue>,
    }

    impl HttpProxy {
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

    fn is_loopback(host: &url::Host<&str>) -> bool {
        match host {
            url::Host::Domain(name) => {
                let name = name.trim_end_matches('.').to_ascii_lowercase();
                name == "localhost" || name.ends_with(".localhost")
            }
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
        }
    }

    /// Open the relay websocket through `proxy`, as the SDK's sink and stream.
    pub(super) async fn connect(
        url: &Url,
        proxy: &HttpProxy,
        timeout: Duration,
    ) -> Result<(WebSocketSink, WebSocketStream), TransportError> {
        let socket = connect_via_proxy(url, proxy, timeout)
            .await
            .map_err(TransportError::backend)?;
        let (tx, rx) = socket.split();
        let sink: WebSocketSink = Box::new(TunnelSink(tx));
        let stream: WebSocketStream = Box::pin(rx.map_err(TransportError::backend));
        Ok((sink, stream))
    }

    /// Open a websocket to `url` through `proxy` with HTTP CONNECT. One deadline, `timeout`, covers
    /// the whole of it: proxy lookup and dial, CONNECT, relay TLS and the websocket upgrade.
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
            let mut tcp = dial(&proxy.host, proxy.port).await.map_err(|e| {
                io::Error::new(e.kind(), format!("could not reach proxy {proxy}: {e}"))
            })?;
            let _ = tcp.set_nodelay(true);
            let mut request =
                format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n").into_bytes();
            if let Some(auth) = &proxy.auth {
                request.extend_from_slice(b"Proxy-Authorization: ");
                request.extend_from_slice(auth.as_bytes());
                request.extend_from_slice(b"\r\n");
            }
            request.extend_from_slice(b"\r\n");
            tcp.write_all(&request).await?;
            let code = read_connect_response(&mut tcp).await.map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("proxy {proxy} gave no usable CONNECT answer for {target}: {e}"),
                )
            })?;
            if !(200..300).contains(&code) {
                // The status code only: the reason phrase and headers are the proxy's text and may
                // echo the credential we sent.
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("proxy {proxy} refused CONNECT {target} with status {code}"),
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

    /// How long one proxy address gets before the next one is tried alongside it.
    const STAGGER: Duration = Duration::from_millis(250);

    /// Dial the proxy. A name with several addresses is dialled Happy-Eyeballs style: the families
    /// alternate, and each further address starts after `STAGGER` (or as soon as the previous one
    /// fails), so one blackholed address cannot eat the whole connect deadline.
    async fn dial(host: &str, port: u16) -> io::Result<TcpStream> {
        let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host, port)).await?.collect(),
        };
        let mut pending = interleave(addrs).into_iter();
        let mut attempts = JoinSet::new();
        let mut last_error = None;
        loop {
            if attempts.is_empty() {
                match pending.next() {
                    Some(addr) => {
                        attempts.spawn(TcpStream::connect(addr));
                    }
                    None => break,
                }
            }
            tokio::select! {
                Some(done) = attempts.join_next() => match done {
                    Ok(Ok(stream)) => return Ok(stream),
                    Ok(Err(error)) => {
                        last_error = Some(error);
                        if let Some(addr) = pending.next() {
                            attempts.spawn(TcpStream::connect(addr));
                        }
                    }
                    Err(error) => last_error = Some(io::Error::other(error)),
                },
                () = tokio::time::sleep(STAGGER), if pending.len() > 0 => {
                    if let Some(addr) = pending.next() {
                        attempts.spawn(TcpStream::connect(addr));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "the proxy name has no addresses")
        }))
    }

    /// Resolver order, with the two families alternating from the first address's family on.
    pub(super) fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
        let Some(first) = addrs.first().copied() else {
            return addrs;
        };
        let (mut lead, mut other): (Vec<_>, Vec<_>) = addrs
            .into_iter()
            .partition(|addr| addr.is_ipv6() == first.is_ipv6());
        let mut out = Vec::with_capacity(lead.len() + other.len());
        lead.reverse();
        other.reverse();
        loop {
            match (lead.pop(), other.pop()) {
                (None, None) => return out,
                (a, b) => out.extend(a.into_iter().chain(b)),
            }
        }
    }

    /// The most a proxy may send before the final CONNECT answer, interim (1xx) answers included.
    const MAX_HEAD: usize = 16 * 1024;

    /// Read the proxy's answer to CONNECT and return its final status code. Interim 1xx answers are
    /// skipped; anything that is not an HTTP/1.x response is an error. Reads one byte at a time so
    /// nothing that belongs to the tunnel is consumed.
    pub(super) async fn read_connect_response<R: AsyncReadExt + Unpin>(
        io: &mut R,
    ) -> io::Result<u16> {
        let mut total = 0usize;
        loop {
            let mut head = Vec::with_capacity(256);
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if total >= MAX_HEAD {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "response header too long",
                    ));
                }
                if io.read(&mut byte).await? == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed",
                    ));
                }
                head.push(byte[0]);
                total += 1;
            }
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut response = httparse::Response::new(&mut headers);
            let code = match response.parse(&head) {
                Ok(httparse::Status::Complete(_)) => response.code,
                _ => None,
            }
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "not an HTTP/1.x response")
            })?;
            match code {
                // A switch of protocols is not a tunnel.
                101 => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected 101 Switching Protocols",
                    ));
                }
                100..=199 => continue,
                _ => return Ok(code),
            }
        }
    }

    struct TunnelSink(SplitSink<WebSocket, Message>);

    impl Sink<Message> for TunnelSink {
        type Error = TransportError;

        fn poll_ready(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.0)
                .poll_ready_unpin(cx)
                .map_err(TransportError::backend)
        }

        fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            Pin::new(&mut self.0)
                .start_send_unpin(item)
                .map_err(TransportError::backend)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.0)
                .poll_flush_unpin(cx)
                .map_err(TransportError::backend)
        }

        fn poll_close(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.0)
                .poll_close_unpin(cx)
                .map_err(TransportError::backend)
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use hyper_util::client::proxy::matcher::Matcher;
    use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
    use std::{
        net::SocketAddr,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    /// Settings as reqwest would build them from these values (empty = unset).
    fn settings(https: &str, http: &str, all: &str, no: &str) -> ProxySettings {
        ProxySettings::with_matcher(
            Matcher::builder()
                .https(https)
                .http(http)
                .all(all)
                .no(no)
                .build(),
        )
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn proxied(s: &ProxySettings, u: &str) -> Option<String> {
        s.proxy_for(&url(u)).map(|p| p.to_string())
    }

    #[test]
    fn wss_takes_the_https_proxy_ws_the_http_proxy_and_all_proxy_is_the_fallback() {
        let s = settings("http://sec:3128", "plain:8080", "", "");
        assert_eq!(
            proxied(&s, "wss://relay.maxplayer.ai"),
            Some("http://sec:3128".into())
        );
        assert_eq!(
            proxied(&s, "ws://relay.example"),
            Some("http://plain:8080".into())
        );
        let s = settings("", "", "http://all:1", "");
        assert_eq!(
            proxied(&s, "wss://relay.maxplayer.ai"),
            Some("http://all:1".into())
        );
        assert_eq!(
            proxied(&s, "ws://relay.example"),
            Some("http://all:1".into())
        );
        // An unparsable scheme-specific value falls back to ALL_PROXY, as in hyper-util.
        let s = settings("bogus://p", "", "http://all:1", "");
        assert_eq!(
            proxied(&s, "wss://relay.maxplayer.ai"),
            Some("http://all:1".into())
        );
        assert_eq!(
            proxied(&settings("", "", "", ""), "wss://relay.maxplayer.ai"),
            None
        );
    }

    #[test]
    fn only_plain_http_proxies_are_tunnelled_and_others_keep_the_direct_dial() {
        for value in ["socks5://p:1080", "socks5h://p:1080", "https://p:443", ""] {
            let s = settings(value, "", "", "");
            assert_eq!(proxied(&s, "wss://r.example"), None, "{value}");
        }
        assert_eq!(
            proxied(&settings("p", "", "", ""), "wss://r.example"),
            Some("http://p:80".into())
        );
        assert_eq!(
            proxied(
                &settings("HTTP://[::1]:3128/", "", "", ""),
                "wss://r.example"
            ),
            Some("http://[::1]:3128".into())
        );
    }

    #[test]
    fn the_proxy_credential_becomes_the_basic_header_and_is_never_printed() {
        use base64::Engine as _;
        let s = settings("http://us%40er:p%3Ass%20w@proxy.corp:3128", "", "", "");
        let proxy = s.proxy_for(&url("wss://r.example")).unwrap();
        let want = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("us@er:p:ss w")
        );
        assert!(format!("{proxy:?}").contains("redacted"));
        let shown = format!("{proxy} {proxy:?} {s:?}");
        for secret in ["us@er", "us%40er", "ss w", "p%3Ass", &want[6..]] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert_eq!(
            proxy,
            settings("http://us%40er:p%3Ass%20w@proxy.corp:3128", "", "", "")
                .proxy_for(&url("wss://other.example"))
                .unwrap()
        );
    }

    #[test]
    fn no_proxy_hosts_and_loopback_are_dialled_directly() {
        let s = settings(
            "http://p:1",
            "http://p:1",
            "",
            " .corp.example, Relay.Maxplayer.ai ,10.0.0.0/8,fd00::1",
        );
        for direct in [
            "wss://relay.maxplayer.ai",
            "wss://a.b.corp.example",
            "wss://corp.example",
            "ws://10.2.3.4:7777",
            "wss://[fd00::1]",
            "ws://127.0.0.1:9",
            "ws://localhost:9",
            "ws://localhost.:9",
            "ws://relay.localhost:9",
            "ws://[::1]:9",
            "ws://[::ffff:127.0.0.1]:9",
        ] {
            assert_eq!(proxied(&s, direct), None, "{direct}");
        }
        for via in [
            "wss://maxplayer.ai",
            "wss://notcorp.example",
            "ws://11.0.0.1",
            "wss://relay.maxplayer.ai.evil",
            "ws://[::ffff:10.0.0.1]:9",
        ] {
            assert!(proxied(&s, via).is_some(), "{via}");
        }
        // hyper-util's "*" is a domain wildcard: an IP literal still goes through the proxy, for an
        // HTTPS call and so for the websocket.
        let star = settings("http://p:1", "", "", "*");
        assert_eq!(proxied(&star, "wss://r.example"), None);
        assert!(proxied(&star, "wss://203.0.113.5").is_some());
    }

    /// Run in a child process by `the_environment_is_read_exactly_as_reqwest_reads_it`, with a
    /// controlled environment: prints where `RelayTransport::from_env`'s settings send `PROBE_URL`.
    #[test]
    #[ignore = "driven by the_environment_is_read_exactly_as_reqwest_reads_it"]
    fn proxy_env_probe() {
        let Ok(target) = std::env::var("PROBE_URL") else {
            return;
        };
        let route = RelayTransport::from_env()
            .proxy
            .proxy_for(&url(&target))
            .map_or_else(|| "direct".to_owned(), |p| p.to_string());
        println!("PROBE_ROUTE={route}");
    }

    #[test]
    fn the_environment_is_read_exactly_as_reqwest_reads_it() {
        const VARS: [&str; 9] = [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "REQUEST_METHOD",
        ];
        let route = |vars: &[(&str, &str)]| -> String {
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
            cmd.args([
                "--exact",
                "relay_transport::tests::proxy_env_probe",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ]);
            for var in VARS {
                cmd.env_remove(var);
            }
            cmd.env("PROBE_URL", "wss://relay.example");
            for (k, v) in vars {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            // libtest prints the test name on the same line, before the probe's own output.
            stdout
                .split_once("PROBE_ROUTE=")
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .map(str::to_owned)
                .unwrap_or_else(|| panic!("probe printed no route: {stdout}"))
        };
        assert_eq!(route(&[]), "direct");
        assert_eq!(
            route(&[
                ("HTTPS_PROXY", "http://upper:1"),
                ("https_proxy", "http://lower:2")
            ]),
            "http://upper:1"
        );
        assert_eq!(
            route(&[("https_proxy", "http://lower:2")]),
            "http://lower:2"
        );
        // Set-but-empty uppercase hides the lowercase value, as in hyper-util (`get_first_env`).
        assert_eq!(
            route(&[("HTTPS_PROXY", ""), ("https_proxy", "http://lower:2")]),
            "direct"
        );
        assert_eq!(
            route(&[("HTTPS_PROXY", "bogus://x"), ("ALL_PROXY", "http://all:3")]),
            "http://all:3"
        );
        assert_eq!(
            route(&[("HTTPS_PROXY", "http://p:1"), ("no_proxy", "example")]),
            "direct"
        );
        // A CGI environment ignores the proxy variables (a request header could set HTTP_PROXY).
        assert_eq!(
            route(&[("HTTPS_PROXY", "http://p:1"), ("REQUEST_METHOD", "")]),
            "direct"
        );
    }

    async fn status(bytes: &[u8]) -> std::io::Result<u16> {
        let mut input = bytes;
        tunnel::read_connect_response(&mut input).await
    }

    #[tokio::test]
    async fn the_connect_answer_is_parsed_as_http_1_and_only_its_final_status_counts() {
        assert_eq!(status(b"HTTP/1.0 200 OK\r\n\r\n").await.unwrap(), 200);
        assert_eq!(
            status(b"HTTP/1.1 204 No Content\r\nVia: squid\r\n\r\n")
                .await
                .unwrap(),
            204
        );
        assert_eq!(
            status(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap(),
            407
        );
        // Interim answers are skipped and the tunnel bytes after the final one are left alone.
        let mut input: &[u8] =
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early\r\nLink: x\r\n\r\nHTTP/1.1 200 OK\r\n\r\nTUNNEL";
        assert_eq!(
            tunnel::read_connect_response(&mut input).await.unwrap(),
            200
        );
        assert_eq!(input, b"TUNNEL");
        for bad in [
            &b"HTTP/garbage 0200\r\n\r\n"[..],
            b"HTTP/2 200\r\n\r\n",
            b"ICY 200 OK\r\n\r\n",
            b"HTTP/1.1 20 OK\r\n\r\n",
            b"HTTP/1.1 101 Switching Protocols\r\n\r\n",
        ] {
            assert!(
                status(bad).await.is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let eof = status(b"HTTP/1.1 200 OK\r\n").await.unwrap_err();
        assert_eq!(eof.kind(), std::io::ErrorKind::UnexpectedEof);
        let mut huge = b"HTTP/1.1 200 OK\r\n".to_vec();
        huge.extend(
            std::iter::repeat_n(b"X-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n", 600).flatten(),
        );
        huge.extend_from_slice(b"\r\n");
        assert!(status(&huge).await.is_err());
        // The cap is cumulative across interim answers.
        let mut many = Vec::new();
        for _ in 0..1000 {
            many.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        many.extend_from_slice(b"HTTP/1.1 200 OK\r\n\r\n");
        assert!(status(&many).await.is_err());
    }

    #[tokio::test]
    async fn a_connect_answer_dribbled_out_byte_by_byte_is_read_whole() {
        let (mut client, mut server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            for byte in b"HTTP/1.0 200 Connection established\r\nProxy-agent: x\r\n\r\n" {
                server.write_all(&[*byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
            server.write_all(b"T").await.unwrap();
            let _keep = server;
        });
        assert_eq!(
            tunnel::read_connect_response(&mut client).await.unwrap(),
            200
        );
        let mut rest = [0u8; 1];
        client.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"T");
    }

    #[test]
    fn proxy_addresses_alternate_families_from_the_resolvers_first_choice() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let got = tunnel::interleave(vec![
            a("[2001:db8::1]:1"),
            a("[2001:db8::2]:1"),
            a("192.0.2.1:1"),
            a("[2001:db8::3]:1"),
        ]);
        assert_eq!(
            got,
            vec![
                a("[2001:db8::1]:1"),
                a("192.0.2.1:1"),
                a("[2001:db8::2]:1"),
                a("[2001:db8::3]:1"),
            ]
        );
        assert!(tunnel::interleave(Vec::new()).is_empty());
    }

    /// A proxy like the sandbox's: CONNECT only (anything else gets 405), optionally a required
    /// credential, optionally refusing every tunnel. Each CONNECT target's port is dialled on
    /// 127.0.0.1, so the target's NAME never has to resolve: as in the sandbox, only the proxy can
    /// reach the relay. Records each request line. A wrong credential is answered with a 407 whose
    /// reason phrase echoes the header it got, as a hostile or careless proxy might.
    async fn connect_only_proxy(
        credential: Option<&'static str>,
        refuse: bool,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        use base64::Engine as _;
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
                        let sent = head.lines().find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| {
                                    name.eq_ignore_ascii_case("proxy-authorization")
                                })
                                .map(|(_, value)| value.trim().to_owned())
                        });
                        if sent.as_deref() != Some(want.as_str()) {
                            let echo = sent.unwrap_or_default();
                            let _ = client
                                .write_all(
                                    format!("HTTP/1.1 407 bad credential {echo}\r\n\r\n")
                                        .as_bytes(),
                                )
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
        RelayTransport::with_settings(settings(&proxy, &proxy, "", ""))
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
        let relay = client.relay(&relay_url).await.unwrap();
        relay
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
        // A reconnect opens a fresh tunnel with the same settings.
        relay.disconnect();
        relay
            .try_connect(Duration::from_secs(5))
            .await
            .expect("reconnect through the proxy");
        let tunnels = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|line| **line == format!("CONNECT relay.invalid:{relay_port} HTTP/1.1"))
            .count();
        assert_eq!(tunnels, 2, "{:?}", seen.lock().unwrap());
        client.shutdown().await;
    }

    #[tokio::test]
    async fn wss_runs_verified_tls_to_the_relay_inside_the_tunnel() {
        // A TLS endpoint with a self-signed certificate, reachable only through the proxy.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = rcgen::generate_simple_self_signed(vec!["relay.invalid".to_owned()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tls_port = listener.local_addr().unwrap().port();
        let hellos = Arc::new(Mutex::new(0usize));
        let count = hellos.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                *count.lock().unwrap() += 1;
                let _ = acceptor.accept(stream).await;
            }
        });
        let (proxy_port, seen) = connect_only_proxy(None, false).await;
        let err = dial(
            &transport_via(proxy_port, ""),
            &format!("wss://relay.invalid:{tls_port}"),
            Duration::from_secs(5),
        )
        .await
        .expect_err("an untrusted certificate must fail");
        assert!(
            err.contains("handshake") && err.to_lowercase().contains("certificate"),
            "{err}"
        );
        assert_eq!(*hellos.lock().unwrap(), 1, "TLS reached the relay end");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![format!("CONNECT relay.invalid:{tls_port} HTTP/1.1")]
        );
    }

    #[tokio::test]
    async fn the_proxy_credential_is_sent_and_a_rejected_one_is_never_echoed_into_the_error() {
        use base64::Engine as _;
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
            err.contains("status 407") && err.contains(&format!("http://127.0.0.1:{proxy_port}")),
            "{err}"
        );
        let token = base64::engine::general_purpose::STANDARD.encode("us@er:hunter2");
        for secret in [
            "hunter2",
            "us%40er",
            "us@er",
            token.as_str(),
            "bad credential",
        ] {
            assert!(!err.contains(secret), "{err}");
        }
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
            err.contains("refused CONNECT relay.invalid:1 with status 403"),
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
        // The outer watchdog turns a broken deadline into a failure instead of a hung test.
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            dial(
                &transport_via(silent_port, ""),
                "ws://relay.invalid:1",
                Duration::from_millis(300),
            ),
        )
        .await
        .expect("the connect deadline must hold")
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

    /// Where a source file builds a relay client or dials a relay websocket without going through
    /// this module, skipping test-only items. Also returns the files of the test-only modules it
    /// declares (`#[cfg(test)] mod x;`), which are test code as a whole.
    fn scan_source(file: &Path, text: &str) -> (Vec<String>, Vec<PathBuf>) {
        use std::collections::HashSet;
        use syn::visit::{self, Visit};

        fn is_test_cfg(attrs: &[syn::Attribute]) -> bool {
            attrs.iter().any(|attr| {
                if !attr.path().is_ident("cfg") {
                    return false;
                }
                let Ok(list) = attr.meta.require_list() else {
                    return false;
                };
                let pred: String = list
                    .tokens
                    .to_string()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                // `test` or `all(.., test, ..)`: compiled only into tests. `any(test, ..)` is not.
                pred == "test"
                    || pred.strip_prefix("all(").is_some_and(|args| {
                        let mut depth = 0;
                        let mut arg = String::new();
                        for c in args.chars() {
                            match c {
                                '(' => depth += 1,
                                ')' if depth == 0 => break,
                                ')' => depth -= 1,
                                ',' if depth == 0 => {
                                    if arg == "test" {
                                        return true;
                                    }
                                    arg.clear();
                                    continue;
                                }
                                _ => {}
                            }
                            arg.push(c);
                        }
                        arg == "test"
                    })
            })
        }

        fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
            use syn::Item::*;
            match item {
                Const(i) => &i.attrs,
                Enum(i) => &i.attrs,
                ExternCrate(i) => &i.attrs,
                Fn(i) => &i.attrs,
                ForeignMod(i) => &i.attrs,
                Impl(i) => &i.attrs,
                Macro(i) => &i.attrs,
                Mod(i) => &i.attrs,
                Static(i) => &i.attrs,
                Struct(i) => &i.attrs,
                Trait(i) => &i.attrs,
                TraitAlias(i) => &i.attrs,
                Type(i) => &i.attrs,
                Union(i) => &i.attrs,
                Use(i) => &i.attrs,
                _ => &[],
            }
        }

        /// The SDK's client constructors and the raw websocket dials that bypass the transport.
        const CTORS: [&str; 3] = ["new", "builder", "default"];
        const DIALS: [&str; 7] = [
            "connect_async",
            "connect_async_with_config",
            "connect_async_tls_with_config",
            "client_async",
            "client_async_tls",
            "client_async_tls_with_config",
            "client_async_with_config",
        ];

        #[derive(Default)]
        struct Imports {
            nostr: HashSet<String>,
            other: HashSet<String>,
            nostr_glob: bool,
        }

        fn collect_use(tree: &syn::UseTree, path: &mut Vec<String>, out: &mut Imports) {
            let from_nostr = |path: &[String]| path.iter().any(|s| s.starts_with("nostr"));
            match tree {
                syn::UseTree::Path(p) => {
                    path.push(p.ident.to_string());
                    collect_use(&p.tree, path, out);
                    path.pop();
                }
                syn::UseTree::Group(g) => {
                    for t in &g.items {
                        collect_use(t, path, out);
                    }
                }
                syn::UseTree::Glob(_) => out.nostr_glob |= from_nostr(path),
                syn::UseTree::Name(n) => {
                    let name = n.ident.to_string();
                    if name == "Client" || name == "ClientBuilder" {
                        if from_nostr(path) {
                            out.nostr.insert(name);
                        } else {
                            out.other.insert(name);
                        }
                    }
                }
                syn::UseTree::Rename(r) => {
                    let name = r.ident.to_string();
                    if name == "Client" || name == "ClientBuilder" {
                        let alias = r.rename.to_string();
                        if from_nostr(path) {
                            out.nostr.insert(alias);
                        } else {
                            out.other.insert(alias);
                        }
                    }
                }
            }
        }

        struct Scan<'a> {
            imports: &'a Imports,
            hits: Vec<String>,
            test_mods: Vec<(String, Option<String>)>,
            collecting: bool,
            uses: Imports,
        }

        impl Scan<'_> {
            fn is_nostr_type(&self, segments: &[String]) -> bool {
                let ty = &segments[segments.len() - 2];
                if segments.len() > 2 {
                    return segments.iter().any(|s| s.starts_with("nostr"))
                        && (ty == "Client" || ty == "ClientBuilder");
                }
                self.imports.nostr.contains(ty)
                    || (self.imports.nostr_glob
                        && !self.imports.other.contains(ty)
                        && (ty == "Client" || ty == "ClientBuilder"))
            }

            fn check_path(&mut self, path: &syn::Path, line: usize) {
                let segments: Vec<String> =
                    path.segments.iter().map(|s| s.ident.to_string()).collect();
                let last = segments.last().map(String::as_str).unwrap_or("");
                let bypass =
                    (segments.len() >= 2 && CTORS.contains(&last) && self.is_nostr_type(&segments))
                        || DIALS.contains(&last)
                        || (last == "connect" && segments.iter().any(|s| s == "async_wsocket"));
                if bypass {
                    self.hits.push(format!("{line}: {}", segments.join("::")));
                }
            }

            /// Macro bodies are tokens, not syntax: look for the same paths in their text.
            fn check_tokens(&mut self, tokens: &proc_macro2::TokenStream, line: usize) {
                let text: String = tokens
                    .to_string()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let mut names: Vec<&str> = self.imports.nostr.iter().map(String::as_str).collect();
                if self.imports.nostr_glob {
                    names.extend(["Client", "ClientBuilder"]);
                }
                for name in names {
                    for ctor in CTORS {
                        let pattern = format!("{name}::{ctor}(");
                        for (at, _) in text.match_indices(&pattern) {
                            let before = &text[..at];
                            let foreign = before
                                .ends_with(|c: char| c.is_alphanumeric() || c == '_')
                                || (before.ends_with("::")
                                    && !before
                                        .rsplit(|c: char| {
                                            !(c.is_alphanumeric() || c == '_' || c == ':')
                                        })
                                        .next()
                                        .unwrap_or("")
                                        .contains("nostr"));
                            if !foreign {
                                self.hits.push(format!("{line}: {pattern}..) in a macro"));
                            }
                        }
                    }
                }
                for dial in DIALS {
                    if text.contains(&format!("{dial}(")) {
                        self.hits.push(format!("{line}: {dial}(..) in a macro"));
                    }
                }
            }
        }

        impl<'ast> Visit<'ast> for Scan<'_> {
            fn visit_item(&mut self, item: &'ast syn::Item) {
                if is_test_cfg(item_attrs(item)) {
                    if let syn::Item::Mod(m) = item
                        && m.content.is_none()
                    {
                        let path = m.attrs.iter().find_map(|a| match &a.meta {
                            syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value
                            {
                                syn::Expr::Lit(syn::ExprLit {
                                    lit: syn::Lit::Str(s),
                                    ..
                                }) => Some(s.value()),
                                _ => None,
                            },
                            _ => None,
                        });
                        self.test_mods.push((m.ident.to_string(), path));
                    }
                    return;
                }
                if self.collecting
                    && let syn::Item::Use(u) = item
                {
                    collect_use(&u.tree, &mut Vec::new(), &mut self.uses);
                }
                visit::visit_item(self, item);
            }

            fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
                if !is_test_cfg(&f.attrs) {
                    visit::visit_impl_item_fn(self, f);
                }
            }

            fn visit_stmt(&mut self, stmt: &'ast syn::Stmt) {
                if let syn::Stmt::Local(local) = stmt
                    && is_test_cfg(&local.attrs)
                {
                    return;
                }
                visit::visit_stmt(self, stmt);
            }

            fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
                if !self.collecting
                    && let syn::Expr::Path(p) = &*call.func
                {
                    use syn::spanned::Spanned;
                    self.check_path(&p.path, call.span().start().line);
                }
                visit::visit_expr_call(self, call);
            }

            fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                if !self.collecting {
                    use syn::spanned::Spanned;
                    self.check_tokens(&mac.tokens, mac.span().start().line);
                }
                visit::visit_macro(self, mac);
            }
        }

        let parsed = syn::parse_file(text)
            .unwrap_or_else(|e| panic!("{} does not parse: {e}", file.display()));
        let empty = Imports::default();
        let mut pass = Scan {
            imports: &empty,
            hits: Vec::new(),
            test_mods: Vec::new(),
            collecting: true,
            uses: Imports::default(),
        };
        pass.visit_file(&parsed);
        let imports = std::mem::take(&mut pass.uses);
        let mut scan = Scan {
            imports: &imports,
            hits: Vec::new(),
            test_mods: Vec::new(),
            collecting: false,
            uses: Imports::default(),
        };
        scan.visit_file(&parsed);

        let dir = file.parent().unwrap();
        let stem = file.file_stem().unwrap().to_string_lossy();
        let child_dir = if ["mod", "lib", "main"].contains(&stem.as_ref()) {
            dir.to_path_buf()
        } else {
            dir.join(stem.as_ref())
        };
        let test_files = scan
            .test_mods
            .iter()
            .flat_map(|(name, path)| match path {
                Some(path) => vec![dir.join(path)],
                None => vec![
                    child_dir.join(format!("{name}.rs")),
                    child_dir.join(name).join("mod.rs"),
                ],
            })
            .collect();
        let hits = scan
            .hits
            .into_iter()
            .map(|hit| format!("{}:{hit}", file.display()))
            .collect();
        (hits, test_files)
    }

    fn product_sources() -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
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
        files
    }

    /// Every relay client the product builds must use [`client`]/[`builder`], or it dials the relay
    /// directly and fails behind an HTTP-only proxy. Test-only items and modules are exempt; the
    /// files are parsed, so a test marker in a comment or on one helper exempts nothing else.
    #[test]
    fn no_relay_client_bypasses_the_proxy_transport() {
        let files = product_sources();
        assert!(
            files.len() > 50,
            "source walk found only {} files",
            files.len()
        );
        let mut test_files = std::collections::HashSet::new();
        let mut found = Vec::new();
        for file in &files {
            if file.ends_with("relay_transport.rs") {
                continue;
            }
            let (hits, tests) = scan_source(file, &std::fs::read_to_string(file).unwrap());
            test_files.extend(tests);
            found.push((file.clone(), hits));
        }
        let is_test_file = |file: &Path| {
            test_files.iter().any(|t: &PathBuf| {
                file == t
                    || file.starts_with(t.with_extension(""))
                    || (t.file_name().is_some_and(|n| n == "mod.rs")
                        && file.starts_with(t.parent().unwrap()))
            })
        };
        let offenders: Vec<String> = found
            .into_iter()
            .filter(|(file, _)| !is_test_file(file))
            .flat_map(|(_, hits)| hits)
            .collect();
        assert!(
            offenders.is_empty(),
            "relay clients built without crate::relay_transport (use relay_transport::client/builder):\n{}",
            offenders.join("\n")
        );
    }

    /// The guard itself: the shapes that slipped past the old line scan are caught, and test code
    /// and other crates' `Client`s are not.
    #[test]
    fn the_bypass_guard_parses_instead_of_cutting_at_the_first_test_marker() {
        let source = r##"
            use nostr_sdk::prelude::*;
            use reqwest::Client as Http;
            /// Docs may mention `#[cfg(test)]` without exempting what follows.
            #[cfg(test)]
            fn helper() -> Client { Client::new(Keys::generate()) }
            pub struct S { #[cfg(test)] field: u8 }
            async fn seller(keys: Keys) {
                let a = Client::new(keys);
                let b = nostr_sdk::Client::builder().build();
                let c = Client::default();
                let d = ClientBuilder::default();
                let e = Http::new();
                let f = reqwest::Client::new();
                tokio::select! { _ = async { Client::new(Keys::generate()) } => {} }
                let g = tokio_tungstenite::connect_async("ws://x");
            }
            #[cfg(all(test, feature = "wallet"))]
            mod tests { fn t() { let _ = Client::new(Keys::generate()); } }
            #[cfg(test)]
            mod more;
            #[cfg(any(test, feature = "x"))]
            fn maybe() { let _ = Client::default(); }
        "##;
        let file = Path::new("/src/seller.rs");
        let (hits, tests) = scan_source(file, source);
        let lines: Vec<&str> = hits
            .iter()
            .map(|h| h.trim_start_matches("/src/seller.rs:"))
            .collect();
        for want in [
            "9: Client::new",
            "10: nostr_sdk::Client::builder",
            "11: Client::default",
            "12: ClientBuilder::default",
            "15: Client::new(..) in a macro",
            "16: tokio_tungstenite::connect_async",
            "23: Client::default",
        ] {
            assert!(lines.contains(&want), "missing {want}: {lines:?}");
        }
        assert_eq!(lines.len(), 7, "{lines:?}");
        assert_eq!(
            tests,
            vec![
                PathBuf::from("/src/seller/more.rs"),
                PathBuf::from("/src/seller/more/mod.rs")
            ]
        );
    }
}
