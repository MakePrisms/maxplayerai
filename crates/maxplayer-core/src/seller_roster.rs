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
                "rebuild with the acp feature: cargo build -p maxplayer --features acp".to_owned()
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

/// A failed execution classified before it reaches the live roster.
///
/// Keeping the job deadline separate from [`Fault`] is load-bearing: [`Fault`] always implicates the
/// harness, while a deadline expiry is attributable to the job clock and must leave both availability
/// and strike history untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionFailure {
    Harness(Fault),
    DeadlineExceeded,
}

/// One harness's live state.
struct HarnessState {
    /// `None` ⇒ serving.
    unavailable: Option<Unavailable>,
    /// The harness-resolved model id (ACP `models.currentModelId`) last observed for THIS harness, or
    /// `None` when nothing has observed one. Written by two sources and read only through
    /// [`LiveRoster::advertisement`], so the advertised model and the advertised name are one
    /// snapshot under one lock (#784):
    ///
    /// - the **setter** — a probe turn that PROVED this harness can serve (boot, or a restore).
    /// - the **refresher** — a job that completed on this harness, carrying the model that job
    ///   actually ran on.
    ///
    /// Written as an `Option` and overwritten UNCONDITIONALLY, including with `None`. A newer
    /// observation that saw no model must clear an older one: keeping the stale value would advertise
    /// a model the harness has stopped reporting, which is the drift this field exists to bound.
    model: Option<String>,
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
    /// Keyed by registry index. Absent ⇒ serving with no history AND no observed model; the map holds
    /// only harnesses something has recorded against — a fault, or a model observation (#784).
    state: Mutex<BTreeMap<usize, HarnessState>>,
    /// The seat-wide capability tokens proved by the boot probe (#784). Seat-wide because they
    /// describe the job execution environment, which is one environment shared by every harness here.
    ///
    /// It lives on the roster rather than beside it so that [`Self::advertisement`] stays the ONE
    /// wire snapshot: a capability read separately from the harness read could disagree with it, and
    /// the single-snapshot rule is what makes [`Advertisement::capability`] a route no emitter can
    /// partially take.
    capabilities: Mutex<Vec<String>>,
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

/// The roster as ONE wire snapshot: whether the seat is serving at all, and the names to advertise.
///
/// `serving` is NOT `!names.is_empty()`. The unlabelled `--agent-argv` hatch serves without a name
/// to publish, so a working seat can advertise nothing — and a heartbeat that read darkness off the
/// name list would take that seat off the market for having no label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advertisement {
    /// At least one entry is serving ⇒ the seat can take work. The `accepting` input.
    pub serving: bool,
    /// Serving entries that have a name, in preference order. Empty ⇒ "states no harness".
    pub names: Vec<String>,
    /// The observed model for each SERVING, NAMED entry that has one, in the same preference order as
    /// [`Self::names`] — a subset of it, never an entry absent from it.
    ///
    /// Built in the SAME locked pass as `names` on purpose (#784). A model belongs to a harness, so
    /// the pairing is the load-bearing part; assembling the two lists separately would let a fault
    /// landing in between attribute a model to the wrong harness, which is precisely the silent
    /// desync that ruled out positional pairing on the wire.
    pub models: Vec<crate::heartbeat::RosterModel>,
    /// The capability tokens this seat PROVED it can run, from
    /// [`crate::capability::probe_capabilities`]. Seat-wide rather than per-harness: the tokens
    /// describe the job EXECUTION ENVIRONMENT, which every harness on this seat shares.
    ///
    /// Empty is the honest answer for a stock image, and it is also the state before anything has
    /// probed — which is exactly why the probe and its zero-control had to ship together. Absent
    /// means unstated, never "none confirmed".
    pub capabilities: Vec<String>,
}

