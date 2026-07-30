//! Which of a node's resolved harnesses are serving RIGHT NOW.
//!
//! [`crate::seller_agents::AgentRegistry`] answers what a node *resolved at boot*: a pure value,
//! fixed for the life of the process. This module answers the live question on top of it. A harness
//! that boots perfectly can still be unable to work — its account is at a spend limit, its provider
//! is unconfigured, the binary lacks the `acp` feature — and a node that keeps claiming for such a
//! harness is a black hole for awards: it takes work it cannot deliver, and under award-is-payment
//! the buyer has already committed the sats by the time we find out.
//!
//! [`LiveRoster`] owns the registry and the live availability state TOGETHER, and exposes the same
//! surface the node already reads (`advertised` / `dispatch` / `serves`). That is deliberate. The
//! registry's contract is *"advertised is exactly what dispatch serves"*; a separate health object
//! placed alongside it would leave the node free to read `advertised()` unfiltered and publish a
//! harness it would then refuse. Wrapping keeps the invariant true by construction instead of by
//! every caller remembering a second lookup.
//!
//! State is keyed by a harness's INDEX in the registry, not by its name, so the unlabelled
//! `--agent-argv` hatch is droppable like any other entry. A name-keyed map could not drop it at all
//! — the hatch has no name to key on — which would leave exactly one configuration permanently
//! unable to stop claiming: the black hole this module exists to close.
//!
//! "Index" and not "slot": in this node a SLOT is a unit of execution concurrency (`[seller] slots`,
//! and the permits `SlotGate` hands out). A harness's position in the registry is a different thing
//! entirely, and the two are independent — every execution slot can run any serving harness.
//!
//! ## Three states, because `CANNOT RUN` is not `FAILED`
//!
//! - **Serving** — advertised and dispatchable.
//! - **[`Unavailable::Dropped`]** — it failed in a way we cannot attribute. Transient (a timeout) and
//!   structural (a provider that will never resolve) arrive here IDENTICALLY, so this state does not
//!   guess: it stops claiming and schedules a self-probe to find out.
//! - **[`Unavailable::Incapable`]** — a NAMED missing capability. No probe is scheduled, because no
//!   number of retries adds a build feature or picks a provider. The state carries the capability and
//!   DERIVES the remedy from it ([`MissingCapability::remedy`]).
//!
//! Collapsing the last two is the defect this shape exists to prevent. A seat whose binary lacks
//! `acp` can never pass a probe, so a two-state design drops it on its first offer and never restores
//! it — silently converting a fixable *build* problem into a permanent roster hole, with no signal
//! saying which of the two it was.
//!
//! ## The backoff window gates the PROBE, never the restoration
//!
//! [`Unavailable::Dropped::probe_due_at`] is when a self-probe becomes DUE. It is not an expiry:
//! nothing returns to service when it passes, and [`LiveRoster::dispatch`] never auto-restores. Only
//! [`LiveRoster::restore`], called after a probe actually passed, puts a harness back.
//!
//! That distinction is load-bearing under award-is-payment. The sats for a job are committed at
//! AWARD, so a backoff that merely expired would make the next real offer our diagnostic and a
//! BUYER would pay for it. The probe is ours to run and ours to pay for.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::seller_agents::{normalize_request, AgentRegistry, RegisteredAgent};

/// Escalating delays before a dropped harness is probed again: 15m, 1h, then 4h for every strike
/// after. A harness that fails once is probably transient and worth re-testing soon; one that keeps
/// failing its probes is worth testing rarely, since each probe spends our own tokens.
const PROBE_BACKOFF: [Duration; 3] = [
    Duration::from_secs(15 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(4 * 60 * 60),
];

/// The delay before strike `strikes` is probed. Saturates at the last step rather than growing
/// without bound — a permanently broken harness should still be re-checked a few times a day, since
/// the fix (a topped-up account, a configured provider) happens outside the process.
fn probe_delay(strikes: u32) -> Duration {
    let index = strikes.max(1) as usize - 1;
    PROBE_BACKOFF[index.min(PROBE_BACKOFF.len() - 1)]
}

/// A capability a harness needs and does not have.
///
/// Typed rather than a free string so every capability has a remedy the compiler insists on: adding
/// a variant without extending [`Self::remedy`] does not build. A hardcoded *"rebuild owed"* remedy
/// would be a lie for a harness whose barrier is an unconfigured provider rather than a missing
/// build feature, and the disposition is only useful if the operator can act on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MissingCapability {
    /// The binary was built without the `acp` feature, so NO harness on this node can run a turn.
    AcpFeature,
    /// The harness launched but its own configuration is incomplete — named as the harness reported
    /// it (e.g. a provider that was never selected).
    HarnessConfig(String),
}

