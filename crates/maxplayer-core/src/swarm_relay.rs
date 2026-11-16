//! Swarm events on the home's relay (docs/specs/swarm.md §5, §6): one relay, `relay_url`.
//!
//! Reads end in the relay's EOSE or fail: a swarm read that came back short would make a member
//! look absent, a closed swarm look open, or a decision look missing, and every caller acts on
//! those. Writes are signed with the home key and count only when the relay accepted them.
use std::time::Duration;

use nostr_sdk::prelude::*;

use crate::gateway::EventDraft;
use crate::home::{self, MaxplayerHome};
use crate::seller_node::stored_read::{StoredRead, read_stored};

/// How long one read waits for EOSE, and one write for the relay's answer.
const TIMEOUT: Duration = Duration::from_secs(15);
/// Authors per membership read; relays cap filter sizes.
pub const AUTHOR_CHUNK: usize = 100;

/// One connection to the home's relay, signed in with the home key when the relay asks.
pub struct SwarmRelay {
    client: Client,
    relay: Relay,
    sign_in: crate::relay_auth::SignInWatch,
    keys: Keys,
    url: String,
}

/// The home key, for signing swarm events.
pub fn home_keys(home: &MaxplayerHome) -> Result<Keys, String> {
    let secret = home::read_secret_key_hex(home).map_err(|_| "could not read home key")?;
    Keys::parse(&secret).map_err(|_| "invalid home key".into())
}

impl SwarmRelay {
    pub async fn connect(home: &MaxplayerHome) -> Result<Self, String> {
        Self::connect_url(home, &home.config.relay_url.clone()).await
    }

    pub async fn connect_url(home: &MaxplayerHome, url: &str) -> Result<Self, String> {
        let keys = home_keys(home)?;
        let client = Client::new(keys.clone());
        client.automatic_authentication(true);
        client
            .add_relay(url)
            .await
            .map_err(|error| format!("relay {url}: {error}"))?;
        let relay = client
            .relay(url)
            .await
            .map_err(|error| format!("relay {url}: {error}"))?;
        // Before connect: buzz refuses an unregistered key's sign-in as soon as the socket opens.
        let sign_in = crate::relay_auth::SignInWatch::new(&relay);
        client.connect().await;
        client.wait_for_connection(TIMEOUT).await;
        if !relay.is_connected() {
            client.disconnect().await;
            return Err(format!("relay {url}: no connection"));
        }
        Ok(Self {
            client,
            relay,
            sign_in,
            keys,
            url: url.to_owned(),
        })
    }

    pub fn public_key(&self) -> String {
        self.keys.public_key().to_hex()
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Every stored event matching `filters`, or an error when the relay did not finish the read.
    pub async fn fetch(&self, filters: Vec<Filter>) -> Result<Vec<Event>, String> {
        match read_stored(&self.relay, filters, TIMEOUT).await {
            StoredRead::Complete(events) => Ok(events),
            StoredRead::Incomplete { reason, .. } => Err(format!(
                "relay {}: read incomplete ({}); nothing was changed",
                self.url,
                self.sign_in.annotate(&reason)
            )),
        }
    }

    /// Like [`Self::fetch`], but chunks a large author list into several reads.
    pub async fn fetch_by_authors(
        &self,
        authors: &[PublicKey],
        filter: impl Fn() -> Filter,
    ) -> Result<Vec<Event>, String> {
        let mut out = Vec::new();
        for chunk in authors.chunks(AUTHOR_CHUNK) {
            out.extend(self.fetch(vec![filter().authors(chunk.iter().copied())]).await?);
        }
        Ok(out)
    }

    /// Sign `draft` at `created_at` and publish it; the event id once the relay accepted it.
    pub async fn publish(
        &self,
        home: &MaxplayerHome,
        draft: EventDraft,
        created_at: u64,
    ) -> Result<String, String> {
        reject_secret_in_draft(home, &draft)?;
        let tags = draft
            .tags
            .into_iter()
            .map(|t| Tag::parse(t.0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "invalid tags")?;
        let event = EventBuilder::new(Kind::from(draft.kind), draft.content)
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(&self.keys)
            .map_err(|_| "swarm event signing failed")?;
        let sent = tokio::time::timeout(TIMEOUT, self.client.send_event(&event)).await;
        match sent {
            Ok(Ok(output)) if !output.success.is_empty() => Ok(event.id.to_hex()),
            Ok(Ok(output)) => {
                let reason = output
                    .failed
                    .values()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "not acknowledged".into());
                Err(crate::relay_refusal::with_hint(format!(
                    "relay {} refused the swarm event: {}; nothing was published",
                    self.url,
                    self.sign_in.annotate(&reason)
                )))
            }
            Ok(Err(error)) => Err(format!(
                "relay {}: publish failed ({}); nothing was published",
                self.url,
                self.sign_in.annotate(&error.to_string())
            )),
            Err(_) => Err(format!(
                "relay {}: no answer to the publish within {}s ({}); it may or may not have landed",
                self.url,
                TIMEOUT.as_secs(),
                self.sign_in.annotate("timeout")
            )),
        }
    }

    pub async fn disconnect(self) {
        self.client.disconnect().await;
    }
}

/// Refuse a draft that would carry the home's secret key anywhere.
pub fn reject_secret_in_draft(home: &MaxplayerHome, draft: &EventDraft) -> Result<(), String> {
    let secret = home::read_secret_key_hex(home).map_err(|_| "could not read home key")?;
    if !secret.is_empty()
        && (draft.content.contains(&secret)
            || draft.tags.iter().any(|tag| tag.0.iter().any(|v| v.contains(&secret))))
    {
        return Err("event would publish secret key".into());
    }
    Ok(())
}

/// `created_at` for a replacement of `(author, kind, d)`: now, or one second after the newest
/// stored version, so the replacement always wins. Refuses when that lands too far ahead.
pub fn replacement_timestamp(
    now: u64,
    events: &[Event],
    author: &str,
    kind: u16,
    d: &str,
) -> Result<u64, String> {
    let previous = events
        .iter()
        .filter(|e| {
            e.pubkey.to_hex() == author
                && e.kind.as_u16() == kind
                && e.tags.identifier() == Some(d)
                && e.verify().is_ok()
        })
        .map(|e| e.created_at.as_secs())
        .max();
    let created = previous.map_or(now, |t| now.max(t.saturating_add(1)));
    if created > now.saturating_add(crate::swarm::CREATED_AT_TOLERANCE) {
        return Err("existing event timestamp is too far in the future".into());
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(keys: &Keys, kind: u16, d: &str, t: u64) -> Event {
        EventBuilder::new(Kind::from(kind), "")
            .tags([Tag::identifier(d)])
            .custom_created_at(Timestamp::from(t))
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn replacement_timestamp_beats_the_stored_version() {
        let k = Keys::generate();
        let me = k.public_key().to_hex();
        let stored = [event(&k, 30342, "x", 1000), event(&Keys::generate(), 30342, "x", 5000)];
        assert_eq!(replacement_timestamp(900, &stored, &me, 30342, "x"), Ok(1001));
        assert_eq!(replacement_timestamp(2000, &stored, &me, 30342, "x"), Ok(2000));
        // Another author's or another `d`'s event does not move it.
        assert_eq!(replacement_timestamp(900, &stored, &me, 30342, "y"), Ok(900));
        let ahead = [event(&k, 30342, "x", 900 + 901)];
        assert!(replacement_timestamp(900, &ahead, &me, 30342, "x").is_err());
    }
}
