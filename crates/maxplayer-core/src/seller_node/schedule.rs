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

/// The boot replay: the stored history the live subscriptions hand back right after start, which
/// can fill the queue on every restart of a seat with any history. A subscription's replay is over
/// at its EOSE or its CLOSED; the whole replay is over at a reconnect, a loop gap, a failed sign-in,
/// a lagged notification stream, and at the latest this long after the subscriptions went out. A
/// queue-full episode that starts after it is reported like any other.
pub(crate) const BOOT_REPLAY_WINDOW: Duration = Duration::from_secs(120);

/// A boot-replay episode still not drained this long after it started is reported at default level
/// (and its end with the ordinary "queue empty again" line): a queue that never drains must not
/// stay invisible just because it started during the replay. A large replay interleaved with the
/// boot backfills can take this long while still draining; the line says how much is left.
pub(crate) const BOOT_REPLAY_STUCK_AFTER: Duration = Duration::from_secs(120);

/// One lane log line and the level it prints at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaneLine {
    /// `opline!`: always printed.
    Default(String),
    /// `opline_verbose!`: only with `MAXPLAYER_VERBOSE`.
    Verbose(String),
}

/// One "WORK-LANE queue full" episode. On a boot re-read the lane frees a slot and the relay stream
/// refills it at once, so a log line latched on "has room again" fired dozens of times in two
/// seconds. The episode instead starts at the first fill, counts every refill, and ends only when
/// the queue is empty again, with one summary line.
#[derive(Debug, Default)]
pub(crate) struct QueueFullEpisode {
    started: Option<Instant>,
    fills: u32,
    /// When the boot subscriptions went out. `None` (`default()`): no episode is the boot replay.
    replay_started: Option<Instant>,
    /// Every boot subscription.
    replay_subs: Vec<String>,
    /// Boot subscriptions whose stored history has not ended yet (no EOSE or CLOSED). Empty: the
    /// replay is over.
    replay_pending: Vec<String>,
    /// An episode has started since the loop did: only the first can be the boot replay.
    seen: bool,
    /// The current (or, after [`Self::after_take`] ended it, the last) episode is the boot replay.
    boot: bool,
}

impl QueueFullEpisode {
    /// For a node whose boot subscriptions `subs` went out at `replay_started`.
    pub(crate) fn new(replay_started: Instant, subs: &[&str]) -> Self {
        let subs: Vec<String> = subs.iter().map(|s| (*s).to_owned()).collect();
        Self {
            replay_started: Some(replay_started),
            replay_pending: subs.clone(),
            replay_subs: subs,
            ..Self::default()
        }
    }

    /// The relay's EOSE (or CLOSED) for subscription `id`: no more of its stored history comes.
    pub(crate) fn stored_history_ended(&mut self, id: &str) {
        self.replay_pending.retain(|s| s != id);
    }

    /// A reconnect, loop gap, failed sign-in or lagged notification stream: whatever arrives next is
    /// not the boot replay (an EOSE may have been lost, so waiting for it would fall back to the
    /// window).
    pub(crate) fn end_boot_replay(&mut self) {
        self.replay_pending.clear();
    }

    /// Is the boot replay still arriving at `now`?
    pub(crate) fn in_boot_replay(&self, now: Instant) -> bool {
        !self.replay_pending.is_empty()
            && self
                .replay_started
                .is_some_and(|at| now.saturating_duration_since(at) < BOOT_REPLAY_WINDOW)
    }

    /// Is an event from subscription `sub` at `now` part of the boot replay? Yes while that boot
    /// subscription's own stored history is still arriving. An event from a subscription that is not
    /// a boot subscription (a transient history read, e.g. a boot backfill) counts while any boot
    /// subscription is still replaying.
    pub(crate) fn replaying(&self, sub: &str, now: Instant) -> bool {
        self.in_boot_replay(now)
            && (self.replay_pending.iter().any(|s| s == sub)
                || !self.replay_subs.iter().any(|s| s == sub))
    }

    /// After an event from subscription `sub` was queued. `true` exactly once per episode: log
    /// [`Self::full_line`].
    pub(crate) fn after_push(&mut self, has_room: bool, sub: &str, now: Instant) -> bool {
        if has_room {
            return false;
        }
        self.fills = self.fills.saturating_add(1);
        if self.started.is_some() {
            return false;
        }
        self.started = Some(now);
        self.boot = !self.seen && self.replaying(sub, now);
        self.seen = true;
        true
    }

    /// Is the current episode (or the one [`Self::after_take`] just ended) the boot replay?
    pub(crate) fn boot(&self) -> bool {
        self.boot
    }

    /// The "queue full" line for the episode [`Self::after_push`] just started: verbose-only for
    /// the boot replay, default otherwise.
    pub(crate) fn full_line(&self, cap: usize) -> LaneLine {
        if self.boot {
            LaneLine::Verbose(format!(
                "seller node WORK-LANE queue full ({cap} events) during the boot replay: relay \
                 reads pause until it drains"
            ))
        } else {
            LaneLine::Default(format!(
                "seller node WORK-LANE queue full ({cap} events): relay reads pause until it drains"
            ))
        }
    }

    /// A boot-replay episode open for [`BOOT_REPLAY_STUCK_AFTER`] or longer stops being treated as
    /// the replay: returns its default-level line once, with the `queued` events still waiting.
    /// Relay reads are said to be paused only when the queue has no room (`has_room` false). Call
    /// after [`Self::after_take`], so an episode that just drained is not reported as stuck.
    pub(crate) fn overdue(&mut self, now: Instant, queued: usize, has_room: bool) -> Option<String> {
        let open = now.saturating_duration_since(self.started?);
        if !self.boot || open < BOOT_REPLAY_STUCK_AFTER {
            return None;
        }
        self.boot = false;
        let paused = if has_room {
            ""
        } else {
            ": relay reads are paused until it drains"
        };
        Some(format!(
            "seller node WORK-LANE boot replay not drained after {}s ({queued} event(s) still \
             queued; the queue filled {} time(s)){paused}",
            open.as_secs(),
            self.fills
        ))
    }

    /// With `queued` events left in the queue. When an episode is open and the queue is empty, it
    /// ends: returns how many times the queue filled and how long the episode lasted.
    pub(crate) fn after_take(&mut self, queued: usize, now: Instant) -> Option<(u32, Duration)> {
        if queued != 0 {
            return None;
        }
        let started = self.started.take()?;
        let fills = std::mem::take(&mut self.fills);
        Some((fills, now.saturating_duration_since(started)))
    }

    /// The line for an episode [`Self::after_take`] just ended (always default level).
    pub(crate) fn ended_line(&self, fills: u32, took: Duration) -> String {
        if self.boot {
            format!(
                "seller node WORK-LANE boot replay done after {}s: the relay's stored events \
                 filled the queue {fills} time(s) after start (expected after a restart; anything \
                 skipped is re-read by the next history backfill)",
                took.as_secs()
            )
        } else {
            format!(
                "seller node WORK-LANE queue empty again after {}s (it filled {fills} time(s) \
                 meanwhile)",
                took.as_secs()
            )
        }
    }
}