impl Advertisement {
    /// The #784 seat capability this snapshot implies — the ONE way a caller turns a roster read into
    /// something emittable.
    ///
    /// It exists to make an omission unrepresentable rather than merely wrong. Both emitters (the
    /// kind-30340 beat and the kind-3402 claim) need names AND models from the SAME snapshot, and a
    /// two-argument constructor lets a call site pass the names and quietly forget the models. That
    /// mistake is invisible on the wire — a seat that states no model is indistinguishable from a
    /// harness that reported none — and on the claim it is worse than cosmetic, because the award
    /// filter decides on the claim, so a forgotten model is a model no buyer can ever require.
    ///
    /// `display` is the operator's declared colour ([`crate::home::SeatConfig`]) — the two fields a
    /// roster read cannot supply, because no probe measures a fork name or a machine description.
    /// It is a required argument for the same reason `models` is: a snapshot that could be turned
    /// into a capability WITHOUT it gives every call site the chance to emit a seat whose declared
    /// colour silently vanished, and an absent tag is indistinguishable on the wire from an operator
    /// who declared nothing.
    ///
    /// ⚠ Passing it here does NOT put it on a claim. The two halves are separated at emit, not at
    /// construction: the claim builder asks for [`crate::heartbeat::SeatCapability::filterable_tags`]
    /// and the beat additionally asks for `display_tags`, so a claim carries these fields nowhere
    /// even while holding them. That is the whole point of the split being structural — this
    /// function does not have to remember which field is which, and neither does its caller.
    pub fn capability(&self, display: &crate::home::SeatConfig) -> crate::heartbeat::SeatCapability {
        let mut capability = crate::heartbeat::SeatCapability::from_roster(&self.names, &self.models);
        capability.capabilities = self.capabilities.clone();
        capability.harness_variant = display.harness_variant.clone();
        capability.hardware = display.hardware.clone();
        capability
    }
}

impl LiveRoster {
    /// Wrap a resolved registry. Every entry starts serving: boot already verified each one is
    /// launchable, and launchable is the only thing boot can verify.
    pub fn new(registry: AgentRegistry) -> Self {
        Self {
            registry,
            state: Mutex::new(BTreeMap::new()),
            capabilities: Mutex::new(Vec::new()),
        }
    }

    /// Record the capability tokens the boot probe PROVED, replacing whatever was there.
    ///
    /// Probed, never configured (#784): there is no config key for this, because enum-binding makes a
    /// token tidy rather than TRUE, and this is a field buyers commit sats against. The tokens are
    /// canonical by construction — they come from [`crate::capability::CAPABILITIES`] itself, not
    /// from anything an operator typed — so there is nothing to canonicalise here.
    pub fn record_capabilities(&self, capabilities: Vec<String>) {
        *self.capabilities.lock().expect("live roster poisoned") = capabilities;
    }

    /// The whole wire view of the roster, read under ONE lock.
    ///
    /// Both halves together rather than as two calls because they are published in the SAME
    /// heartbeat: read separately, a fault landing in between could emit `accepting=y` beside an
    /// empty advertisement — the exact incoherence a live roster exists to make unrepresentable.
    pub fn advertisement(&self) -> Advertisement {
        let state = self.state.lock().expect("live roster poisoned");
        let mut ad = Advertisement {
            serving: false,
            names: Vec::new(),
            models: Vec::new(),
            capabilities: self
                .capabilities
                .lock()
                .expect("live roster poisoned")
                .clone(),
        };
        for (index, entry) in self.registry.entries().iter().enumerate() {
            if !Self::serving(&state, &index) {
                continue;
            }
            ad.serving = true;
            let Some(name) = entry.name.clone() else {
                // The unlabelled `--agent-argv` hatch serves without a name. It may well have an
                // observed model, but a model tag is keyed by the harness it belongs to, so a model
                // with no harness to attribute it to is not advertisable — it is dropped here rather
                // than emitted unattached.
                continue;
            };
            if let Some(model) = state.get(&index).and_then(|harness| harness.model.clone()) {
                ad.models.push(crate::heartbeat::RosterModel {
                    harness: name.clone(),
                    model,
                });
            }
            ad.names.push(name);
        }
        ad
    }

