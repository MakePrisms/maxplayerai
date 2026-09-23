//! Authenticated content transport. Relay acceptance is not an application ACK.
//! A timed-out query MUST NOT advance the cursor: nostr-sdk's convenience fetch
//! returns partial results on timeout, so this lane explicitly observes EOSE.
use super::{Error, Result, store::ContentStore};
use crate::relay_auth::{AuthWait, wait_for_nip42_auth};
use nostr_sdk::{pool::RelayNotification, prelude::*};
use std::{collections::BTreeMap, future::Future, time::Duration};

const WINDOW_LIMIT: usize = 128;
const IO_TIMEOUT: Duration = Duration::from_secs(20);
pub trait ContentSender {
    fn send(&mut self, event: Event) -> impl Future<Output = Result<()>> + Send;
}
#[derive(Default, Debug, PartialEq, Eq)]
pub struct FlushReport {
    pub accepted: usize,
    pub pending: usize,
}
/// Each copy gets an independent attempt. An unavailable service recipient does not
/// prevent buyer/seller copies, and failed copies survive restart in the same outbox.
pub async fn flush<S: ContentSender>(
    db: &mut ContentStore,
    keys: &Keys,
    sender: &mut S,
    limit: usize,
) -> Result<FlushReport> {
    let mut report = FlushReport::default();
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    for copy in db.pending(&keys.public_key().to_hex(), limit.min(64))? {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            report.pending += 1;
            continue;
        };
        let event = copy.wrap(keys).await?;
        if matches!(
            tokio::time::timeout(remaining, sender.send(event)).await,
            Ok(Ok(()))
        ) {
            db.relay_accepted(
                &copy.content.body().job_id,
                &copy.content.body().message_id,
                &copy.recipient,
            )?;
            report.accepted += 1;
        } else {
            report.pending += 1;
        }
    }
    // Lifecycle publication does not await service decryption or successful service
    // delivery; failed recipient copies remain durable and are retried next round.
    for event in db.pending_carriers(&keys.public_key().to_hex(), limit.min(64))? {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            report.pending += 1;
            continue;
        };
        if matches!(
            tokio::time::timeout(remaining, sender.send(event.clone())).await,
            Ok(Ok(()))
        ) {
            db.carrier_accepted(&event)?;
            report.accepted += 1;
        } else {
            report.pending += 1;
        }
    }
    Ok(report)
}

pub struct AuthenticatedContentRelay {
    client: Client,
    relay: Relay,
    owns_connection: bool,
}
impl AuthenticatedContentRelay {
    pub async fn connect(keys: &Keys, url: &str) -> Result<Self> {
        let client = Client::new(keys.clone());
        client.automatic_authentication(true);
        client
            .add_relay(url)
            .await
            .map_err(|_| Error("content relay registration failed"))?;
        let url = RelayUrl::parse(url).map_err(|_| Error("invalid content relay URL"))?;
        let relay = client
            .relays()
            .await
            .get(&url)
            .cloned()
            .ok_or(Error("content relay unavailable"))?;
        let mut notifications = relay.notifications();
        client.connect().await;
        if wait_for_nip42_auth(&mut notifications, IO_TIMEOUT).await != Ok(AuthWait::Authenticated)
        {
            client.disconnect().await;
            return Err(Error("content relay authentication required"));
        }
        Ok(Self {
            client,
            relay,
            owns_connection: true,
        })
    }
    /// Attach to the seller's existing connection only after its live connection
    /// state has observed NIP-42 authentication. No new signer/key copy is created.
    pub async fn shared(client: Client, url: &str, authentication: AuthWait) -> Result<Self> {
        if authentication != AuthWait::Authenticated {
            return Err(Error("content relay authentication required"));
        }
        let relay = client
            .relay(url)
            .await
            .map_err(|_| Error("content relay unavailable"))?;
        Ok(Self {
            client,
            relay,
            owns_connection: false,
        })
    }
    pub async fn disconnect(self) {
        if self.owns_connection {
            self.client.disconnect().await;
        }
    }

