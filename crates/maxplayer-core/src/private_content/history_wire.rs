//! Count historical frames before the SDK can discard expired, invalid, deleted,
//! or policy-rejected events. No event payloads or credentials are retained.
use super::{Error, Result};
use async_wsocket::{ConnectionMode, Message};
use futures_util::StreamExt;
use nostr_sdk::{
    pool::transport::{
        error::TransportError,
        websocket::{
            DefaultWebsocketTransport, WebSocketSink, WebSocketStream, WebSocketTransport,
        },
    },
    prelude::*,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Default)]
struct Page {
    rows: usize,
    ended: bool,
    failed: bool,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct WireHistory(Arc<Mutex<BTreeMap<(String, String), Page>>>);

impl WireHistory {
    pub(crate) fn client(keys: &Keys) -> (Client, Self) {
        let wire = Self::default();
        let client = Client::builder()
            .signer(keys.clone())
            .websocket_transport(wire.clone())
            .build();
        (client, wire)
    }

    pub(super) fn page(&self, relay: &Relay, id: &SubscriptionId) -> PageGuard {
        // RelayUrl preserves the user's omitted trailing slash; the transport
        // receives a canonical Url. Use the same canonical representation here.
        let url: &Url = relay.url().into();
        let key = (url.to_string(), id.to_string());
        self.0
            .lock()
            .expect("history wire lock")
            .insert(key.clone(), Page::default());
        PageGuard {
            wire: self.clone(),
            key,
        }
    }

    fn failed(&self, url: &str) {
        for ((peer, _), page) in self.0.lock().expect("history wire lock").iter_mut() {
            if peer == url && !page.ended {
                page.failed = true;
            }
        }
    }

    fn observe(&self, url: &str, text: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            self.failed(url);
            return;
        };
        let (Some(kind), Some(id)) = (value[0].as_str(), value[1].as_str()) else {
            self.failed(url);
            return;
        };
        let mut pages = self.0.lock().expect("history wire lock");
        let Some(page) = pages.get_mut(&(url.to_owned(), id.to_owned())) else {
            return;
        };
        // Freeze at the first EOSE; live events after that aren't history.
        if page.ended {
            return;
        }
        match kind {
            "EVENT" => page.rows = page.rows.saturating_add(1),
            "EOSE" => page.ended = true,
            "CLOSED" => {
                // NIP-42 closes then reissues the same subscription. A response
                // already containing events cannot be silently restarted.
                if page.rows != 0
                    || !value[2]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("auth-required:")
                {
                    page.failed = true;
                }
            }
            _ => {}
        }
    }
}

pub(super) struct PageGuard {
    wire: WireHistory,
    key: (String, String),
}
impl PageGuard {
    pub(super) fn complete(&self, accepted: usize) -> Result<()> {
        let pages = self.wire.0.lock().expect("history wire lock");
        let page = pages
            .get(&self.key)
            .ok_or(Error("lifecycle wire history missing"))?;
        if page.failed || !page.ended || page.rows != accepted {
            return Err(Error("lifecycle wire history incomplete"));
        }
        Ok(())
    }
}
impl Drop for PageGuard {
    fn drop(&mut self) {
        self.wire
            .0
            .lock()
            .expect("history wire lock")
            .remove(&self.key);
    }
}

impl WebSocketTransport for WireHistory {
    fn support_ping(&self) -> bool {
        DefaultWebsocketTransport.support_ping()
    }
    fn connect<'a>(
        &'a self,
        url: &'a Url,
        mode: &'a ConnectionMode,
        timeout: Duration,
    ) -> BoxedFuture<'a, std::result::Result<(WebSocketSink, WebSocketStream), TransportError>>
    {
        Box::pin(async move {
            let (sink, stream) = DefaultWebsocketTransport
                .connect(url, mode, timeout)
                .await?;
            let wire = self.clone();
            let url = url.to_string();
            // A reconnect must not turn an interrupted page into a fresh empty page.
            wire.failed(&url);
            let stream = stream.map(move |frame| {
                match &frame {
                    Ok(Message::Text(text)) => wire.observe(&url, text),
                    Ok(Message::Binary(_)) | Ok(Message::Close(_)) | Err(_) => wire.failed(&url),
                    _ => {}
                }
                frame
            });
            Ok((sink, Box::pin(stream) as WebSocketStream))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page(wire: &WireHistory, peer: &str, id: &str) -> PageGuard {
        let key = (peer.into(), id.into());
        wire.0.lock().unwrap().insert(key.clone(), Page::default());
        PageGuard {
            wire: wire.clone(),
            key,
        }
    }
    #[test]
    fn counts_before_validation_and_freezes_before_live_events() {
        let wire = WireHistory::default();
        let page = page(&wire, "a", "s");
        // Even an event that fails SDK parsing must count.
        wire.observe("a", r#"["EVENT","s",{}]"#);
        wire.observe("a", r#"["EOSE","s"]"#);
        wire.observe("a", r#"["EVENT","s",{}]"#);
        assert!(page.complete(0).is_err());
        assert!(page.complete(1).is_ok());
        drop(page);
        assert!(wire.0.lock().unwrap().is_empty());
    }
    #[test]
    fn peer_subscription_and_exact_eose_are_required() {
        let wire = WireHistory::default();
        let page = page(&wire, "a", "s");
        wire.observe("b", r#"["EOSE","s"]"#);
        wire.observe("a", r#"["EOSE","other"]"#);
        assert!(page.complete(0).is_err());
        wire.observe("a", r#"["EOSE","s"]"#);
        assert!(page.complete(0).is_ok());
    }
    #[test]
    fn malformed_frames_and_connection_gaps_cannot_certify_empty_history() {
        for corrupt in [true, false] {
            let wire = WireHistory::default();
            let page = page(&wire, "a", "s");
            if corrupt {
                wire.observe("a", "not json");
            } else {
                wire.failed("a");
            }
            wire.observe("a", r#"["EOSE","s"]"#);
            assert!(page.complete(0).is_err());
        }
    }
    #[test]
    fn auth_retry_only_before_rows_and_never_after_query_failure() {
        let wire = WireHistory::default();
        let empty = page(&wire, "a", "s");
        wire.observe("a", r#"["CLOSED","s","auth-required: sign in"]"#);
        wire.observe("a", r#"["EOSE","s"]"#);
        assert!(empty.complete(0).is_ok());
        for reason in ["auth-required: sign in", "error: database"] {
            let partial = page(&wire, "a", "partial");
            wire.observe("a", r#"["EVENT","partial",{}]"#);
            wire.observe(
                "a",
                &serde_json::json!(["CLOSED", "partial", reason]).to_string(),
            );
            wire.observe("a", r#"["EOSE","partial"]"#);
            assert!(partial.complete(1).is_err());
        }
    }
}

#[cfg(test)]
#[tokio::test]
async fn canonical_transport_url_matches_relay_with_or_without_slash() {
    for url in ["ws://127.0.0.1:1234", "ws://127.0.0.1:1234/"] {
        let (client, wire) = WireHistory::client(&Keys::generate());
        client.add_relay(url).await.unwrap();
        let relay = client.relay(url).await.unwrap();
        let page = wire.page(&relay, &SubscriptionId::new("s"));
        wire.observe(Url::parse(url).unwrap().as_str(), r#"["EOSE","s"]"#);
        assert!(page.complete(0).is_ok());
    }
}
