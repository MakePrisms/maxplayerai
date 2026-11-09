//! Reading stored events with an explicit completeness verdict.
//!
//! `Client::fetch_events` returns whatever arrived by its timeout as a plain `Ok`, so a relay that
//! never sends EOSE produces an empty "success" that looks exactly like an empty history. The seller's
//! periodic backfills (payment wraps, offers) read through [`read_stored`] instead: a read that did
//! not end in the relay's EOSE is reported as [`StoredRead::Incomplete`], logged as such and retried,
//! never reported as "0 returned".
//!
//! The read rides an auto-closing subscription: the SDK sends its CLOSE, never re-sends it after a
//! reconnect (auto-closing REQs are not part of the resubscribe set), and re-issues it after NIP-42
//! when the relay answers `auth-required:`. Events are taken from the relay's message lane, which
//! carries every stored event the relay returns, including ones this client has seen before; the
//! event lane would drop those, and a backfill exists precisely to re-read them.

use std::collections::BTreeMap;
use std::time::Duration;

use nostr_sdk::pool::RelayNotification;
use nostr_sdk::prelude::*;
use tokio::sync::broadcast::error::RecvError;

/// What one stored-events read established.
#[derive(Debug)]
pub(crate) enum StoredRead {
    /// The relay sent EOSE: these are the stored events it holds for the filters.
    Complete(Vec<Event>),
    /// The read ended without EOSE. `events` arrived first and are genuine, but absence proves
    /// nothing: more may exist.
    Incomplete { events: Vec<Event>, reason: String },
}

impl StoredRead {
    pub(crate) fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    pub(crate) fn into_events(self) -> Vec<Event> {
        match self {
            Self::Complete(events) | Self::Incomplete { events, .. } => events,
        }
    }
}

/// Read the events `relay` stores for `filters`, waiting at most `wait` for its EOSE.
pub(crate) async fn read_stored(relay: &Relay, filters: Vec<Filter>, wait: Duration) -> StoredRead {
    let id = SubscriptionId::generate();
    // Receiver BEFORE the REQ, so an EOSE that lands first cannot be missed.
    let mut notifications = relay.notifications();
    let options = SubscribeOptions::default().close_on(Some(
        SubscribeAutoCloseOptions::default()
            .exit_policy(ReqExitPolicy::ExitOnEOSE)
            .timeout(Some(wait)),
    ));
    if let Err(error) = relay
        .subscribe_with_id(id.clone(), filters.clone(), options)
        .await
    {
        return StoredRead::Incomplete {
            events: Vec::new(),
            reason: format!("the request could not be sent ({error})"),
        };
    }
    let mut events = BTreeMap::new();
    let mut unverifiable = 0usize;
    let verdict = tokio::time::timeout(
        wait,
        collect(&mut notifications, &id, &filters, &mut events, &mut unverifiable),
    )
    .await;
    // Newest first, the order `fetch_events` handed its callers.
    let mut events: Vec<Event> = events.into_values().collect();
    events.sort();
    match verdict {
        // RC3-02 (advisor r3 Low): a read that dropped a matching frame because it did not verify
        // is not a complete answer. A caller that acts on emptiness (kind-0 merge-then-publish)
        // would otherwise publish over a profile it never saw; backfills retry, as on any
        // incomplete read.
        Ok(Ok(())) if unverifiable > 0 => StoredRead::Incomplete {
            events,
            reason: format!("the relay returned {unverifiable} matching event(s) that do not verify"),
        },
        Ok(Ok(())) => StoredRead::Complete(events),
        Ok(Err(reason)) => StoredRead::Incomplete { events, reason },
        Err(_) => StoredRead::Incomplete {
            events,
            reason: format!("no EOSE within {}s", wait.as_secs()),
        },
    }
}

