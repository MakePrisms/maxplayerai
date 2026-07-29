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
    /// The relay refused us, or the probe proved we cannot confirm admission. Terminal until new
    /// signal arrives: nothing is sent to a denied relay, and no timer resurrects it.
    Denied { reason: String },
}

impl AccessState {
    /// Whether the node may send frames — REQ, publish, anything — to this relay.
    ///
    /// Deliberately strict. `Unprobed` is not usable (the probe itself is the exception, and it
    /// goes through [`RelayRoster::relays_to_probe`]); `Denied` is not usable at all.
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
    /// The relay refused us outright — auth rejected, admission denied, connection unusable.
    Refused(String),
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

    /// The relays a probe is owed on: unprobed, and not yet out of attempts.
    ///
    /// ★ This is the ONLY route by which a frame reaches a relay that is not `Admitted`, and it
    /// cannot return a denied one. That is what makes "zero frames to a denied relay" a property
    /// of the type rather than a discipline.
    pub fn relays_to_probe(&self) -> impl Iterator<Item = &RelayEntry> {
        self.entries.values().filter(|entry| {
            matches!(entry.state, AccessState::Unprobed)
                && entry.probe_attempts < MAX_PROBE_ATTEMPTS
        })
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
        entry.state = match outcome {
            ProbeOutcome::EchoObserved => AccessState::Admitted,
            ProbeOutcome::Authenticated => AccessState::Authed,
            ProbeOutcome::OpenRead => AccessState::Open,
            ProbeOutcome::Refused(reason) => AccessState::Denied { reason },
            ProbeOutcome::EchoMissing => AccessState::Denied {
                reason: "published but never read the event back — admission unproven".into(),
            },
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
        if matches!(entry.state, AccessState::Denied { .. }) {
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
    fn a_missing_echo_denies_rather_than_reading_as_an_empty_relay() {
        let mut roster = roster();
        roster.record_probe("wss://a.example", ProbeOutcome::EchoMissing, 100);
        assert!(matches!(
            roster.get("wss://a.example").unwrap().state,
            AccessState::Denied { .. }
        ));
        assert_eq!(roster.usable().count(), 0);
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
                .relays_to_probe()
                .any(|entry| entry.url == "wss://a.example")
        );

        // And no amount of elapsed time changes that — there is no backoff to expire.
        assert!(
            !roster
                .relays_to_probe()
                .any(|entry| entry.url == "wss://a.example")
        );
    }

    #[test]
    fn new_signal_is_the_only_thing_that_reopens_a_denied_relay() {
        let mut roster = roster();
        roster.record_probe("wss://a.example", ProbeOutcome::Refused("nope".into()), 100);
        assert_eq!(roster.relays_to_probe().count(), 1); // only b

        roster.note_new_signal("wss://a.example");

        let probeable: Vec<_> = roster
            .relays_to_probe()
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
                roster.relays_to_probe().count(),
                1,
                "attempt {attempt} should still be owed"
            );
            // A probe that resolves to nothing conclusive leaves it Unprobed-but-spent.
            roster.record_probe("wss://a.example", ProbeOutcome::OpenRead, 100);
            roster.entries.get_mut("wss://a.example").unwrap().state = AccessState::Unprobed;
        }
        assert_eq!(roster.relays_to_probe().count(), 0);
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
