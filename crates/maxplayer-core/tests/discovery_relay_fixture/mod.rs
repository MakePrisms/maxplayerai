//! A SCRIPTED NIP-01 relay for the discovery read, and a record of every frame the client sent.
//!
//! The discovery read's whole contract is about how it ends: an `EOSE` for its own subscription is
//! the only thing that may confirm it, and a timeout, a drop or a `CLOSED` must each land somewhere
//! different. None of those endings can be reached through `nostr-relay-builder` — a conforming
//! relay always answers — so the ending has to be scripted here.
//!
//! This is deliberately the crudest relay that can express them: accept a socket, read frames,
//! answer the one `REQ` according to a script. It records inbound frames by VERB, which is what
//! makes "discovery published nothing" an observable rather than a promise — an `EVENT` frame from a
//! read-only path would be recorded exactly like any other.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::Mutex;

type Writer = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio_tungstenite::tungstenite::Message,
>;

/// How the relay answers the directory `REQ`. One script per fixture, spent on every `REQ`.
#[derive(Clone, Debug)]
pub enum Script {
    /// Serve these events, then `EOSE`. A COMPLETED answer — the only ending that may confirm a
    /// read. An empty vec is the answered-empty market.
    ServeThenEose(Vec<String>),
    /// Serve these events and then go quiet, socket UP, no `EOSE` ever. The unanswered read, and
    /// with a nonempty vec the partial one.
    ServeThenSilence(Vec<String>),
    /// Serve these events, then drop the socket without an `EOSE`.
    ServeThenDrop(Vec<String>),
    /// Refuse: `CLOSED` naming the subscription, with a reason.
    Close(String),
    /// NIP-42, refused: challenge on connect, then answer the client's signed `AUTH` with a
    /// NEGATIVE `OK` and nothing else — no `CLOSED`, no `EOSE`, ever.
    ///
    /// This is the shape that downgraded a rejection into silence. A relay that refuses the
    /// identity owes the subscription no reply at all, so a reader watching only for `EOSE` or
    /// `CLOSED` waits out its deadline and reports an unanswered read — throwing away a rejection
    /// that has both a reason and a fix.
    ChallengeThenRejectAuth(String),
}

/// One inbound frame, by verb and subscription id. Enough to answer both questions the tests ask of
/// the wire: did an `EVENT` ever go out (it must not), and did the `CLOSE` cleanup arrive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub verb: String,
    pub subscription_id: Option<String>,
}

/// A running scripted relay.
///
/// The accept task outlives a dropped handle — there is no `Drop` impl and none is claimed. Each
/// test's runtime ends with the test and takes the task with it; a fixture reused on a persistent
/// runtime would need an explicit abort.
pub struct ScriptedRelay {
    url: String,
    frames: Arc<Mutex<Vec<Frame>>>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
    _accept: tokio::task::JoinHandle<()>,
}

