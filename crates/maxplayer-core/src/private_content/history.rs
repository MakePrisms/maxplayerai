//! Complete lifecycle reads. SDK fetches may return partial events on timeout;
//! exact EOSE plus unsaturated pages are required to certify the full history.
use super::{Error, Result, history_wire::WireHistory};
use nostr_sdk::{pool::RelayNotification, prelude::*};
use std::{collections::BTreeMap, future::Future, time::Duration};

// Below the bundled database's 1,000-row default cap. A supported relay must honor
// this requested page size; EOSE alone does not promise an uncapped history.
const PAGE_LIMIT: usize = 128;
const MAX_WINDOWS: usize = 32;

pub(crate) async fn fetch(wire: &WireHistory, relay: &Relay, filter: Filter, timeout: Duration) -> Result<Vec<Event>> {
    let deadline = tokio::time::Instant::now() + timeout;
    // Buzz applies #t only AFTER its SQL LIMIT. Request that superset on the
    // wire, count/paginate every row, then apply the namespace filter locally.
    // Otherwise a page of foreign tags could become an apparently complete empty
    // response even with an explicit limit. The #e/kind/author/time filters are
    // pushed into SQL by the supported relay and remain on every request.
    let mut wire_filter = filter.clone();
    wire_filter
        .generic_tags
        .remove(&SingleLetterTag::lowercase(Alphabet::T));
    let events = paginate(wire_filter, |filter| async move {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or(Error("lifecycle history incomplete"))?;
        fetch_page(wire, relay, filter, remaining).await
    })
    .await?;
    Ok(events
        .into_iter()
        .filter(|event| filter.match_event(event, Default::default()))
        .collect())
}

/// A full newest-first page may cut THROUGH a timestamp. Read that entire second
/// separately before moving until backwards; never skip unseen ties with `min-1`.
/// A saturated second or work budget is unknown state, not successful absence.
async fn paginate<F, Fut>(mut filter: Filter, mut page: F) -> Result<Vec<Event>>
where
    F: FnMut(Filter) -> Fut,
    Fut: Future<Output = Result<Vec<Event>>>,
{
    let lower = filter.since.unwrap_or(Timestamp::from(0));
    filter.limit = Some(PAGE_LIMIT);
    let mut events = BTreeMap::new();
    for _ in 0..MAX_WINDOWS {
        let batch = page(filter.clone()).await?;
        if batch.len() > PAGE_LIMIT {
            return Err(Error("lifecycle history page overflow"));
        }
        let full = batch.len() == PAGE_LIMIT;
        let oldest = batch.iter().map(|event| event.created_at).min();
        events.extend(batch.into_iter().map(|event| (event.id, event)));
        if !full {
            return Ok(events.into_values().collect());
        }
        let oldest = oldest.expect("full page has a timestamp");
        let boundary = page(filter.clone().since(oldest).until(oldest)).await?;
        if boundary.len() >= PAGE_LIMIT {
            return Err(Error("lifecycle history timestamp saturated"));
        }
        // Even an empty boundary after a full page is not proof that the missing
        // rows never existed (pruning/racing reads). Fail closed on disappearing ties.
        if events
            .values()
            .filter(|event| event.created_at == oldest)
            .any(|event| !boundary.iter().any(|candidate| candidate.id == event.id))
        {
            return Err(Error("lifecycle history boundary changed"));
        }
        events.extend(boundary.into_iter().map(|event| (event.id, event)));
        if oldest <= lower {
            return Ok(events.into_values().collect());
        }
        filter.until = Some(Timestamp::from(oldest.as_secs() - 1));
    }
    Err(Error("lifecycle history pagination budget exhausted"))
}

async fn fetch_page(wire: &WireHistory, relay: &Relay, filter: Filter, timeout: Duration) -> Result<Vec<Event>> {
    let id = SubscriptionId::generate();
    let mut notifications = relay.notifications();
    let page = wire.page(relay, &id);
    let result = tokio::time::timeout(timeout, async {
        relay
            .subscribe_with_id(id.clone(), filter.clone(), SubscribeOptions::default())
            .await
            .map_err(|_| Error("lifecycle subscription failed"))?;
        let events = collect(&mut notifications, &id, &filter).await?;
        page.complete(events.len())?;
        Ok(events)
    })
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
                    if events.len() > PAGE_LIMIT {
                        return Err(Error("lifecycle history page overflow"));
                    }
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

    fn events_at(keys: &Keys, time: u64, count: usize) -> Vec<Event> {
        (0..count)
            .map(|n| {
                EventBuilder::new(Kind::Custom(3403), n.to_string())
                    .custom_created_at(Timestamp::from(time))
                    .sign_with_keys(keys)
                    .unwrap()
            })
            .collect()
    }

    fn model_page(events: &[Event], filter: &Filter) -> Vec<Event> {
        let mut matches: Vec<_> = events
            .iter()
            .filter(|event| filter.match_event(event, Default::default()))
            .cloned()
            .collect();
        matches.sort_by_key(|event| (std::cmp::Reverse(event.created_at), event.id));
        matches.truncate(filter.limit.unwrap_or(2000).min(2000));
        matches
    }

    #[tokio::test]
    async fn pagination_recovers_unseen_boundary_ties_and_older_results() {
        let keys = Keys::generate();
        let mut events = events_at(&keys, 20, PAGE_LIMIT - 1);
        events.extend(events_at(&keys, 10, PAGE_LIMIT - 1));
        events.extend(events_at(&keys, 1, 1));
        let read = paginate(Filter::new().kind(Kind::Custom(3403)), |filter| {
            let page = model_page(&events, &filter);
            async move { Ok(page) }
        })
        .await
        .unwrap();
        let actual: std::collections::BTreeSet<_> = read.iter().map(|e| e.id).collect();
        assert_eq!(actual, events.iter().map(|e| e.id).collect());
    }

    #[tokio::test]
    async fn full_single_timestamp_and_disappearing_boundary_fail_closed() {
        let keys = Keys::generate();
        let events = events_at(&keys, 10, PAGE_LIMIT);
        assert!(matches!(
            paginate(Filter::new(), |filter| {
                let page = model_page(&events, &filter);
                async move { Ok(page) }
            })
            .await,
            Err(Error("lifecycle history timestamp saturated"))
        ));
        let mut events = events_at(&keys, 20, PAGE_LIMIT - 1);
        events.extend(events_at(&keys, 10, 1));
        let mut calls = 0;
        assert!(matches!(
            paginate(Filter::new(), |filter| {
                calls += 1;
                let page = if calls == 1 {
                    model_page(&events, &filter)
                } else {
                    vec![]
                };
                async move { Ok(page) }
            })
            .await,
            Err(Error("lifecycle history boundary changed"))
        ));
    }

    #[tokio::test]
    async fn pagination_budget_never_returns_a_partial_success() {
        let keys = Keys::generate();
        let mut events = Vec::new();
        for time in 1..=(PAGE_LIMIT * MAX_WINDOWS + 1) {
            events.extend(events_at(&keys, time as u64, 1));
        }
        assert!(matches!(
            paginate(Filter::new(), |filter| {
                let page = model_page(&events, &filter);
                async move { Ok(page) }
            })
            .await,
            Err(Error("lifecycle history pagination budget exhausted"))
        ));
    }
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
