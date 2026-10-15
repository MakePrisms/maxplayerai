//! Retry transient driver failures without bypassing reserve-then-award.
use std::{future::Future, time::Duration};
use tokio::time::Instant;

/// Never cancel an in-flight reserve/publish operation at the deadline. The driver
/// and pinned-attempt chokepoint own that check; this bounds starting new retries.
pub(super) async fn run<D, F, Fut>(mut deadline: D, mut attempt: F) -> Result<(), String>
where
    D: FnMut() -> Option<Instant>,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                // An unavailable cache is unknown, not permission to invent an
                // expiry. Re-read it on every failure; a successful driver also
                // checks the relay offer's deadline before reserving/publishing.
                let remaining = deadline()
                    .map(|end| end.saturating_duration_since(Instant::now()))
                    .unwrap_or(backoff);
                if remaining.is_zero() {
                    return Err(error);
                }
                tokio::time::sleep(backoff.min(remaining)).await;
                if deadline().is_some_and(|end| Instant::now() >= end) {
                    return Err(error);
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[tokio::test(start_paused = true)]
    async fn retries_transient_failures_then_stops_on_success() {
        let start = Instant::now();
        let calls = Cell::new(0);
        run(
            || Some(start + Duration::from_secs(120)),
            || {
                calls.set(calls.get() + 1);
                let result = if calls.get() < 4 {
                    Err("incomplete history".into())
                } else {
                    Ok(())
                };
                async { result }
            },
        )
        .await
        .unwrap();
        assert_eq!(calls.get(), 4);
        assert_eq!(start.elapsed(), Duration::from_secs(7));
    }

    #[tokio::test(start_paused = true)]
    async fn caps_backoff_and_stops_retrying_at_deadline() {
        let start = Instant::now();
        let calls = std::cell::RefCell::new(Vec::new());
        let error = run(
            || Some(start + Duration::from_secs(100)),
            || {
                calls.borrow_mut().push(start.elapsed().as_secs());
                async { Err("relay unavailable".into()) }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error, "relay unavailable");
        assert_eq!(*calls.borrow(), [0, 1, 3, 7, 15, 31, 61, 91]);
        assert_eq!(start.elapsed(), Duration::from_secs(100));
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_deadline_is_retried_and_recovered_deadline_stops_the_worker() {
        let start = Instant::now();
        let calls = Cell::new(0);
        let result = run(
            || (calls.get() >= 3).then_some(start + Duration::from_secs(4)),
            || {
                calls.set(calls.get() + 1);
                async { Err("transient local cache / relay failure".into()) }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 3);
        assert_eq!(start.elapsed(), Duration::from_secs(4));
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_cancel_in_flight_award_or_retry_a_terminal_success() {
        let start = Instant::now();
        run(
            || Some(start + Duration::from_secs(1)),
            || async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(2));
    }
}
