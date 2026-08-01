//! The relay roster: which relays this node participates on, and what access it has proven on
//! each.
//!
//! The hard problem this solves is that **an access-scoped relay looks exactly like an empty one**.
//! Buzz silently narrows a global REQ to the channels the asker may see, so a healthy authenticated
//! connection with no admission returns `EOSE` and nothing else — byte-identical to a relay that
//! genuinely holds no matching events. Read-silence therefore cannot classify anything.
//!
//! So classification is **positive only**: a relay is admitted when we have published an event to
//! it and read that same event back by id. Nothing else promotes. Everything weaker is recorded as
//! the weaker thing.
//!
//! The probe rides the presence heartbeat the node already publishes rather than posting anything
//! of its own — which is what makes the reactive-only invariant structural here instead of a rule
//! someone has to remember. See [`ProbeOutcome`].
//!
//! (Naming note: `seller_node::roster` is the *harness* roster — which agents this node can run.
//! This is the relay roster. They are unrelated and must not be conflated.)

use std::collections::BTreeMap;

/// What access we have PROVEN on a relay. The first three are a ladder — each one is strictly more
/// than the one before, and only the top of it means we can actually participate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessState {
    /// Never probed, or a probe is owed after new signal. The starting state and the only one a
    /// probe may be launched from.
    Unprobed,
    /// The relay answers reads without NIP-42. Says nothing about whether it will answer OURS —
    /// p-gated kinds still require authentication.
    Open,
    /// The NIP-42 handshake completed. ★ Not admission: the deployed relay authenticates a plain
    /// keypair happily and then serves it nothing, because authentication proves who we are and
    /// admission is a separate grant.
    Authed,
    /// We published an event here and read it back. The only state that licenses real work.
    Admitted,
    /// A probe went out and nothing came back, and we do not know why — the wire ate it, the relay
    /// was slow through the handshake, the socket died mid-flight. Not usable, because admission is
    /// unproven and no frames may follow; but NOT a refusal, because the relay never said no. It
    /// earns another probe once `retry_after_unix` has passed, up to [`MAX_PROBE_ATTEMPTS`].
    ///
    /// ★ This state exists because silence and refusal are different facts about a relay. They
    /// shared [`AccessState::Denied`] for its no-frames property, and TERMINALITY came along
    /// uninvited: the only exit from `Denied` fires on inbound traffic from that relay, but a probe
    /// that timed out has already had its reader disconnected — so recovery depended on the very
    /// transport that failed, and a briefly-slow relay was lost until process restart.
    Quarantined {
        reason: String,
        retry_after_unix: i64,
    },
    /// The relay REFUSED us — auth rejected, admission denied, connection unusable — or a
    /// quarantined relay exhausted [`MAX_PROBE_ATTEMPTS`] without ever proving admission. Terminal
    /// until new signal arrives: nothing is sent to a denied relay, and no timer resurrects it,
    /// because polling a relay that told us no is the one thing the charter forbids.
    Denied { reason: String },
}

impl AccessState {
    /// Whether the node may send frames — REQ, publish, anything — to this relay.
    ///
    /// Deliberately strict. `Unprobed` is not usable (the probe itself is the exception, and it
    /// goes through [`RelayRoster::relays_to_probe`]); `Quarantined` and `Denied` are not usable at
    /// all.
    ///
    /// ★ UNCHANGED by the quarantine split, and that is the point: admission stays the ONE thing
    /// that licenses a frame, so adding a retryable state cannot widen what may be addressed.
    pub fn is_usable(&self) -> bool {
        matches!(self, AccessState::Admitted)
    }

    /// A stable lowercase label for logs, config and the store. Round-trips through
    /// [`AccessState::from_label`] for everything except the denial reason, which is prose.
    pub fn label(&self) -> &'static str {
        match self {
            AccessState::Unprobed => "unprobed",
            AccessState::Open => "open",
            AccessState::Authed => "authed",
            AccessState::Admitted => "admitted",
            AccessState::Quarantined { .. } => "quarantined",
            AccessState::Denied { .. } => "denied",
        }
    }
}

