//! When the seller run loop's work runs, kept free of relay I/O so it is testable on a paused clock.
//!
//! The loop used to await each event handler, backfill and outbox pass inline, inside the same
//! `select!` that carries the heartbeat, the relay-stall watchdog and the shutdown request. A relay
//! that stopped acknowledging publications therefore froze all of them: every pending outbox row cost
//! one SDK OK timeout, serially, so a backlog of 120 rows was a 20-minute freeze. That work now runs
//! on ONE serialized lane, which the loop polls alongside everything else. This module holds what
//! feeds the lane ([`WorkQueue`]), what reports a lane that stays busy ([`LaneBusy`]), the pacing of
//! routine outbox drains while the relay acknowledges nothing ([`OutboxPacing`]), the retry clock of
//! an incomplete backfill ([`RetryBackoff`]), and the watchdog's liveness clock ([`LivenessClock`]).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::time::Instant;

/// One piece of work for the lane.
#[derive(Debug)]
pub(crate) enum Work<E> {
    /// A relay event for its handler: offer, award, acceptance, rejection, receipt or payment wrap.
    Event(E),
    /// The drain tick's housekeeping: staged-offer re-drives, the claim-lapse sweep, capacity
    /// reconsideration, harness re-probes and a paced outbox drain.
    Tick,
    /// The periodic stored-history pass: payment wraps (with private content), then offers.
    Backfill,
}

impl<E> Work<E> {
    /// What the operator log calls this work.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Event(_) => "a relay event",
            Self::Tick => "the drain tick",
            Self::Backfill => "the stored-history backfill",
        }
    }
}

/// What feeds the lane. Events wait in arrival order; the two periodic passes are flags, because a
/// tick that arrives while the same pass is already waiting adds nothing.
///
/// Events and periodic passes ALTERNATE while both are waiting, and so do the two periodic passes:
/// a flood of offers cannot postpone the outbox drain, a slow drain cannot hold back every queued
/// award, and a drain tick that keeps coming due cannot starve the backfill.
#[derive(Debug)]
pub(crate) struct WorkQueue<E> {
    events: VecDeque<E>,
    cap: usize,
    tick_due: bool,
    backfill_due: bool,
    periodic_turn: bool,
    backfill_turn: bool,
}

impl<E> WorkQueue<E> {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            events: VecDeque::new(),
            cap: cap.max(1),
            tick_due: false,
            backfill_due: false,
            periodic_turn: false,
            backfill_turn: false,
        }
    }

    /// Whether another event fits. While the queue is full the loop stops reading the relay's event
    /// stream (which the SDK keeps buffering) rather than letting this queue grow without bound.
    pub(crate) fn has_room(&self) -> bool {
        self.events.len() < self.cap
    }

    pub(crate) fn push_event(&mut self, event: E) {
        self.events.push_back(event);
    }

    pub(crate) fn request_tick(&mut self) {
        self.tick_due = true;
    }

    pub(crate) fn request_backfill(&mut self) {
        self.backfill_due = true;
    }

    pub(crate) fn queued_events(&self) -> usize {
        self.events.len()
    }

    pub(crate) fn periodic_waiting(&self) -> usize {
        usize::from(self.tick_due) + usize::from(self.backfill_due)
    }

    /// The next piece of work, or `None` when nothing is waiting.
    ///
    /// `backfill_allowed = false` holds a due backfill (it stays due). The loop passes it while a
    /// relay recovery is rebuilding the connection, so the backfill's history reads cannot race the
    /// recovery's re-authentication.
    pub(crate) fn next(&mut self, backfill_allowed: bool) -> Option<Work<E>> {
        let backfill_ready = self.backfill_due && backfill_allowed;
        let periodic_ready = self.tick_due || backfill_ready;
        let take_periodic = match (self.events.is_empty(), periodic_ready) {
            (true, false) => return None,
            (true, true) => true,
            (false, false) => false,
            (false, true) => self.periodic_turn,
        };
        if !take_periodic {
            self.periodic_turn = true;
            return self.events.pop_front().map(Work::Event);
        }
        self.periodic_turn = false;
        if backfill_ready && (!self.tick_due || self.backfill_turn) {
            self.backfill_due = false;
            self.backfill_turn = false;
            Some(Work::Backfill)
        } else {
            self.tick_due = false;
            self.backfill_turn = true;
            Some(Work::Tick)
        }
    }
}

