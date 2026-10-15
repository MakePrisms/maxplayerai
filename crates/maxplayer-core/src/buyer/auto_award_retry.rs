//! Retry transient driver failures without bypassing reserve-then-award.
use std::{future::Future, time::Duration};
use tokio::time::Instant;

/// Never cancel an in-flight reserve/publish operation at the deadline. The driver
/// and pinned-attempt chokepoint own that check; this bounds starting new retries.
pub(super) async fn run<F, Fut>(deadline: Instant, mut attempt: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error);
                }
                tokio::time::sleep(backoff.min(remaining)).await;
                if Instant::now() >= deadline {
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
        run(start + Duration::from_secs(120), || {
            calls.set(calls.get() + 1);
            let result = if calls.get() < 4 {
                Err("incomplete history".into())
            } else {
                Ok(())
            };
            async { result }
        })
        .await
        .unwrap();
        assert_eq!(calls.get(), 4);
        assert_eq!(start.elapsed(), Duration::from_secs(7));
    }

    #[tokio::test(start_paused = true)]
    async fn caps_backoff_and_stops_retrying_at_deadline() {
        let start = Instant::now();
        let calls = std::cell::RefCell::new(Vec::new());
        let error = run(start + Duration::from_secs(100), || {
            calls.borrow_mut().push(start.elapsed().as_secs());
            async { Err("relay unavailable".into()) }
        })
        .await
        .unwrap_err();
        assert_eq!(error, "relay unavailable");
        assert_eq!(*calls.borrow(), [0, 1, 3, 7, 15, 31, 61, 91]);
        assert_eq!(start.elapsed(), Duration::from_secs(100));
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_cancel_in_flight_award_or_retry_a_terminal_success() {
        let start = Instant::now();
        run(start + Duration::from_secs(1), || async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(2));
    }
}