    /// Record the model id observed for `index`, replacing whatever was there.
    ///
    /// The ONE writer both model sources go through (#784) — the probe that PROVED a harness can
    /// serve, and a job that completed on it. Deliberately a plain per-index setter taking an
    /// `Option`: a second source is one call, and a fresh observation of NO model clears a stale one
    /// rather than leaving it standing.
    ///
    /// Recording against a harness with no prior state CREATES its entry. That is why the state map
    /// no longer means "has faulted" — see [`LiveRoster::state`]. Availability is untouched: a new
    /// entry starts `unavailable: None`, so observing a model never takes a harness out of service
    /// and never puts one back.
    pub fn record_model(&self, index: usize, model: Option<String>) {
        self.state
            .lock()
            .expect("live roster poisoned")
            .entry(index)
            .or_insert(HarnessState {
                unavailable: None,
                strikes: 0,
                probing: false,
                model: None,
            })
            .model = model;
    }

    /// The harness names to advertise on the wire, in preference order, EXCLUDING anything not
    /// currently serving. Exactly the set [`Self::dispatch`] can serve BY NAME, which is what makes a
    /// dropped harness stop attracting awards rather than merely failing them later.
    ///
    /// ⚠ Empty does NOT mean nothing serves — the unlabelled hatch has no name to publish. Ask
    /// [`Advertisement::serving`] for that.
    pub fn advertised(&self) -> Vec<String> {
        self.advertisement().names
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

    /// Record a typed execution failure. Deadline expiry is a deliberate no-op; harness faults retain
    /// the existing drop/incapable rule through [`Self::fault`].
    pub fn execution_failure(
        &self,
        index: usize,
        failure: ExecutionFailure,
        now: Instant,
    ) -> Option<Unavailable> {
        match failure {
            ExecutionFailure::Harness(fault) => Some(self.fault(index, fault, now)),
            ExecutionFailure::DeadlineExceeded => None,
        }
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
            model: None,
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

    /// Release a claimed probe's in-flight mark WITHOUT reaching a verdict — the path neither
    /// [`Self::restore`] nor [`Self::fault`] covers.
    ///
    /// A restore self-probe runs OFF the event loop as a spawned task whose `JoinHandle` is dropped
    /// (run.rs `start_due_harness_probes`). If that task PANICS — a poisoned mutex `.expect`, say —
    /// neither verdict arm runs and the panic is otherwise silent, so `probing` would stay `true` for
    /// the life of the process and [`Self::claim_due_probes`] would never hand the harness out again:
    /// it sits `Dropped`, never re-probed. This method exists to be driven from a Drop guard armed
    /// BEFORE the probe runs, so the mark is released as the stack unwinds (#301).
    ///
    /// It NEVER clears `unavailable`. A probe that died proved nothing, so a harness abandoned
    /// mid-probe stays exactly as unavailable as it was — this can never turn a dead harness back to
    /// serving. When the harness is still `Dropped` and still marked probing, it advances a strike and
    /// re-arms the backoff window from `now`, so a harness whose probe panics or hangs repeatedly backs
    /// off on the normal schedule instead of hot-looping panics every tick.
    ///
    /// Idempotent by design: the two verdict paths clear `probing` themselves, so once a verdict has
    /// landed this is a no-op. That is what lets the Drop guard fire unconditionally on every path —
    /// verdict or not — without double-counting a strike on the happy path.
    pub fn abandon_probe(&self, index: usize, now: Instant) {
        let mut state = self.state.lock().expect("live roster poisoned");
        let Some(harness) = state.get_mut(&index) else {
            return;
        };
        if !harness.probing {
            return;
        }
        harness.probing = false;
        if let Some(Unavailable::Dropped {
            probe_due_at,
            strikes,
        }) = &mut harness.unavailable
        {
            *strikes = strikes.saturating_add(1);
            *probe_due_at = now + probe_delay(*strikes);
        }
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

    fn model_of(ad: &Advertisement, harness: &str) -> Option<String> {
        ad.models
            .iter()
            .find(|observed| observed.harness == harness)
            .map(|observed| observed.model.clone())
    }

    #[test]
    fn a_recorded_model_is_advertised_against_its_own_harness() {
        let roster = named(&["claude", "codex"]);
        roster.record_model(0, Some("claude-opus-5".to_owned()));
        roster.record_model(1, Some("gpt-5.6-terra[medium]".to_owned()));

        let ad = roster.advertisement();
        // The DENOMINATOR before any per-entry claim: an empty `models` would satisfy every
        // `find(...) == None` assertion below while proving nothing.
        assert_eq!(ad.models.len(), 2, "both observations must reach the wire view: {ad:?}");
        assert_eq!(model_of(&ad, "claude").as_deref(), Some("claude-opus-5"));
        assert_eq!(model_of(&ad, "codex").as_deref(), Some("gpt-5.6-terra[medium]"));
    }

    #[test]
    fn a_dropped_harness_takes_its_model_off_the_wire_with_its_name() {
        // THE desync this design exists to prevent. Two harnesses, two DIFFERENT models; drop the
        // first. The survivor must keep its OWN model — a wire view that assembled names and models
        // in separate passes could pair `codex` with the model that belonged to `claude`, and the
        // event would still parse.
        let roster = named(&["claude", "codex"]);
        roster.record_model(0, Some("claude-opus-5".to_owned()));
        roster.record_model(1, Some("gpt-5.6-terra[medium]".to_owned()));
        roster.fault(0, Fault::Unproven, Instant::now());

        let ad = roster.advertisement();
        assert_eq!(ad.names, vec!["codex"], "the dropped harness must not be advertised");
        assert_eq!(ad.models.len(), 1, "exactly one model survives: {ad:?}");
        assert_eq!(
            model_of(&ad, "codex").as_deref(),
            Some("gpt-5.6-terra[medium]"),
            "the survivor kept its OWN model, not the dropped harness's"
        );
        assert_eq!(model_of(&ad, "claude"), None);
    }

    #[test]
    fn a_fresh_observation_of_no_model_clears_a_stale_one() {
        // The reason `record_model` takes an `Option` and overwrites unconditionally. A harness that
        // stops reporting a model must stop advertising one; keeping the last value it ever gave is
        // exactly the stale-advertisement drift this field is meant to bound.
        let roster = named(&["claude"]);
        roster.record_model(0, Some("claude-opus-5".to_owned()));
        assert_eq!(model_of(&roster.advertisement(), "claude").as_deref(), Some("claude-opus-5"));

        roster.record_model(0, None);
        let ad = roster.advertisement();
        assert!(ad.models.is_empty(), "a None observation must clear, not preserve: {ad:?}");
        assert_eq!(ad.names, vec!["claude"], "clearing a model never unadvertises the harness");
    }

    #[test]
    fn a_later_observation_replaces_an_earlier_one() {
        // The refresher's whole purpose: a job that ran on a newly-changed default must correct the
        // model the boot probe recorded, not sit behind it.
        let roster = named(&["claude"]);
        roster.record_model(0, Some("claude-opus-5".to_owned()));
        roster.record_model(0, Some("claude-opus-6".to_owned()));
        assert_eq!(model_of(&roster.advertisement(), "claude").as_deref(), Some("claude-opus-6"));
    }

    #[test]
    fn recording_a_model_never_changes_availability() {
        // `record_model` CREATES a state entry for a harness that has never faulted, and the state
        // map is what `serving` reads. A default that came out `unavailable` would take a working
        // harness off the market for the crime of reporting its model.
        let roster = named(&["claude", "codex"]);
        roster.record_model(0, Some("claude-opus-5".to_owned()));

        assert!(roster.serves(Some("claude")), "observing a model must not drop a harness");
        assert_eq!(roster.advertised(), vec!["claude", "codex"]);
        assert_eq!(roster.unavailable(0), None);

        // And the other direction: it must not RESTORE one either.
        roster.fault(1, Fault::Unproven, Instant::now());
        roster.record_model(1, Some("gpt-5.6-terra[medium]".to_owned()));
        assert_eq!(roster.advertised(), vec!["claude"], "a model must not put a dropped harness back");
        let ad = roster.advertisement();
        assert_eq!(model_of(&ad, "codex"), None, "a non-serving harness advertises no model");
    }

    #[test]
    fn the_unlabelled_hatch_serves_with_a_model_but_advertises_neither() {
        // A model tag is keyed by the harness it belongs to, so a model with no name to attribute it
        // to cannot go on the wire. It is dropped rather than emitted unattached.
        let roster = roster(&[None, Some("codex")]);
        roster.record_model(0, Some("some-model".to_owned()));
        roster.record_model(1, Some("gpt-5.6-terra[medium]".to_owned()));

        let ad = roster.advertisement();
        assert!(ad.serving, "the hatch still serves");
        assert_eq!(ad.names, vec!["codex"]);
        assert_eq!(ad.models.len(), 1, "only the NAMED entry contributes a model: {ad:?}");
        assert_eq!(model_of(&ad, "codex").as_deref(), Some("gpt-5.6-terra[medium]"));
    }

    #[test]
    fn probed_capabilities_ride_the_same_snapshot_as_the_harnesses() {
        // Capabilities are seat-wide while harnesses are per-index, so they could easily have been
        // read separately — and a separate read is a second snapshot that can disagree with the
        // first. This asserts they come out of ONE `advertisement()` call, and survive a fault that
        // changes the harness half.
        let roster = named(&["claude", "codex"]);
        roster.record_capabilities(vec!["node".to_owned(), "rust".to_owned()]);
        roster.fault(1, Fault::Unproven, Instant::now());

        let ad = roster.advertisement();
        assert_eq!(ad.names, vec!["claude"], "the harness half still narrows");
        assert_eq!(
            ad.capabilities,
            vec!["node", "rust"],
            "a dropped harness does not change what the ENVIRONMENT can run"
        );
        // And they reach the emittable capability through the one route both emitters use.
        assert_eq!(
            ad.capability(&crate::home::SeatConfig::default()).capabilities,
            vec!["node", "rust"]
        );
    }

    #[test]
    fn a_seat_that_has_probed_nothing_advertises_no_capabilities() {
        // The pre-probe state, and the honest stock-image answer. Paired with its positive control
        // in the SAME test: an empty result here is indistinguishable from a roster that never wired
        // capabilities at all unless the other direction is asserted beside it.
        let roster = named(&["claude"]);
        assert!(
            roster.advertisement().capabilities.is_empty(),
            "nothing probed means nothing advertised"
        );

        roster.record_capabilities(vec!["python".to_owned()]);
        assert_eq!(
            roster.advertisement().capabilities,
            vec!["python"],
            "POSITIVE CONTROL: the same read must surface a recorded token, or the empty above \
             proves only that the field is unreachable"
        );
    }

    #[test]
    fn the_capability_a_snapshot_implies_reaches_the_filterable_tags() {
        // The whole chain in one place: roster state → snapshot → capability → the tags an award
        // filter reads. The pieces are tested apart; this asserts they are actually wired together,
        // which is the part a call site can break without any single unit test noticing.
        let roster = named(&["claude", "codex"]);
        roster.record_model(0, Some("claude-opus-5".to_owned()));
        roster.record_model(1, Some("gpt-5.6-terra[medium]".to_owned()));
        roster.fault(1, Fault::Unproven, Instant::now());

        let capability = roster
            .advertisement()
            .capability(&crate::home::SeatConfig::default());
        assert_eq!(
            capability.harness_families,
            vec!["claude-code"],
            "the dropped harness contributes no family"
        );
        assert_eq!(
            capability.models,
            vec![crate::heartbeat::HarnessModel {
                family: "claude-code".to_owned(),
                model: "claude-opus-5".to_owned(),
            }],
            "only the SERVING harness's model, keyed by its wire family"
        );

        // And on the emitted surface, since that is what a buyer actually reads. The count is the
        // positive control: `contains` over an empty tag list would assert nothing.
        let tags = capability.filterable_tags();
        assert_eq!(tags.len(), 2, "harness_family + one harness_model: {tags:?}");
        assert!(tags.contains(&crate::gateway::TagSpec::new([
            "harness_model",
            "claude-code",
            "claude-opus-5",
        ])));
    }

    #[test]
    fn an_undeclared_seat_states_no_colour_at_all() {
        // The two directions of the `[seat]` key, in one test because either alone is uninformative.
        // Absent must mean the tag is OMITTED, not emitted empty: a reader cannot tell an empty
        // string from a machine the operator declined to describe, and #784 makes absent the only
        // spelling of unstated.
        let roster = named(&["claude"]);
        let undeclared = roster
            .advertisement()
            .capability(&crate::home::SeatConfig::default());
        assert_eq!(undeclared.harness_variant, None);
        assert_eq!(undeclared.hardware, None);
        assert!(
            undeclared.display_tags().is_empty(),
            "an operator who declared nothing publishes no display tag"
        );

        // POSITIVE CONTROL: the same read must carry a DECLARED value, or the emptiness above proves
        // only that the field is unreachable — which was the defect, not the fix.
        let declared = roster.advertisement().capability(&crate::home::SeatConfig {
            harness_variant: Some("my-fork".to_owned()),
            hardware: Some("mac studio, 64GB".to_owned()),
        });
        assert_eq!(
            declared.display_tags(),
            vec![
                crate::gateway::TagSpec::new(["harness_variant", "my-fork"]),
                crate::gateway::TagSpec::new(["hardware", "mac studio, 64GB"]),
            ]
        );
    }

    #[test]
    fn a_deadline_expiry_neither_drops_the_harness_nor_consumes_a_strike() {
        let roster = named(&["claude"]);
        let now = Instant::now();

        assert_eq!(
            roster.execution_failure(0, ExecutionFailure::DeadlineExceeded, now),
            None,
            "the job clock is not a harness fault"
        );
        assert!(roster.serves(Some("claude")));
        assert_eq!(roster.advertised(), vec!["claude"]);
        assert_eq!(roster.unavailable(0), None);

        // A later genuinely unproven failure must still drop, and must be strike ONE: the deadline
        // event above did not silently advance the backoff history.
        let dropped = roster
            .execution_failure(
                0,
                ExecutionFailure::Harness(Fault::Unproven),
                now,
            )
            .expect("unproven failures still drop");
        assert!(matches!(dropped, Unavailable::Dropped { strikes: 1, .. }));
        assert!(!roster.serves(Some("claude")));
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

    /// Why the heartbeat's `accepting` input cannot be `!names.is_empty()`.
    ///
    /// This seat works and publishes no name, so an empty advertisement and a dark seat are DIFFERENT
    /// states that a name list alone cannot tell apart. Collapse them and a hatch-only node takes
    /// itself off the market for lacking a label; that is what `serving` exists to carry.
    #[test]
    fn the_advertisement_separates_having_no_name_from_having_nothing_to_serve() {
        let roster = roster(&[None]);

        let live = roster.advertisement();
        assert!(live.names.is_empty(), "a hatch has no name to publish");
        assert!(live.serving, "yet the seat is serving, and must advertise as accepting");

        roster.fault(0, Fault::Unproven, Instant::now());

        let dark = roster.advertisement();
        assert_eq!(
            dark.names, live.names,
            "the name list cannot see this transition — it was empty on both sides"
        );
        assert!(!dark.serving, "only `serving` moves, and it is what makes the seat state `accepting=n`");
    }

    /// The named case, so the pairing is proven on both roster shapes: dropping every named harness
    /// empties the advertisement AND reports the seat dark, under one read.
    #[test]
    fn a_fully_dropped_named_roster_reports_dark_alongside_an_empty_advertisement() {
        let roster = named(&["claude", "codex"]);
        assert_eq!(roster.advertisement().names, vec!["claude", "codex"]);
        assert!(roster.advertisement().serving);

        roster.fault(0, Fault::Unproven, Instant::now());
        let partial = roster.advertisement();
        assert_eq!(partial.names, vec!["codex"], "one harness left is still a working seat");
        assert!(partial.serving);

        roster.fault(1, Fault::Incapable(MissingCapability::AcpFeature), Instant::now());
        let dark = roster.advertisement();
        assert!(dark.names.is_empty());
        assert!(
            !dark.serving,
            "both unavailability kinds count as dark, not just the droppable one"
        );
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
    fn abandoning_a_claimed_probe_releases_the_mark_and_re_arms_it_without_restoring() {
        // The panic/hang path #301 closes: a probe task that reaches NEITHER verdict (it panicked, or
        // an outer wall-clock ceiling elapsed and the guard fired) must still release `probing`, or the
        // harness sits Dropped and is never re-probed for the life of the process. Abandoning must NOT
        // restore it — a probe that died proved nothing, and flipping a dead harness to serving is the
        // one thing this whole module exists to prevent.
        let roster = named(&["claude"]);
        let now = Instant::now();
        roster.fault(0, Fault::Unproven, now); // strike 1, 15m window
        let due = now + PROBE_BACKOFF[0];
        assert_eq!(roster.claim_due_probes(due), vec![0], "the window is due, claim marks probing");
        assert!(
            roster.claim_due_probes(due).is_empty(),
            "the in-flight mark suppresses a second claim"
        );

        // The probe never reached a verdict; the guard fires.
        roster.abandon_probe(0, due);

        assert!(!roster.serves(None), "abandoning must NEVER restore a dropped harness to service");
        assert!(roster.advertised().is_empty());
        match roster.unavailable(0) {
            Some(Unavailable::Dropped { strikes, probe_due_at }) => {
                assert_eq!(strikes, 2, "a panicking probe advances a strike so it backs off");
                assert_eq!(
                    probe_due_at,
                    due + PROBE_BACKOFF[1],
                    "the window is re-armed from the abandonment, not left in the past"
                );
            }
            other => panic!("must stay Dropped, got {other:?}"),
        }

        // Released: once the re-armed window passes the harness is claimable again — the property the
        // stuck-probing bug destroyed.
        assert!(
            roster.claim_due_probes(due).is_empty(),
            "the re-armed window has not passed yet"
        );
        assert_eq!(
            roster.claim_due_probes(due + PROBE_BACKOFF[1]),
            vec![0],
            "once the re-armed window passes the harness is re-probed — probing WAS released"
        );
    }

    #[test]
    fn abandoning_after_a_verdict_landed_is_an_idempotent_no_op() {
        // The Drop guard fires on EVERY path, including the happy one where a verdict already cleared
        // `probing`. It must not then advance a phantom strike or disturb the harness the verdict left.
        let now = Instant::now();

        // Happy path: a probe passed → restore cleared probing. Abandon after must not re-drop it.
        let restored = named(&["claude"]);
        restored.fault(0, Fault::Unproven, now);
        let due = now + PROBE_BACKOFF[0];
        assert_eq!(restored.claim_due_probes(due), vec![0]);
        restored.restore(0);
        restored.abandon_probe(0, due);
        assert!(restored.serves(None), "abandon after restore must not un-restore the harness");
        assert_eq!(restored.unavailable(0), None);

        // Fault path: a probe failed → fault cleared probing and set strike 2. Abandon must not make it 3.
        let faulted = named(&["claude"]);
        faulted.fault(0, Fault::Unproven, now);
        assert_eq!(faulted.claim_due_probes(due), vec![0]);
        faulted.fault(0, Fault::Unproven, due); // the verdict: strike 2
        faulted.abandon_probe(0, due);
        match faulted.unavailable(0) {
            Some(Unavailable::Dropped { strikes, .. }) => {
                assert_eq!(strikes, 2, "abandon after a fault must not double-count the strike");
            }
            other => panic!("expected Dropped, got {other:?}"),
        }
    }

    #[test]
    fn abandoning_an_incapable_probe_leaves_the_capability_untouched() {
        // An Incapable harness is never handed to a probe, but should a mark ever be set on one,
        // abandoning it must not rewrite the capability into a Dropped/strike state — the remedy the
        // operator needs is in that variant.
        let roster = named(&["claude"]);
        roster.fault(0, Fault::Incapable(MissingCapability::AcpFeature), Instant::now());
        roster.abandon_probe(0, Instant::now());
        assert!(
            matches!(roster.unavailable(0), Some(Unavailable::Incapable(MissingCapability::AcpFeature))),
            "abandon must not convert an Incapable state into a Dropped one"
        );
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