/// How long one piece of lane work may run before the loop reports the lane as busy. Later reports
/// come at double the previous wait (60 s, 120 s, ...), so a lane held for an hour costs a handful of
/// lines rather than one per tick.
pub(crate) const LANE_BUSY_WARN_AFTER: Duration = Duration::from_secs(30);

/// Watches how long the lane has been on its current piece of work.
#[derive(Debug)]
pub(crate) struct LaneBusy {
    current: Option<(Instant, &'static str)>,
    next_warning: Duration,
    warned: bool,
}

impl Default for LaneBusy {
    fn default() -> Self {
        Self {
            current: None,
            next_warning: LANE_BUSY_WARN_AFTER,
            warned: false,
        }
    }
}

impl LaneBusy {
    pub(crate) fn start(&mut self, label: &'static str, now: Instant) {
        self.current = Some((now, label));
        self.next_warning = LANE_BUSY_WARN_AFTER;
        self.warned = false;
    }

    /// The lane finished its work. Returns `(label, how long it took)` when that work had been
    /// reported as busy, so the loop can say when the lane cleared.
    pub(crate) fn finish(&mut self, now: Instant) -> Option<(&'static str, Duration)> {
        let (since, label) = self.current.take()?;
        let warned = std::mem::replace(&mut self.warned, false);
        warned.then(|| (label, now.saturating_duration_since(since)))
    }

    /// `Some((label, busy for))` once per threshold while one piece of work keeps the lane.
    pub(crate) fn due_warning(&mut self, now: Instant) -> Option<(&'static str, Duration)> {
        let (since, label) = self.current?;
        let busy = now.saturating_duration_since(since);
        if busy < self.next_warning {
            return None;
        }
        self.warned = true;
        while self.next_warning <= busy {
            self.next_warning = self.next_warning.saturating_mul(2);
        }
        Some((label, busy))
    }

    /// The work in progress and how long it has run.
    pub(crate) fn current(&self, now: Instant) -> Option<(&'static str, Duration)> {
        self.current
            .map(|(since, label)| (label, now.saturating_duration_since(since)))
    }
}

/// The first pause after a drain pass the relay acknowledged none of.
pub(crate) const OUTBOX_BACKOFF_MIN: Duration = Duration::from_secs(5);
/// The longest pause. A routine drain still runs at least this often while publishing fails.
pub(crate) const OUTBOX_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Paces the lane's ROUTINE outbox drains (after each event, on the drain tick, after a backfill)
/// while the relay acknowledges none of our publications.
///
/// A bounded pass against such a relay spends its whole budget and confirms nothing; repeated after
/// every event, that keeps the lane busy on futile publishing while offers, awards and payments wait
/// behind it. After an unproductive pass, routine drains pause (5 s, doubling to 30 s); any pass
/// that confirms a publication, or finds nothing to publish, resumes them at once. Rows are never
/// touched by a pause: they stay `pending` and are retried as the same events. The drains that
/// cannot wait (an award's claim, a finished job's result) do not consult this.
#[derive(Debug)]
pub(crate) struct OutboxPacing {
    resume_at: Option<Instant>,
    next_delay: Duration,
}

impl Default for OutboxPacing {
    fn default() -> Self {
        Self {
            resume_at: None,
            next_delay: OUTBOX_BACKOFF_MIN,
        }
    }
}

impl OutboxPacing {
    pub(crate) fn may_drain(&self, now: Instant) -> bool {
        self.resume_at.is_none_or(|resume_at| now >= resume_at)
    }

    /// Lift any pause: the relay has just acknowledged one of our publications (a heartbeat).
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Fold one finished pass in: `confirmed` publications, and `unconfirmed` rows it failed, cut
    /// short or never reached. Returns the pause now in force when the pass was unproductive.
    pub(crate) fn record(
        &mut self,
        confirmed: usize,
        unconfirmed: usize,
        now: Instant,
    ) -> Option<Duration> {
        if confirmed > 0 || unconfirmed == 0 {
            *self = Self::default();
            return None;
        }
        let delay = self.next_delay;
        self.resume_at = Some(now + delay);
        self.next_delay = delay.saturating_mul(2).min(OUTBOX_BACKOFF_MAX);
        Some(delay)
    }
}

/// A doubling retry delay between `min` and `max`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryBackoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl RetryBackoff {
    pub(crate) fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max: max.max(min),
            next: min,
        }
    }

    pub(crate) fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = delay.saturating_mul(2).min(self.max);
        delay
    }

    pub(crate) fn reset(&mut self) {
        self.next = self.min;
    }
}

