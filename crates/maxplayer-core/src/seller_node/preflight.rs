//! Off-loop pre-claim staging of private inputs and contribution bases.
//!
//! Staging reads the pinned base from the relay before the seller claims. The relay packs a
//! whole repository before its first byte, so a large base legitimately takes minutes. Awaiting
//! that on the seller's event loop would stall awards, heartbeats, receipts and shutdown, so the
//! loop only asks this gate: the first ask starts the work in the background, later asks see it
//! pending, and a finished offer is handed back for re-drive on the drain tick.
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Concurrent stagings per seat. Each holds one blocking thread for up to two long HTTP legs.
pub(crate) const MAX_RUNNING: usize = 4;
/// Remembered offers (running, finished, waiting for a slot). Finished entries are pruned first.
pub(crate) const MAX_TRACKED: usize = 256;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Inputs are staged; continue to classification and claim.
    Ready,
    /// Staging is running or waiting for a slot; the offer is re-driven when that changes.
    Pending,
    /// Staging failed. The entry is forgotten, so a later sighting of the offer retries.
    Failed(String),
}

enum State {
    Running,
    Done(Result<(), String>),
}

struct Inner<E> {
    state: BTreeMap<String, State>,
    /// Offers to hand back to the event loop: finished, or waiting for a free slot.
    redrive: BTreeMap<String, E>,
}

pub(crate) struct PreflightGate<E> {
    inner: Arc<Mutex<Inner<E>>>,
}