impl MissingCapability {
    /// What an operator must do, derived from the capability itself.
    pub fn remedy(&self) -> String {
        match self {
            Self::AcpFeature => {
                "rebuild with the acp feature: cargo build -p mobee --features acp".to_owned()
            }
            Self::HarnessConfig(detail) => {
                format!("configure the harness ({detail}); a rebuild will not fix this")
            }
        }
    }

    /// The capability's short name, for a log line or a decline reason.
    pub fn name(&self) -> &str {
        match self {
            Self::AcpFeature => "acp feature",
            Self::HarnessConfig(_) => "harness configuration",
        }
    }
}

/// Why a harness is not serving right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// A named missing capability. No probe is scheduled — retrying cannot supply it.
    Incapable(MissingCapability),
    /// Dropped after a failure we could not attribute. `probe_due_at` is when a self-probe becomes
    /// DUE; it is not an expiry, and nothing returns to service when it passes.
    Dropped { probe_due_at: Instant, strikes: u32 },
}

impl Unavailable {
    /// The operator-facing reason, remedy included when one can be derived.
    pub fn reason(&self) -> String {
        match self {
            Self::Incapable(capability) => format!(
                "INCAPABLE: missing {} — {}",
                capability.name(),
                capability.remedy()
            ),
            Self::Dropped { strikes, .. } => format!(
                "DROPPED after {strikes} unattributable execution failure(s); awaiting a self-probe \
                 (transient and structural are indistinguishable from the failure alone)"
            ),
        }
    }
}

/// A harness-attributable execution failure, as classified by the CALL SITE.
///
/// Attribution is the caller's job, not this module's: whether a given failure implicates the
/// harness depends on which step of the execute path raised it, and only that step knows. A push
/// that failed against a remote, a receipt our own signer refused, and a policy WE declined are all
/// execution failures that say nothing about the harness — recording them here would open a roster
/// hole we inflicted on ourselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// A named capability the harness lacks ⇒ [`Unavailable::Incapable`], no probe scheduled.
    Incapable(MissingCapability),
    /// An execution failure we cannot attribute to a cause ⇒ dropped with an escalating probe delay.
    Unproven,
}

/// One harness's live state.
struct HarnessState {
    /// `None` ⇒ serving.
    unavailable: Option<Unavailable>,
    /// Unattributable faults ever recorded for this harness. Kept ACROSS a restore, so a harness
    /// that flaps backs off further each time instead of pinning at the shortest window.
    strikes: u32,
    /// Set while a self-probe for this harness is in flight. A probe runs OFF the event loop and can
    /// take a whole turn, while the housekeeping tick that starts one fires every few seconds — so
    /// without this the tick would launch a new probe on every tick and the harness would be running
    /// a dozen concurrent turns of our own tokens.
    probing: bool,
}

/// The harnesses a booted node is serving with right now: its resolved registry plus live
/// availability. Shared behind an `Arc` — execution runs off the event loop, so the task that
/// discovers a failure is not the one that publishes the advertisement.
///
/// Deliberately NOT `Clone`: two clones would each carry half the failure history, so a harness
/// dropped by an execution task would still be advertised by the loop. There is one roster per node.
pub struct LiveRoster {
    registry: AgentRegistry,
    /// Keyed by registry index. Absent ⇒ serving with no history; the map holds only harnesses that
    /// have faulted at least once.
    state: Mutex<BTreeMap<usize, HarnessState>>,
}