/// The relay half of the periodic status line: whether this socket completed NIP-42, when the relay
/// last confirmed a seat announcement (heartbeat), and how the last relay drop ended. Fed by the
/// loop from the same supervision reports the individual RELAY-* lines come from, so an operator
/// reading only the status line still sees an interruption and its recovery.
#[derive(Debug, Default)]
pub(crate) struct RelayHealth {
    beat_landed: Option<Instant>,
    last_drop: Option<LastDrop>,
}

#[derive(Debug, Clone, Copy)]
struct LastDrop {
    /// When the recovery attempt finished.
    at: Instant,
    /// How long the relay had gone unconfirmed when the recovery finished.
    outage: Duration,
    recovered: bool,
}

impl RelayHealth {
    /// A heartbeat supervision finished; `landed` = the relay confirmed its seat announcement.
    pub(crate) fn beat(&mut self, landed: bool, now: Instant) {
        if landed {
            self.beat_landed = Some(now);
        }
    }

    /// A relay recovery finished, `outage` after the last confirmation.
    pub(crate) fn recovery(&mut self, recovered: bool, outage: Duration, now: Instant) {
        self.last_drop = Some(LastDrop {
            at: now,
            outage,
            recovered,
        });
    }

    /// The relay confirmed liveness without a recovery of ours running: after a failed recovery
    /// that means the SDK's own background reconnect ended the drop, so it is reported as recovered
    /// (`outage` after the last confirmation) instead of "FAILED, retrying" on a healthy socket.
    pub(crate) fn confirmed(&mut self, outage: Duration, now: Instant) {
        if let Some(drop) = self.last_drop.as_mut()
            && !drop.recovered
        {
            *drop = LastDrop {
                at: now,
                outage,
                recovered: true,
            };
        }
    }

    /// `relay confirmed Ns ago` (or `relay RECOVERING …`) plus sign-in, heartbeat and last-drop
    /// state. The leading phrase is unchanged, since scripts and docs key on it.
    /// `signed_in`: `None` when the loop missed relay notifications and cannot tell.
    pub(crate) fn line(
        &self,
        signed_in: Option<bool>,
        recovering: bool,
        unconfirmed: Duration,
        now: Instant,
    ) -> String {
        let ago = |at: Instant| now.saturating_duration_since(at).as_secs();
        let mut line = if recovering {
            format!("relay RECOVERING (unconfirmed for {}s)", unconfirmed.as_secs())
        } else {
            format!("relay confirmed {}s ago", unconfirmed.as_secs())
        };
        line.push_str(match signed_in {
            Some(true) => " · signed in",
            Some(false) => " · NOT signed in (no NIP-42 sign-in on this connection)",
            None => " · sign-in unknown (missed relay notifications; the next sign-in updates it)",
        });
        match self.beat_landed {
            Some(at) => line.push_str(&format!(" · heartbeat landed {}s ago", ago(at))),
            None => line.push_str(" · no heartbeat landed yet"),
        }
        if let Some(drop) = self.last_drop {
            if drop.recovered {
                line.push_str(&format!(
                    " · last relay drop recovered {}s ago after {}s",
                    ago(drop.at),
                    drop.outage.as_secs()
                ));
            } else {
                line.push_str(&format!(
                    " · relay reconnect FAILED {}s ago ({}s unconfirmed), retrying",
                    ago(drop.at),
                    drop.outage.as_secs()
                ));
            }
        }
        line
    }
}

/// How long the relay has gone unconfirmed as of `now`, by the later of the monotonic and the
/// wall clock: the monotonic one stops while the host sleeps, which is exactly when an outage is
/// longest, so it alone would report a 20-minute sleep as a few seconds.
pub(crate) fn relay_outage(clock: &LivenessClock, now: Instant, now_unix: i64) -> Duration {
    let wall = u64::try_from(now_unix.saturating_sub(clock.confirmed_unix())).unwrap_or(0);
    clock.unconfirmed_for(now).max(Duration::from_secs(wall))
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

/// What the loop does after a backfill pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackfillNext {
    /// Every read completed: the retry backoff resets.
    Done,
    /// A relay recovery overlapped the pass and cut its reads off (it drops every subscription).
    /// That is not evidence the relay withholds history: re-run once the relay is back, without
    /// growing the backoff.
    RerunAfterRecovery,
    /// The relay did not complete the reads on its own: retry after this delay (backing off).
    RetryAfter(Duration),
}

/// Whether a recovery overlapped lane work that started at `lane_started`: one is reconnecting
/// right now, or the last one ended after the work began.
pub(crate) fn overlapped_recovery(
    lane_started: Instant,
    recovering_now: bool,
    last_recovery_end: Option<Instant>,
) -> bool {
    recovering_now || last_recovery_end.is_some_and(|ended| ended >= lane_started)
}