/// The result of one probe attempt against one relay.
///
/// ★ There is no `NothingCameBack ⇒ empty relay` variant, and that absence is the point: the probe
/// asks a question the relay cannot answer with silence. Either our own event comes back, or we
/// learned nothing about admission and must not claim we did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// We published (the presence beat the node was going to send anyway) and read that exact
    /// event back by id. Positive proof of admission.
    EchoObserved,
    /// NIP-42 completed but the echo has not been seen. Authenticated, not admitted.
    Authenticated,
    /// The relay serves reads unauthenticated. Weakest useful signal.
    OpenRead,
    /// We published and the echo never arrived within the probe window. Not a claim that the relay
    /// is empty or broken — a claim that admission is UNPROVEN, which is the only thing we may act
    /// on. Treated as denial so that no frames follow.
    EchoMissing,
    /// The relay refused us outright — auth rejected, admission denied.
    Refused(String),
    /// The socket never opened at all, so the relay had no chance to say anything.
    ///
    /// ★ NOT a refusal, and recording it as one was the SAME conflation as `EchoMissing` — one step
    /// earlier in the sequence. A relay that is briefly unreachable has not denied us access; it has
    /// told us nothing, which is exactly the fact a bounded retry is for. Mapping this to `Refused`
    /// meant a relay that was down for one attempt was written off until process restart.
    Unreachable(String),
}

/// One relay's entry in the roster.
#[derive(Debug, Clone)]
pub struct RelayEntry {
    /// Websocket URL, as configured. The roster key.
    pub url: String,
    pub state: AccessState,
    /// When the last probe resolved. `None` ⇒ never probed.
    pub last_probe_unix: Option<i64>,
    /// How many probes this relay has cost us. Bounded by [`MAX_PROBE_ATTEMPTS`] so an endpoint
    /// that neither refuses nor echoes cannot be probed forever.
    pub probe_attempts: u32,
}

/// How many times a relay may be probed before it is left alone.
///
/// A relay that accepts a connection and then does nothing would otherwise absorb probes
/// indefinitely: each attempt looks recoverable in isolation. Three is enough to ride out a
/// transient, and small enough that the cost is bounded.
pub const MAX_PROBE_ATTEMPTS: u32 = 3;

/// The first quarantine wait, doubled per failed probe: 30s, then 60s, across
/// [`MAX_PROBE_ATTEMPTS`].
///
/// Long enough that a relay still failing for the same reason — a slow NIP-42 handshake, a transient
/// network fault — is not re-probed while it is still in that state; short enough that a seller does
/// not sit deaf for minutes on a relay that has already come back.
pub const QUARANTINE_BACKOFF_BASE_SECS: i64 = 30;

/// The wait owed after `attempts` failed probes: `BASE * 2^(attempts-1)`.
///
/// `attempts` is the count AFTER the failed probe has been tallied, so the first quarantine waits
/// exactly `BASE`. The shift is capped because a shift wider than the type is undefined and panics
/// in debug; capping is harmless here because [`MAX_PROBE_ATTEMPTS`] ends the sequence far below it.
fn quarantine_backoff_secs(attempts: u32) -> i64 {
    let doublings = attempts.saturating_sub(1).min(16);
    QUARANTINE_BACKOFF_BASE_SECS.saturating_mul(1i64 << doublings)
}

/// The relays this node participates on and the access proven on each.
#[derive(Debug, Clone, Default)]
pub struct RelayRoster {
    entries: BTreeMap<String, RelayEntry>,
}

impl RelayRoster {
    /// Build a roster from configured URLs. Every relay starts [`AccessState::Unprobed`] —
    /// including one the node has used before, because access is a property of the relay's current
    /// grant to us, not of our history with it.
    pub fn new(urls: impl IntoIterator<Item = String>) -> Self {
        let entries = urls
            .into_iter()
            .map(|url| {
                (
                    url.clone(),
                    RelayEntry {
                        url,
                        state: AccessState::Unprobed,
                        last_probe_unix: None,
                        probe_attempts: 0,
                    },
                )
            })
            .collect();
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, url: &str) -> Option<&RelayEntry> {
        self.entries.get(url)
    }