impl<E: Clone + Send + 'static> PreflightGate<E> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: BTreeMap::new(),
                redrive: BTreeMap::new(),
            })),
        }
    }

    /// Never awaits the staging work. `start` is called at most once per running staging.
    pub(crate) fn poll<F, Fut>(&self, id: &str, event: &E, start: F) -> Gate
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let Ok(mut inner) = self.inner.lock() else {
            return Gate::Failed("preflight state lock poisoned".into());
        };
        match inner.state.get(id) {
            Some(State::Done(Ok(()))) => return Gate::Ready,
            Some(State::Done(Err(_))) => {
                let Some(State::Done(Err(error))) = inner.state.remove(id) else {
                    unreachable!("matched above")
                };
                return Gate::Failed(error);
            }
            Some(State::Running) => return Gate::Pending,
            None => {}
        }
        let running = inner
            .state
            .values()
            .filter(|s| matches!(s, State::Running))
            .count();
        if inner.state.len() >= MAX_TRACKED {
            inner.state.retain(|_, s| matches!(s, State::Running));
        }
        if running >= MAX_RUNNING || inner.state.len() >= MAX_TRACKED {
            if inner.redrive.len() >= MAX_TRACKED && !inner.redrive.contains_key(id) {
                // Nothing would re-drive it: say so, and let the next sighting retry.
                return Gate::Failed("private input staging queue full".into());
            }
            inner.redrive.insert(id.to_owned(), event.clone());
            return Gate::Pending;
        }
        inner.redrive.remove(id);
        inner.state.insert(id.to_owned(), State::Running);
        let work = start();
        let shared = Arc::clone(&self.inner);
        let (id, event) = (id.to_owned(), event.clone());
        tokio::spawn(async move {
            // A panicking staging is a failure, not a forever-pending offer.
            let result = tokio::spawn(work)
                .await
                .unwrap_or_else(|_| Err("private input staging stopped".into()));
            if let Ok(mut inner) = shared.lock() {
                inner.state.insert(id.clone(), State::Done(result));
                inner.redrive.insert(id, event);
            }
        });
        Gate::Pending
    }

    /// Offers whose staging finished, or that waited for a slot, to run through `on_offer` again.
    pub(crate) fn take_redrive(&self) -> Vec<E> {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.redrive).into_values().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    async fn until_redrive(gate: &PreflightGate<String>) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ready = gate.take_redrive();
                if !ready.is_empty() {
                    return ready;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("staging finished")
    }

    /// Regression (#1096 review B2): staging ran inline on the seller event loop, so a slow
    /// relay pack held awards, heartbeats and shutdown until it finished.
    #[tokio::test]
    async fn slow_staging_never_blocks_the_caller_and_runs_once() {
        let gate = PreflightGate::new();
        let starts = Arc::new(AtomicUsize::new(0));
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let counted = starts.clone();
        let first = tokio::time::timeout(Duration::from_millis(200), async {
            gate.poll("offer", &"event".to_owned(), move || {
                counted.fetch_add(1, Ordering::SeqCst);
                async move {
                    wait.await.ok();
                    Ok(())
                }
            })
        })
        .await
        .expect("poll returns without awaiting the staging");
        assert_eq!(first, Gate::Pending);
        let again = gate.poll("offer", &"event".to_owned(), || async {
            panic!("a second staging for the same offer")
        });
        assert_eq!(again, Gate::Pending);
        assert!(gate.take_redrive().is_empty(), "nothing to re-drive yet");
        release.send(()).unwrap();
        assert_eq!(until_redrive(&gate).await, vec!["event".to_owned()]);
        assert_eq!(
            gate.poll("offer", &"event".to_owned(), || async {
                panic!("re-staged")
            }),
            Gate::Ready
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failure_is_reported_once_then_retried() {
        let gate = PreflightGate::new();
        gate.poll("o", &"e".to_owned(), || async {
            Err("relay 503".to_owned())
        });
        until_redrive(&gate).await;
        assert_eq!(
            gate.poll("o", &"e".to_owned(), || async { panic!("not yet") }),
            Gate::Failed("relay 503".into())
        );
        assert_eq!(
            gate.poll("o", &"e".to_owned(), || async { Ok(()) }),
            Gate::Pending,
            "the next sighting starts a fresh staging"
        );
        until_redrive(&gate).await;
        assert_eq!(
            gate.poll("o", &"e".to_owned(), || async { Ok(()) }),
            Gate::Ready
        );
    }

    #[tokio::test]
    async fn a_panicking_staging_fails_instead_of_staying_pending() {
        let gate = PreflightGate::new();
        gate.poll("o", &"e".to_owned(), || async { panic!("staging bug") });
        until_redrive(&gate).await;
        assert!(matches!(
            gate.poll("o", &"e".to_owned(), || async { Ok(()) }),
            Gate::Failed(_)
        ));
    }

    #[tokio::test]
    async fn a_full_waiting_queue_reports_the_skip_instead_of_dropping_it() {
        let gate = PreflightGate::new();
        let mut releases = Vec::new();
        for i in 0..MAX_RUNNING {
            let (release, wait) = tokio::sync::oneshot::channel::<()>();
            releases.push(release);
            gate.poll(&format!("run{i}"), &format!("e{i}"), move || async move {
                wait.await.ok();
                Ok(())
            });
        }
        for i in 0..MAX_TRACKED {
            assert_eq!(
                gate.poll(&format!("wait{i}"), &format!("w{i}"), || async { Ok(()) }),
                Gate::Pending
            );
        }
        assert!(matches!(
            gate.poll("overflow", &"x".to_owned(), || async { Ok(()) }),
            Gate::Failed(_)
        ));
        drop(releases);
    }

    #[tokio::test]
    async fn stagings_beyond_the_slot_cap_wait_and_are_re_driven() {
        let gate = PreflightGate::new();
        let mut releases = Vec::new();
        for i in 0..MAX_RUNNING {
            let (release, wait) = tokio::sync::oneshot::channel::<()>();
            releases.push(release);
            assert_eq!(
                gate.poll(&format!("o{i}"), &format!("e{i}"), move || async move {
                    wait.await.ok();
                    Ok(())
                }),
                Gate::Pending
            );
        }
        assert_eq!(
            gate.poll("late", &"late-event".to_owned(), || async {
                panic!("started past the cap")
            }),
            Gate::Pending
        );
        assert_eq!(gate.take_redrive(), vec!["late-event".to_owned()]);
        drop(releases);
        let mut finished = Vec::new();
        while finished.len() < MAX_RUNNING {
            finished.extend(until_redrive(&gate).await);
        }
        assert_eq!(
            gate.poll("late", &"late-event".to_owned(), || async { Ok(()) }),
            Gate::Pending,
            "a freed slot starts the waiting offer"
        );
    }
}