/// Decide what follows a backfill pass. Incomplete history is never taken as empty either way.
pub(crate) fn after_backfill(
    complete: bool,
    overlapped_recovery: bool,
    backoff: &mut RetryBackoff,
) -> BackfillNext {
    if complete {
        backoff.reset();
        BackfillNext::Done
    } else if overlapped_recovery {
        BackfillNext::RerunAfterRecovery
    } else {
        BackfillNext::RetryAfter(backoff.next_delay())
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

/// How long the seat announcement (kind-30340) has gone without a relay-confirmed publish, so the
/// operator log says so once it falls behind its cadence. Buyers and the market board judge a seat
/// by the announcement's age: a seat whose beats stop landing looks offline while the process runs,
/// and nothing said so until the relay-stall watchdog tripped, which also needs the READ probe to
/// fail. Checked from the drain tick, so a supervision that never returns is reported too.
pub(crate) struct AnnouncementGap {
    last_landed: Instant,
    warned: bool,
}

impl AnnouncementGap {
    pub(crate) fn new(now: Instant) -> Self {
        Self { last_landed: now, warned: false }
    }

    /// A supervision finished; `landed` = the relay confirmed one of its beats. Returns the line to
    /// log when that ends a gap [`Self::check`] reported.
    pub(crate) fn on_publish(&mut self, landed: bool, now: Instant) -> Option<String> {
        if !landed {
            return None;
        }
        let gap = now.saturating_duration_since(self.last_landed);
        self.last_landed = now;
        std::mem::take(&mut self.warned).then(|| {
            format!(
                "seller node seat announcement renewed: relay confirmed kind-30340 after {}s without one",
                gap.as_secs()
            )
        })
    }

    /// ONE line per gap, once no beat has landed for 1.5 intervals. `supervision_running_for` is
    /// how long the in-flight heartbeat supervision has run, if one is in flight.
    pub(crate) fn check(
        &mut self,
        now: Instant,
        interval: Duration,
        supervision_running_for: Option<Duration>,
    ) -> Option<String> {
        let since = now.saturating_duration_since(self.last_landed);
        if self.warned || since < interval.saturating_mul(3) / 2 {
            return None;
        }
        self.warned = true;
        let why = match supervision_running_for {
            Some(running) => format!("a heartbeat supervision has been running for {}s", running.as_secs()),
            None => "no supervision is running; the last beats got no relay OK".to_owned(),
        };
        Some(format!(
            "seller node seat announcement OVERDUE: no relay-confirmed kind-30340 for {}s (interval \
             {}s); {why}. Buyers and the market board may show this seat offline",
            since.as_secs(),
            interval.as_secs()
        ))
    }
}

/// A drain-tick gap this long is reported: the loop turns every 5 s, so a minute without a turn means
/// the process did not run (the host slept, or the process was stopped) or the loop was blocked.
pub(crate) const LOOP_GAP_REPORT: Duration = Duration::from_secs(60);
/// A gap this long also forces a relay recovery on the next heartbeat: the sockets are presumed dead,
/// and the watchdog's own clock cannot see a host sleep (it is monotonic, which stops while asleep).
pub(crate) const LOOP_GAP_RECONNECT: Duration = Duration::from_secs(120);
/// Wall time that ran this far ahead of monotonic time over one gap means the host was asleep.
const HOST_SLEEP_SLACK: Duration = Duration::from_secs(30);

/// Milliseconds since the Unix epoch (0 before it), for [`LoopTurns`].
pub(crate) fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// A stretch in which the seller loop did not turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoopGap {
    pub(crate) secs: u64,
    /// Wall time ran ahead of monotonic time: the host was asleep for most of it.
    pub(crate) host_slept: bool,
}

impl LoopGap {
    /// Why a relay recovery was forced for this gap (the `RELAY-RECOVERY triggered:` line).
    pub(crate) fn recovery_reason(&self) -> String {
        if self.host_slept {
            format!("the host slept for about {}s", self.secs)
        } else {
            format!("the event loop did not run for {}s", self.secs)
        }
    }

    /// The operator line for this gap. `reannounce`: the seat heartbeat is on, so the seat is
    /// re-announced now. `reconnect`: a relay recovery was forced for it.
    pub(crate) fn line(&self, reannounce: bool, reconnect: bool) -> String {
        let what = if self.host_slept {
            format!(
                "seller node RESUMED after the host slept for about {}s",
                self.secs
            )
        } else {
            format!(
                "seller node event loop did not run for {}s (the process was stopped or the loop was \
                 blocked)",
                self.secs
            )
        };
        format!(
            "{what}: no seat announcement, offer read or claim happened meanwhile; {}{}",
            if reannounce {
                "re-announcing the seat and re-reading offers now"
            } else {
                "re-reading offers now"
            },
            if reconnect {
                ", and reconnecting to the relay"
            } else {
                ""
            }
        )
    }
}

/// What the loop still owes after a [`LoopGap`], across the heartbeat supervision that may be in
/// flight when the gap is seen.
///
/// - A gap seen while a supervision runs cannot re-announce at once (the heartbeat tick is not armed
///   then), so the beat is owed until that supervision ends; without this the re-announce, and any
///   forced recovery, waited a whole heartbeat interval.
/// - A gap that forces a reconnect holds every offer/wrap backfill until a forced recovery that
///   started after it has SUCCEEDED: a read on the dead socket fails, backs off and is retried
///   late. A later reconnect gap supersedes a recovery already running.
/// - A forced recovery that FAILED is re-armed for the next heartbeat tick. The watchdog cannot take
///   over: its clock is monotonic and did not count the gap, so it would not trip for intervals.
#[derive(Debug, Default)]
pub(crate) struct RecoveryDebt {
    beat_owed: bool,
    backfill_owed: bool,
    /// The forced reason the running supervision carries, and whether the owed backfill waits on it.
    running_forced: Option<String>,
    running_releases_backfill: bool,
}

/// What [`RecoveryDebt::supervision_done`] tells the loop to do now.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DebtDue {
    /// Fire the heartbeat tick now.
    pub(crate) beat: bool,
    /// Request the offer/wrap backfill now.
    pub(crate) backfill: bool,
    /// Force this recovery again on the next heartbeat tick.
    pub(crate) rearm_forced: Option<String>,
}

impl RecoveryDebt {
    /// A gap was reported. `supervision_running`: a heartbeat supervision is in flight.
    /// `reconnect`: the gap forces a relay recovery. Returns (fire the heartbeat now, backfill now).
    pub(crate) fn gap(&mut self, supervision_running: bool, reconnect: bool) -> (bool, bool) {
        if supervision_running {
            self.beat_owed = true;
        }
        if reconnect {
            self.backfill_owed = true;
            // A recovery already running predates this gap; it cannot release this backfill.
            self.running_releases_backfill = false;
        }
        (!supervision_running, !self.backfill_owed)
    }

    /// Hold backfills: a reconnect gap's recovery has not succeeded yet.
    pub(crate) fn backfill_held(&self) -> bool {
        self.backfill_owed
    }

    /// A heartbeat supervision starts, carrying `forced` (a recovery it will run).
    pub(crate) fn supervision_started(&mut self, forced: Option<&str>) {
        self.running_forced = forced.map(str::to_owned);
        self.running_releases_backfill = forced.is_some() && self.backfill_owed;
    }

    /// The running supervision ended. `recovered`: `Some(ok)` when it ran a recovery.
    pub(crate) fn supervision_done(&mut self, recovered: Option<bool>) -> DebtDue {
        let forced = self.running_forced.take();
        let releases = std::mem::take(&mut self.running_releases_backfill);
        let backfill = releases && recovered == Some(true);
        if backfill {
            self.backfill_owed = false;
        }
        DebtDue {
            beat: std::mem::take(&mut self.beat_owed),
            backfill,
            // Anything but a success re-arms a forced recovery: a supervision that ended without
            // running it (`None`) must not drop it while its backfill stays held (advisor r3 L3).
            rearm_forced: if recovered == Some(true) { None } else { forced },
        }
    }
}

/// When the drain tick last ran, on both clocks. Tokio's timers and the heartbeat watchdog use the
/// monotonic clock, which stops while the host sleeps (Linux, macOS), so after a sleep the next beat
/// comes up to a whole interval later and nothing logs the gap. Wall time keeps counting, so the
/// first tick after a sleep, a SIGSTOP or a blocked loop sees the gap here.
pub(crate) struct LoopTurns {
    last_mono: std::time::Instant,
    last_wall_ms: i64,
    report_after: Duration,
}

impl LoopTurns {
    pub(crate) fn new(mono: std::time::Instant, wall_ms: i64, report_after: Duration) -> Self {
        Self {
            last_mono: mono,
            last_wall_ms: wall_ms,
            report_after,
        }
    }

    /// The drain tick ran at (`mono`, `wall_ms`). Returns the gap since the previous tick once it
    /// reaches the report threshold. A wall clock stepped backwards counts as no wall time.
    pub(crate) fn turn(&mut self, mono: std::time::Instant, wall_ms: i64) -> Option<LoopGap> {
        let mono_gap = mono.saturating_duration_since(self.last_mono);
        let wall_gap = Duration::from_millis(
            u64::try_from(wall_ms.saturating_sub(self.last_wall_ms)).unwrap_or(0),
        );
        self.last_mono = mono;
        self.last_wall_ms = wall_ms;
        let gap = mono_gap.max(wall_gap);
        (gap >= self.report_after).then(|| LoopGap {
            secs: gap.as_secs(),
            host_slept: wall_gap.saturating_sub(mono_gap) >= HOST_SLEEP_SLACK,
        })
    }
}

/// How often the loop watchdog thread looks at the loop's pulse.
const LOOP_WATCH_EVERY: Duration = Duration::from_secs(10);