impl ScriptedRelay {
    /// Bind an ephemeral loopback port and serve `script` to every `REQ`.
    pub async fn start(script: Script) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted relay");
        let addr: SocketAddr = listener.local_addr().expect("scripted relay addr");
        let frames: Arc<Mutex<Vec<Frame>>> = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let accept = tokio::spawn({
            let frames = Arc::clone(&frames);
            let connections = Arc::clone(&connections);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let frames = Arc::clone(&frames);
                    let script = script.clone();
                    tokio::spawn(async move {
                        let _ = serve_connection(stream, script, frames).await;
                    });
                }
            }
        });

        Self {
            url: format!("ws://{addr}"),
            frames,
            connections,
            _accept: accept,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// Every inbound frame, in arrival order.
    pub async fn frames(&self) -> Vec<Frame> {
        self.frames.lock().await.clone()
    }

    /// Verbs seen, in arrival order — the shape assertions read this.
    pub async fn verbs(&self) -> Vec<String> {
        self.frames()
            .await
            .into_iter()
            .map(|frame| frame.verb)
            .collect()
    }

    /// Sockets accepted so far.
    pub fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait until `predicate` holds over the inbound frames, or give up. Returns whether it held, so
    /// a caller asserts rather than hangs. Needed for the CLOSE cleanup: `disconnect()` returns as
    /// soon as the frame is written, and the relay reads it a moment later.
    pub async fn wait_for_frames<F>(&self, timeout: Duration, predicate: F) -> bool
    where
        F: Fn(&[Frame]) -> bool,
    {
        tokio::time::timeout(timeout, async {
            loop {
                if predicate(&self.frames().await) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    script: Script,
    frames: Arc<Mutex<Vec<Frame>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (writer, mut reader) = ws.split();
    let writer = Arc::new(Mutex::new(writer));

    // The challenge goes out unprompted, exactly as a NIP-42 relay does it — before the client has
    // asked for anything, so the AUTH exchange overlaps connect and the REQ that follows it.
    if let Script::ChallengeThenRejectAuth(_) = &script {
        send(
            &writer,
            json!(["AUTH", "maxplayer-discovery-fixture-challenge"]),
        )
        .await?;
    }

    while let Some(message) = reader.next().await {
        let message = message?;
        let text = match message {
            tokio_tungstenite::tungstenite::Message::Text(text) => text,
            tokio_tungstenite::tungstenite::Message::Close(_) => {
                frames.lock().await.push(Frame {
                    verb: "SOCKET_CLOSE".to_owned(),
                    subscription_id: None,
                });
                break;
            }
            _ => continue,
        };
        let Ok(frame) = serde_json::from_str::<Vec<Value>>(&text) else {
            continue;
        };
        let verb = frame
            .first()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let subscription_id = match verb.as_str() {
            // REQ and CLOSE both carry the subscription id in slot 1.
            "REQ" | "CLOSE" => frame.get(1).and_then(Value::as_str).map(str::to_owned),
            _ => None,
        };
        frames.lock().await.push(Frame {
            verb: verb.clone(),
            subscription_id: subscription_id.clone(),
        });

        match verb.as_str() {
            "REQ" => {
                let Some(sub_id) = subscription_id else {
                    continue;
                };
                match &script {
                    Script::ServeThenEose(events) => {
                        for event in events {
                            send_event(&writer, &sub_id, event).await?;
                        }
                        send(&writer, json!(["EOSE", sub_id])).await?;
                    }
                    Script::ServeThenSilence(events) => {
                        for event in events {
                            send_event(&writer, &sub_id, event).await?;
                        }
                        // And nothing more, deliberately: socket up, answer never finished.
                    }
                    Script::ServeThenDrop(events) => {
                        for event in events {
                            send_event(&writer, &sub_id, event).await?;
                        }
                        return Ok(());
                    }
                    Script::Close(reason) => {
                        send(&writer, json!(["CLOSED", sub_id, reason])).await?;
                    }
                    // Deliberately mute: the rejection was already delivered on the AUTH frame, and
                    // the point of this case is that NOTHING answers the subscription afterwards.
                    Script::ChallengeThenRejectAuth(_) => {}
                }
            }
            // The client's signed NIP-42 answer. Refused with a negative OK, which is what the SDK
            // turns into its `AuthenticationFailed` notification.
            "AUTH" => {
                if let Script::ChallengeThenRejectAuth(reason) = &script {
                    let id = frame
                        .get(1)
                        .and_then(|event| event.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    send(&writer, json!(["OK", id, false, reason])).await?;
                }
            }
            // An EVENT here would mean the read published something. It is recorded above, and
            // answered so a publishing client would not hang and mask the failure as a timeout.
            "EVENT" => {
                let id = frame
                    .get(1)
                    .and_then(|event| event.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                send(&writer, json!(["OK", id, true, ""])).await?;
            }
            _ => continue,
        }
    }
    Ok(())
}

/// Send one pre-signed event, verbatim. The test signs it with a real key; the fixture must not
/// re-encode it, or an invalid-signature case would be silently repaired in transit.
async fn send_event(
    writer: &Arc<Mutex<Writer>>,
    subscription_id: &str,
    event_json: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let event: Value = serde_json::from_str(event_json)?;
    send(writer, json!(["EVENT", subscription_id, event])).await
}

async fn send(
    writer: &Arc<Mutex<Writer>>,
    frame: Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    writer
        .lock()
        .await
        .send(tokio_tungstenite::tungstenite::Message::Text(
            frame.to_string().into(),
        ))
        .await?;
    Ok(())
}