    pub fn entries(&self) -> impl Iterator<Item = &RelayEntry> {
        self.entries.values()
    }

    /// The relays the node may work on. Everything else — unprobed, authed-but-unadmitted, denied
    /// — is excluded, so a caller that iterates this cannot accidentally address a relay we have
    /// not proven.
    pub fn usable(&self) -> impl Iterator<Item = &RelayEntry> {
        self.entries
            .values()
            .filter(|entry| entry.state.is_usable())
    }

    /// The relays a probe is owed on: unprobed, or quarantined with its backoff elapsed, and in
    /// either case not yet out of attempts.
    ///
    /// ★ This is the ONLY route by which a frame reaches a relay that is not `Admitted`, and it
    /// cannot return a denied one. That is what makes "zero frames to a denied relay" a property
    /// of the type rather than a discipline.
    ///
    /// Takes `now_unix` because a quarantined relay is owed a probe only once it is due — without a
    /// clock this could not tell "resting" from "ready" and would have to re-probe immediately,
    /// which is the busy-poll the backoff exists to prevent.
    pub fn relays_to_probe(&self, now_unix: i64) -> impl Iterator<Item = &RelayEntry> {
        self.entries.values().filter(move |entry| {
            entry.probe_attempts < MAX_PROBE_ATTEMPTS
                && match &entry.state {
                    AccessState::Unprobed => true,
                    AccessState::Quarantined {
                        retry_after_unix, ..
                    } => *retry_after_unix <= now_unix,
                    AccessState::Open
                    | AccessState::Authed
                    | AccessState::Admitted
                    | AccessState::Denied { .. } => false,
                }
        })
    }

    /// The state a relay lands in when it told us NOTHING: quarantined with a backoff while attempts
    /// remain, denied once they are spent.
    ///
    /// ★ Shared by BOTH silences — no socket, and a socket that swallowed the probe — so the two can
    /// never drift into different verdicts for the same fact. `attempts` is the count AFTER this
    /// probe was tallied.
    fn silence_state(attempts: u32, now_unix: i64, reason: &str) -> AccessState {
        if attempts >= MAX_PROBE_ATTEMPTS {
            AccessState::Denied {
                reason: format!("admission unproven after {MAX_PROBE_ATTEMPTS} probes: {reason}"),
            }
        } else {
            AccessState::Quarantined {
                reason: reason.to_owned(),
                retry_after_unix: now_unix.saturating_add(quarantine_backoff_secs(attempts)),
            }
        }
    }

    /// Fold a probe result into the roster.
    ///
    /// Promotion is monotonic within one probe cycle but never sticky across signal: a relay that
    /// echoed once and later refuses is denied, because the refusal is the newer fact.
    pub fn record_probe(&mut self, url: &str, outcome: ProbeOutcome, now_unix: i64) {
        let Some(entry) = self.entries.get_mut(url) else {
            return;
        };
        entry.last_probe_unix = Some(now_unix);
        entry.probe_attempts = entry.probe_attempts.saturating_add(1);
        let attempts = entry.probe_attempts;
        entry.state = match outcome {
            ProbeOutcome::EchoObserved => AccessState::Admitted,
            ProbeOutcome::Authenticated => AccessState::Authed,
            ProbeOutcome::OpenRead => AccessState::Open,
            // The relay said no. Terminal, and the charter forbids polling it.
            ProbeOutcome::Refused(reason) => AccessState::Denied { reason },
            // The relay said NOTHING — either the socket never opened, or it opened and swallowed the
            // probe. Different moments, one fact: admission is unproven and nobody refused us. A
            // bounded retry first, denial only once the attempts are spent. Both silences go through
            // ONE function so they cannot drift apart later.
            ProbeOutcome::EchoMissing => Self::silence_state(
                attempts,
                now_unix,
                "published but never read the event back — admission unproven",
            ),
            ProbeOutcome::Unreachable(error) => Self::silence_state(
                attempts,
                now_unix,
                &format!("could not open a socket — admission unproven ({error})"),
            ),
        };
    }