/// What the loop watchdog thread decides on one wake-up: report a loop that stopped turning while
/// this process kept running. The loop's own [`LoopTurns`] reports the gap once it turns again; this
/// says so WHILE it is stuck, which a loop-side check never can.
#[derive(Debug, Default)]
pub(crate) struct LoopWatch {
    warned: bool,
}

impl LoopWatch {
    /// `since_turn`: how long since the loop last turned. `overslept`: how much later than asked this
    /// thread woke. A late wake means the whole process was not running (host asleep, SIGSTOP), which
    /// is not a blocked loop; the loop reports that gap itself.
    pub(crate) fn check(
        &mut self,
        since_turn: Duration,
        overslept: Duration,
        warn_after: Duration,
    ) -> Option<String> {
        if since_turn < warn_after {
            self.warned = false;
            return None;
        }
        if self.warned || overslept >= HOST_SLEEP_SLACK {
            return None;
        }
        self.warned = true;
        Some(format!(
            "seller node event loop BLOCKED: it has not turned for {}s while the process runs; no \
             seat announcement, offer read, claim or status line happens until it does",
            since_turn.as_secs()
        ))
    }
}

/// The loop's pulse for [`spawn_loop_watchdog`]: monotonic time of its last turn.
pub(crate) struct LoopPulse {
    base: std::time::Instant,
    last_turn_ms: std::sync::atomic::AtomicU64,
    stopped: std::sync::atomic::AtomicBool,
}

impl LoopPulse {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            base: std::time::Instant::now(),
            last_turn_ms: std::sync::atomic::AtomicU64::new(0),
            stopped: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The loop turned now.
    pub(crate) fn beat(&self) {
        self.last_turn_ms
            .store(self.elapsed_ms(), Ordering::Relaxed);
    }

    fn since_turn(&self) -> Duration {
        Duration::from_millis(
            self.elapsed_ms()
                .saturating_sub(self.last_turn_ms.load(Ordering::Relaxed)),
        )
    }
}

/// Stops and wakes the loop watchdog thread when the loop that owns it ends, on every exit path
/// (without joining it; see `Drop`).
pub(crate) struct LoopWatchdog {
    pulse: std::sync::Arc<LoopPulse>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for LoopWatchdog {
    /// Stops and wakes the thread without waiting for it: this runs on the async loop's thread, and
    /// a join could block it behind the watchdog's own log write.
    fn drop(&mut self) {
        self.pulse.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
        }
    }
}

