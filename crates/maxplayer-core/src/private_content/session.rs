//! Authenticated content transport. Relay acceptance is not an application ACK.
//! A timed-out query MUST NOT advance the cursor: nostr-sdk's convenience fetch
//! returns partial results on timeout, so this lane explicitly observes EOSE.
use super::{Error, Result, store::ContentStore};
use crate::relay_auth::{AuthWait, wait_for_nip42_auth};
use nostr_sdk::{pool::RelayNotification, prelude::*};
use std::{collections::BTreeMap, future::Future, time::Duration};

const WINDOW_LIMIT: usize = 128;
const COPY_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(20);

/// Why one send did not land. `permanent` means the relay refused these exact
/// signed bytes with a terminal NIP-01 prefix (`invalid:`/`blocked:`): a resend of
/// the same event can never succeed, so the caller must stop retrying it and keep
/// the reason. Everything else stays retryable.
#[derive(Debug)]
pub struct SendFailure {
    pub permanent: bool,
    pub reason: String,
}
impl From<super::Error> for SendFailure {
    fn from(error: super::Error) -> Self {
        Self {
            permanent: false,
            reason: error.to_string(),
        }
    }
}
pub trait ContentSender {
    fn send(&mut self, event: Event)
    -> impl Future<Output = std::result::Result<(), SendFailure>> + Send;
}
#[derive(Default, Debug, PartialEq, Eq)]
pub struct FlushReport {
    pub accepted: usize,
    pub pending: usize,
    /// Carriers the relay refused permanently this pass; they left the retry set.
    pub refused: usize,
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
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()).filter(|remaining| !remaining.is_zero()) else {
            report.pending += 1;
            continue;
        };
        db.copy_attempted(&copy)?;
        let attempt = async {
            let event = copy.wrap(keys).await?;
            sender.send(event).await
        };
        if matches!(
            tokio::time::timeout(remaining.min(COPY_TIMEOUT), attempt).await,
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
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    for event in db.pending_carriers(&keys.public_key().to_hex(), limit.min(64))? {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()).filter(|remaining| !remaining.is_zero()) else {
            report.pending += 1;
            continue;
        };
        db.carrier_attempted(&event)?;
        match tokio::time::timeout(remaining.min(COPY_TIMEOUT), sender.send(event.clone())).await {
            Ok(Ok(())) => {
                db.carrier_accepted(&event)?;
                report.accepted += 1;
            }
            // Terminal refusal of these exact signed bytes: record the reason, leave
            // the retry set. Before this, a refused offer was retried forever and the
            // only trace was a once-per-tick relay read that came back empty (#1115).
            Ok(Err(failure)) if failure.permanent => {
                db.carrier_refused(&event, &failure.reason, nostr_sdk::Timestamp::now().as_secs())?;
                crate::opline!(
                    "content carrier {} permanently refused by the relay: {}",
                    event.id,
                    failure.reason
                );
                report.refused += 1;
            }
            _ => {
                report.pending += 1;
            }
        }
    }
    Ok(report)
}

pub struct AuthenticatedContentRelay {
    client: Client,
    relay: Relay,
    owns_connection: bool,
    /// This connection's sign-in state (#1228), so a later refused write names a refused
    /// re-sign-in. `None` on a shared connection, whose owner watches its own sign-in.
    sign_in: Option<crate::relay_auth::SignInWatch>,
}

/// [`AuthenticatedContentRelay::connect`] failed. Unlike the protocol [`Error`], it carries text
/// built at run time: the relay's refusal of the sign-in and the link hint (#1228, advisor round 1
/// finding 4). The static prefix is kept so readers of the old message still match.
#[derive(Debug)]
pub struct ConnectError(pub String);
impl std::fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for ConnectError {}
impl From<Error> for ConnectError {
    fn from(error: Error) -> Self {
        Self(error.to_string())
    }
}

/// The connect failure for a sign-in that did not complete: the `wait_for_nip42_auth` error
/// (already hinted for a registration refusal) after the old static text.
fn sign_in_not_completed(auth: &std::result::Result<AuthWait, crate::relay_auth::RelayAuthError>) -> ConnectError {
    match auth {
        Err(error) => ConnectError(format!("content relay authentication required: {error}")),
        Ok(_) => Error("content relay authentication required").into(),
    }
}