/// A harness selected to run a job: which registry index it is, and the entry itself.
///
/// The index travels with the selection because the task that runs the job is the one that must
/// report a fault, and by then it is long past the dispatch call — a name would not do, since the
/// unlabelled hatch has none.
#[derive(Clone, Copy, Debug)]
pub struct Selected<'a> {
    pub index: usize,
    pub agent: &'a RegisteredAgent,
}

impl LiveRoster {
    /// Wrap a resolved registry. Every entry starts serving: boot already verified each one is
    /// launchable, and launchable is the only thing boot can verify.
    pub fn new(registry: AgentRegistry) -> Self {
        Self {
            registry,
            state: Mutex::new(BTreeMap::new()),
        }
    }

    /// The harness names to advertise on the wire, in preference order, EXCLUDING anything not
    /// currently serving. Exactly the set [`Self::dispatch`] can serve, which is what makes a
    /// dropped harness stop attracting awards rather than merely failing them later.
    pub fn advertised(&self) -> Vec<String> {
        let state = self.state.lock().expect("live roster poisoned");
        self.registry
            .entries()
            .iter()
            .enumerate()
            .filter(|(index, _)| Self::serving(&state, index))
            .filter_map(|(_, entry)| entry.name.clone())
            .collect()
    }

    /// The harness that will run a job requesting `requested`, skipping anything not serving.
    ///
    /// `None` or `any` ⇒ the preferred SERVING entry, so a two-harness node that drops one still
    /// serves untargeted work with the other. A named request still matches by exact name and
    /// nothing else: a dropped harness returns `None` so the caller declines, rather than
    /// substituting a harness the buyer did not ask for.
    pub fn dispatch(&self, requested: Option<&str>) -> Option<Selected<'_>> {
        let state = self.state.lock().expect("live roster poisoned");
        let mut serving = self
            .registry
            .entries()
            .iter()
            .enumerate()
            .filter(|(index, _)| Self::serving(&state, index));
        let selected = match normalize_request(requested) {
            None => serving.next(),
            Some(name) => serving.find(|(_, entry)| entry.name.as_deref() == Some(name.as_str())),
        };
        selected.map(|(index, agent)| Selected { index, agent })
    }

    /// Whether a job requesting `requested` can be served right now — the claim-decision predicate.
    pub fn serves(&self, requested: Option<&str>) -> bool {
        self.dispatch(requested).is_some()
    }

    /// Record a harness-attributable failure against `index` and return the state it produced, so the
    /// caller can log ONE line naming both the drop and its remedy.
    ///
    /// [`Fault::Unproven`] escalates: the strike count grows and the probe delay grows with it.
    /// [`Fault::Incapable`] does not schedule a probe at all.
    pub fn fault(&self, index: usize, fault: Fault, now: Instant) -> Unavailable {
        let mut state = self.state.lock().expect("live roster poisoned");
        let entry = state.entry(index).or_insert(HarnessState {
            unavailable: None,
            strikes: 0,
            probing: false,
        });
        // Whatever a probe was testing, its answer arrived: this fault IS the answer.
        entry.probing = false;
        let unavailable = match fault {
            Fault::Incapable(capability) => Unavailable::Incapable(capability),
            Fault::Unproven => {
                entry.strikes = entry.strikes.saturating_add(1);
                Unavailable::Dropped {
                    probe_due_at: now + probe_delay(entry.strikes),
                    strikes: entry.strikes,
                }
            }
        };
        entry.unavailable = Some(unavailable.clone());
        unavailable
    }

    /// Put a harness back in service. The ONLY restoration path, and it is meant to be called after
    /// a self-probe actually passed — never on a timer, because a timer would make the next real
    /// award the test and a buyer would pay for our diagnostic.
    ///
    /// Strike history survives, so a harness that flaps escalates instead of resetting to 15m.
    pub fn restore(&self, index: usize) {
        if let Some(entry) = self
            .state
            .lock()
            .expect("live roster poisoned")
            .get_mut(&index)
        {
            entry.unavailable = None;
            entry.probing = false;
        }
    }

    /// Take the harnesses whose self-probe is due, marking each as being probed in the SAME locked
    /// step. Claiming and reporting are one operation on purpose: the caller runs each probe off the
    /// event loop, so a plain "which are due?" query would hand the same harness out again on every
    /// tick until the first probe finished.
    ///
    /// Never yields an [`Unavailable::Incapable`] harness. There is nothing a probe could establish
    /// about a missing build feature, and running one would spend tokens to re-learn a known answer.
    ///
    /// A claimed probe is released by whichever verdict lands: [`Self::restore`] when it passed,
    /// [`Self::fault`] when it did not. Both are reachable from every path the probe can take.
    pub fn claim_due_probes(&self, now: Instant) -> Vec<usize> {
        let mut state = self.state.lock().expect("live roster poisoned");
        let due: Vec<usize> = state
            .iter()
            .filter_map(|(index, harness)| match &harness.unavailable {
                Some(Unavailable::Dropped { probe_due_at, .. })
                    if now >= *probe_due_at && !harness.probing =>
                {
                    Some(*index)
                }
                _ => None,
            })
            .collect();
        for index in &due {
            if let Some(harness) = state.get_mut(index) {
                harness.probing = true;
            }
        }
        due
    }

    /// A index's current unavailability, or `None` when it is serving.
    pub fn unavailable(&self, index: usize) -> Option<Unavailable> {
        self.state
            .lock()
            .expect("live roster poisoned")
            .get(&index)
            .and_then(|harness| harness.unavailable.clone())
    }

    /// A index's harness name, for logs and decline reasons. `None` for the unlabelled hatch.
    pub fn label(&self, index: usize) -> Option<String> {
        self.registry
            .entries()
            .get(index)
            .and_then(|entry| entry.name.clone())
    }

    /// The launch argv for a index, for a self-probe that must run the same harness that failed.
    pub fn argv(&self, index: usize) -> Option<Vec<String>> {
        self.registry
            .entries()
            .get(index)
            .map(|entry| entry.argv.clone())
    }

    /// Every index the node resolved at boot, serving or not. The denominator for a roster report: a
    /// count of serving harnesses means nothing without it.
    pub fn entry_count(&self) -> usize {
        self.registry.entries().len()
    }

    fn serving(state: &BTreeMap<usize, HarnessState>, index: &usize) -> bool {
        state
            .get(index)
            .is_none_or(|harness| harness.unavailable.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(names: &[Option<&str>]) -> LiveRoster {
        LiveRoster::new(AgentRegistry::new(
            names
                .iter()
                .map(|name| RegisteredAgent {
                    name: name.map(str::to_owned),
                    argv: vec![format!("{}-acp", name.unwrap_or("hatch"))],
                })
                .collect(),
        ))
    }

    fn named(names: &[&str]) -> LiveRoster {
        roster(&names.iter().map(|n| Some(*n)).collect::<Vec<_>>())
    }

    #[test]
    fn a_dropped_harness_leaves_the_advertisement_and_the_dispatch_table_together() {
        // The invariant the wrapping exists to hold: there is no window in which the wire offers a
        // harness this node would then refuse to run.
        let roster = named(&["claude", "codex"]);
        assert_eq!(roster.advertised(), vec!["claude", "codex"]);

        roster.fault(0, Fault::Unproven, Instant::now());

        assert_eq!(roster.advertised(), vec!["codex"]);
        assert!(!roster.serves(Some("claude")), "dropped must stop dispatching");
        for name in roster.advertised() {
            assert!(
                roster.serves(Some(&name)),
                "advertised {name} must still dispatch"
            );
        }
    }

    #[test]
    fn dropping_the_preferred_harness_falls_through_to_the_next_serving_one() {
        // A two-harness seller that loses one is still a working one-harness seller: untargeted work
        // must keep flowing to the remainder rather than stopping with the preference.
        let roster = named(&["claude", "codex"]);
        roster.fault(0, Fault::Unproven, Instant::now());

        let selected = roster.dispatch(None).expect("the remainder still serves");
        assert_eq!(selected.agent.name.as_deref(), Some("codex"));
        assert_eq!(selected.index, 1);
    }

    #[test]
    fn every_harness_dropped_serves_nothing_rather_than_falling_back() {
        let roster = named(&["claude", "codex"]);
        let now = Instant::now();
        roster.fault(0, Fault::Unproven, now);
        roster.fault(1, Fault::Unproven, now);

        assert!(roster.advertised().is_empty());
        assert!(roster.dispatch(None).is_none(), "never substitute a harness");
        assert!(!roster.serves(None));
        assert_eq!(roster.entry_count(), 2, "the denominator survives the drop");
    }

    #[test]
    fn the_probe_window_elapsing_does_not_restore_anything() {
        // The distinction that keeps a buyer from paying for our diagnostic: `probe_due_at` schedules
        // a PROBE, it does not expire the drop. Long after it passes the harness is still dropped.
        let roster = named(&["claude"]);
        let now = Instant::now();
        roster.fault(0, Fault::Unproven, now);

        let long_after = now + Duration::from_secs(24 * 60 * 60);
        assert_eq!(
            roster.claim_due_probes(long_after),
            vec![0],
            "a probe is due once the window passes"
        );
        assert!(
            !roster.serves(None),
            "the window elapsing must NOT put the harness back in service"
        );
        assert!(roster.advertised().is_empty());

        // Only an explicit restore — what a PASSING probe calls — returns it.
        roster.restore(0);
        assert!(roster.serves(None));
        assert_eq!(roster.advertised(), vec!["claude"]);
        assert!(
            roster.claim_due_probes(long_after).is_empty(),
            "a serving harness owes no probe"
        );
    }

    #[test]
    fn a_claimed_probe_is_not_handed_out_again_until_its_verdict_lands() {
        // The probe runs off the event loop while the tick that starts one fires every few seconds.
        // Without the in-flight mark the same harness would be handed out on every tick and we would
        // be paying for a dozen concurrent turns of our own.
        let roster = named(&["claude"]);
        let now = Instant::now();
        roster.fault(0, Fault::Unproven, now);
        let due = now + Duration::from_secs(24 * 60 * 60);

        assert_eq!(roster.claim_due_probes(due), vec![0], "first claim takes it");
        assert!(
            roster.claim_due_probes(due).is_empty(),
            "a probe already in flight must not be started again"
        );

        // A verdict releases it. A FAILING probe re-arms with the escalated window…
        roster.fault(0, Fault::Unproven, now);
        assert_eq!(
            roster.claim_due_probes(due),
            vec![0],
            "the next window is claimable once the previous verdict landed"
        );

        // …and a PASSING one puts the harness back, owing nothing.
        roster.restore(0);
        assert!(roster.serves(None));
        assert!(roster.claim_due_probes(due).is_empty());
    }

    #[test]
    fn repeated_failures_escalate_the_probe_delay_and_saturate() {
        let roster = named(&["claude"]);
        let now = Instant::now();

        for (strike, expected) in [(1, PROBE_BACKOFF[0]), (2, PROBE_BACKOFF[1]), (3, PROBE_BACKOFF[2])]
        {
            let state = roster.fault(0, Fault::Unproven, now);
            match state {
                Unavailable::Dropped {
                    probe_due_at,
                    strikes,
                } => {
                    assert_eq!(strikes, strike);
                    assert_eq!(probe_due_at, now + expected, "strike {strike} delay");
                }
                other => panic!("unproven must drop, got {other:?}"),
            }
        }
        // A fourth strike saturates rather than growing without bound.
        match roster.fault(0, Fault::Unproven, now) {
            Unavailable::Dropped { probe_due_at, .. } => {
                assert_eq!(probe_due_at, now + PROBE_BACKOFF[2]);
            }
            other => panic!("expected a drop, got {other:?}"),
        }
    }

    #[test]
    fn strike_history_survives_a_restore_so_a_flapping_harness_backs_off_further() {
        let roster = named(&["claude"]);
        let now = Instant::now();
        roster.fault(0, Fault::Unproven, now);
        roster.restore(0);
        assert!(roster.serves(None), "restored");

        // The second failure is the harness's SECOND strike, not a fresh first one.
        match roster.fault(0, Fault::Unproven, now) {
            Unavailable::Dropped {
                strikes,
                probe_due_at,
            } => {
                assert_eq!(strikes, 2, "a restore must not forget the history");
                assert_eq!(probe_due_at, now + PROBE_BACKOFF[1]);
            }
            other => panic!("expected a drop, got {other:?}"),
        }
    }

    #[test]
    fn incapable_names_the_capability_schedules_no_probe_and_derives_its_own_remedy() {
        // The three-state point: a missing build feature can NEVER pass a probe, so scheduling one
        // would burn tokens forever to re-learn a known answer. And the remedy is derived, because a
        // hardcoded "rebuild" would be a lie for a harness whose barrier is configuration.
        let roster = named(&["claude", "goose"]);
        let now = Instant::now();

        roster.fault(0, Fault::Incapable(MissingCapability::AcpFeature), now);
        roster.fault(
            1,
            Fault::Incapable(MissingCapability::HarnessConfig(
                "GOOSE_PROVIDER is unset".into(),
            )),
            now,
        );

        assert!(
            roster
                .claim_due_probes(now + Duration::from_secs(365 * 24 * 60 * 60))
                .is_empty(),
            "an incapable harness is never probed, however long we wait"
        );
        assert!(roster.advertised().is_empty());

        let build = roster.unavailable(0).expect("dropped");
        assert!(build.reason().contains("acp feature"), "{}", build.reason());
        assert!(
            build.reason().contains("--features acp"),
            "the build remedy must name the rebuild: {}",
            build.reason()
        );

        let config = roster.unavailable(1).expect("dropped");
        assert!(
            config.reason().contains("GOOSE_PROVIDER"),
            "the reason must carry what the harness reported: {}",
            config.reason()
        );
        assert!(
            config.reason().contains("a rebuild will not fix this"),
            "a config barrier must NOT be reported as a build problem: {}",
            config.reason()
        );
    }

    #[test]
    fn the_unlabelled_hatch_is_droppable_even_though_it_has_no_name() {
        // Keying by index rather than name is what makes this possible. A name-keyed map could not
        // drop the raw-argv hatch at all, leaving one configuration permanently unable to stop
        // claiming — the exact black hole this module closes.
        let roster = roster(&[None]);
        assert!(
            roster.advertised().is_empty(),
            "a hatch advertises nothing to begin with"
        );
        assert!(roster.serves(None), "but it does serve untargeted work");

        roster.fault(0, Fault::Unproven, Instant::now());

        assert!(
            !roster.serves(None),
            "a dropped hatch must stop claiming untargeted work"
        );
        assert!(roster.dispatch(None).is_none());
    }

    #[test]
    fn a_serving_harness_reports_no_unavailability_and_keeps_its_argv_reachable() {
        // The probe needs to launch the SAME harness that failed, so argv stays reachable by index
        // while the harness is dropped.
        let roster = named(&["claude"]);
        assert!(roster.unavailable(0).is_none());
        assert_eq!(roster.label(0), Some("claude".to_owned()));

        roster.fault(0, Fault::Unproven, Instant::now());
        assert_eq!(
            roster.argv(0),
            Some(vec!["claude-acp".to_owned()]),
            "a dropped harness must stay launchable for its own probe"
        );
        assert_eq!(roster.label(0), Some("claude".to_owned()));
    }

    #[test]
    fn a_named_request_for_a_serving_harness_is_unaffected_by_another_ones_drop() {
        let roster = named(&["claude", "codex"]);
        roster.fault(1, Fault::Unproven, Instant::now());

        assert!(roster.serves(Some("claude")));
        assert!(!roster.serves(Some("codex")));
        // Case/whitespace canonicalisation still applies through the live view.
        assert!(roster.serves(Some(" Claude ")));
        assert_eq!(roster.advertised(), vec!["claude"]);
    }
}