/// Watch `pulse` from an OS thread, which keeps running while the loop's thread is blocked, and log
/// once per stall. Returns the guard that stops the thread; a thread that cannot start is logged and
/// the loop runs without it.
pub(crate) fn spawn_loop_watchdog(
    pulse: std::sync::Arc<LoopPulse>,
    warn_after: Duration,
) -> LoopWatchdog {
    let watched = std::sync::Arc::clone(&pulse);
    let spawned = std::thread::Builder::new()
        .name("seller-loop-watch".into())
        .spawn(move || {
            let mut watch = LoopWatch::default();
            loop {
                let asked = std::time::Instant::now();
                // Parked rather than slept, so the guard's drop wakes it at once. A spurious wake only
                // checks early (no oversleep), which is harmless.
                std::thread::park_timeout(LOOP_WATCH_EVERY);
                if watched.stopped.load(Ordering::Acquire) {
                    return;
                }
                let overslept = asked.elapsed().saturating_sub(LOOP_WATCH_EVERY);
                if let Some(line) = watch.check(watched.since_turn(), overslept, warn_after) {
                    crate::opline!("{line}");
                }
            }
        });
    let thread = match spawned {
        Ok(thread) => Some(thread),
        Err(error) => {
            crate::opline!(
                "seller node: the event-loop watchdog thread could not start ({error}); continuing \
                 without it"
            );
            None
        }
    };
    LoopWatchdog { pulse, thread }
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

/// How often [`settle`] re-reads the redeem counter while a redeem is in flight, so the money bound
/// ends as soon as the redeem does.
const SETTLE_REDEEM_POLL: Duration = Duration::from_millis(250);

/// Let `work` finish: the short grace for ordinary work, the money bound while a payment redeem is
/// in flight — including one that began during the grace. The money bound lasts only as long as the
/// redeem: once it finishes, whatever the lane is still doing gets one more grace from then (never
/// past the money bound), not the rest of the 60 s. `redeeming` in the result is whether a redeem
/// was still in flight when the work was abandoned.
pub(crate) async fn settle<F: Future + Unpin>(work: &mut F, redeems_in_flight: &AtomicUsize) -> Settled {
    let started = Instant::now();
    let money_deadline = started + LANE_SETTLE_MONEY_BOUND;
    let mut deadline = started + LANE_SETTLE_GRACE;
    loop {
        let redeeming = redeems_in_flight.load(Ordering::SeqCst) > 0;
        let wake = if redeeming {
            (Instant::now() + SETTLE_REDEEM_POLL).min(money_deadline)
        } else {
            deadline
        };
        if tokio::time::timeout_at(wake, &mut *work).await.is_ok() {
            return Settled::Finished(started.elapsed());
        }
        let now = Instant::now();
        if redeems_in_flight.load(Ordering::SeqCst) > 0 {
            if now >= money_deadline {
                return Settled::Abandoned {
                    after: started.elapsed(),
                    redeeming: true,
                };
            }
            continue;
        }
        if redeeming {
            // The redeem just finished: the rest of the work gets one grace from now.
            deadline = deadline.max(now + LANE_SETTLE_GRACE).min(money_deadline);
            continue;
        }
        if now >= deadline {
            return Settled::Abandoned {
                after: started.elapsed(),
                redeeming: false,
            };
        }
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
    /// A boot flood: the lane takes one event, the relay stream refills the slot, 56 times. The
    /// "queue full" line is logged once, and one summary with the count closes the episode when
    /// the queue empties; the next flood is a new episode.
    #[test]
    fn queue_full_is_logged_once_per_episode_with_a_count() {
        let t0 = Instant::now();
        let mut queue: WorkQueue<u32> = WorkQueue::new(4);
        let mut episode = QueueFullEpisode::default();
        let mut logged = 0;
        let mut ended = Vec::new();
        let mut push = |queue: &mut WorkQueue<u32>, episode: &mut QueueFullEpisode, at| {
            queue.push_event(0);
            u32::from(episode.after_push(queue.has_room(), "offers", at))
        };
        while queue.has_room() {
            logged += push(&mut queue, &mut episode, t0);
        }
        for i in 1..56u64 {
            let at = t0 + Duration::from_millis(30 * i);
            assert!(matches!(queue.next(true), Some(Work::Event(_))));
            ended.extend(episode.after_take(queue.queued_events(), at));
            logged += push(&mut queue, &mut episode, at);
        }
        assert_eq!(logged, 1, "one queue-full line per episode");
        assert!(ended.is_empty(), "no summary while the queue still holds events");
        let at = t0 + Duration::from_secs(5);
        while queue.next(true).is_some() {
            ended.extend(episode.after_take(queue.queued_events(), at));
        }
        assert_eq!(ended, vec![(56, Duration::from_secs(5))]);
        // A later flood is a new episode, logged again.
        while queue.has_room() {
            logged += push(&mut queue, &mut episode, at);
        }
        assert_eq!(logged, 2);
    }

    const BOOT_SUBS: &[&str] = &["offers", "awards", "wraps"];

    /// The first episode while the boot subscriptions' stored history is still arriving is the boot
    /// replay; a later one, or a first one after every EOSE, is not. `default()` never marks one.
    #[test]
    fn only_the_first_episode_during_the_stored_replay_is_the_boot_replay() {
        let t0 = Instant::now();
        let mut episode = QueueFullEpisode::new(t0, BOOT_SUBS);
        assert!(episode.after_push(false, "offers", t0 + Duration::from_secs(3)));
        assert!(episode.boot(), "first fill during the replay");
        assert!(matches!(episode.full_line(1024), LaneLine::Verbose(_)));
        assert!(!episode.after_push(false, "offers", t0 + Duration::from_secs(4)), "same episode");
        let (fills, took) = episode.after_take(0, t0 + Duration::from_secs(9)).unwrap();
        assert_eq!((fills, took), (2, Duration::from_secs(6)));
        assert!(episode.boot(), "still readable after the episode ended");
        assert!(episode.ended_line(fills, took).contains("boot replay done after 6s"));
        assert!(episode.after_push(false, "offers", t0 + Duration::from_secs(30)));
        assert!(!episode.boot(), "a second episode, even during the replay, is live");
        assert!(matches!(episode.full_line(1024), LaneLine::Default(_)));

        let mut unset = QueueFullEpisode::default();
        assert!(unset.after_push(false, "offers", t0));
        assert!(!unset.boot(), "no boot subscriptions: never the boot replay");
    }

    /// The replay ends at the EOSE of EVERY boot subscription, not at a timer: a burst after the
    /// last EOSE is live even seconds after start, and one before it is still the replay.
    #[test]
    fn the_boot_replay_ends_at_the_last_eose() {
        let t0 = Instant::now();
        let mut episode = QueueFullEpisode::new(t0, BOOT_SUBS);
        episode.stored_history_ended("offers");
        episode.stored_history_ended("awards");
        episode.stored_history_ended("not-ours");
        assert!(episode.in_boot_replay(t0 + Duration::from_secs(5)), "wraps still replaying");
        episode.stored_history_ended("wraps");
        assert!(!episode.in_boot_replay(t0 + Duration::from_secs(5)));
        assert!(episode.after_push(false, "offers", t0 + Duration::from_secs(5)));
        assert!(!episode.boot(), "a live burst 5 s after start, after every EOSE");
        assert!(matches!(episode.full_line(1024), LaneLine::Default(_)));
        let (fills, took) = episode.after_take(0, t0 + Duration::from_secs(8)).unwrap();
        assert!(episode.ended_line(fills, took).contains("queue empty again after 3s"));
    }

    /// A reconnect or loop gap ends the replay, and so does the window, exclusive at its end.
    #[test]
    fn a_reconnect_or_the_window_ends_the_boot_replay() {
        let t0 = Instant::now();
        let mut gap = QueueFullEpisode::new(t0, BOOT_SUBS);
        gap.end_boot_replay();
        assert!(gap.after_push(false, "offers", t0 + Duration::from_secs(1)));
        assert!(!gap.boot(), "after a reconnect");

        let episode = QueueFullEpisode::new(t0, BOOT_SUBS);
        assert!(episode.in_boot_replay(t0 + BOOT_REPLAY_WINDOW - Duration::from_millis(1)));
        assert!(!episode.in_boot_replay(t0 + BOOT_REPLAY_WINDOW), "no EOSE, window over");
        let mut late = QueueFullEpisode::new(t0, BOOT_SUBS);
        assert!(late.after_push(false, "offers", t0 + BOOT_REPLAY_WINDOW));
        assert!(!late.boot(), "first episode at the window's end");
    }

    /// A boot-replay episode that never drains is reported once at default level after
    /// [`BOOT_REPLAY_STUCK_AFTER`], and its end then prints the ordinary line.
    #[test]
    fn a_stuck_boot_replay_episode_is_reported() {
        let t0 = Instant::now();
        let mut episode = QueueFullEpisode::new(t0, BOOT_SUBS);
        assert!(episode.overdue(t0, 0, true).is_none(), "no episode");
        assert!(episode.after_push(false, "offers", t0 + Duration::from_secs(2)));
        assert!(episode.boot());
        let opened = t0 + Duration::from_secs(2);
        let just_before = opened + BOOT_REPLAY_STUCK_AFTER - Duration::from_millis(1);
        assert!(episode.overdue(just_before, 7, false).is_none());
        let at = opened + BOOT_REPLAY_STUCK_AFTER;
        let line = episode.overdue(at, 7, false).expect("reported at the threshold");
        assert!(line.contains("not drained after 120s (7 event(s) still queued"), "{line}");
        assert!(line.ends_with("relay reads are paused until it drains"), "{line}");
        assert!(!episode.boot());
        assert!(episode.overdue(at + Duration::from_secs(60), 7, false).is_none(), "once");
        let (fills, took) = episode.after_take(0, at + Duration::from_secs(60)).unwrap();
        assert!(episode.ended_line(fills, took).contains("queue empty again"));
        // A live episode is never "overdue": its full line already printed at default level.
        let mut live = QueueFullEpisode::default();
        assert!(live.after_push(false, "offers", t0));
        assert!(live.overdue(t0 + BOOT_REPLAY_STUCK_AFTER * 3, 7, false).is_none());
    }

    /// Per subscription: an event of a boot subscription that already sent its EOSE is live even
    /// while another boot subscription is still replaying; one whose replay is still arriving is
    /// the boot replay, and so is a transient history read (not a boot subscription) meanwhile.
    #[test]
    fn the_boot_replay_is_tracked_per_subscription() {
        let t0 = Instant::now();
        let at = t0 + Duration::from_secs(5);
        let mut episode = QueueFullEpisode::new(t0, BOOT_SUBS);
        episode.stored_history_ended("offers");
        assert!(!episode.replaying("offers", at), "offers ended, wraps still replaying");
        assert!(episode.replaying("wraps", at));
        assert!(episode.replaying("a-backfill-read", at), "transient read during the replay");
        assert!(episode.after_push(false, "offers", at));
        assert!(!episode.boot(), "a live offers flood while wraps replays");
        assert!(matches!(episode.full_line(1024), LaneLine::Default(_)));

        let mut wraps = QueueFullEpisode::new(t0, BOOT_SUBS);
        wraps.stored_history_ended("offers");
        assert!(wraps.after_push(false, "wraps", at));
        assert!(wraps.boot(), "wraps' own replay");

        let mut done = QueueFullEpisode::new(t0, BOOT_SUBS);
        for sub in BOOT_SUBS {
            done.stored_history_ended(sub);
        }
        assert!(!done.replaying("a-backfill-read", at), "no replay left");
    }

    /// An episode that drains on the same turn the stuck threshold passes ends with one line:
    /// [`QueueFullEpisode::after_take`] runs first and [`QueueFullEpisode::overdue`] then sees no
    /// episode. With room in the queue the stuck line does not claim reads are paused.
    #[test]
    fn a_boot_episode_draining_at_the_threshold_prints_one_line() {
        let t0 = Instant::now();
        let mut episode = QueueFullEpisode::new(t0, BOOT_SUBS);
        assert!(episode.after_push(false, "wraps", t0));
        let at = t0 + BOOT_REPLAY_STUCK_AFTER;
        let (fills, took) = episode.after_take(0, at).expect("drained");
        assert!(episode.ended_line(fills, took).contains("boot replay done after 120s"));
        assert!(episode.overdue(at, 0, true).is_none(), "no stuck line after the end");

        let mut draining = QueueFullEpisode::new(t0, BOOT_SUBS);
        assert!(draining.after_push(false, "wraps", t0));
        let line = draining.overdue(at, 200, true).expect("still draining");
        assert!(line.contains("200 event(s) still queued"), "{line}");
        assert!(!line.contains("paused"), "{line}");
    }

    /// The run loop feeds the episode. Pinned on the source (whitespace-normalized), so the arms
    /// are checked without driving a 1024-event fill through the run loop's relay fixture.
    #[test]
    fn the_run_loop_feeds_the_boot_replay_episode() {
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let src = flat(include_str!("run.rs"));
        let at = |needle: &str| {
            src.find(&flat(needle))
                .unwrap_or_else(|| panic!("run.rs lost {needle:?}"))
        };
        // The boot subscriptions are the ones subscribe_all issues, and the replay clock starts
        // before they go out.
        at("for id in self.long_lived_sub_ids() { self.subscribe_one(id, since).await?; }");
        let started = at("let boot_subs = self.long_lived_sub_ids(); let boot_replay_started = tokio::time::Instant::now(); self.subscribe_all(None).await?;");
        assert!(started < at("schedule::QueueFullEpisode::new(boot_replay_started, &boot_subs)"));
        // Every EOSE and every CLOSED ends that subscription's replay, unguarded.
        at("message: nostr_sdk::RelayMessage::EndOfStoredEvents(eose_id), .. }) => { queue_full.stored_history_ended(eose_id.as_str());");
        at("queue_full.stored_history_ended(&id); let label = subscription_label(&id);");
        // Each event counts against its own subscription.
        at(".after_push( queue.has_room(), subscription_id.as_str(), tokio::time::Instant::now(), )");
        // The level comes from the episode.
        at("match queue_full.full_line(EVENT_QUEUE_CAP) { schedule::LaneLine::Default(line) => opline!(\"{line}\"), schedule::LaneLine::Verbose(line) => { opline_verbose!(\"{line}\") } }");
        // The end is read before the stuck check, on the same turn.
        let take = at("queue_full.after_take(queue.queued_events(), tokio::time::Instant::now())");
        let overdue = at("queue_full.overdue( tokio::time::Instant::now(), queue.queued_events(), queue.has_room(), )");
        assert!(take < overdue, "after_take must run before overdue");
        // Everything that can lose an EOSE or reconnect ends the replay.
        for needle in [
            "if let Some(gap) = loop_turns.turn(std::time::Instant::now(), schedule::wall_ms()) { // After a gap (host sleep included) nothing that arrives is the boot replay. queue_full.end_boot_replay();",
            "if recovering.get() { queue_full.end_boot_replay(); }",
            "if report.recovered.is_some() { // A recovery ran (even one too quick for the loop to see it in flight): // what its resubscribe brings is not the boot replay. queue_full.end_boot_replay();",
            "Err(error) => { // A broadcast lag is recoverable — never go permanently deaf. // A lag may have dropped a boot EOSE. queue_full.end_boot_replay();",
            "Err(error) => { queue_full.end_boot_replay(); opline!( \"seller node RELAY-AUTH RESUBSCRIBE failed",
            "Ok(RelayNotification::AuthenticationFailed) => { queue_full.end_boot_replay();",
            "if status != nostr_sdk::prelude::RelayStatus::Connected => { // A reconnect follows: its resubscribe is not the boot replay. queue_full.end_boot_replay();",
        ] {
            at(needle);
        }
        assert_eq!(src.matches("queue_full.end_boot_replay();").count(), 7);
    }

    /// The status line's relay half names sign-in, the last heartbeat and how the last relay drop
    /// ended, after the unchanged leading phrase.
    use super::*;

    #[test]
    fn relay_health_names_sign_in_heartbeat_and_the_last_drop() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut health = RelayHealth::default();
        assert_eq!(
            health.line(Some(false), false, s(3), t0),
            "relay confirmed 3s ago · NOT signed in (no NIP-42 sign-in on this connection) · no heartbeat landed yet"
        );
        health.beat(true, t0);
        health.beat(false, t0 + s(60));
        assert_eq!(
            health.line(Some(true), false, s(5), t0 + s(90)),
            "relay confirmed 5s ago · signed in · heartbeat landed 90s ago"
        );
        assert_eq!(
            health.line(Some(true), true, s(140), t0 + s(140)),
            "relay RECOVERING (unconfirmed for 140s) · signed in · heartbeat landed 140s ago"
        );
        health.recovery(false, s(150), t0 + s(150));
        assert!(
            health
                .line(Some(false), false, s(160), t0 + s(160))
                .ends_with(" · relay reconnect FAILED 10s ago (150s unconfirmed), retrying"),
        );
        health.recovery(true, s(200), t0 + s(200));
        health.beat(true, t0 + s(200));
        assert_eq!(
            health.line(Some(true), false, s(1), t0 + s(230)),
            "relay confirmed 1s ago · signed in · heartbeat landed 30s ago · last relay drop recovered 30s ago after 200s"
        );
        assert!(
            health
                .line(None, false, s(1), t0 + s(230))
                .contains(" · sign-in unknown (missed relay notifications;"),
        );
    }

    /// advisor r1 of #1438: a failed recovery followed by the SDK's own background reconnect is
    /// reported as recovered, not "FAILED, retrying" forever. A confirmation with no failed drop
    /// pending changes nothing. RED ON REVERT: make `confirmed` a no-op.
    #[test]
    fn a_background_reconnect_ends_a_failed_drop() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut health = RelayHealth::default();
        health.confirmed(s(9), t0);
        assert!(!health.line(Some(true), false, s(1), t0).contains("drop"), "nothing to end");
        health.recovery(false, s(150), t0 + s(150));
        health.confirmed(s(170), t0 + s(170));
        let line = health.line(Some(true), false, s(1), t0 + s(200));
        assert!(line.ends_with(" · last relay drop recovered 30s ago after 170s"), "{line}");
        assert!(!line.contains("FAILED"), "{line}");
        // A later confirmation does not move a drop that already recovered.
        health.confirmed(s(5), t0 + s(300));
        assert!(health.line(Some(true), false, s(1), t0 + s(330)).ends_with("recovered 160s ago after 170s"));
    }

    /// advisor r1 of #1438: the outage counts host sleep (the monotonic clock pauses then; the wall
    /// clock does not). RED ON REVERT: use only `unconfirmed_for` and the slept outage reads 5 s.
    #[test]
    fn a_relay_outage_counts_time_the_host_slept() {
        let t0 = Instant::now();
        let clock = LivenessClock::new(t0, 1_000);
        assert_eq!(relay_outage(&clock, t0 + Duration::from_secs(5), 1_005), Duration::from_secs(5));
        assert_eq!(relay_outage(&clock, t0 + Duration::from_secs(5), 2_205), Duration::from_secs(1_205), "slept 20 min");
        assert_eq!(relay_outage(&clock, t0 + Duration::from_secs(5), 900), Duration::from_secs(5), "wall clock stepped back");
    }


    #[test]
    fn loop_turns_report_nothing_on_a_loop_that_keeps_turning() {
        let t0 = std::time::Instant::now();
        let mut turns = LoopTurns::new(t0, 1_000_000, LOOP_GAP_REPORT);
        for tick in 1..=30u64 {
            let at = t0 + Duration::from_secs(5 * tick);
            assert_eq!(
                turns.turn(at, 1_000_000 + 5_000 * tick as i64),
                None,
                "tick {tick}"
            );
        }
        // Just under the threshold on both clocks: still quiet.
        let at = t0 + Duration::from_secs(150 + 59);
        assert_eq!(turns.turn(at, 1_000_000 + 209_000), None);
    }

    #[test]
    fn loop_turns_report_a_host_sleep_that_only_the_wall_clock_saw() {
        let t0 = std::time::Instant::now();
        let mut turns = LoopTurns::new(t0, 0, LOOP_GAP_REPORT);
        // Monotonic time stood still while asleep: 5 s of it against 20 minutes of wall time.
        let gap = turns
            .turn(t0 + Duration::from_secs(5), 1_205_000)
            .expect("a 20-minute sleep is reported");
        assert_eq!(
            gap,
            LoopGap {
                secs: 1205,
                host_slept: true
            }
        );
        let line = gap.line(true, true);
        assert!(
            line.contains("RESUMED after the host slept for about 1205s"),
            "{line}"
        );
        assert!(
            line.contains("re-announcing the seat and re-reading offers now, and reconnecting"),
            "{line}"
        );
        // The next on-time tick is quiet again: one line per gap.
        assert_eq!(turns.turn(t0 + Duration::from_secs(10), 1_210_000), None);
    }

    #[test]
    fn loop_turns_report_a_stopped_process_or_blocked_loop_on_both_clocks() {
        let t0 = std::time::Instant::now();
        let mut turns = LoopTurns::new(t0, 0, LOOP_GAP_REPORT);
        let gap = turns
            .turn(t0 + Duration::from_secs(600), 600_000)
            .expect("ten minutes without a turn is reported");
        assert_eq!(
            gap,
            LoopGap {
                secs: 600,
                host_slept: false
            }
        );
        let line = gap.line(true, false);
        assert!(line.contains("event loop did not run for 600s"), "{line}");
        assert!(!line.contains("reconnecting"), "{line}");
        assert_eq!(gap.recovery_reason(), "the event loop did not run for 600s");
        // With the seat heartbeat off there is no announcement to renew, and the line says so.
        let quiet = gap.line(false, false);
        assert!(quiet.ends_with("happened meanwhile; re-reading offers now"), "{quiet}");
        let slept = LoopGap { secs: 700, host_slept: true };
        assert_eq!(slept.recovery_reason(), "the host slept for about 700s");
    }

    #[test]
    fn recovery_debt_without_a_running_supervision_beats_now_and_holds_the_read_for_a_reconnect() {
        let mut debt = RecoveryDebt::default();
        // A short gap: beat and re-read at once, nothing owed.
        assert_eq!(debt.gap(false, false), (true, true));
        debt.supervision_started(None);
        assert_eq!(debt.supervision_done(None), DebtDue::default());
        // A reconnect gap: beat at once, but the re-read waits for the forced recovery.
        assert_eq!(debt.gap(false, true), (true, false));
        debt.supervision_started(Some("the host slept for about 700s"));
        assert_eq!(
            debt.supervision_done(Some(true)),
            DebtDue { beat: false, backfill: true, rearm_forced: None }
        );
        // Released once.
        debt.supervision_started(None);
        assert_eq!(debt.supervision_done(None), DebtDue::default());
    }

    #[test]
    fn recovery_debt_owes_the_beat_across_a_supervision_that_was_running_at_the_gap() {
        let mut debt = RecoveryDebt::default();
        // The supervision in flight when the host slept carries no forced recovery.
        debt.supervision_started(None);
        assert_eq!(debt.gap(true, true), (false, false), "the tick is not armed: nothing fires now");
        // It ends (on the dead socket, no recovery): the beat is due now; the re-read is not, since
        // this supervision ran no recovery.
        assert_eq!(
            debt.supervision_done(None),
            DebtDue { beat: true, backfill: false, rearm_forced: None }
        );
        // The beat it fired carries the gap's forced recovery, and that one releases the re-read.
        debt.supervision_started(Some("the host slept for about 700s"));
        assert_eq!(
            debt.supervision_done(Some(true)),
            DebtDue { beat: false, backfill: true, rearm_forced: None }
        );
    }

    #[test]
    fn recovery_debt_rearms_a_forced_recovery_that_failed_until_one_succeeds() {
        let mut debt = RecoveryDebt::default();
        assert_eq!(debt.gap(false, true), (true, false));
        let reason = "the host slept for about 700s";
        debt.supervision_started(Some(reason));
        let due = debt.supervision_done(Some(false));
        assert_eq!(due.rearm_forced.as_deref(), Some(reason), "a failed forced recovery is re-armed");
        assert!(!due.backfill, "no re-read on the still-dead socket");
        assert!(debt.backfill_held(), "backfills stay held until a recovery succeeds");
        // The re-armed attempt succeeds: now the re-read runs, once.
        debt.supervision_started(due.rearm_forced.as_deref());
        assert_eq!(
            debt.supervision_done(Some(true)),
            DebtDue { beat: false, backfill: true, rearm_forced: None }
        );
        assert!(!debt.backfill_held());
        // A failed STALL recovery (not forced) is the watchdog's to retry, not re-armed here.
        debt.supervision_started(None);
        assert_eq!(debt.supervision_done(Some(false)), DebtDue::default());
        // A forced supervision that ended WITHOUT running its recovery keeps it armed (r3 L3).
        debt.supervision_started(Some(reason));
        assert_eq!(debt.supervision_done(None).rearm_forced.as_deref(), Some(reason));
    }

    #[test]
    fn recovery_debt_a_second_reconnect_gap_supersedes_the_recovery_already_running() {
        let mut debt = RecoveryDebt::default();
        assert_eq!(debt.gap(false, true), (true, false));
        assert!(debt.backfill_held());
        debt.supervision_started(Some("gap 1"));
        // A second reconnect gap while gap 1's recovery runs: that recovery cannot release it.
        assert_eq!(debt.gap(true, true), (false, false));
        // A short gap meanwhile does not sneak a backfill in either.
        assert_eq!(debt.gap(true, false), (false, false));
        assert_eq!(
            debt.supervision_done(Some(true)),
            DebtDue { beat: true, backfill: false, rearm_forced: None }
        );
        assert!(debt.backfill_held(), "gap 2's recovery has not run yet");
        // The beat that fired carries gap 2's forced recovery; its success releases the read.
        debt.supervision_started(Some("gap 2"));
        assert_eq!(
            debt.supervision_done(Some(true)),
            DebtDue { beat: false, backfill: true, rearm_forced: None }
        );
        assert!(!debt.backfill_held());
    }

    #[test]
    fn loop_watchdog_stops_promptly_when_its_guard_drops() {
        let pulse = LoopPulse::new();
        pulse.beat();
        let guard = spawn_loop_watchdog(std::sync::Arc::clone(&pulse), LOOP_GAP_REPORT);
        assert!(guard.thread.is_some(), "harness check: the watchdog thread started");
        let started = std::time::Instant::now();
        drop(guard);
        assert!(started.elapsed() < Duration::from_millis(500), "the drop itself never waits");
        // The thread is woken, not left to finish its sleep: it releases the pulse at once.
        while std::sync::Arc::strong_count(&pulse) > 1 {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the woken thread ends, not a {}s sleep later",
                LOOP_WATCH_EVERY.as_secs()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn loop_turns_survive_a_wall_clock_stepped_backwards() {
        let t0 = std::time::Instant::now();
        let mut turns = LoopTurns::new(t0, 10_000_000, LOOP_GAP_REPORT);
        assert_eq!(
            turns.turn(t0 + Duration::from_secs(5), 9_000_000),
            None,
            "a backwards step is no gap"
        );
        assert_eq!(
            turns.turn(t0 + Duration::from_secs(10), 9_005_000),
            None,
            "and re-anchors on it"
        );
    }

    #[test]
    fn loop_watch_warns_once_per_stall_and_not_for_a_paused_process() {
        let warn = Duration::from_secs(60);
        let mut watch = LoopWatch::default();
        assert_eq!(
            watch.check(Duration::from_secs(59), Duration::ZERO, warn),
            None
        );
        let line = watch
            .check(Duration::from_secs(61), Duration::ZERO, warn)
            .expect("a blocked loop is reported");
        assert!(
            line.contains("event loop BLOCKED") && line.contains("for 61s"),
            "{line}"
        );
        assert_eq!(
            watch.check(Duration::from_secs(120), Duration::ZERO, warn),
            None,
            "once per stall"
        );
        // The loop turned again: a later stall is a new one.
        assert_eq!(
            watch.check(Duration::from_secs(1), Duration::ZERO, warn),
            None
        );
        assert!(
            watch
                .check(Duration::from_secs(70), Duration::ZERO, warn)
                .is_some(),
            "a new stall warns"
        );
        // This thread woke a long time late: the whole process was not running, not a blocked loop.
        let mut watch = LoopWatch::default();
        assert_eq!(
            watch.check(Duration::from_secs(900), Duration::from_secs(890), warn),
            None
        );
    }

    #[test]
    fn announcement_gap_warns_once_at_one_and_a_half_intervals_and_reports_the_renewal() {
        let t0 = Instant::now();
        let interval = Duration::from_secs(300);
        let mut gap = AnnouncementGap::new(t0);
        assert_eq!(gap.check(t0 + Duration::from_secs(449), interval, None), None, "inside 1.5 intervals");
        let line = gap
            .check(t0 + Duration::from_secs(450), interval, None)
            .expect("warns at 1.5 intervals");
        assert!(line.contains("OVERDUE") && line.contains("for 450s") && line.contains("interval 300s"), "{line}");
        assert!(line.contains("no supervision is running"), "{line}");
        assert_eq!(gap.check(t0 + Duration::from_secs(900), interval, None), None, "one line per gap");
        // A failed beat neither clears the warning nor logs a renewal.
        assert_eq!(gap.on_publish(false, t0 + Duration::from_secs(901)), None);
        assert_eq!(gap.check(t0 + Duration::from_secs(902), interval, None), None);
        let renewed = gap.on_publish(true, t0 + Duration::from_secs(1000)).expect("renewal ends the gap");
        assert!(renewed.contains("after 1000s"), "{renewed}");
        // The clock restarted at the renewal: the next gap is measured from it and warns again.
        assert_eq!(gap.check(t0 + Duration::from_secs(1449), interval, None), None);
        assert!(gap.check(t0 + Duration::from_secs(1450), interval, None).is_some(), "a new gap warns again");
    }

    #[test]
    fn announcement_gap_names_a_supervision_that_has_not_returned() {
        let t0 = Instant::now();
        let mut gap = AnnouncementGap::new(t0);
        let line = gap
            .check(t0 + Duration::from_secs(3), Duration::from_secs(2), Some(Duration::from_secs(3)))
            .expect("overdue");
        assert!(line.contains("a heartbeat supervision has been running for 3s"), "{line}");
    }

    #[test]
    fn announcement_gap_landed_beats_never_warn() {
        let t0 = Instant::now();
        let interval = Duration::from_secs(300);
        let mut gap = AnnouncementGap::new(t0);
        for n in 1..=10u64 {
            let at = t0 + Duration::from_secs(300 * n);
            assert_eq!(gap.check(at, interval, None), None, "beat {n} is on time");
            assert_eq!(gap.on_publish(true, at), None, "no renewal line without a warning");
        }
    }

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

        // A redeem that finishes at ~5 s: the money bound ends with it (detected within one poll),
        // and the work still running gets one more grace — abandoned at ~8 s, not 60 s — reported
        // as not redeeming.
        let short = std::sync::Arc::new(AtomicUsize::new(1));
        let flag = std::sync::Arc::clone(&short);
        let finisher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5_100)).await;
            flag.fetch_sub(1, Ordering::SeqCst);
        });
        let Settled::Abandoned { after, redeeming } = settle(&mut Never, &short).await else {
            panic!("work that never finishes is abandoned");
        };
        assert!(!redeeming, "the redeem had finished when the work was abandoned");
        let earliest = Duration::from_millis(5_100) + LANE_SETTLE_GRACE;
        assert!(
            after >= earliest && after <= earliest + SETTLE_REDEEM_POLL,
            "abandoned {after:?} after start; expected one grace after the redeem ended"
        );
        finisher.await.expect("finisher task");

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

    #[test]
    fn a_backfill_cut_off_by_a_recovery_reruns_without_growing_the_backoff() {
        let min = Duration::from_secs(30);
        let mut backoff = RetryBackoff::new(min, Duration::from_secs(300));
        let started = Instant::now();
        let ended_after = started + Duration::from_secs(2);

        // A recovery reconnecting now, or one that ended after the pass began: re-run.
        assert!(overlapped_recovery(started, true, None));
        assert!(overlapped_recovery(started, false, Some(ended_after)));
        assert!(overlapped_recovery(started, false, Some(started)));
        // One that ended before the pass began is unrelated.
        assert!(!overlapped_recovery(ended_after, false, Some(started)));
        assert!(!overlapped_recovery(started, false, None));

        assert_eq!(after_backfill(false, true, &mut backoff), BackfillNext::RerunAfterRecovery);
        assert_eq!(after_backfill(false, true, &mut backoff), BackfillNext::RerunAfterRecovery);
        // The re-runs did not consume backoff steps: a genuine failure still starts at the minimum.
        assert_eq!(after_backfill(false, false, &mut backoff), BackfillNext::RetryAfter(min));
        assert_eq!(after_backfill(false, false, &mut backoff), BackfillNext::RetryAfter(min * 2));
        // A complete pass resets it.
        assert_eq!(after_backfill(true, false, &mut backoff), BackfillNext::Done);
        assert_eq!(after_backfill(false, false, &mut backoff), BackfillNext::RetryAfter(min));
        // Complete wins even when a recovery overlapped.
        assert_eq!(after_backfill(true, true, &mut backoff), BackfillNext::Done);
    }

}
