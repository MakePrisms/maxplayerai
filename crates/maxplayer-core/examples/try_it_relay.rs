//! Loopback-only disposable relay for web/app/scripts/try-local.ts. No production config.
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
#[tokio::main]
async fn main() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("local relay");
    println!("{}", relay.url().await);
    std::future::pending::<()>().await;
}