/// When the relay last CONFIRMED liveness: on one heartbeat tick both the read probe and the
/// heartbeat write succeeded (#509).
///
/// Stamped with the instant the confirming tick STARTED, not when its publish completed, and judged
/// by [`watchdog_tripped`] at the start of each later tick. The clock this replaces was stamped
/// after the publish completed, so on the third missed tick of a 3 × 300 s threshold it read a
/// fraction of a second short of 900 s, and the watchdog fired a whole interval late — the field's
/// 1229 s "outage" is 1200 s of that plus the recovery itself.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LivenessClock {
    confirmed_at: Instant,
    confirmed_unix: i64,
}

impl LivenessClock {
    /// Seeded as confirmed now, so a node never trips before its first tick has run.
    pub(crate) fn new(now: Instant, now_unix: i64) -> Self {
        Self {
            confirmed_at: now,
            confirmed_unix: now_unix,
        }
    }

    /// Liveness confirmed as of `at` (wall clock `unix`). Never moves the clock backwards: a tick
    /// that started before a recovery's grace stamp confirms nothing newer than that stamp.
    pub(crate) fn confirm(&mut self, at: Instant, unix: i64) {
        if at >= self.confirmed_at {
            self.confirmed_at = at;
            self.confirmed_unix = unix;
        }
    }

    /// How long liveness has gone unconfirmed, as of `at`.
    pub(crate) fn unconfirmed_for(&self, at: Instant) -> Duration {
        at.saturating_duration_since(self.confirmed_at)
    }

    /// Wall-clock second of the last confirmation: the overlap cursor for a re-subscribe.
    pub(crate) fn confirmed_unix(&self) -> i64 {
        self.confirmed_unix
    }
}

/// Whether the watchdog trips on a tick that finds liveness unconfirmed for `unconfirmed`.
///
/// `threshold` is `interval × stall_missed_intervals`, and the watchdog should trip on the tick that
/// completes it. Ticks are spaced by `interval` but never land exactly on schedule, so a tick due at
/// the threshold can read a hair short of it; half an interval of tolerance absorbs that without
/// letting the PREVIOUS tick (a whole interval earlier) trip.
pub(crate) fn watchdog_tripped(unconfirmed: Duration, threshold: Duration, interval: Duration) -> bool {
    unconfirmed.saturating_add(interval / 2) >= threshold
}

/// How long shutdown waits for ordinary lane work to finish. Everything it does is retried after a
/// restart: offers and awards are re-delivered, outbox rows stay pending, history is re-read.
pub(crate) const LANE_SETTLE_GRACE: Duration = Duration::from_secs(3);
/// How long shutdown waits while the lane is redeeming a payment at the mint. A swap cut off midway
/// may already have moved the ecash, so it gets the same bound as the platform-fee drain.
pub(crate) const LANE_SETTLE_MONEY_BOUND: Duration = Duration::from_secs(60);

/// The shutdown wait for in-flight lane work: the money bound while a payment redeem is in progress,
/// the grace otherwise.
pub(crate) fn lane_settle_bound(money_critical: bool) -> Duration {
    if money_critical {
        LANE_SETTLE_MONEY_BOUND
    } else {
        LANE_SETTLE_GRACE
    }
}

/// How a shutdown's wait for in-flight lane work ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled {
    Finished(Duration),
    Abandoned { after: Duration, redeeming: bool },
}

/// Let `work` finish within [`lane_settle_bound`]: the short grace, or the money bound while a
/// payment redeem is in flight — including one that began during the grace.
pub(crate) async fn settle<F: Future + Unpin>(work: &mut F, redeems_in_flight: &AtomicUsize) -> Settled {
    let started = Instant::now();
    loop {
        let redeeming = redeems_in_flight.load(Ordering::SeqCst) > 0;
        let deadline = started + lane_settle_bound(redeeming);
        if tokio::time::timeout_at(deadline, &mut *work).await.is_ok() {
            return Settled::Finished(started.elapsed());
        }
        if !redeeming && redeems_in_flight.load(Ordering::SeqCst) > 0 {
            continue;
        }
        return Settled::Abandoned {
            after: started.elapsed(),
            redeeming,
        };
    }
}

