//! The outbox → relay publisher: the concrete [`EventPublisher`] the node's drain loop uses.
//!
//! Each pending outbox row carries a full [`EventDraft`](crate::gateway::EventDraft) and the fixed
//! authored-at second it must be signed with. This publisher signs the draft THROUGH the signer
//! actor — the only holder of the seller key — and sends the resulting event to the seller's relay,
//! returning the published event id. Signing with the stored `created_at` makes the event id
//! deterministic, so a re-publish after a crash is idempotent at the relay (the store's `dedup_key`
//! stops a second enqueue; this stops a second on-wire event).
//!
//! It shares the run loop's ONE authenticated relay client (a cheap `Arc` clone) rather than
//! opening its own, so there is a single connection and a single NIP-42 session per node. The event
//! is signed by the signer actor before it reaches the client, so the client's own signer is never
//! used to sign an outbox event.

use nostr_sdk::prelude::*;

use super::outbox::EventPublisher;
use super::signer::SignerHandle;
use super::store::OutboxItem;

/// Signs outbox drafts through the signer actor and publishes them to the seller's relay over the
/// run loop's shared authenticated client.
pub struct RelayPublisher {
    signer: SignerHandle,
    client: Client,
    relay_url: String,
}

impl RelayPublisher {
    /// Build a publisher over the run loop's already-connected, NIP-42-authenticated `client` (an
    /// `Arc` clone — same connection). The seller Keys live in that client's single construction
    /// site in the runner, never here.
    pub fn new(signer: SignerHandle, client: Client, relay_url: &str) -> Self {
        Self {
            signer,
            client,
            relay_url: relay_url.to_owned(),
        }
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
