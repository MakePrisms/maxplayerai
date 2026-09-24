//! A local NIP-01 relay for the seller-credits end-to-end run (spec stage 3). Prints its URL and
//! serves until killed. Ephemeral kinds (23410/23411) are delivered and not stored, like the real
//! relay, and NIP-42 is required for reads and writes, as on relay.maxplayer.ai.
//!
//! relay.maxplayer.ai sends its NIP-42 challenge as soon as a socket opens, and core's receipt
//! publisher relies on that: it waits for `Authenticated` before sending anything. The
//! `nostr-relay-builder` relay only challenges once a client reads or writes. So the relay runs on
//! `port + 1` behind a front on `port` that, per connection, triggers the challenge upstream at
//! once (a REQ the upstream refuses with AUTH) and hides that probe's own CLOSED from the client.
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use nostr_relay_builder::prelude::{
    LocalRelay, RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

const PROBE: &str = "__auth_on_connect";

#[tokio::main]
async fn main() -> Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(47810);
    let relay = LocalRelay::new(
        RelayBuilder::default()
            .port(port + 1)
            .nip42(RelayBuilderNip42 {
                mode: RelayBuilderNip42Mode::Both,
            }),
    );
    relay.run().await?;
    let upstream = relay.url().await.to_string();
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    println!("ws://127.0.0.1:{port} (upstream {upstream})");
    loop {
        let (tcp, _) = listener.accept().await?;
        let upstream = upstream.clone();
        tokio::spawn(async move {
            if let Err(error) = bridge(tcp, &upstream).await {
                eprintln!("front: {error}");
            }
        });
    }
}

async fn bridge(tcp: TcpStream, upstream: &str) -> Result<()> {
    let client = tokio_tungstenite::accept_async(tcp).await?;
    let (relay, _) = tokio_tungstenite::connect_async(upstream).await?;
    let (mut to_client, mut from_client) = client.split();
    let (mut to_relay, mut from_relay) = relay.split();
    to_relay
        .send(Message::text(format!(r#"["REQ","{PROBE}",{{"limit":0}}]"#)))
        .await?;
    let up = async {
        while let Some(message) = from_client.next().await {
            let message = message?;
            if message.is_close() {
                break;
            }
            to_relay.send(message).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let down = async {
        while let Some(message) = from_relay.next().await {
            let message = message?;
            if let Message::Text(text) = &message
                && text.as_str().contains(PROBE)
            {
                continue;
            }
            to_client.send(message).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        result = up => result,
        result = down => result,
    }
}