    /// Re-open a relay for probing because something new happened: an inbound event arrived from
    /// it, an admission was granted out of band, or the operator changed the config.
    ///
    /// This is the ONLY way out of [`AccessState::Denied`], and it is deliberately not a timer.
    /// Polling a relay that told us no is the exact behaviour the charter forbids; a relay that
    /// changes its mind will say so, and saying so is the signal.
    pub fn note_new_signal(&mut self, url: &str) {
        let Some(entry) = self.entries.get_mut(url) else {
            return;
        };
        // Quarantine is included, and it short-circuits the backoff on purpose: the wait exists
        // because we had no evidence about the transport, and inbound traffic IS that evidence. A
        // fresh attempt budget follows for the same reason it does after denial — the relay has
        // demonstrably just spoken to us.
        if matches!(
            entry.state,
            AccessState::Denied { .. } | AccessState::Quarantined { .. }
        ) {
            entry.state = AccessState::Unprobed;
            entry.probe_attempts = 0;
        }
    }

    /// Apply a fresh config: add relays that appeared, drop relays that left, and leave the state
    /// of relays present in both untouched (re-probing a relay we already proved would cost a
    /// round trip to learn what we know).
    pub fn reconcile(&mut self, urls: impl IntoIterator<Item = String>) {
        let desired: Vec<String> = urls.into_iter().collect();
        self.entries.retain(|url, _| desired.contains(url));
        for url in desired {
            self.entries.entry(url.clone()).or_insert(RelayEntry {
                url,
                state: AccessState::Unprobed,
                last_probe_unix: None,
                probe_attempts: 0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> RelayRoster {
        RelayRoster::new(["wss://a.example".to_string(), "wss://b.example".to_string()])
    }

    #[test]
    fn only_reading_our_own_event_back_counts_as_admission() {
        let mut roster = roster();
        roster.record_probe("wss://a.example", ProbeOutcome::EchoObserved, 100);
        roster.record_probe("wss://b.example", ProbeOutcome::Authenticated, 100);

        assert_eq!(
            roster.get("wss://a.example").unwrap().state,
            AccessState::Admitted
        );
        // Authenticated is NOT admitted — the deployed relay authenticates a plain keypair and
        // still serves it nothing.
        assert_eq!(
            roster.get("wss://b.example").unwrap().state,
            AccessState::Authed
        );
        let usable: Vec<_> = roster.usable().map(|entry| entry.url.as_str()).collect();
        assert_eq!(usable, ["wss://a.example"]);
    }

    #[test]
    fn a_missing_echo_quarantines_rather_than_reading_as_an_empty_relay() {
        let mut roster = roster();
        roster.record_probe("wss://a.example", ProbeOutcome::EchoMissing, 100);
        // ★ RENAMED, NOT REPURPOSED. The property this test has always protected is intact: a
        // missing echo must NEVER read as "the relay is fine, it just had nothing for us". Only the
        // state it lands in changed — silence now earns a bounded retry instead of the refusal
        // verdict it was borrowing.
        assert!(matches!(
            roster.get("wss://a.example").unwrap().state,
            AccessState::Quarantined { .. }
        ));
        assert_eq!(
            roster.usable().count(),
            0,
            "unproven admission may never be addressed, quarantined or not"
        );
    }

    #[test]
    fn a_denied_relay_is_never_probed_or_addressed_again_on_its_own() {
        let mut roster = roster();
        roster.record_probe(
            "wss://a.example",
            ProbeOutcome::Refused("insufficient-scope".into()),
            100,
        );

        // Neither work nor probes may reach it: these two iterators are the only ways to obtain a
        // relay to send to, and neither yields it.
        assert!(!roster.usable().any(|entry| entry.url == "wss://a.example"));
        assert!(
            !roster
                .relays_to_probe(100)
                .any(|entry| entry.url == "wss://a.example")
        );

        // And no amount of elapsed time changes that — a refusal has no backoff to expire.
        //
        // ★ This assertion USED TO BE A DUPLICATE OF THE ONE ABOVE. The comment claimed a
        // time-invariance property, but `relays_to_probe` took no clock, so the test had no way to
        // vary the only input the claim was about. It now passes the furthest clock there is, which
        // is what the comment always meant.
        assert!(
            !roster
                .relays_to_probe(i64::MAX)
                .any(|entry| entry.url == "wss://a.example")
        );
    }

    #[test]
    fn new_signal_is_the_only_thing_that_reopens_a_denied_relay() {
        let mut roster = roster();
        roster.record_probe("wss://a.example", ProbeOutcome::Refused("nope".into()), 100);
        assert_eq!(roster.relays_to_probe(100).count(), 1); // only b

        roster.note_new_signal("wss://a.example");

        let probeable: Vec<_> = roster
            .relays_to_probe(100)
            .map(|entry| entry.url.as_str())
            .collect();
        assert_eq!(probeable, ["wss://a.example", "wss://b.example"]);
        // The attempt budget resets with it, or one old failure would spend the new chance.
        assert_eq!(roster.get("wss://a.example").unwrap().probe_attempts, 0);
    }

    #[test]
    fn new_signal_does_not_demote_a_relay_that_is_already_working() {
        let mut roster = roster();
        roster.record_probe("wss://a.example", ProbeOutcome::EchoObserved, 100);
        roster.note_new_signal("wss://a.example");
        assert_eq!(
            roster.get("wss://a.example").unwrap().state,
            AccessState::Admitted
        );
    }

    #[test]
    fn a_relay_that_never_answers_stops_costing_probes() {
        let mut roster = RelayRoster::new(["wss://a.example".to_string()]);
        for attempt in 0..MAX_PROBE_ATTEMPTS {
            assert_eq!(
                roster.relays_to_probe(100).count(),
                1,
                "attempt {attempt} should still be owed"
            );
            // A probe that resolves to nothing conclusive leaves it Unprobed-but-spent.
            roster.record_probe("wss://a.example", ProbeOutcome::OpenRead, 100);
            roster.entries.get_mut("wss://a.example").unwrap().state = AccessState::Unprobed;
        }
        assert_eq!(roster.relays_to_probe(100).count(), 0);
    }

    #[test]
    fn a_refusal_still_denies_immediately_and_is_never_quarantined() {
        let mut roster = roster();
        let url = "wss://a.example";
        roster.record_probe(url, ProbeOutcome::Refused("auth rejected".into()), 100);
        assert!(
            matches!(roster.get(url).unwrap().state, AccessState::Denied { .. }),
            "a relay that SAID NO is denied on its first word — the charter forbids polling it, and \
             the quarantine split must not have leaked into this path"
        );
        assert!(
            !roster.relays_to_probe(i64::MAX).any(|e| e.url == url),
            "no clock, however far forward, reopens a refusal"
        );
    }

    #[test]
    fn a_quarantined_relay_is_probed_again_only_once_its_backoff_has_passed() {
        let mut roster = roster();
        let url = "wss://a.example";
        roster.record_probe(url, ProbeOutcome::EchoMissing, 1_000);
        let due = match &roster.get(url).unwrap().state {
            AccessState::Quarantined {
                retry_after_unix, ..
            } => *retry_after_unix,
            other => panic!("expected a quarantine, got {other:?}"),
        };
        assert_eq!(
            due,
            1_000 + QUARANTINE_BACKOFF_BASE_SECS,
            "the first quarantine waits exactly the base interval"
        );
        // A DISCRIMINATING PAIR, not one assertion: one tick before due proves it is resting, and
        // due itself proves it wakes. Either alone would pass for a roster that never probes at all.
        assert!(
            !roster.relays_to_probe(due - 1).any(|e| e.url == url),
            "still resting — a backoff that does not hold is a busy-poll"
        );
        assert!(
            roster.relays_to_probe(due).any(|e| e.url == url),
            "due, so owed a probe"
        );

        // ★ THE FLOOR. Without this the state machine could quarantine correctly and still have no
        // way home, which is the half-fix the ceiling alone would hide.
        roster.record_probe(url, ProbeOutcome::EchoObserved, due);
        assert!(matches!(
            roster.get(url).unwrap().state,
            AccessState::Admitted
        ));
        assert_eq!(
            roster.usable().count(),
            1,
            "a relay that came back is usable again"
        );
    }

    #[test]
    fn quarantine_ends_in_denial_once_the_attempts_are_spent() {
        let mut roster = roster();
        let url = "wss://a.example";
        for attempt in 1..MAX_PROBE_ATTEMPTS {
            roster.record_probe(url, ProbeOutcome::EchoMissing, 1_000);
            assert!(
                matches!(
                    roster.get(url).unwrap().state,
                    AccessState::Quarantined { .. }
                ),
                "attempt {attempt} of {MAX_PROBE_ATTEMPTS} must still be retryable"
            );
        }
        roster.record_probe(url, ProbeOutcome::EchoMissing, 1_000);
        assert!(
            matches!(roster.get(url).unwrap().state, AccessState::Denied { .. }),
            "★ THE CEILING: a relay that never echoes cannot be probed forever"
        );
        assert!(
            !roster.relays_to_probe(i64::MAX).any(|e| e.url == url),
            "and it is the spent ATTEMPTS that end it, not a clock that has not arrived yet"
        );
    }

    #[test]
    fn the_quarantine_backoff_doubles_per_failed_probe() {
        assert_eq!(quarantine_backoff_secs(1), QUARANTINE_BACKOFF_BASE_SECS);
        assert_eq!(quarantine_backoff_secs(2), QUARANTINE_BACKOFF_BASE_SECS * 2);
        assert_eq!(quarantine_backoff_secs(3), QUARANTINE_BACKOFF_BASE_SECS * 4);
        // An absurd count must neither shift past the type nor come back as a wait in the past,
        // which would busy-probe the relay the backoff exists to rest.
        assert!(quarantine_backoff_secs(u32::MAX) > 0);
    }

    #[test]
    fn new_signal_short_circuits_a_quarantine_backoff() {
        let mut roster = roster();
        let url = "wss://a.example";
        roster.record_probe(url, ProbeOutcome::EchoMissing, 1_000);
        assert!(
            !roster.relays_to_probe(1_000).any(|e| e.url == url),
            "resting, before any signal arrives"
        );
        roster.note_new_signal(url);
        assert!(
            roster.relays_to_probe(1_000).any(|e| e.url == url),
            "inbound traffic IS the transport evidence the backoff was waiting for, so the wait ends"
        );
    }

    #[test]
    fn a_later_refusal_overrides_an_earlier_admission() {
        let mut roster = roster();
        roster.record_probe("wss://a.example", ProbeOutcome::EchoObserved, 100);
        roster.record_probe(
            "wss://a.example",
            ProbeOutcome::Refused("access revoked".into()),
            200,
        );
        assert!(matches!(
            roster.get("wss://a.example").unwrap().state,
            AccessState::Denied { .. }
        ));
    }

    #[test]
    fn reconcile_adds_and_drops_without_forgetting_what_was_proven() {
        let mut roster = roster();
        roster.record_probe("wss://a.example", ProbeOutcome::EchoObserved, 100);

        roster.reconcile(["wss://a.example".to_string(), "wss://c.example".to_string()]);

        assert_eq!(
            roster.get("wss://a.example").unwrap().state,
            AccessState::Admitted,
            "a relay present in both configs keeps its proven access"
        );
        assert!(roster.get("wss://b.example").is_none());
        assert_eq!(
            roster.get("wss://c.example").unwrap().state,
            AccessState::Unprobed
        );
    }
}