    /// Fetch a complete interval, including author self-copies. The Message lane is
    /// deliberate: RelayNotification::Event suppresses events sent by this client.
    async fn window(&self, recipient: PublicKey, start: u64, end: u64) -> Result<Vec<Event>> {
        let id = SubscriptionId::generate();
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(recipient)
            .since(Timestamp::from(start))
            .until(Timestamp::from(end))
            .limit(WINDOW_LIMIT);
        let mut notifications = self.relay.notifications();
        self.relay
            .subscribe_with_id(id.clone(), filter, SubscribeOptions::default())
            .await
            .map_err(|_| Error("content subscription failed"))?;
        let result = tokio::time::timeout(
            IO_TIMEOUT,
            collect_window(&mut notifications, &id, recipient, start, end),
        )
        .await
        .map_err(|_| Error("content backfill incomplete"));
        // Closing failure is not success and does not permit advancing the cursor.
        let closed = self.relay.unsubscribe(&id).await;
        if closed.is_err() {
            return Err(Error("content subscription close failed"));
        }
        result?
    }
    /// At most 32 complete query intervals per call. A full interval is bisected,
    /// never mistaken for all available events. Cursor updates occur oldest-first.
    /// Invalid/foreign-domain wrappers are ignored, never copied into the content store.
    pub async fn backfill(
        &self,
        db: &mut ContentStore,
        keys: &Keys,
        through: u64,
    ) -> Result<usize> {
        self.backfill_using(db, keys.public_key(), through, |event| async move {
            super::transport::unwrap_content(keys, &event).await
        })
        .await
    }
    pub async fn backfill_actor(
        &self,
        db: &mut ContentStore,
        signer: &crate::seller_node::signer::SignerHandle,
        through: u64,
    ) -> Result<usize> {
        let recipient = PublicKey::from_hex(signer.public_key_hex())
            .map_err(|_| Error("invalid content recipient"))?;
        self.backfill_using(db, recipient, through, |event| async move {
            signer
                .unwrap_private_content(event)
                .await
                .map_err(|_| Error("content signer unavailable"))?
        })
        .await
    }
    async fn backfill_using<F, Fut>(
        &self,
        db: &mut ContentStore,
        recipient_key: PublicKey,
        through: u64,
        unwrap: F,
    ) -> Result<usize>
    where
        F: Fn(Event) -> Fut,
        Fut: Future<Output = Result<super::PreparedContent>>,
    {
        let recipient = recipient_key.to_hex();
        let (start, through) = db.scan_window(&recipient, through)?;
        if start > through {
            db.scan_finished(&recipient)?;
            return Ok(0);
        }
        let mut ranges = vec![(start, through)];
        let mut staged = 0;
        for _ in 0..32 {
            let Some((start, end)) = ranges.pop() else {
                break;
            };
            let events = self.window(recipient_key, start, end).await?;
            if events.len() == WINDOW_LIMIT {
                if start == end {
                    return Err(Error("content backfill timestamp saturated"));
                }
                let mid = start + (end - start) / 2;
                ranges.push((mid + 1, end));
                ranges.push((start, mid));
                continue;
            }
            for event in events {
                if let Ok(content) = unwrap(event).await {
                    staged += usize::from(db.stage(&content, &recipient, through)?);
                }
            }
            db.scan_progress(&recipient, end)?;
        }
        db.scan_finished(&recipient)?;
        Ok(staged)
    }
}
impl ContentSender for AuthenticatedContentRelay {
    async fn send(&mut self, event: Event) -> Result<()> {
        let sent = self
            .client
            .send_event(&event)
            .await
            .map_err(|_| Error("content publication failed"))?;
        if sent.success.is_empty() {
            return Err(Error("content publication refused"));
        }
        Ok(())
    }
}

