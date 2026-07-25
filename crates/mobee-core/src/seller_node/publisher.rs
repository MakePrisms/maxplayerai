//! The outbox → relay publisher: the concrete [`EventPublisher`] the node's drain loop uses.
//!
//! Each pending outbox row carries a full [`EventDraft`](crate::gateway::EventDraft) and the fixed
//! authored-at second it must be signed with. This publisher signs the draft THROUGH the signer
//! actor — the only holder of the seller key — and sends the resulting event to the seller's relay,
//! returning the published event id. Signing with the stored `created_at` makes the event id
//! deterministic, so a re-publish after a crash is idempotent at the relay (the store's `dedup_key`
//! stops a second enqueue; this stops a second on-wire event).
//!
//! One long-lived client per node (reconnect handled by nostr-sdk), matching the node's
//! one-process/one-identity shape rather than the legacy connect-per-send. The event is signed
//! before it reaches the client, so the client's own signer is never used and never sees the key.

use nostr_sdk::prelude::*;

use super::outbox::EventPublisher;
use super::signer::SignerHandle;
use super::store::OutboxItem;

/// Signs outbox drafts through the signer actor and publishes them to the seller's relay.
pub struct RelayPublisher {
    signer: SignerHandle,
    client: Client,
    relay_url: String,
}

impl RelayPublisher {
    /// Connect a publisher to `relay_url`. The client only ever sends PRE-SIGNED events, so it is
    /// built with a throwaway identity — the seller secret stays inside the signer actor.
    pub async fn connect(signer: SignerHandle, relay_url: &str) -> Result<Self, String> {
        let client = Client::new(Keys::generate());
        client
            .add_relay(relay_url)
            .await
            .map_err(|error| format!("add relay: {error}"))?;
        client.connect().await;
        Ok(Self {
            signer,
            client,
            relay_url: relay_url.to_owned(),
        })
    }

    /// Disconnect the underlying relay client (node shutdown).
    pub async fn disconnect(&self) {
        self.client.disconnect().await;
    }
}

impl EventPublisher for RelayPublisher {
    async fn publish(&self, item: &OutboxItem) -> Result<String, String> {
        // Sign through the actor with the row's FIXED created_at — deterministic id ⇒ a re-publish
        // after a crash is the same event, which the relay dedupes.
        let signed = self
            .signer
            .sign(item.draft.clone(), item.created_at_unix)
            .await
            .map_err(|error| error.to_string())??;
        let event = Event::from_json(&signed.json).map_err(|error| format!("decode signed: {error}"))?;
        let output = self
            .client
            .send_event_to([self.relay_url.as_str()], &event)
            .await
            .map_err(|error| format!("send: {error}"))?;
        if output.success.is_empty() {
            return Err(format!("no relay accepted event {}", signed.id));
        }
        // The accepted id equals the signed id by construction; return the signed id (the value the
        // store records as `published_event_id`).
        Ok(signed.id)
    }
}