async fn collect(
    notifications: &mut tokio::sync::broadcast::Receiver<RelayNotification>,
    id: &SubscriptionId,
    filters: &[Filter],
    events: &mut BTreeMap<EventId, Event>,
    unverifiable: &mut usize,
) -> Result<(), String> {
    loop {
        let notification = match notifications.recv().await {
            Ok(notification) => notification,
            // A lagged receiver may have dropped events or the EOSE itself.
            Err(RecvError::Lagged(missed)) => {
                return Err(format!("missed {missed} relay notification(s)"));
            }
            Err(RecvError::Closed) => return Err("the relay notification stream closed".into()),
        };
        match notification {
            RelayNotification::Message {
                message:
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    },
            } if subscription_id.as_ref() == id => {
                // RC3-02: only frames that verify may occupy an id. relay-pool 0.44.3 re-emits a
                // same-id, content-swapped twin on this (message) lane because its verification
                // cache is keyed by id; inserting it would REPLACE the genuine frame and the handler
                // would then (correctly) reject the survivor, silently dropping a valid offer or
                // payment wrap. Two different bodies cannot both verify under one id.
                if filters
                    .iter()
                    .any(|filter| filter.match_event(&event, Default::default()))
                {
                    if event.verify().is_ok() {
                        events.insert(event.id, event.into_owned());
                    } else {
                        *unverifiable += 1;
                    }
                }
            }
            RelayNotification::Message {
                message: RelayMessage::EndOfStoredEvents(subscription_id),
            } if subscription_id.as_ref() == id => return Ok(()),
            RelayNotification::Message {
                message:
                    RelayMessage::Closed {
                        subscription_id,
                        message,
                    },
            } if subscription_id.as_ref() == id => {
                // The SDK re-issues this REQ itself once NIP-42 completes.
                if !message.starts_with("auth-required:") {
                    return Err(format!("the relay closed the read ({message})"));
                }
            }
            RelayNotification::AuthenticationFailed => {
                return Err("relay authentication failed".into());
            }
            RelayNotification::Shutdown => return Err("the relay connection shut down".into()),
            RelayNotification::RelayStatus { status }
                if matches!(
                    status,
                    RelayStatus::Disconnected | RelayStatus::Terminated | RelayStatus::Banned
                ) =>
            {
                return Err(format!("the relay connection went {status}"));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seller_node::p_gate_relay_fixture::PGateRelay;

    // RC3-02 (advisor r2): the backfill collector keys frames by id. A same-id, content-swapped twin
    // (re-emitted by relay-pool 0.44.3 on the message lane) arriving AFTER the genuine frame must not
    // replace it, and one arriving FIRST must not occupy the id. Driven through `collect` with the
    // exact notification shape the relay lane produces. Red on revert: drop the verify and the
    // genuine offer is replaced by the twin.
    #[tokio::test]
    async fn an_unverifiable_twin_never_evicts_or_occupies_a_stored_id() {
        use nostr_sdk::JsonUtil;
        use std::borrow::Cow;
        let keys = Keys::generate();
        let genuine = EventBuilder::new(Kind::Custom(crate::kinds::JOB_OFFER_KIND), "genuine")
            .sign_with_keys(&keys)
            .unwrap();
        let twin = {
            let mut value = serde_json::to_value(&genuine).unwrap();
            value["content"] = serde_json::Value::String("swapped".into());
            Event::from_json(value.to_string()).unwrap()
        };
        assert_eq!(twin.id, genuine.id);
        let filters = vec![Filter::new().kind(Kind::Custom(crate::kinds::JOB_OFFER_KIND))];
        let id = SubscriptionId::generate();
        let frame = |event: &Event| RelayNotification::Message {
            message: RelayMessage::Event {
                subscription_id: Cow::Owned(id.clone()),
                event: Cow::Owned(event.clone()),
            },
        };
        let eose = RelayNotification::Message {
            message: RelayMessage::EndOfStoredEvents(Cow::Owned(id.clone())),
        };
        for order in [[&genuine, &twin], [&twin, &genuine]] {
            let (tx, mut rx) = tokio::sync::broadcast::channel(8);
            for event in order {
                tx.send(frame(event)).unwrap();
            }
            tx.send(eose.clone()).unwrap();
            let mut events = BTreeMap::new();
            let mut unverifiable = 0;
            collect(&mut rx, &id, &filters, &mut events, &mut unverifiable).await.expect("complete");
            assert_eq!(events.len(), 1);
            assert_eq!(events[&genuine.id].content, "genuine", "the genuine frame survives either order");
            assert_eq!(unverifiable, 1, "the twin is counted, so read_stored reports Incomplete");
        }
        // Twin only: nothing occupies the id, and the read is flagged (never a clean empty).
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        tx.send(frame(&twin)).unwrap();
        tx.send(eose.clone()).unwrap();
        let mut events = BTreeMap::new();
        let mut unverifiable = 0;
        collect(&mut rx, &id, &filters, &mut events, &mut unverifiable).await.expect("eose");
        assert!(events.is_empty());
        assert_eq!(unverifiable, 1);
    }

    async fn connected(url: &str) -> (Client, Relay) {
        let client = Client::new(Keys::generate());
        client.automatic_authentication(true);
        client.add_relay(url).await.expect("add relay");
        let relay = client.relay(url).await.expect("relay handle");
        client.connect().await;
        client.wait_for_connection(Duration::from_secs(5)).await;
        (client, relay)
    }

    /// A relay that answers with EOSE gives a complete read; one that never does gives an INCOMPLETE
    /// read within the wait, where the SDK's own fetch reports the same silence as an empty success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_without_eose_is_incomplete_not_an_empty_success() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let (client, relay) = connected(&fixture.url()).await;
        let filter = Filter::new().kind(Kind::TextNote).limit(10);

        let read = read_stored(&relay, vec![filter.clone()], Duration::from_secs(5)).await;
        assert!(read.is_complete(), "the relay sent EOSE: {read:?}");
        assert!(read.into_events().is_empty());

        fixture.withhold_eose_for(&[1]);
        let started = tokio::time::Instant::now();
        let read = read_stored(&relay, vec![filter.clone()], Duration::from_secs(2)).await;
        assert!(started.elapsed() < Duration::from_secs(4), "the read is bounded by its wait");
        match read {
            StoredRead::Incomplete { events, reason } => {
                assert!(events.is_empty());
                assert!(reason.contains("no EOSE"), "{reason}");
            }
            complete => panic!("a read with no EOSE must not be complete: {complete:?}"),
        }

        // The SDK's convenience fetch, for contrast: the same withheld EOSE is an `Ok` with nothing in
        // it, which a caller cannot tell from an empty history.
        let fetched = client
            .fetch_events(filter, Duration::from_secs(2))
            .await
            .expect("fetch_events reports success");
        assert!(fetched.is_empty());
        client.disconnect().await;
    }

    /// A refusal ends the read as incomplete, with the relay's reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_read_is_incomplete_with_the_reason() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let (client, relay) = connected(&fixture.url()).await;
        // Another key's `#p`: the fixture refuses it `restricted:` whatever the session's auth.
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(Keys::generate().public_key());
        match read_stored(&relay, vec![filter], Duration::from_secs(5)).await {
            StoredRead::Incomplete { reason, .. } => {
                assert!(reason.contains("restricted:"), "{reason}")
            }
            complete => panic!("a refused read must not be complete: {complete:?}"),
        }
        client.disconnect().await;
    }

    /// Stored events come back, including ones this client has already seen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_complete_read_returns_stored_events_this_client_has_already_seen() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        let fixture = LocalRelay::new(RelayBuilder::default());
        fixture.run().await.expect("relay run");
        let url = fixture.url().await.to_string();
        let (client, relay) = connected(&url).await;
        let keys = Keys::generate();
        for n in 0..3 {
            let note = EventBuilder::text_note(format!("stored {n}"))
                .sign_with_keys(&keys)
                .expect("sign");
            client.send_event(&note).await.expect("publish");
        }
        let filter = Filter::new().kind(Kind::TextNote).author(keys.public_key());
        for _ in 0..2 {
            let read = read_stored(&relay, vec![filter.clone()], Duration::from_secs(5)).await;
            assert!(read.is_complete(), "{read:?}");
            assert_eq!(read.into_events().len(), 3, "every stored event, on every read");
        }
        client.disconnect().await;
    }
}