impl AuthenticatedContentRelay {
    pub async fn connect(keys: &Keys, url: &str) -> std::result::Result<Self, ConnectError> {
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
        let sign_in = crate::relay_auth::SignInWatch::new(&relay);
        client.connect().await;
        let auth = wait_for_nip42_auth(&mut notifications, IO_TIMEOUT).await;
        if auth != Ok(AuthWait::Authenticated) {
            let error = sign_in_not_completed(&auth);
            crate::opline!("private content relay: {error}");
            client.disconnect().await;
            return Err(error);
        }
        Ok(Self {
            client,
            relay,
            owns_connection: true,
            sign_in: Some(sign_in),
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
            sign_in: None,
        })
    }
    pub async fn disconnect(self) {
        if self.owns_connection {
            self.client.disconnect().await;
        }
    }

    /// Fetch a complete interval, including author self-copies. The Message lane is
    /// deliberate: RelayNotification::Event suppresses events sent by this client.
    async fn window(
        &self,
        recipient: PublicKey,
        start: u64,
        end: u64,
        timeout: Duration,
    ) -> Result<Vec<Event>> {
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
            timeout,
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
        self.backfill_using(db, keys.public_key(), through, None, |event| async move {
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
        self.backfill_actor_within(db, signer, through, None).await
    }
    /// [`Self::backfill_actor`] bounded as a WHOLE by `deadline`, not only per query interval: no
    /// interval starts once it has passed, and the one in flight gets only the time left. Running
    /// out of time is an error, never a short success — the cursor keeps only the intervals that
    /// completed, so the next call resumes where this one stopped.
    pub async fn backfill_actor_within(
        &self,
        db: &mut ContentStore,
        signer: &crate::seller_node::signer::SignerHandle,
        through: u64,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<usize> {
        let recipient = PublicKey::from_hex(signer.public_key_hex())
            .map_err(|_| Error("invalid content recipient"))?;
        self.backfill_using(db, recipient, through, deadline, |event| async move {
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
        deadline: Option<tokio::time::Instant>,
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
            let timeout = match deadline {
                None => IO_TIMEOUT,
                Some(deadline) => deadline
                    .checked_duration_since(tokio::time::Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or(Error("content backfill budget exhausted"))?
                    .min(IO_TIMEOUT),
            };
            let events = self.window(recipient_key, start, end, timeout).await?;
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
                    staged += usize::from(db.stage_untrusted(&content, &recipient, through)?);
                }
            }
            db.scan_progress(&recipient, end)?;
        }
        db.scan_finished(&recipient)?;
        Ok(staged)
    }
}
impl ContentSender for AuthenticatedContentRelay {
    async fn send(&mut self, event: Event) -> std::result::Result<(), SendFailure> {
        let sent = self.client.send_event(&event).await;
        let refusal = || self.sign_in.as_ref().and_then(|watch| watch.refusal());
        let sent = match sent {
            Ok(sent) => sent,
            Err(_) => {
                return Err(send_failure("content publication failed".into(), refusal().as_deref()));
            }
        };
        if sent.success.is_empty() {
            let reason = sent
                .failed
                .values()
                .next()
                .cloned()
                .unwrap_or_else(|| "content publication refused".into());
            return Err(send_failure(reason, refusal().as_deref()));
        }
        Ok(())
    }
}

/// A failed content write. `permanent` is decided on the relay's own reason ONLY; the refused
/// sign-in (#1228) is added to the text afterwards, so it can never turn a retryable failure
/// terminal or a terminal one retryable.
fn send_failure(reason: String, sign_in_refusal: Option<&str>) -> SendFailure {
    // NIP-01 terminal prefixes. `rate-limited:`/`error:` stay retryable.
    let permanent = reason.starts_with("invalid:") || reason.starts_with("blocked:");
    let reason = crate::relay_refusal::with_hint(crate::relay_refusal::annotate_refused_sign_in(
        &reason,
        sign_in_refusal,
    ));
    SendFailure { permanent, reason }
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
                    || !event.tags.iter().any(|tag| tag == &Tag::public_key(recipient))
                {
                    continue;
                }
                // Application shape/domain/signatures are checked by unwrap_content.
                // Keep foreign matches in the page count so the limit is not hidden.
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

    /// Advisor round 1, finding 4: a sign-in that did not complete returns the hinted relay
    /// refusal, not only the static text.
    #[test]
    fn a_refused_content_sign_in_returns_the_hinted_refusal() {
        let refused = crate::relay_auth::RelayAuthError(crate::relay_refusal::with_hint(
            "NIP-42 sign-in refused: restricted: not a relay member",
        ));
        let error = sign_in_not_completed(&Err(refused)).to_string();
        assert!(error.starts_with("content relay authentication required: "), "{error}");
        assert!(error.contains("not a relay member"), "{error}");
        assert!(error.contains(crate::relay_refusal::LINK_COMMAND), "{error}");
        assert_eq!(
            sign_in_not_completed(&Ok(AuthWait::NoChallenge)).to_string(),
            "content relay authentication required"
        );
    }

    /// Advisor round 1, finding 4: a later content write names a refused re-sign-in like every
    /// other write path, and the refusal never changes whether the failure is permanent.
    #[test]
    fn a_content_write_after_a_refused_sign_in_names_it_without_changing_permanence() {
        let member = Some("restricted: not a relay member");
        let timeout = send_failure("timeout".into(), member);
        assert!(!timeout.permanent);
        assert!(timeout.reason.contains("after the relay refused NIP-42 sign-in: restricted: not a relay member"), "{}", timeout.reason);
        assert!(timeout.reason.contains(crate::relay_refusal::LINK_COMMAND), "{}", timeout.reason);

        let terminal = send_failure("invalid: bad signature".into(), member);
        assert!(terminal.permanent, "classified on the relay's own prefix");
        let banned = send_failure("timeout".into(), Some("blocked: you are banned from this community"));
        assert!(!banned.permanent, "a banned sign-in does not make a timeout terminal");
        assert!(!banned.reason.contains(crate::relay_refusal::LINK_COMMAND), "{}", banned.reason);

        let plain = send_failure("rate-limited: slow down".into(), None);
        assert!(!plain.permanent);
        assert_eq!(plain.reason, "rate-limited: slow down");
    }
    #[tokio::test]
    async fn foreign_wrapper_does_not_poison_mixed_inbox_or_hide_full_page() {
        let keys = Keys::generate();
        let recipient = keys.public_key();
        let id = SubscriptionId::new("mixed");
        let (tx, mut rx) = tokio::sync::broadcast::channel(256);
        let foreign = EventBuilder::new(Kind::GiftWrap, "unrelated app")
            .tags([
                Tag::public_key(recipient),
                Tag::parse(["x", "foreign"]).unwrap(),
            ])
            .allow_self_tagging()
            .sign_with_keys(&keys)
            .unwrap();
        let valid = super::super::PreparedContent::new(super::super::tests::body()).unwrap();
        let recipient_keys = super::super::tests::keys(2);
        let valid_wrapper = super::super::transport::wrap(
            &super::super::tests::keys(1),
            recipient_keys.public_key(),
            valid.envelope().to_owned(),
        )
        .await
        .unwrap();
        // Separate recipient for the actual mixed mailbox fixture.
        let foreign = EventBuilder::new(Kind::GiftWrap, &foreign.content)
            .tags([
                Tag::public_key(recipient_keys.public_key()),
                Tag::parse(["x", "foreign"]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .unwrap();
        for event in [foreign, valid_wrapper] {
            tx.send(RelayNotification::Message {
                message: RelayMessage::Event {
                    subscription_id: Cow::Owned(id.clone()),
                    event: Cow::Owned(event),
                },
            })
            .unwrap();
        }
        tx.send(RelayNotification::Message {
            message: RelayMessage::EndOfStoredEvents(Cow::Owned(id.clone())),
        })
        .unwrap();
        let page = collect_window(&mut rx, &id, recipient_keys.public_key(), 0, u64::MAX)
            .await
            .unwrap();
        assert_eq!(page.len(), 2); // even foreign matches count against the relay limit
        let mut accepted = Vec::new();
        for event in page {
            if let Ok(content) = super::super::transport::unwrap_content(&recipient_keys, &event).await
            {
                accepted.push(content);
            }
        }
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].envelope(), valid.envelope());
        for n in 0..WINDOW_LIMIT {
            let event = EventBuilder::new(Kind::GiftWrap, n.to_string())
                .tags([
                    Tag::public_key(recipient),
                    Tag::parse(["x", "foreign"]).unwrap(),
                ])
                .allow_self_tagging()
                .sign_with_keys(&keys)
                .unwrap();
            tx.send(RelayNotification::Message {
                message: RelayMessage::Event {
                    subscription_id: Cow::Owned(id.clone()),
                    event: Cow::Owned(event),
                },
            })
            .unwrap();
        }
        tx.send(RelayNotification::Message {
            message: RelayMessage::EndOfStoredEvents(Cow::Owned(id.clone())),
        })
        .unwrap();
        assert_eq!(
            collect_window(&mut rx, &id, recipient, 0, u64::MAX)
                .await
                .unwrap()
                .len(),
            WINDOW_LIMIT
        );
    }
    #[tokio::test]
    async fn mixed_inbox_backfill_stages_valid_content_and_advances_only_complete_scan() {
        use super::super::{
            PreparedContent, store,
            tests::{body, keys},
        };
        use nostr_relay_builder::prelude::{
            LocalRelay, RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode,
        };
        use nostr_sdk::prelude::*;
        let fixture = LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42 {
            mode: RelayBuilderNip42Mode::Both,
        }));
        fixture.run().await.unwrap();
        let url = fixture.url().await.to_string();
        // LocalRelay challenges lazily, unlike the deployed relay's connect-time AUTH.
        let client = Client::new(keys(2));
        client.automatic_authentication(true);
        client.add_relay(&url).await.unwrap();
        let raw = client.relay(&url).await.unwrap();
        let mut notifications = raw.notifications();
        client.connect().await;
        client
            .send_event(
                &EventBuilder::text_note("trigger fixture auth")
                    .sign_with_keys(&keys(2))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            wait_for_nip42_auth(&mut notifications, IO_TIMEOUT)
                .await
                .unwrap(),
            AuthWait::Authenticated
        );
        let mut relay = AuthenticatedContentRelay { client, relay: raw, owns_connection: true, sign_in: None };
        let valid = PreparedContent::new(body()).unwrap();
        let recipient = keys(2).public_key();
        let unrelated = EventBuilder::new(Kind::GiftWrap, "another application's payload")
            .tags([
                Tag::public_key(recipient),
                Tag::parse(["x", "foreign"]).unwrap(),
            ])
            .sign_with_keys(&keys(4))
            .unwrap();
        relay.send(unrelated).await.unwrap();
        relay
            .send(
                super::super::transport::wrap(&keys(1), recipient, valid.envelope().into())
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut db = store::ContentStore::in_memory().unwrap();
        let now = Timestamp::now().as_secs();
        assert_eq!(relay.backfill(&mut db, &keys(2), now).await.unwrap(), 1);
        assert_eq!(
            db.staged(
                &valid.body().author,
                &valid.body().job_id,
                &valid.body().message_id,
                now
            )
            .unwrap()
            .unwrap()
            .envelope(),
            valid.envelope()
        );
        assert_eq!(
            db.receive_since(&recipient.to_hex()).unwrap(),
            super::super::transport::receive_since(now)
        );
        relay.disconnect().await;
    }

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
    flush_actor_within(db, signer, sender, limit, None).await
}

/// [`flush_actor`] bounded as a WHOLE by `deadline` (each phase still keeps its own
/// [`IO_TIMEOUT`]). Copies and carriers the pass does not reach stay pending, exactly as a failed
/// send leaves them, and rotate ahead of the ones it attempted.
pub async fn flush_actor_within<S: ContentSender>(
    db: &mut ContentStore,
    signer: &crate::seller_node::signer::SignerHandle,
    sender: &mut S,
    limit: usize,
    deadline: Option<tokio::time::Instant>,
) -> Result<FlushReport> {
    let phase_deadline = || {
        let phase = tokio::time::Instant::now() + IO_TIMEOUT;
        deadline.map_or(phase, |overall| phase.min(overall))
    };
    let mut report = FlushReport::default();
    let deadline = phase_deadline();
    for copy in db.pending(signer.public_key_hex(), limit.min(64))? {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()).filter(|remaining| !remaining.is_zero()) else {
            report.pending += 1;
            continue;
        };
        db.copy_attempted(&copy)?;
        let accepted = tokio::time::timeout(remaining.min(COPY_TIMEOUT), async {
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
    let deadline = phase_deadline();
    for event in db.pending_carriers(signer.public_key_hex(), limit.min(64))? {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()).filter(|remaining| !remaining.is_zero()) else {
            report.pending += 1;
            continue;
        };
        db.carrier_attempted(&event)?;
        match tokio::time::timeout(remaining.min(COPY_TIMEOUT), sender.send(event.clone())).await {
            Ok(Ok(())) => {
                db.carrier_accepted(&event)?;
                report.accepted += 1;
            }
            Ok(Err(failure)) if failure.permanent => {
                db.carrier_refused(&event, &failure.reason, nostr_sdk::Timestamp::now().as_secs())?;
                crate::opline!(
                    "content carrier {} permanently refused by the relay: {}",
                    event.id,
                    failure.reason
                );
                report.refused += 1;
            }
            _ => {
                report.pending += 1;
            }
        }
    }
    Ok(report)
}