async fn collect_window(
    notifications: &mut tokio::sync::broadcast::Receiver<RelayNotification>,
    id: &SubscriptionId,
    recipient: PublicKey,
    start: u64,
    end: u64,
) -> Result<Vec<Event>> {
    let mut events = BTreeMap::new();
    loop {
        let notification = notifications
            .recv()
            .await
            .map_err(|_| Error("content backfill notification gap"))?;
        match notification {
            RelayNotification::Message {
                message:
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    },
            } if subscription_id.as_ref() == id => {
                if event.kind != Kind::GiftWrap
                    || event.created_at.as_secs() < start
                    || event.created_at.as_secs() > end
                    || event.tags.len() != 1
                    || event.tags.iter().next() != Some(&Tag::public_key(recipient))
                    || event.content.len() > super::transport::MAX_WRAPPER_CONTENT
                {
                    return Err(Error("unexpected content query event"));
                }
                event
                    .verify()
                    .map_err(|_| Error("invalid content wrapper signature"))?;
                events.insert(event.id, event.into_owned());
                if events.len() > WINDOW_LIMIT {
                    return Err(Error("content query overflow"));
                }
            }
            RelayNotification::Message {
                message: RelayMessage::EndOfStoredEvents(subscription_id),
            } if subscription_id.as_ref() == id => return Ok(events.into_values().collect()),
            RelayNotification::Message {
                message:
                    RelayMessage::Closed {
                        subscription_id, ..
                    },
            } if subscription_id.as_ref() == id => {
                return Err(Error("content subscription refused"));
            }
            RelayNotification::AuthenticationFailed
            | RelayNotification::Shutdown
            | RelayNotification::RelayStatus {
                status: RelayStatus::Disconnected | RelayStatus::Terminated,
            } => return Err(Error("content relay disconnected")),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    #[tokio::test]
    async fn cursor_requires_exact_eose_not_foreign_eose_or_connection_close() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        let id = SubscriptionId::new("content");
        tx.send(RelayNotification::Message {
            message: RelayMessage::EndOfStoredEvents(Cow::Owned(SubscriptionId::new("other"))),
        })
        .unwrap();
        tx.send(RelayNotification::Shutdown).unwrap();
        assert!(
            collect_window(&mut rx, &id, Keys::generate().public_key(), 0, u64::MAX)
                .await
                .is_err()
        );
        tx.send(RelayNotification::Message {
            message: RelayMessage::EndOfStoredEvents(Cow::Owned(id.clone())),
        })
        .unwrap();
        assert!(
            collect_window(&mut rx, &id, Keys::generate().public_key(), 0, u64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

/// Seller equivalent of `flush`, with all content signing inside the existing actor.
pub async fn flush_actor<S: ContentSender>(
    db: &mut ContentStore,
    signer: &crate::seller_node::signer::SignerHandle,
    sender: &mut S,
    limit: usize,
) -> Result<FlushReport> {
    let mut report = FlushReport::default();
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    for copy in db.pending(signer.public_key_hex(), limit.min(64))? {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            report.pending += 1;
            continue;
        };
        let accepted = tokio::time::timeout(remaining, async {
            let event = signer
                .wrap_private_content(copy.content.clone(), copy.recipient.clone())
                .await
                .map_err(|_| Error("content signer unavailable"))??;
            sender.send(event).await
        })
        .await;
        if matches!(accepted, Ok(Ok(()))) {
            db.relay_accepted(
                &copy.content.body().job_id,
                &copy.content.body().message_id,
                &copy.recipient,
            )?;
            report.accepted += 1;
        } else {
            report.pending += 1;
        }
    }
    for event in db.pending_carriers(signer.public_key_hex(), limit.min(64))? {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            report.pending += 1;
            continue;
        };
        if matches!(
            tokio::time::timeout(remaining, sender.send(event.clone())).await,
            Ok(Ok(()))
        ) {
            db.carrier_accepted(&event)?;
            report.accepted += 1;
        } else {
            report.pending += 1;
        }
    }
    Ok(report)
}
