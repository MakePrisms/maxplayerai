//! Complete lifecycle reads. SDK fetches may return partial events on timeout;
//! only this subscription's EOSE can certify a result/selection history.
use super::{Error, Result};
use nostr_sdk::{pool::RelayNotification, prelude::*};
use std::{collections::BTreeMap, time::Duration};

pub async fn fetch(relay: &Relay, filter: Filter, timeout: Duration) -> Result<Vec<Event>> {
    let id = SubscriptionId::generate();
    let mut notifications = relay.notifications();
    relay
        .subscribe_with_id(id.clone(), filter.clone(), SubscribeOptions::default())
        .await
        .map_err(|_| Error("lifecycle subscription failed"))?;
    let result = tokio::time::timeout(timeout, collect(&mut notifications, &id, &filter))
        .await
        .map_err(|_| Error("lifecycle history incomplete"));
    relay
        .unsubscribe(&id)
        .await
        .map_err(|_| Error("lifecycle subscription close failed"))?;
    result?
}

async fn collect(
    notifications: &mut tokio::sync::broadcast::Receiver<RelayNotification>,
    id: &SubscriptionId,
    filter: &Filter,
) -> Result<Vec<Event>> {
    let mut events = BTreeMap::new();
    loop {
        match notifications
            .recv()
            .await
            .map_err(|_| Error("lifecycle notification gap"))?
        {
            RelayNotification::Message {
                message:
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    },
            } if subscription_id.as_ref() == id => {
                if filter.match_event(&event, Default::default()) && event.verify().is_ok() {
                    events.insert(event.id, event.into_owned());
                }
            }
            RelayNotification::Message {
                message: RelayMessage::EndOfStoredEvents(subscription_id),
            } if subscription_id.as_ref() == id => return Ok(events.into_values().collect()),
            RelayNotification::Message {
                message:
                    RelayMessage::Closed {
                        subscription_id,
                        message,
                    },
            } if subscription_id.as_ref() == id => {
                // The SDK automatically reissues the same REQ after NIP-42 auth.
                if !message.starts_with("auth-required:") {
                    return Err(Error("lifecycle subscription refused"));
                }
            }
            RelayNotification::AuthenticationFailed
            | RelayNotification::Shutdown
            | RelayNotification::RelayStatus {
                status: RelayStatus::Disconnected | RelayStatus::Terminated,
            } => return Err(Error("lifecycle relay disconnected")),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    #[tokio::test]
    async fn partial_result_history_never_certifies_absence() {
        let id = SubscriptionId::new("results");
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let event = EventBuilder::new(Kind::Custom(3403), "present result")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        let filter = Filter::new().kind(Kind::Custom(3403));
        let send = |event: Event| {
            tx.send(RelayNotification::Message {
                message: RelayMessage::Event {
                    subscription_id: Cow::Owned(id.clone()),
                    event: Cow::Owned(event),
                },
            })
            .unwrap()
        };
        send(event.clone());
        tx.send(RelayNotification::Message {
            message: RelayMessage::EndOfStoredEvents(Cow::Owned(SubscriptionId::new("other"))),
        })
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), collect(&mut rx, &id, &filter))
                .await
                .is_err()
        );
        send(event.clone());
        tx.send(RelayNotification::Message {
            message: RelayMessage::Closed {
                subscription_id: Cow::Owned(id.clone()),
                message: Cow::Borrowed("refused"),
            },
        })
        .unwrap();
        assert!(collect(&mut rx, &id, &filter).await.is_err());
        send(event.clone());
        tx.send(RelayNotification::Message {
            message: RelayMessage::EndOfStoredEvents(Cow::Owned(id.clone())),
        })
        .unwrap();
        assert_eq!(collect(&mut rx, &id, &filter).await.unwrap(), vec![event]);
    }
}