/// Counts one operation in flight for as long as it lives, including when its future is dropped.
pub(crate) struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    pub(crate) fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_all(queue: &mut WorkQueue<u32>, backfill_allowed: bool) -> Vec<String> {
        let mut order = Vec::new();
        while let Some(work) = queue.next(backfill_allowed) {
            order.push(match work {
                Work::Event(n) => format!("e{n}"),
                Work::Tick => "tick".to_owned(),
                Work::Backfill => "backfill".to_owned(),
            });
        }
        order
    }

    /// A flood of events and the periodic passes take turns. Revert the alternation (always prefer
    /// events) and the tick only runs after the fourth event.
    #[test]
    fn events_and_periodic_passes_alternate_so_neither_starves() {
        let mut queue = WorkQueue::new(16);
        for n in 1..=4 {
            queue.push_event(n);
        }
        queue.request_tick();
        queue.request_backfill();
        assert_eq!(
            drain_all(&mut queue, true),
            ["e1", "tick", "e2", "backfill", "e3", "e4"]
        );
    }

    /// A drain tick that is due again every time the lane looks must not starve the backfill.
    #[test]
    fn a_tick_that_keeps_coming_due_does_not_starve_the_backfill() {
        let mut queue: WorkQueue<u32> = WorkQueue::new(16);
        queue.request_backfill();
        let mut ran_backfill = false;
        for _ in 0..4 {
            queue.request_tick();
            if matches!(queue.next(true), Some(Work::Backfill)) {
                ran_backfill = true;
                break;
            }
        }
        assert!(ran_backfill, "the backfill never got a turn");
    }

    /// While a recovery holds backfills, the due backfill waits without blocking ticks or events,
    /// and runs once it is allowed again.
    #[test]
    fn a_held_backfill_stays_due_and_runs_once_allowed() {
        let mut queue = WorkQueue::new(16);
        queue.request_backfill();
        queue.push_event(1);
        queue.request_tick();
        assert_eq!(drain_all(&mut queue, false), ["e1", "tick"]);
        assert_eq!(queue.periodic_waiting(), 1, "the backfill is still due");
        assert_eq!(drain_all(&mut queue, true), ["backfill"]);
    }

    #[test]
    fn the_queue_has_no_room_at_its_cap() {
        let mut queue = WorkQueue::new(2);
        queue.push_event(1);
        assert!(queue.has_room());
        queue.push_event(2);
        assert!(!queue.has_room());
        assert_eq!(queue.queued_events(), 2);
        assert!(matches!(queue.next(true), Some(Work::Event(1))));
        assert!(queue.has_room());
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_lane_is_reported_at_doubling_thresholds_then_when_it_clears() {
        let mut busy = LaneBusy::default();
        let start = Instant::now();
        busy.start("the drain tick", start);
        assert_eq!(busy.due_warning(start + Duration::from_secs(29)), None);
        assert_eq!(
            busy.due_warning(start + Duration::from_secs(31)),
            Some(("the drain tick", Duration::from_secs(31)))
        );
        assert_eq!(busy.due_warning(start + Duration::from_secs(45)), None);
        assert!(busy.due_warning(start + Duration::from_secs(61)).is_some());
        assert_eq!(busy.due_warning(start + Duration::from_secs(100)), None);
        assert_eq!(
            busy.finish(start + Duration::from_secs(100)),
            Some(("the drain tick", Duration::from_secs(100)))
        );

        // Work that never crossed a threshold finishes quietly.
        busy.start("a relay event", start);
        assert_eq!(busy.finish(start + Duration::from_secs(2)), None);
    }

    #[tokio::test(start_paused = true)]
    async fn outbox_pacing_pauses_only_after_unproductive_passes() {
        let mut pacing = OutboxPacing::default();
        let now = Instant::now();
        assert!(pacing.may_drain(now));
        assert_eq!(pacing.record(0, 0, now), None, "nothing to publish is not a failure");
        assert_eq!(pacing.record(3, 5, now), None, "any confirmation keeps draining");
        assert_eq!(pacing.record(0, 2, now), Some(Duration::from_secs(5)));
        assert!(!pacing.may_drain(now + Duration::from_secs(4)));
        assert!(pacing.may_drain(now + Duration::from_secs(5)));
        assert_eq!(pacing.record(0, 2, now), Some(Duration::from_secs(10)));
        assert_eq!(pacing.record(0, 2, now), Some(Duration::from_secs(20)));
        assert_eq!(pacing.record(0, 2, now), Some(OUTBOX_BACKOFF_MAX));
        assert_eq!(pacing.record(0, 2, now), Some(OUTBOX_BACKOFF_MAX));
        assert_eq!(pacing.record(1, 0, now), None);
        assert!(pacing.may_drain(now), "a productive pass lifts the pause at once");
        assert_eq!(
            pacing.record(0, 1, now),
            Some(OUTBOX_BACKOFF_MIN),
            "and the next pause starts short again"
        );
    }

    #[test]
    fn retry_backoff_doubles_to_its_cap_and_resets() {
        let mut retry = RetryBackoff::new(Duration::from_secs(30), Duration::from_secs(300));
        let delays: Vec<u64> = (0..6).map(|_| retry.next_delay().as_secs()).collect();
        assert_eq!(delays, [30, 60, 120, 240, 300, 300]);
        retry.reset();
        assert_eq!(retry.next_delay().as_secs(), 30);
    }

    /// The watchdog trips on the tick that completes the threshold — the third missed tick of a
    /// 3 × 300 s threshold, even when that tick runs early — and never on the tick before. Revert to
    /// a strict `unconfirmed >= threshold` and the early third tick misses, which is the field's
    /// one-interval-late detection.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_trips_on_the_tick_that_completes_its_threshold() {
        let interval = Duration::from_secs(300);
        let threshold = interval * 3;
        let start = Instant::now();
        let mut clock = LivenessClock::new(start, 1_000);
        // Confirmed by the tick that started at `start`; its publish completing later changes nothing.
        clock.confirm(start, 1_000);
        let early = Duration::from_millis(250);
        for missed in 1..=2u32 {
            let tick = start + interval * missed - early;
            assert!(
                !watchdog_tripped(clock.unconfirmed_for(tick), threshold, interval),
                "missed tick {missed} must not trip"
            );
        }
        let third = start + interval * 3 - early;
        assert!(watchdog_tripped(clock.unconfirmed_for(third), threshold, interval));
        assert!(watchdog_tripped(clock.unconfirmed_for(start + threshold), threshold, interval));
        // A single-interval threshold trips on the first unconfirmed tick, not on a confirming one.
        assert!(!watchdog_tripped(Duration::ZERO, interval, interval));
        assert!(watchdog_tripped(interval - early, interval, interval));
        assert_eq!(clock.confirmed_unix(), 1_000);
    }

    /// A recovery's grace stamp is never moved backwards by the tick that started before it.
    #[tokio::test(start_paused = true)]
    async fn confirming_an_earlier_instant_does_not_rewind_the_clock() {
        let start = Instant::now();
        let mut clock = LivenessClock::new(start, 10);
        let grace = start + Duration::from_secs(40);
        clock.confirm(grace, 50);
        clock.confirm(start + Duration::from_secs(5), 15);
        assert_eq!(clock.unconfirmed_for(grace + Duration::from_secs(1)), Duration::from_secs(1));
        assert_eq!(clock.confirmed_unix(), 50);
    }

    struct Never;
    impl Future for Never {
        type Output = ();
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            std::task::Poll::Pending
        }
    }

    /// Ordinary lane work gets the short grace; a payment redeem gets the money bound, including
    /// one that begins during the grace. Drop the re-check and the late redeem is cut off at 3 s.
    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_a_redeem_but_not_for_ordinary_work() {
        let redeems = AtomicUsize::new(0);
        assert_eq!(
            settle(&mut Never, &redeems).await,
            Settled::Abandoned { after: LANE_SETTLE_GRACE, redeeming: false }
        );

        let redeem = InFlight::enter(&redeems);
        assert_eq!(
            settle(&mut Never, &redeems).await,
            Settled::Abandoned { after: LANE_SETTLE_MONEY_BOUND, redeeming: true }
        );
        drop(redeem);
        assert_eq!(redeems.load(Ordering::SeqCst), 0, "the guard releases on drop");

        // A redeem that began during the grace.
        let late = std::sync::Arc::new(AtomicUsize::new(0));
        let flag = std::sync::Arc::clone(&late);
        let starter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2_500)).await;
            flag.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            settle(&mut Never, &late).await,
            Settled::Abandoned { after: LANE_SETTLE_MONEY_BOUND, redeeming: true }
        );
        starter.await.expect("starter task");

        // Work that finishes is not abandoned.
        let mut quick = Box::pin(tokio::time::sleep(Duration::from_secs(1)));
        assert_eq!(settle(&mut quick, &redeems).await, Settled::Finished(Duration::from_secs(1)));
    }


    #[test]
    fn shutdown_waits_longer_only_while_a_payment_is_being_redeemed() {
        assert_eq!(lane_settle_bound(false), LANE_SETTLE_GRACE);
        assert_eq!(lane_settle_bound(true), LANE_SETTLE_MONEY_BOUND);
        assert!(LANE_SETTLE_GRACE < LANE_SETTLE_MONEY_BOUND);
    }
}
