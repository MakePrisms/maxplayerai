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
    home: Option<crate::home::MaxplayerHome>,
    authenticated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The shared connection's NIP-42 sign-in state (#1228): a write refused because the relay
    /// refused this key's sign-in says so, instead of a bare `timeout`.
    sign_in: Option<std::sync::Arc<crate::relay_auth::SignInWatch>>,
}

impl RelayPublisher {
    /// Build a publisher over the run loop's already-connected, NIP-42-authenticated `client` (an
    /// `Arc` clone — same connection). The seller Keys live in that client's single construction
    /// site in the runner, never here.
    pub fn new(signer: SignerHandle, client: Client, relay_url: &str) -> Self {
        Self {
            home: None,
            authenticated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sign_in: None,
            signer,
            client,
            relay_url: relay_url.to_owned(),
        }
    }
    pub fn with_auth(mut self, auth: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self { self.authenticated = auth; self }
    pub fn with_home(mut self, home: crate::home::MaxplayerHome) -> Self { self.home = Some(home); self }
    pub fn with_sign_in(mut self, sign_in: std::sync::Arc<crate::relay_auth::SignInWatch>) -> Self {
        self.sign_in = Some(sign_in);
        self
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
        if super::privacy::is_private(&item.draft) {
            if !self.authenticated.load(std::sync::atomic::Ordering::Relaxed) { return Err("private publication requires authenticated relay".into()); }
            let home = self.home.as_ref().ok_or("private publisher home unavailable")?;
            super::privacy::record_publication(home, self.signer.public_key_hex(), &event).map_err(|e| e.to_string())?;
            // The signed carrier and all recipient copies are durable before the first
            // publication. Independent-copy retry is driven by the normal drain tick.
        }
        if crate::private_content::public_v2::is_public(&event) {
            let home=self.home.as_ref().ok_or("public v2 publisher home unavailable")?;
            crate::private_content::public_v2::publication(home,&event).map_err(|e|e.to_string())?;
        }
        let output = self
            .client
            .send_event_to([self.relay_url.as_str()], &event)
            .await
            .map_err(|error| format!("send: {error}"))?;
        if output.success.is_empty() {
            if output.failed.is_empty() {
                return Err(format!("no relay accepted event {}", signed.id));
            }
            let reasons: Vec<String> = output
                .failed
                .values()
                .map(|reason| match &self.sign_in {
                    Some(sign_in) => sign_in.annotate(reason),
                    None => reason.clone(),
                })
                .collect();
            return Err(crate::relay_refusal::with_hint(format!(
                "no relay accepted event {} ({})",
                signed.id,
                reasons.join("; ")
            )));
        }
        // The accepted id equals the signed id by construction; return the signed id (the value the
        // store records as `published_event_id`).
        Ok(signed.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seller_node::p_gate_relay_fixture::PGateRelay;
    use std::sync::Arc;
    use std::time::Duration;

    /// #1228: an outbox write (ad, claim, result) refused because the relay refused the seat's
    /// sign-in names the refusal and the link command, instead of a bare `timeout`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn issue_1228_a_write_refused_at_sign_in_names_the_refusal() {
        let relay = PGateRelay::start(Duration::from_millis(0)).await;
        relay.refuse_auth_and_close();
        let dir = tempfile::tempdir().unwrap();
        let home = crate::home::bootstrap(dir.path()).unwrap();
        let signer = crate::seller_node::signer::spawn(&home).unwrap();
        let client = Client::new(Keys::generate());
        client.add_relay(relay.url()).await.unwrap();
        let handle = client.relay(relay.url()).await.unwrap();
        let sign_in = Arc::new(crate::relay_auth::SignInWatch::new(&handle));
        client.connect().await;
        let publisher =
            RelayPublisher::new(signer, client.clone(), &relay.url()).with_sign_in(sign_in);
        let item = OutboxItem {
            id: 1,
            dedup_key: "issue-1228".into(),
            draft: crate::gateway::EventDraft::new(1, Vec::new(), "ad stand-in"),
            created_at_unix: Timestamp::now().as_secs() as i64,
            attempts: 0,
            expires_at_unix: i64::MAX,
        };
        let error = publisher.publish(&item).await.expect_err("the write is refused");
        client.disconnect().await;
        assert!(error.contains("after the relay refused NIP-42 sign-in"), "{error}");
        assert!(error.contains("not a relay member"), "{error}");
        assert!(error.contains("run `maxplayer account link`"), "{error}");
    }
}
