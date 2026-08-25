//! Seat announcement — addressable kind-30340 capability + liveness (protocol-v1 §4.2).
//!
//! A running seller republishes an **addressable** (NIP-01 parameterized-replaceable) event,
//! `d="maxplayer-seller"`, on a ~5-minute cadence. It carries every seat-level fact a buyer needs
//! before it trades: whether the seat is `accepting` new work, its `queue_depth`, its `rate`, the
//! `accepted_mints` it can be paid on, and the `agents` it can run. Every fact is current as of that
//! beat EXCEPT two: [`HARNESS_MODEL_TAG`] is last-observed and [`CAPABILITIES_TAG`] is bounded by
//! the seat's uptime. `docs/protocol-v1.md` §4.5.4 is normative for both.
//!
//! **This is the seat's only capability surface.** Issue #645 retired the kind-31990 handler
//! announce that used to carry the mints and the harness label; a reader must take capability from
//! here and nowhere else. Old 31990 events persist on relays as residue (replaceable events are
//! never deleted) — they are not live capability.
//!
//! None of this feeds the pay gate, journal, or receipt bind: it is pre-trade discovery. The
//! payable mint for one trade is the one carried by that trade's `creq`.
//!
//! **Resolve by `(pubkey, d)`, never by event id.** An addressable event is superseded in place,
//! so a superseded id goes empty and a by-id lookup would read as "seller gone." Consumers must
//! always resolve the latest heartbeat by author + `d`. See [`HeartbeatKey`].
//!
//! **A seat that leaves the role publishes one last beat saying so** — [`retraction_for_state`],
//! issue #747. Because the kind is addressable, whatever a seat published last is its permanent
//! public answer: a seat that simply stops beating leaves `accepting=y` standing forever, and no
//! amount of waiting produces a newer event to correct it. The retraction is not a new event type;
//! it is this same publisher told `accepting=false`, which is the only thing that can overwrite that
//! answer at the same address.
//!
//! ⛔ That terminal beat is INSURANCE, NOT REPAIR, and it must never be described as making the
//! directory truthful. Only a process that is still running can publish it, so it covers a graceful
//! exit and nothing else — SIGKILL, a panic that skips unwinding, an OOM kill and a power cut all
//! leave the last `accepting=y` exactly where it was. Consumer-side recency filtering (protocol-v1
//! §4.4: a recent announcement proves only that the seat published) stays the only cover for those,
//! and stays required with this in place. Belt AND braces.

use serde::Serialize;

use crate::gateway::{EventDraft, MAXPLAYER_TAG, PROTOCOL_VERSION, TagSpec};
use crate::seller_agents::AGENT_TAG;

pub use crate::kinds::SELLER_HEARTBEAT_KIND;

/// The addressable `d` identifier for the seller heartbeat.
pub const SELLER_HEARTBEAT_D: &str = "maxplayer-seller";

/// Env override for the heartbeat cadence (seconds). Takes precedence over `[seller_heartbeat]
/// interval_secs`; intended for tests that cannot wait 5 minutes.
pub const HEARTBEAT_INTERVAL_ENV: &str = "MAXPLAYER_HEARTBEAT_INTERVAL_SECS";

/// Env override for heartbeat enablement (`0`/`false`/`no` disable, `1`/`true`/`yes` enable).
/// Takes precedence over `[seller_heartbeat] enabled`; intended for tests.
pub const HEARTBEAT_ENABLED_ENV: &str = "MAXPLAYER_HEARTBEAT_ENABLED";

/// Env override for the relay-stall watchdog threshold (missed heartbeat intervals). Takes
/// precedence over `[seller_heartbeat] stall_missed_intervals`; intended for tests that cannot
/// wait several 5-minute intervals for the watchdog to trip.
pub const HEARTBEAT_STALL_MISSED_INTERVALS_ENV: &str = "MAXPLAYER_HEARTBEAT_STALL_MISSED_INTERVALS";

/// Wire tag listing every mint the seat accepts payment on (§4.2). Multi-value, order preserved.
pub const ACCEPTED_MINTS_TAG: &str = "accepted_mints";

/// Wire tag naming the harness FAMILIES this seat serves (#784) — the enum-bound, filterable axis.
/// Multi-value like [`crate::seller_agents::AGENT_TAG`], because a seat may serve several harnesses.
/// Values come from [`crate::agent_presets::harness_family_for_preset`] and are therefore always in
/// [`crate::agent_presets::HARNESS_FAMILIES`]; a preset with no family contributes nothing.
pub const HARNESS_FAMILY_TAG: &str = "harness_family";

/// Offer param naming the harness family a job REQUESTS (#897):
/// `["param", "harness_family", "<family>"]`, single-value, matched against
/// [`HARNESS_FAMILY_TAG`].
///
/// Same word as the advertisement deliberately: the buyer's request and the seat's claim are
/// compared by exact equality, so spelling them from one constant is what keeps the two sides in one
/// vocabulary. A request param that drifted from the advertisement tag would filter on a word no
/// seat can say, and nothing would flag it.
///
/// Distinct from [`crate::seller_agents::AGENT_PARAM`], which names a PRESET, and the difference is
/// what each one BINDS. A family request selects WHICH SEATS MAY CLAIM: it is matched against
/// [`HARNESS_FAMILY_TAG`] at award time and refuses a seat that does not advertise it. It does NOT
/// choose which harness a winning seat then dispatches — only the preset reaches execution
/// (`offer_row` persists `requested_agent` alone, `classify_offer` gates on it, and `execute_job`
/// hands it to `SellerAgents::dispatch`, which runs the seat's FIRST configured preset when it is
/// absent). So a multi-harness seat can satisfy a family request and run a different harness.
///
/// A request that must bind execution therefore names the preset, and a model request REQUIRES one
/// — see [`crate::buyer::lifecycle::CapabilityRefusal::ModelWithoutHarnessPreset`].
pub const HARNESS_FAMILY_PARAM: &str = "harness_family";

/// Wire tag pairing ONE serving harness to the model it LAST REPORTED (#784):
/// `["harness_model", "<family>", "<model-id>"]`, REPEATED once per serving harness.
///
/// ⚠ Last-observed, NOT a promise about the next job. Nothing here selects or pins a model: the seat
/// states what a harness reported when it was last read, and the job that arrives later starts its
/// own session. §4.5.4 is normative; #785 carries the model-SELECTION work that would make this a
/// commitment rather than an observation.
///
/// The value is whatever that harness's own ACP `session/new` reports as its resolved session model,
/// verbatim — never operator-typed. TWO wire shapes carry it and the driver reads both: legacy
/// `models.currentModelId`, or the first model-category Session Config Option's `currentValue` for
/// adapters that publish it there instead (see `driver::acp_driver::session_model_from_result`).
///
/// The SAME MODEL-ID NAMESPACE later supplies the seller's `["model", name]` on the RESULT event
/// (`docs/protocol-v1.md` §6.4), which a buyer records as its own `model_used` attribution. Not one
/// FIELD — an adapter may advertise from one shape — but one namespace of ids, which is the property
/// that makes advertised and delivered directly COMPARABLE.
///
/// ⚠ One namespace is not one value: the two are separate reads and CAN differ honestly — see below.
///
/// ## The one thing this field must never be written as
///
/// ⚠ THE REFERENT IS NOT A TENSE. Every wording that upgrades this SELF-REPORT into an EXECUTION
/// FACT is the same defect, and it has been written in all three tenses already — "the model it
/// will use" (future), "the model it is serving" (present), "the model the job actually ran on"
/// (past). Rewriting one tense leaves the others, because the tense was never the error.
///
/// The test to apply to any new wording: ACP reports the resolved session model on the `session/new`
/// RESPONSE, before the harness does any work, and nothing in this codebase pins the model or reads
/// back what executed. Reading a second wire shape (#896) changed WHERE the value is found and
/// nothing about WHEN — it is still a pre-work self-report, so every wording rule below is untouched. So the field can carry only "what this harness said about itself, when it was
/// last asked". Anything stronger — ran, runs, will run, is serving, guarantees — is a claim no code
/// here supports.
///
/// What the value is worth is PROVENANCE: machine-sourced beats operator-typed, because an operator
/// can type a model the harness has never reported and nothing would notice. That is the argument
/// for reading it off the run rather than off config — never an argument that it binds execution.
///
/// ⚠ Comparable is not checked. Both ids are the seller's own report, and §6.4 states that nothing
/// verifies the execution-metadata block and that a reader MUST NOT treat it as proof a given model
/// ran. A divergence is an inconsistency a buyer can see in its own records — never a falsifier.
///
/// ## Why paired, and why not a flat list
///
/// A model belongs to a harness; it is not an independent axis. On a multi-harness seat a bare model
/// does not say which harness would run, so the ADVERTISEMENT pairs each model to the family that
/// carries it — a flat list could not say which is which.
///
/// ⚠ Pairing the advertisement is not the same as binding execution, and the two must not be run
/// together. What dispatch selects on is the PRESET
/// ([`crate::seller_agents::AgentRegistry::dispatch`] is exact-or-nothing on the preset NAME, and
/// falls back to the seat's first preset when none is named). A family is not read there at all. So
/// a model request inherits no guarantee from a family request; it is the `agent` preset that a
/// model must be paired with on the REQUEST side, and only that pairing reaches execution.
///
/// The flat `["models", id, …]` this replaces cannot express that pairing. Pairing by POSITION
/// against the family list was rejected for a sharper reason: a harness that reports no model
/// contributes no entry, which desyncs every index after it SILENTLY — the tag still parses and the
/// filter matches a model attributed to the wrong harness. A single composite token was rejected too,
/// because model ids are not separator-safe (`org/model:tag` exists), which would put a parse that
/// can fail inside the award decision. This 3-element form has no separator, no index coupling, and
/// an absent model simply means an absent tag with nothing else shifting. It is also the grammar this
/// codebase already uses for keyed values (`["param", "agent", "claude"]`).
///
/// ## One namespace is not one value
///
/// The advertisement and the RESULT are two SEPARATE `session/new` invocations, minutes apart, and
/// `reasoning_effort` — which composes into the bracket suffix of the very id being filtered on — is
/// a settable harness control. So an advertised id CAN differ from the delivered one with no liar
/// involved: two honest reads at two times. This design does not eliminate that drift; it reduces it
/// to the rate #784's advertised-versus-delivered detector was built to catch. Advertising the
/// harness's full `availableModels` instead would have made that detector fire on nearly every job,
/// and a detector that always fires is not a detector. Issue #785 carries the model-selection work
/// that would close the gap properly.
pub const HARNESS_MODEL_TAG: &str = "harness_model";

/// Offer param naming the model a job REQUESTS (#897):
/// `["param", "harness_model", "<model>"]`, single-value, matched against [`HARNESS_MODEL_TAG`].
///
/// ⚠ ONLY MEANINGFUL PAIRED WITH THE `agent` PRESET, and a model arriving without one refuses every
/// claim rather than being ignored — the PAIR is the unit, because a bare model on a multi-harness
/// seat does not say which harness would run it. The PRESET is the anchor rather than
/// [`HARNESS_FAMILY_PARAM`] because it is the only part of a request that reaches execution: the
/// seat dispatches on the preset alone and runs its first configured preset when none is named, so
/// a model hung off a family would pass the filter and then run on whichever preset the seat happens
/// to list first. The family is DERIVED from the preset when it is not stated.
///
/// ⚠ And it filters on a LAST-OBSERVED self-report, never a promise: see [`HARNESS_MODEL_TAG`]. A
/// model request narrows who is CONSIDERED; it does not pin what executes. #785 carries the
/// selection work that would make it a commitment.
pub const HARNESS_MODEL_PARAM: &str = "harness_model";

/// Wire tag carrying the seat's harness VARIANT (#784) — fork/config colour, free text, single value.
///
/// DISPLAY ONLY. It is deliberately absent from #784's filterable set {family, model, capabilities},
/// so drift in it is harmless — which is the whole reason it is allowed to be free text. Anything
/// that becomes filterable must first become enum-bound or machine-sourced.
pub const HARNESS_VARIANT_TAG: &str = "harness_variant";

/// Wire tag listing what this seat can actually run (#784) — enum-bound, multi-value, filterable.
///
/// ## PROBED, never declared
///
/// Each token is PROVEN by probing the job execution environment before it is advertised, the same
/// prove-before-advertise discipline the harness roster already follows. There is deliberately no
/// config key for it, because enum-binding would not make a configured value TRUE.
///
/// ENUM-BINDING SOLVES CANONICALISATION, NOT TRUTH. It guarantees `rust` and `Rust` become one
/// spelling; it says nothing about whether the seat can build Rust. A filterable field needs
/// provenance, not just a tidy spelling — a buyer commits sats at award, and an operator-typed
/// capability has nothing that could contradict it.
///
/// The concrete case is measured, not hypothetical: the shipped Docker runtime stage installs
/// `ca-certificates` and `tini` and copies one binary, so a seat on the stock image has no cargo, no
/// node and no python — every token in the vocabulary would be false there if declared (#358).
/// Probed, that same seat honestly advertises none, and #358 stops being reachable by construction.
///
/// ⚠ THE PROBE RUNS WHERE JOBS RUN, NEVER ON THE SEAT HOST. The execution environment is
/// operator-built, so only the seat can answer. A host-side `which` is the right predicate in the
/// wrong environment: it would prove a capability the JOB will not have, which is #358's own shape
/// one level down.
///
/// ## What filtering a token GUARANTEES, and what it does not
///
/// Filtering `rust` guarantees "`cargo` resolved on PATH in the JOB environment AT PROBE TIME". It
/// does NOT guarantee a build succeeds. Presence is necessary, not sufficient. The gap is honest and
/// unavoidable — but a buyer commits sats on the token, so the contract is written here rather than
/// left to a reader's inference. The probe command IS the token's definition: `rust` ⇒ `cargo`,
/// `node` ⇒ `node`, `python` ⇒ `python3`. A new token ships WITH its probe command.
///
/// ## Known asymmetry — probing buys provenance, not detectability
///
/// No filterable field has dispatch enforcement, and this one also has no echo a buyer could
/// compare against. `harness_family` is NOT enforced by dispatch: dispatch selects on the offer's
/// `agent` preset alone and runs the seat's first preset when none is named, so a family decides who
/// may be CONSIDERED and never what executes. `harness_model` is merely ECHOED: the result carries
/// `["model", name]`, so a buyer can notice a divergence from what it awarded on, but both values
/// are the SELLER'S OWN WORD and `docs/protocol-v1.md` §6.4 states that nothing verifies that block.
/// It is an inconsistency signal, not a falsifier. No event carries a capability back at all.
///
/// The residual is accepted because the probe makes the claim true at the SOURCE, and because
/// presence is honestly necessary-not-sufficient for any capability signal — but the three are one
/// echo and two silences, and must not be read as grades of the same proof.
///
/// ## Freshness — bounded by UPTIME, not by the beat
///
/// The probe runs ONCE, at seat start, and every beat for the life of the process republishes that
/// same snapshot. So the staleness bound is how long the seat has been running, and a recent beat is
/// no evidence of a recent measurement.
///
/// TWO fields narrow §4.2's general rule, not one: this one is uptime-bounded and
/// [`HARNESS_MODEL_TAG`] is last-observed. [`HARNESS_FAMILY_TAG`] is the only filterable field that
/// is genuinely current as of the beat carrying it. See `docs/protocol-v1.md` §4.5.4, normative.
///
/// Drift runs in BOTH directions and they are not symmetric. A toolchain INSTALLED into a running
/// seat's environment is not advertised until restart — the seat under-claims and loses awards it
/// could have won. A toolchain REMOVED from a running seat keeps being advertised until restart, so
/// the seat OVER-claims: it can be awarded work it can no longer do, and that is caught at delivery
/// rather than at the filter. Bounding the probe on a cadence is #891.
pub const CAPABILITIES_TAG: &str = "capabilities";

/// Offer param naming the capability tokens a job REQUIRES (#897):
/// `["param", "capability", "<token>", …]`, multi-value, matched against [`CAPABILITIES_TAG`].
///
/// Singular against the plural advertisement, exactly as [`crate::seller_agents::AGENT_PARAM`] is
/// singular against [`crate::seller_agents::AGENT_TAG`] — the request names one requirement at a
/// time, the advertisement names a set.
///
/// ONE multi-value tag, never repeated single-value tags. The readers here take the FIRST matching
/// tag, so a second `["param", "capability", …]` would be silently dropped and a buyer would be
/// filtered on a subset of what it asked for — the failure direction that awards work nobody
/// verified was requestable.
pub const CAPABILITY_PARAM: &str = "capability";

/// Wire tag carrying operator colour about the machine (#784) — e.g. "mac studio, 64GB". Free text,
/// single value.
///
/// EXPLICITLY UNVERIFIED AND NEVER FILTERED. #784 states hardware "renders but never enters a filter
/// predicate". That is not a convention to remember at each call site — it is why this value is
/// allowed to be arbitrary text at all, and a test names the filter surface to keep it true.
pub const HARDWARE_TAG: &str = "hardware";

/// The capability a seat advertises (#784), as ONE object rather than five loose fields.
///
/// It exists to make the split structural instead of remembered. #784 has two kinds of field:
///
/// - **Filterable** — `harness_family`, `capabilities`, `harness_model`. A buyer's award filter
///   reads these off the CLAIM, so they appear on the kind-3402 claim as well as the kind-30340
///   beat, and must be spelled identically on both. [`Self::filterable_tags`] is that single
///   spelling.
/// - **Display-only** — `harness_variant`, `hardware`. Colour for a human or a seat directory. They
///   go on the beat alone, because the award decision never reads them, and putting them on every
///   claim would be weight with no reader.
///
/// ## The line between them is PROVENANCE
///
/// **Filterable ⟺ machine-sourced.** Only a field the seat MEASURED may gate a payment, because a
/// buyer commits sats at award and an operator-typed claim has nothing to contradict it. Each
/// filterable field earns its place by being measured: `harness_family` from the dispatchable
/// roster, `harness_model` from the harness handshake, `capabilities` from a probe of the job
/// execution environment. The display-only two are operator-declared, which is exactly why they are
/// harmless — nothing pays out on them.
///
/// Enum-binding is NOT what buys this. Enum-binding solves canonicalisation — that `rust` and `Rust`
/// become one spelling — and says nothing about whether the seat can build Rust. That is why
/// `capabilities` is probed rather than configured; see [`CAPABILITIES_TAG`].
///
/// Two call sites could have been asked to remember which field is which. They are not: each site
/// asks for the SET it needs, so a new field joins one list or the other and cannot be
/// half-plumbed — the failure where a filterable field reaches the beat but not the claim, making a
/// seat visible in a directory yet unmatchable by the filter that is supposed to find it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SeatCapability {
    /// Enum-bound harness families this seat serves, from
    /// [`crate::agent_presets::harness_family_for_preset`]. Empty ⇒ unstated.
    pub harness_families: Vec<String>,
    /// One machine-sourced resolved model id per serving harness, PAIRED to that harness. Empty ⇒
    /// unstated. See [`HARNESS_MODEL_TAG`] for why it is paired, and for why one namespace with the
    /// RESULT's `model` tag is not one value.
    pub models: Vec<HarnessModel>,
    /// Capability tokens this seat PROVED it can run, from
    /// [`crate::capability::probe_capabilities`]. Canonical by PROVENANCE — the only emitter yields
    /// entries of [`crate::capability::CAPABILITIES`] itself, so no spelling an operator could type
    /// ever reaches here. Empty ⇒ unstated.
    pub capabilities: Vec<String>,
    /// Free-text fork/config colour. Never filtered.
    pub harness_variant: Option<String>,
    /// Free-text machine colour. Never filtered — see [`HARDWARE_TAG`].
    pub hardware: Option<String>,
}

impl SeatCapability {
    /// The capability implied by the harness roster alone: the family of every ADVERTISED preset.
    ///
    /// Derived from the SAME list that feeds the `agents` tag, so the families and the harness names
    /// can never describe different rosters. Taking it from `advertised()` (rather than from config)
    /// is what keeps "advertise only what is dispatchable" true for this field too: a harness dropped
    /// from service leaves both tags at once.
    ///
    /// A preset with no family in the spec vocabulary contributes nothing — see
    /// [`crate::agent_presets::harness_family_for_preset`]. Duplicates are collapsed: two presets can
    /// alias one family, and a repeated value on the wire would say nothing extra.
    ///
    /// `models` is the roster's observed model per entry (#784), resolved to families through the
    /// SAME function as the names above, so a model can never be attributed to a family the roster
    /// does not advertise. There is deliberately no models-less constructor: a call site that could
    /// forget the models would silently emit a seat that states no model, which is indistinguishable
    /// on the wire from a harness that reported none. Pass an empty slice to mean genuinely none.
    pub fn from_roster(advertised: &[String], models: &[RosterModel]) -> Self {
        let mut harness_families: Vec<String> = Vec::new();
        for preset in advertised {
            let Some(family) = crate::agent_presets::harness_family_for_preset(preset) else {
                continue;
            };
            if !harness_families.iter().any(|kept| kept == family) {
                harness_families.push(family.to_owned());
            }
        }
        let mut pairs: Vec<HarnessModel> = Vec::new();
        for observed in models {
            // An entry whose name has no family in the vocabulary cannot key a tag, so its model is
            // dropped rather than emitted under a guessed family.
            let Some(family) = crate::agent_presets::harness_family_for_preset(&observed.harness)
            else {
                continue;
            };
            let pair = HarnessModel {
                family: family.to_owned(),
                model: observed.model.clone(),
            };
            // Two entries aliasing one family with the SAME model would repeat a tag that says
            // nothing extra. Two entries with DIFFERENT models both stand: each is true of a real
            // serving harness, and collapsing them would hide one.
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
        Self {
            harness_families,
            models: pairs,
            ..Self::default()
        }
    }

    /// Whether this capability states NOTHING — every field unset.
    ///
    /// Used to keep an unstated capability out of serialised output, so a claim that carried no
    /// #784 tags serialises exactly as it did before the field existed. ⚠ Unstated is NOT
    /// "matches nothing on purpose": a filter refuses both an unstated field and a stated
    /// non-matching one, and only a test that separates them can go red on a parser that reads
    /// nothing.
    pub fn is_unstated(&self) -> bool {
        *self == Self::default()
    }

    /// The capability an event's tags state — the single READ path, and the exact mirror of
    /// [`Self::filterable_tags`] on the write side.
    ///
    /// It exists for the same reason that one does. The beat and the claim carry the same fields,
    /// so two readers assembling them separately are two things that must agree; a second reader
    /// that merely HAPPENS to agree today is precisely what the single-write-path rule exists to
    /// prevent, and the read side is worth no less. Every consumer — the seat directory, the buyer's
    /// claim parse, the award filter — goes through here.
    ///
    /// Reading all five off ANY event is deliberate, including a claim, which carries no display
    /// fields: those simply come back `None`. Absent means unstated, so there is nothing to
    /// special-case per event kind, and no place for a per-kind rule to be applied inconsistently.
    pub fn from_tags(tags: &[TagSpec]) -> Self {
        Self {
            harness_families: harness_families_from_tags(tags),
            models: harness_models_from_tags(tags),
            capabilities: capabilities_from_tags(tags),
            harness_variant: harness_variant_from_tags(tags),
            hardware: hardware_from_tags(tags),
        }
    }

    /// The tags a buyer's award filter may read. Emitted on BOTH the kind-30340 beat and the
    /// kind-3402 claim, from this one function, so the two events cannot spell them differently.
    pub fn filterable_tags(&self) -> Vec<TagSpec> {
        let mut tags: Vec<TagSpec> = [
            harness_family_tag(&self.harness_families),
            capabilities_tag(&self.capabilities),
        ]
        .into_iter()
        .flatten()
        .collect();
        tags.extend(harness_model_tags(&self.models));
        tags
    }

    /// The display-only tags. Beat only — never read by the award decision, so never on a claim.
    pub fn display_tags(&self) -> Vec<TagSpec> {
        [
            harness_variant_tag(self.harness_variant.as_deref()),
            hardware_tag(self.hardware.as_deref()),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// A heartbeat ready to sign + publish. Build from live daemon state via [`heartbeat_for_state`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeartbeatDraft {
    /// Is the seller taking new work right now (`y`/`n`).
    pub accepting: bool,
    /// Current in-flight job count.
    pub queue_depth: u32,
    /// The seller's advertised rate (sats).
    pub rate_sats: u64,
    /// Every mint this seat accepts payment on, in config order. §4.2 requires at least one: a
    /// buyer can pay this seat only on a mint in this list, so a seat stating none is unpayable.
    pub accepted_mints: Vec<String>,
    /// The agent harnesses this seller can run, in preference order. Empty ⇒ the seller states no
    /// harness and the tag is omitted entirely (an unlabelled `agent_command` seller has no honest
    /// name to publish).
    pub agents: Vec<String>,
    /// The #784 capability advertisement. Default (all-unstated) emits no new tags at all, so a
    /// seat that has not been taught to fill this publishes exactly the §4.2 tag set it always did.
    pub capability: SeatCapability,
}

impl HeartbeatDraft {
    pub fn new(
        accepting: bool,
        queue_depth: u32,
        rate_sats: u64,
        accepted_mints: Vec<String>,
    ) -> Self {
        Self {
            accepting,
            queue_depth,
            rate_sats,
            accepted_mints,
            agents: Vec::new(),
            capability: SeatCapability::default(),
        }
    }

    /// Advertise `agents` (preference order) on this heartbeat.
    pub fn with_agents(mut self, agents: Vec<String>) -> Self {
        self.agents = agents;
        self
    }

    /// Advertise the seat's #784 capability on this heartbeat.
    pub fn with_capability(mut self, capability: SeatCapability) -> Self {
        self.capability = capability;
        self
    }

    /// The §4.2 tag set, in the order the spec table lists it: `d`, `t`, `v`, `rate`, `accepting`,
    /// `queue_depth`, `accepted_mints`, and `agents` when the seat states a roster — followed by the
    /// #784 capability tags, filterable first then display-only.
    ///
    /// The beat emits BOTH capability sets; a claim emits only the filterable one. Both take them
    /// from [`SeatCapability`], so the two events cannot spell a shared field differently.
    pub fn to_event_draft(&self) -> EventDraft {
        let accepting = if self.accepting { "y" } else { "n" };
        let queue_depth = self.queue_depth.to_string();
        let rate = self.rate_sats.to_string();

        let mut tags = vec![
            TagSpec::new(["d", SELLER_HEARTBEAT_D]),
            TagSpec::new(["t", MAXPLAYER_TAG]),
            TagSpec::new(["v", PROTOCOL_VERSION]),
            TagSpec::new(["rate", &rate]),
            TagSpec::new(["accepting", accepting]),
            TagSpec::new(["queue_depth", &queue_depth]),
            multi_value_tag(ACCEPTED_MINTS_TAG, &self.accepted_mints),
        ];
        if let Some(tag) = agent_tag(&self.agents) {
            tags.push(tag);
        }
        tags.extend(self.capability.filterable_tags());
        tags.extend(self.capability.display_tags());
        EventDraft::new(SELLER_HEARTBEAT_KIND, tags, "")
    }
}

/// The `["agents", …]` advertisement tag, or `None` for a seller that states no harness (the
/// tag is then omitted rather than emitted empty — absent means "unstated", never "none").
pub fn agent_tag(agents: &[String]) -> Option<TagSpec> {
    if agents.is_empty() {
        return None;
    }
    Some(multi_value_tag(AGENT_TAG, agents))
}

/// Read an `["agents", …]` advertisement off any event's tags. Absent ⇒ empty.
pub fn agents_from_tags(tags: &[TagSpec]) -> Vec<String> {
    tag_values(tags, AGENT_TAG)
}

/// The `["harness_family", …]` tag, or `None` when the seat names no family (#784).
///
/// ONE BUILDER, BOTH EMITTERS — this is called by the kind-30340 beat AND by
/// [`crate::gateway::claim_draft`], exactly as [`agent_tag`] already is. The filterable fields must
/// be spelled by one function on both events: the buyer's award filter reads the CLAIM, so a claim
/// that spelled a tag differently from the beat would be filtered differently from how the seat is
/// displayed, and nothing would flag it.
pub fn harness_family_tag(families: &[String]) -> Option<TagSpec> {
    if families.is_empty() {
        return None;
    }
    Some(multi_value_tag(HARNESS_FAMILY_TAG, families))
}

/// Read the `["harness_family", …]` list off any event's tags. Absent ⇒ empty, which never
/// satisfies a buyer that named a family — silence is not a capability.
///
/// Whitespace-normalized: see [`stated`].
pub fn harness_families_from_tags(tags: &[TagSpec]) -> Vec<String> {
    stated_values(tag_values(tags, HARNESS_FAMILY_TAG))
}

/// One wire value, normalized to the "stated or absent" contract: trimmed, and `None` when nothing
/// survives. Blank and all-whitespace are ABSENT, exactly as §4.5.2 defines them.
///
/// ⚠ THE EMITTERS ALREADY HONOUR THIS AND THE READERS DID NOT, which is a narrower hole than it
/// looks: [`single_value_tag`] trims and drops empty, so every tag THIS code writes is already
/// clean. The gap only opens for tags written by someone else — which is every tag a reader ever
/// sees. An all-whitespace value read raw becomes a `Some("   ")` that no operator typed and no
/// buyer can match, and for the filterable fields it decides awards.
///
/// Trimming rather than only rejecting is deliberate, and it is what makes the reader agree with the
/// emitter: a padded `" claude "` that stayed padded would never equal the `"claude"` a buyer names,
/// so a seat would advertise a family it could never be matched on.
///
/// ⚠ Applied at the FIVE #784 readers individually, NOT inside [`tag_values`]/[`first_tag_value`].
/// Those helpers are shared with `agents` and `accepted_mints`, and changing how MINT LISTS parse is
/// not something to do as a side effect of a capability change. Unifying at the helper is a coherent
/// proposal; it is a separate one.
fn stated(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// [`stated`] across a list, dropping the values that state nothing.
fn stated_values(values: Vec<String>) -> Vec<String> {
    values.iter().filter_map(|value| stated(value)).collect()
}

/// One serving harness and the model it LAST REPORTED — the decoded form of [`HARNESS_MODEL_TAG`].
///
/// Last-observed, never a commitment about the next job: see [`HARNESS_MODEL_TAG`].
///
/// The pair is the unit on purpose. Splitting it into two parallel lists is what reintroduces the
/// positional-desync failure the tag shape exists to prevent, so there is deliberately no API here
/// that hands out models without their families.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HarnessModel {
    /// The harness family this model belongs to — a value from
    /// [`crate::agent_presets::HARNESS_FAMILIES`].
    ///
    /// ⚠ Naming this in a request does NOT make dispatch bind — that claim was here before the
    /// request side existed and it is false. A family narrows which seats may claim; the PRESET is
    /// the only requested axis execution reads.
    pub family: String,
    /// The harness-resolved session model id, verbatim.
    pub model: String,
}

/// One serving harness's observed model, keyed by the roster NAME it was observed for (#784).
///
/// The roster's own pairing, before the name is resolved to a wire family. It exists as a distinct
/// type from [`HarnessModel`] because the two are keyed differently and the conversion is a real
/// step, not a rename: a roster name is an operator's preset label (`claude`), a family is the spec
/// vocabulary (`claude-code`), and a name with no family in the vocabulary resolves to nothing at all.
///
/// Paired for the same reason [`HarnessModel`] is: a model belongs to a harness, and every API that
/// hands out models without their harness is a positional desync waiting to happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RosterModel {
    /// The roster entry's advertised NAME, as it appears in the `agents` tag.
    pub harness: String,
    /// The harness-resolved session model id last observed for that entry, verbatim.
    pub model: String,
}

/// The `["harness_model", family, model]` tags, one per serving harness that named a model (#784).
/// Both emitters, for the same reason as [`harness_family_tag`].
///
/// A harness that reported no model id in EITHER wire shape contributes NO tag — absent means
/// unstated, and an
/// unstated model never satisfies a buyer that named one. Because each pair is its own tag, that
/// absence shifts nothing else.
pub fn harness_model_tags(models: &[HarnessModel]) -> Vec<TagSpec> {
    models
        .iter()
        .map(|entry| TagSpec::new([HARNESS_MODEL_TAG, &entry.family, &entry.model]))
        .collect()
}

/// Read every `["harness_model", family, model]` pair off any event's tags. Absent ⇒ empty.
///
/// A malformed tag — one missing either element — is SKIPPED rather than half-decoded. A pair with
/// an empty family would be a model no buyer could attach to a harness, which is exactly the
/// unpairable state this shape exists to rule out.
///
/// Both halves are whitespace-normalized by [`stated`], and an all-whitespace half is as unpairable
/// as an empty one: the pair is the unit, so a stated model under a blank family is not a partial
/// answer to salvage.
pub fn harness_models_from_tags(tags: &[TagSpec]) -> Vec<HarnessModel> {
    tags.iter()
        .filter(|tag| tag.0.first().map(String::as_str) == Some(HARNESS_MODEL_TAG))
        .filter_map(|tag| {
            let family = stated(tag.0.get(1)?)?;
            let model = stated(tag.0.get(2)?)?;
            Some(HarnessModel { family, model })
        })
        .collect()
}

/// The `["capabilities", …]` tag, or `None` when the seat states none (#784). Both emitters, for the
/// same reason as [`harness_family_tag`].
pub fn capabilities_tag(capabilities: &[String]) -> Option<TagSpec> {
    if capabilities.is_empty() {
        return None;
    }
    Some(multi_value_tag(CAPABILITIES_TAG, capabilities))
}

/// Read the `["capabilities", …]` list off any event's tags. Absent ⇒ empty.
///
/// Whitespace-normalized: see [`stated`]. This one is filterable and a buyer commits sats on it, so
/// a blank token that survived into the set would be a capability no seat can be held to.
pub fn capabilities_from_tags(tags: &[TagSpec]) -> Vec<String> {
    stated_values(tag_values(tags, CAPABILITIES_TAG))
}

/// The `["harness_variant", …]` tag, or `None` for a seat that states no variant (#784).
///
/// Beat-only: a variant is display colour, and the claim carries only what the award decision reads.
/// An all-whitespace variant is treated as unstated rather than emitted empty — the same
/// absent-means-unstated rule the list tags follow.
pub fn harness_variant_tag(variant: Option<&str>) -> Option<TagSpec> {
    single_value_tag(HARNESS_VARIANT_TAG, variant)
}

/// Read the `["harness_variant", …]` value off a seat announcement's tags. Absent ⇒ `None`.
///
/// Whitespace-normalized: see [`stated`]. Display-only, so this is the mildest of the five — but a
/// `Some("   ")` renders as a variant that is present and blank, which is not a state the field has.
pub fn harness_variant_from_tags(tags: &[TagSpec]) -> Option<String> {
    first_tag_value(tags, HARNESS_VARIANT_TAG).and_then(stated)
}

/// The `["hardware", …]` tag, or `None` for a seat that states none (#784). Beat-only and never
/// filtered — see [`HARDWARE_TAG`].
pub fn hardware_tag(hardware: Option<&str>) -> Option<TagSpec> {
    single_value_tag(HARDWARE_TAG, hardware)
}

/// Read the `["hardware", …]` value off a seat announcement's tags. Absent ⇒ `None`.
///
/// Whitespace-normalized: see [`stated`]. Never filtered, so this one is cosmetic — normalized with
/// the others because a reader that treats one of the five differently is the thing a later change
/// reasons from.
pub fn hardware_from_tags(tags: &[TagSpec]) -> Option<String> {
    first_tag_value(tags, HARDWARE_TAG).and_then(stated)
}

/// `["<name>", value]` for the single-value free-text tags, or `None` when there is nothing honest
/// to say. Blank and whitespace-only collapse to `None`: an empty tag on the wire would read as a
/// stated-but-empty value, and these fields have no such state.
fn single_value_tag(name: &str, value: Option<&str>) -> Option<TagSpec> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    Some(TagSpec::new([name, value]))
}

/// Read the `["accepted_mints", …]` list off a seat announcement's tags. Absent ⇒ empty, which
/// [`parse_heartbeat`] rejects — §4.2 requires at least one mint.
pub fn accepted_mints_from_tags(tags: &[TagSpec]) -> Vec<String> {
    tag_values(tags, ACCEPTED_MINTS_TAG)
}

/// `["<name>", v0, v1, …]` — the multi-value tag convention both list tags use.
fn multi_value_tag(name: &str, values: &[String]) -> TagSpec {
    let mut tag = vec![name.to_owned()];
    tag.extend(values.iter().cloned());
    TagSpec(tag)
}

fn tag_values(tags: &[TagSpec], name: &str) -> Vec<String> {
    first_tag(tags, name)
        .map(|tag| tag.0[1..].to_vec())
        .unwrap_or_default()
}

/// Build the heartbeat for a seller's live state. `accepting` is `y` only when the seat has a free
/// slot AND something is actually serving: a busy seller is not taking new work, and neither is a
/// seat that has dropped every harness. `agents` is what the live roster advertises. This is the
/// single mapping the daemon loop uses, factored out so the flip is unit-testable without a relay.
///
/// ⚠ **`in_flight` is a COUNT, and the type is load-bearing.** This parameter was a `bool`, which
/// destroyed the count at the signature — the `queue_depth` on the wire could then only ever be 0 or
/// 1 no matter what the caller knew, while this doc still claimed it was the in-flight count. The
/// caller then supplied that bool as `COUNT(*) FROM jobs > 0`, a LIFETIME row count, so a seat
/// published `accepting=n` permanently from its first job onward (#313).
/// ★ The 0/1 cast is why it survived: a seat holding 5 finished jobs advertised `1`, which reads as
/// plausible. A literal `5` on an idle seat would have looked absurd to anyone. **A lossy encoding of
/// a quantity hides a wrong answer inside a believable one** — so keep this a count, and let the wire
/// carry a number a reader can sanity-check.
///
/// ⚠ `anything_serving` is NOT derivable from `agents`. The roster advertises NAMES, and the
/// unlabelled `--agent-argv` hatch has none — so a seat serving only the hatch advertises an empty
/// list while being perfectly able to work, and reading darkness off that list would take it off the
/// market for lacking a label. The signal comes from the roster's own dispatch predicate.
///
/// WHY `accepting` rather than a marker on the agents tag: an ABSENT tag already means "unstated",
/// so there is no spare state there to mean "none" without a protocol change every reader would have
/// to learn. `accepting` has no unstated value, so the truth fits in a field that already exists.
pub fn heartbeat_for_state(
    in_flight: u32,
    anything_serving: bool,
    rate_sats: u64,
    accepted_mints: Vec<String>,
    agents: Vec<String>,
    capability: SeatCapability,
) -> HeartbeatDraft {
    // The capability arrives already derived, from `LiveRoster::Advertisement::capability()` — the
    // ONE route from a roster read to something emittable. Deriving it here instead would make this
    // a SECOND derivation site alongside the claim's, and two sites that must agree are two sites
    // that can drift; the fields are observed STATE (models, probed capabilities) that cannot be
    // recomputed from the `agents` list anyway. Callers pass names and capability from the same
    // single locked snapshot, so they cannot describe different rosters.
    HeartbeatDraft::new(
        in_flight == 0 && anything_serving,
        in_flight,
        rate_sats,
        accepted_mints,
    )
    .with_agents(agents)
    .with_capability(capability)
}

/// The seat's **terminal beat** (#747): the ordinary announcement, published one last time with
/// `accepting=n`, as the seat leaves the selling role — shutdown, or any role change away from
/// selling.
///
/// WHY THE EXISTING EVENT rather than a new kind or a deletion: kind-30340 is addressable, so the
/// relay holds exactly ONE announcement per `(pubkey, d)` and each beat replaces the last IN PLACE.
/// That is also precisely the defect — a seat that stops beating leaves its final `accepting=y`
/// standing as its permanent public answer, and since the kind is replaceable there is no newer
/// event to correct it and no amount of waiting produces one. The lie is stable, not transient. The
/// only thing that can overwrite it is another event at the SAME address, which is exactly this.
///
/// ⛔ **INSURANCE, NOT REPAIR.** A beat can only be published by a process that is still running, so
/// this covers a GRACEFUL exit and nothing else: SIGKILL, a panic that skips unwinding, an OOM kill
/// and a power cut leave the last `accepting=y` in place exactly as before. Consumer-side recency
/// filtering stays the only cover for those, and stays REQUIRED with this in place. Never document
/// this as making the directory truthful.
///
/// WHY NOT A KIND-5 DELETION (#747 item 2, deliberately not taken): a NIP-09 deletion request is
/// advisory — the relay decides — so a reader could not rely on it, and every reader would have to
/// learn a second event class to discover the same fact this one already carries. It also asks for
/// the announcement to VANISH, and an absent announcement is indistinguishable from a seat that
/// never published, whereas `accepting=n` is a seat saying it is closed. The retraction needs
/// neither relay cooperation nor a new reader rule.
///
/// `in_flight` stays the LIVE count rather than a hopeful `0`: a seat can leave holding non-terminal
/// jobs it will resume on its next boot, and a terminal beat is no licence to misreport them.
/// `agents` stays the roster for the same reason — leaving the market is not a claim to have
/// forgotten how to work, and `accepting` is the field that carries "not taking work" (see
/// [`heartbeat_for_state`] on why the roster tag has no spare state to mean it).
pub fn retraction_for_state(
    in_flight: u32,
    rate_sats: u64,
    accepted_mints: Vec<String>,
    agents: Vec<String>,
    capability: SeatCapability,
) -> HeartbeatDraft {
    // `anything_serving = false` BY CONSTRUCTION: nothing serves a seat that is leaving the role. It
    // is passed as a literal, not taken as a parameter, so no caller and no in-flight count can make
    // a terminal beat come out `accepting=y` — the one property this whole path exists to guarantee.
    heartbeat_for_state(
        in_flight,
        false,
        rate_sats,
        accepted_mints,
        agents,
        capability,
    )
}

/// A parsed heartbeat's payload. The author pubkey is NOT carried here — combine it with [`d`]
/// via [`ParsedHeartbeat::key`] to get the `(pubkey, d)` identity.
///
/// [`d`]: ParsedHeartbeat::d
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParsedHeartbeat {
    pub d: String,
    pub accepting: bool,
    pub queue_depth: u32,
    pub rate_sats: u64,
    /// Every mint this seat accepts payment on. Never empty — [`parse_heartbeat`] rejects a seat
    /// that states none.
    pub accepted_mints: Vec<String>,
    /// Advertised harnesses, preference order. Empty ⇒ the seller stated none (the tag was
    /// absent) — NOT a claim that it can run nothing.
    pub agents: Vec<String>,
    /// The seat's #784 capability advertisement, read back off the same tags the beat emitted.
    /// Every field defaults to unstated, so a beat from a seat that predates #784 parses to a
    /// [`SeatCapability::default`] rather than failing — that is what lets emitters and readers ship
    /// without a `v` bump.
    pub capability: SeatCapability,
}

impl ParsedHeartbeat {
    /// The `(pubkey, d)` key for this heartbeat given its author.
    ///
    /// **Always key a heartbeat by this, never by event id.** An addressable event is superseded
    /// in place, so an old id goes empty and a by-id lookup would read as "seller gone"
    /// (NIP-01).
    pub fn key(&self, author_pubkey: &str) -> HeartbeatKey {
        HeartbeatKey {
            pubkey: author_pubkey.to_owned(),
            d: self.d.clone(),
        }
    }
}

/// Identity of a seller heartbeat: `(pubkey, d)`. This — never the event id — is how consumers
/// resolve the latest heartbeat for a seller (see [`ParsedHeartbeat::key`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HeartbeatKey {
    pub pubkey: String,
    pub d: String,
}

/// Reasons a kind-30340 event fails to parse as a maxplayer seller heartbeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeartbeatParseError {
    WrongKind(u16),
    MissingMaxplayerTag,
    /// The `d` tag is absent or not `maxplayer-seller`.
    WrongDTag(Option<String>),
    /// The `v` tag is absent or names a protocol major this reader does not speak (§2.1).
    WrongVersion(Option<String>),
    MissingTag(&'static str),
    InvalidAccepting(String),
    InvalidQueueDepth(String),
    InvalidRate(String),
    /// The `accepted_mints` tag is absent or lists no mint. §4.2 requires at least one — a seat a
    /// buyer cannot pay is not a tradeable seat, so this is a rejection rather than an empty list.
    MissingAcceptedMints,
}

impl std::fmt::Display for HeartbeatParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKind(kind) => {
                write!(f, "expected kind {SELLER_HEARTBEAT_KIND}, got {kind}")
            }
            Self::MissingMaxplayerTag => write!(f, "missing t={MAXPLAYER_TAG} tag"),
            Self::WrongDTag(d) => write!(
                f,
                "expected d={SELLER_HEARTBEAT_D}, got {}",
                d.as_deref().unwrap_or("<none>")
            ),
            Self::WrongVersion(version) => write!(
                f,
                "expected v={PROTOCOL_VERSION}, got {}",
                version.as_deref().unwrap_or("<none>")
            ),
            Self::MissingTag(name) => write!(f, "missing {name} tag"),
            Self::InvalidAccepting(value) => {
                write!(f, "accepting must be y/n, got {value}")
            }
            Self::InvalidQueueDepth(value) => write!(f, "invalid queue_depth: {value}"),
            Self::InvalidRate(value) => write!(f, "invalid rate: {value}"),
            Self::MissingAcceptedMints => write!(
                f,
                "missing {ACCEPTED_MINTS_TAG}: a seat must state at least one payable mint"
            ),
        }
    }
}

impl std::error::Error for HeartbeatParseError {}

/// Parse a kind-30340 event into a [`ParsedHeartbeat`] — the buyer-side seat reader. Rejects a
/// wrong kind, a missing `t=maxplayer` guard, a `d` other than `maxplayer-seller`, a `v` other than
/// the protocol major this build speaks, or a seat that states no payable mint.
///
/// This is the ONLY source of a seat's capability. Before #645 the mints and the harness label came
/// off the kind-31990 handler content, so a reader that still consulted 31990 would read residue —
/// a replaceable event a seat stopped republishing does not disappear from the relay.
pub fn parse_heartbeat(event: &EventDraft) -> Result<ParsedHeartbeat, HeartbeatParseError> {
    if event.kind != SELLER_HEARTBEAT_KIND {
        return Err(HeartbeatParseError::WrongKind(event.kind));
    }
    if !has_tag_value(&event.tags, "t", MAXPLAYER_TAG) {
        return Err(HeartbeatParseError::MissingMaxplayerTag);
    }
    let d = first_tag_value(&event.tags, "d");
    if d != Some(SELLER_HEARTBEAT_D) {
        return Err(HeartbeatParseError::WrongDTag(d.map(str::to_owned)));
    }
    let version = first_tag_value(&event.tags, "v");
    if version != Some(PROTOCOL_VERSION) {
        return Err(HeartbeatParseError::WrongVersion(version.map(str::to_owned)));
    }

    let accepting = match first_tag_value(&event.tags, "accepting") {
        Some("y") => true,
        Some("n") => false,
        Some(other) => return Err(HeartbeatParseError::InvalidAccepting(other.to_owned())),
        None => return Err(HeartbeatParseError::MissingTag("accepting")),
    };

    let queue_raw = first_tag_value(&event.tags, "queue_depth")
        .ok_or(HeartbeatParseError::MissingTag("queue_depth"))?;
    let queue_depth = queue_raw
        .parse()
        .map_err(|_| HeartbeatParseError::InvalidQueueDepth(queue_raw.to_owned()))?;

    let rate_raw =
        first_tag_value(&event.tags, "rate").ok_or(HeartbeatParseError::MissingTag("rate"))?;
    let rate_sats = rate_raw
        .parse()
        .map_err(|_| HeartbeatParseError::InvalidRate(rate_raw.to_owned()))?;

    let accepted_mints = accepted_mints_from_tags(&event.tags);
    if accepted_mints.is_empty() {
        return Err(HeartbeatParseError::MissingAcceptedMints);
    }

    Ok(ParsedHeartbeat {
        d: SELLER_HEARTBEAT_D.to_owned(),
        accepting,
        queue_depth,
        rate_sats,
        accepted_mints,
        agents: agents_from_tags(&event.tags),
        capability: SeatCapability::from_tags(&event.tags),
    })
}

/// Effective cadence (seconds): env override ([`HEARTBEAT_INTERVAL_ENV`]) wins over the
/// `[seller_heartbeat] interval_secs` config. A `0` or unparseable env value is ignored.
pub fn resolve_interval_secs(config: &crate::home::SellerHeartbeatConfig) -> u64 {
    match std::env::var(HEARTBEAT_INTERVAL_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => secs,
            _ => config.interval_secs,
        },
        Err(_) => config.interval_secs,
    }
}

/// Effective enablement: env override ([`HEARTBEAT_ENABLED_ENV`]) wins over the
/// `[seller_heartbeat] enabled` config. Unrecognised env values fall back to config.
pub fn resolve_enabled(config: &crate::home::SellerHeartbeatConfig) -> bool {
    match std::env::var(HEARTBEAT_ENABLED_ENV) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => config.enabled,
        },
        Err(_) => config.enabled,
    }
}

/// Effective relay-stall watchdog threshold (missed heartbeat intervals): env override
/// ([`HEARTBEAT_STALL_MISSED_INTERVALS_ENV`]) wins over the `[seller_heartbeat]
/// stall_missed_intervals` config. A `0` or unparseable value is ignored (falls back to config).
/// Clamped to at least 1 so a misconfiguration can never make the watchdog trip on the first tick.
pub fn resolve_stall_missed_intervals(config: &crate::home::SellerHeartbeatConfig) -> u32 {
    let configured = match std::env::var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV) {
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(n) if n > 0 => n,
            _ => config.stall_missed_intervals,
        },
        Err(_) => config.stall_missed_intervals,
    };
    configured.max(1)
}

fn first_tag<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a TagSpec> {
    tags.iter()
        .find(|tag| tag.0.first().map(String::as_str) == Some(name))
}

fn first_tag_value<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a str> {
    first_tag(tags, name).and_then(TagSpec::value)
}

fn has_tag_value(tags: &[TagSpec], name: &str, value: &str) -> bool {
    tags.iter().any(|tag| {
        tag.0.first().map(String::as_str) == Some(name)
            && tag.0.get(1).map(String::as_str) == Some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::SellerHeartbeatConfig;

    /// The mints a test seat states. §4.2 makes the tag required, so every draft carries one.
    fn mints() -> Vec<String> {
        vec!["https://testnut.example/Bitcoin".to_owned()]
    }

    fn draft(accepting: bool, queue_depth: u32, rate_sats: u64) -> HeartbeatDraft {
        HeartbeatDraft::new(accepting, queue_depth, rate_sats, mints())
    }

    fn tag_names(event: &EventDraft) -> Vec<&str> {
        let mut names: Vec<&str> = event.tags.iter().filter_map(TagSpec::first).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn heartbeat_addressable() {
        // Kind is in NIP-01's addressable range so the relay replaces it in place by (pubkey, d).
        assert!((30000..=39999).contains(&SELLER_HEARTBEAT_KIND));
        assert_eq!(SELLER_HEARTBEAT_KIND, 30340);

        // Keyed by (pubkey, d), never by event id.
        let parsed =
            parse_heartbeat(&draft(true, 0, 5).to_event_draft()).expect("parse own draft");
        let key = parsed.key("seller-pubkey-hex");
        assert_eq!(key.pubkey, "seller-pubkey-hex");
        assert_eq!(key.d, SELLER_HEARTBEAT_D);
        // The same author with the same d always resolves to one identity regardless of the
        // (superseded) event that carried it.
        assert_eq!(key, parsed.key("seller-pubkey-hex"));
    }

    /// RED-PROOF (#645): the announcement carries EXACTLY the §4.2 tag set — no more, no less.
    ///
    /// Set equality, not a list of presence checks, because a presence check cannot fail on a tag
    /// that should have LEFT. `protocol_versions` and `mobee_agent` satisfied every presence
    /// assertion this file used to make, and they are precisely the two tags #645 removes.
    /// Re-adding either turns this red; so does dropping `v` or `accepted_mints`.
    #[test]
    fn the_announcement_carries_exactly_the_spec_4_2_tag_set() {
        // ⚠ THIS IS THE **ABSENT** ROW, AND IT IS DELIBERATE — NOT A FIXTURE SOMEONE FORGOT TO
        // UPDATE WHEN #784 ADDED FOUR FIELDS.
        //
        // Every #784 tag is conditional on capability state this fixture does not set, so this
        // assertion stays green however many fields the emitter gains. That makes it worthless as a
        // tripwire for those fields, and exactly right as the proof of the property the design does
        // claim: **a seat that states no capability emits no capability tags** — absent means
        // unstated. The PRESENT row is
        // `the_announcement_carries_every_stated_capability_field` below, and neither row alone
        // separates a working emitter from a fixture that populates nothing.
        //
        // ★ The general trap, stated here because this test is where someone will meet it: an
        // exact-set assertion is a tripwire only for tags that are UNCONDITIONAL. It reads like it
        // constrains the SCHEMA and it constrains the schema only on the one input it builds.
        //
        // No roster stated: `agents` is the one optional tag (§4.2 cardinality 0..1).
        let bare = draft(true, 0, 7).to_event_draft();
        assert_eq!(
            tag_names(&bare),
            ["accepted_mints", "accepting", "d", "queue_depth", "rate", "t", "v"]
        );

        let with_roster = draft(true, 0, 7)
            .with_agents(vec!["claude".into()])
            .to_event_draft();
        assert_eq!(
            tag_names(&with_roster),
            ["accepted_mints", "accepting", "agents", "d", "queue_depth", "rate", "t", "v"]
        );

        // Named individually so a revert says WHICH tag came back rather than only diffing a list.
        for retired in ["protocol_versions", "mobee_agent"] {
            assert!(
                with_roster.tags.iter().all(|tag| tag.first() != Some(retired)),
                "#645 retired {retired} from the seat announcement"
            );
        }

        // …and every tag carries the value §4.2 specifies.
        assert_eq!(with_roster.kind, SELLER_HEARTBEAT_KIND);
        assert_eq!(first_tag_value(&with_roster.tags, "d"), Some(SELLER_HEARTBEAT_D));
        assert_eq!(first_tag_value(&with_roster.tags, "t"), Some(MAXPLAYER_TAG));
        assert_eq!(first_tag_value(&with_roster.tags, "v"), Some(PROTOCOL_VERSION));
        assert_eq!(first_tag_value(&with_roster.tags, "rate"), Some("7"));
        assert_eq!(first_tag_value(&with_roster.tags, "accepting"), Some("y"));
        assert_eq!(first_tag_value(&with_roster.tags, "queue_depth"), Some("0"));
        assert_eq!(accepted_mints_from_tags(&with_roster.tags), mints());
        assert_eq!(agents_from_tags(&with_roster.tags), vec!["claude"]);
        assert!(bare.content.is_empty(), "capability rides tags, never content");
    }

    /// The **PRESENT** row: the exact tag set of a beat that states EVERY #784 field.
    ///
    /// This is the one that goes red when a tag is added, renamed or dropped, because its input
    /// states all five names. Its sibling above states none, so between them a tag cannot appear
    /// without being declared here nor vanish without failing there. **One row alone cannot tell a
    /// working emitter from a fixture that populates nothing** — the same argument
    /// `capability::probe_capabilities`'s own `a_stock_image_with_no_toolchain_advertises_nothing`
    /// makes for its positive control, applied to a conditional-tag schema.
    ///
    /// ★ FOUR #784 FIELDS PRODUCE FIVE TAG NAMES, because Harness contributes both `harness_family`
    /// and `harness_variant`. A reader who counts four builds a parser that is one short.
    ///
    /// `harness_model` appears TWICE and that is the point: it is the one tag emitted once per
    /// serving harness, so entries exceed distinct names by exactly the number of extra models. The
    /// other four are single tags — two multi-value, two single-value.
    #[test]
    fn the_announcement_carries_every_stated_capability_field() {
        let event = draft(true, 0, 7)
            .with_agents(vec!["claude".into()])
            .with_capability(full_capability())
            .to_event_draft();

        assert_eq!(
            tag_names(&event),
            [
                "accepted_mints",
                "accepting",
                "agents",
                "capabilities",
                "d",
                "hardware",
                "harness_family",
                "harness_model",
                "harness_model",
                "harness_variant",
                "queue_depth",
                "rate",
                "t",
                "v",
            ]
        );

        // The denominator, stated rather than left to be counted off the list above: 8 pre-#784 tags
        // (7 plus `agents`) and 6 capability tags, because `full_capability` carries two models.
        assert_eq!(event.tags.len(), 14, "14 tags, 13 distinct names: {:?}", tag_names(&event));
        assert_eq!(
            tag_names(&event).iter().filter(|name| **name == HARNESS_MODEL_TAG).count(),
            2,
            "one harness_model tag per model, never one tag carrying both"
        );
    }

    /// RED-PROOF (#645): the buyer-side seat reader takes mints AND roster off the kind-30340
    /// announcement. Before #645 both lived in the kind-31990 handler content, which a seat no
    /// longer republishes — a reader still sourcing them there would read relay residue, because a
    /// replaceable event a seat stops publishing does not disappear.
    #[test]
    fn the_buyer_reader_resolves_mints_and_agents_from_the_announcement() {
        let announced = vec![
            "https://testnut.example/Bitcoin".to_owned(),
            "https://second.example/Bitcoin".to_owned(),
        ];
        let event = HeartbeatDraft::new(true, 0, 21, announced.clone())
            .with_agents(vec!["claude".into(), "codex".into()])
            .to_event_draft();

        let seat = parse_heartbeat(&event).expect("a buyer parses the seat announcement");
        // Order is preserved on both lists: entry 0 is the seat's own preference, and a reader
        // that reordered would pay on — or dispatch to — something the seat ranked lower.
        assert_eq!(seat.accepted_mints, announced);
        assert_eq!(seat.agents, vec!["claude", "codex"]);
        assert_eq!(seat.rate_sats, 21);
        assert!(seat.accepting);
        assert_eq!(seat.queue_depth, 0);
    }

    /// A seat that names no payable mint is REJECTED, never read as "pays on anything".
    #[test]
    fn a_seat_stating_no_mint_is_not_a_resolvable_seat() {
        let mut absent = draft(true, 0, 5).to_event_draft();
        absent.tags.retain(|tag| tag.first() != Some(ACCEPTED_MINTS_TAG));
        assert_eq!(
            parse_heartbeat(&absent),
            Err(HeartbeatParseError::MissingAcceptedMints)
        );

        // Present-but-valueless is the same rejection: the seat still named nothing payable.
        let mut valueless = draft(true, 0, 5).to_event_draft();
        for tag in valueless.tags.iter_mut() {
            if tag.first() == Some(ACCEPTED_MINTS_TAG) {
                tag.0.truncate(1);
            }
        }
        assert_eq!(
            parse_heartbeat(&valueless),
            Err(HeartbeatParseError::MissingAcceptedMints)
        );
    }

    #[test]
    fn advertises_every_harness_in_preference_order() {
        let draft = heartbeat_for_state(0, true, 5, mints(), vec!["claude".into(), "codex".into()], cap(&["claude", "codex"]))
            .to_event_draft();
        let tag = first_tag(&draft.tags, "agents").expect("agents tag");
        assert_eq!(tag.0, vec!["agents", "claude", "codex"]);
        // The reader gets the same ordered list back.
        let parsed = parse_heartbeat(&draft).expect("round-trip");
        assert_eq!(parsed.agents, vec!["claude", "codex"]);
    }

    #[test]
    fn a_seller_stating_no_harness_omits_the_roster_tag() {
        // A raw `agent_command` seller has no preset label, so it advertises no roster and the tag
        // is omitted rather than emitted empty. It IS serving (hence `true`), which is why an
        // unstated list must never read as dark.
        let stated_none = heartbeat_for_state(0, true, 5, mints(), Vec::new(), SeatCapability::default()).to_event_draft();
        assert_eq!(stated_none, draft(true, 0, 5).to_event_draft());
        assert!(
            first_tag(&stated_none.tags, AGENT_TAG).is_none(),
            "an unstated harness list must omit the tag, never emit it empty"
        );
        assert!(parse_heartbeat(&stated_none).expect("parse").agents.is_empty());
    }

    #[test]
    fn accepting_flips_with_in_flight_state() {
        let idle = heartbeat_for_state(0, true, 5, mints(), Vec::new(), SeatCapability::default());
        assert!(idle.accepting);
        assert_eq!(idle.queue_depth, 0);
        assert_eq!(
            first_tag_value(&idle.to_event_draft().tags, "accepting"),
            Some("y")
        );

        let busy = heartbeat_for_state(1, true, 5, mints(), Vec::new(), SeatCapability::default());
        assert!(!busy.accepting);
        assert_eq!(busy.queue_depth, 1);
        assert_eq!(
            first_tag_value(&busy.to_event_draft().tags, "accepting"),
            Some("n")
        );
        assert_eq!(
            first_tag_value(&busy.to_event_draft().tags, "queue_depth"),
            Some("1")
        );
    }

    /// The whole point of the change: an idle seat with nothing serving is NOT accepting.
    ///
    /// Written as the full truth table so every row is pinned, not just the one that motivated the
    /// change. Transposing the two arguments no longer even compiles — `in_flight` is a `u32` and
    /// `anything_serving` a `bool` — which is a stronger guard than the assertion below; the table
    /// stays because it pins the four OUTPUTS, which the types cannot.
    #[test]
    fn accepting_requires_a_free_slot_and_something_serving() {
        let accepting_of = |in_flight, serving| {
            let draft = heartbeat_for_state(in_flight, serving, 5, mints(), Vec::new(), SeatCapability::default()).to_event_draft();
            (
                first_tag_value(&draft.tags, "accepting")
                    .expect("accepting tag")
                    .to_owned(),
                first_tag_value(&draft.tags, "queue_depth")
                    .expect("queue_depth tag")
                    .to_owned(),
            )
        };

        assert_eq!(accepting_of(0, true), ("y".into(), "0".into()), "idle + serving");
        assert_eq!(accepting_of(1, true), ("n".into(), "1".into()), "busy");
        // The row this change adds. Before it, a fully dark seat published `y` and kept drawing work
        // it could only decline.
        assert_eq!(accepting_of(0, false), ("n".into(), "0".into()), "idle + dark");
        assert_eq!(accepting_of(1, false), ("n".into(), "1".into()), "busy + dark");

        // And dark is DISTINGUISHABLE from busy, which the pair could not express before: both say
        // "not taking work", and `queue_depth` says which reason.
        assert_ne!(accepting_of(0, false), accepting_of(1, true));
    }

    /// `queue_depth` must carry the DEPTH, not a busy flag.
    ///
    /// This is the assertion a `bool` parameter made unwriteable, and its absence is what let #313
    /// live: the wire reported `1` for a seat holding five finished jobs, and `1` reads as plausible.
    /// Any depth above 1 is therefore the discriminator — it cannot be produced by a flag.
    #[test]
    fn queue_depth_is_the_depth_not_a_busy_flag() {
        for depth in [2_u32, 3, 17] {
            let draft = heartbeat_for_state(depth, true, 5, mints(), Vec::new(), SeatCapability::default()).to_event_draft();
            assert_eq!(
                first_tag_value(&draft.tags, "queue_depth"),
                Some(depth.to_string().as_str()),
                "queue_depth must publish the count itself, not a 0/1 cast of it"
            );
            assert_eq!(
                first_tag_value(&draft.tags, "accepting"),
                Some("n"),
                "any non-zero depth means the seat is occupied"
            );
        }

        // And the boundary that #313 got wrong in the field: nothing in flight ⇒ available, no
        // matter how much this seat has done in the past. The store-side half of this is
        // `a_store_holding_only_terminal_jobs_reports_none_in_flight`.
        let free = heartbeat_for_state(0, true, 5, mints(), Vec::new(), SeatCapability::default()).to_event_draft();
        assert_eq!(first_tag_value(&free.tags, "accepting"), Some("y"));
        assert_eq!(first_tag_value(&free.tags, "queue_depth"), Some("0"));
    }

    /// #747 — the terminal beat says `accepting=n`, and there is no input that makes it say
    /// anything else. An idle seat is the case that matters: it is the one whose ordinary beat says
    /// `y`, so it is the one whose stopped-forever announcement advertises an open seat forever.
    #[test]
    fn the_terminal_beat_is_accepting_n_whatever_the_seat_was_doing() {
        for in_flight in [0_u32, 1, 9] {
            let event = retraction_for_state(in_flight, 5, mints(), vec!["claude".into()], cap(&["claude"]))
                .to_event_draft();
            assert_eq!(
                first_tag_value(&event.tags, "accepting"),
                Some("n"),
                "a terminal beat must retract the seat (in_flight={in_flight})"
            );
            // The live count rides it unchanged: leaving is not a licence to misreport held work.
            assert_eq!(
                first_tag_value(&event.tags, "queue_depth"),
                Some(in_flight.to_string().as_str())
            );
            // Capability is unchanged — the seat still knows how to work, it is just not taking any.
            assert_eq!(agents_from_tags(&event.tags), vec!["claude"]);
            assert_eq!(accepted_mints_from_tags(&event.tags), mints());
        }
    }

    /// #747 RED-PROOF — the retraction must land at the SAME addressable slot as the live beat, or
    /// it corrects nothing: kind-30340 is replaceable by `(pubkey, d)`, so only an event at that
    /// address can overwrite the `accepting=y` a departed seat left standing. Anything published
    /// under a different `d` (or kind) would sit BESIDE the stale announcement rather than replace
    /// it, and the directory would go on reading the old one.
    #[test]
    fn the_terminal_beat_replaces_the_live_one_at_the_same_address() {
        let live = heartbeat_for_state(0, true, 5, mints(), vec!["claude".into()], cap(&["claude"])).to_event_draft();
        let terminal = retraction_for_state(0, 5, mints(), vec!["claude".into()], cap(&["claude"])).to_event_draft();

        assert_eq!(first_tag_value(&live.tags, "accepting"), Some("y"));
        assert_eq!(terminal.kind, live.kind, "same kind, or it is not a replacement");
        assert_eq!(
            first_tag_value(&terminal.tags, "d"),
            first_tag_value(&live.tags, "d"),
            "same d, or the relay keeps BOTH and the stale accepting=y survives"
        );

        // A buyer reads it as a full, valid §4.2 announcement — including `v`, whose absence is the
        // pre-v1 shape a stopped seat's residue is stuck in — and reads the seat as closed.
        let parsed = parse_heartbeat(&terminal).expect("a terminal beat is an ordinary heartbeat");
        assert!(!parsed.accepting, "the retraction must read as a closed seat");
        assert_eq!(first_tag_value(&terminal.tags, "v"), Some(PROTOCOL_VERSION));
        assert_eq!(
            parsed.key("seller-pubkey-hex"),
            parse_heartbeat(&live).expect("parse live").key("seller-pubkey-hex"),
            "the two beats must resolve to ONE identity — that is what makes this a retraction"
        );
        assert_eq!(tag_names(&terminal), tag_names(&live), "no new tag, no missing tag");
    }

    #[test]
    fn reader_round_trip() {
        let announced = vec!["https://a.example/x".to_owned(), "https://b.example/y".to_owned()];
        let draft = HeartbeatDraft::new(false, 3, 21, announced.clone());
        let parsed = parse_heartbeat(&draft.to_event_draft()).expect("round-trip parse");
        assert_eq!(parsed.d, SELLER_HEARTBEAT_D);
        assert!(!parsed.accepting);
        assert_eq!(parsed.queue_depth, 3);
        assert_eq!(parsed.rate_sats, 21);
        assert_eq!(parsed.accepted_mints, announced);
    }

    #[test]
    fn parse_rejects_wrong_kind_and_missing_guards() {
        let mut wrong_kind = draft(true, 0, 5).to_event_draft();
        wrong_kind.kind = 30341;
        assert_eq!(
            parse_heartbeat(&wrong_kind),
            Err(HeartbeatParseError::WrongKind(30341))
        );

        // Drop the t=maxplayer guard.
        let mut no_maxplayer = draft(true, 0, 5).to_event_draft();
        no_maxplayer.tags.retain(|tag| tag.first() != Some("t"));
        assert_eq!(
            parse_heartbeat(&no_maxplayer),
            Err(HeartbeatParseError::MissingMaxplayerTag)
        );

        // Wrong d.
        let mut wrong_d = draft(true, 0, 5).to_event_draft();
        for tag in wrong_d.tags.iter_mut() {
            if tag.first() == Some("d") {
                tag.0[1] = "not-maxplayer-seller".to_owned();
            }
        }
        assert_eq!(
            parse_heartbeat(&wrong_d),
            Err(HeartbeatParseError::WrongDTag(Some(
                "not-maxplayer-seller".to_owned()
            )))
        );

        // A foreign protocol major, and an announcement with no `v` at all. #645 put the tag on
        // this event; gating on it here is what stops it from being decoration (§2.1).
        let mut wrong_version = draft(true, 0, 5).to_event_draft();
        for tag in wrong_version.tags.iter_mut() {
            if tag.first() == Some("v") {
                tag.0[1] = "2".to_owned();
            }
        }
        assert_eq!(
            parse_heartbeat(&wrong_version),
            Err(HeartbeatParseError::WrongVersion(Some("2".to_owned())))
        );

        let mut no_version = draft(true, 0, 5).to_event_draft();
        no_version.tags.retain(|tag| tag.first() != Some("v"));
        assert_eq!(
            parse_heartbeat(&no_version),
            Err(HeartbeatParseError::WrongVersion(None))
        );
    }

    #[test]
    fn interval_respects_config() {
        // Serialize env access across the two env-reading tests (process-global env).
        // SAFETY (edition 2024): mutations are serialized by ENV_LOCK and these are the only
        // tests that touch the heartbeat env vars.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        unsafe {
            std::env::remove_var(HEARTBEAT_INTERVAL_ENV);
            std::env::remove_var(HEARTBEAT_ENABLED_ENV);
        }

        // Default cadence is 300s (5 min).
        let default_cfg = SellerHeartbeatConfig::default();
        assert_eq!(default_cfg.interval_secs, 300);
        assert!(default_cfg.enabled);
        assert_eq!(resolve_interval_secs(&default_cfg), 300);

        // Config override (no env) is honoured.
        let custom = SellerHeartbeatConfig {
            enabled: true,
            interval_secs: 42,
            ..SellerHeartbeatConfig::default()
        };
        assert_eq!(resolve_interval_secs(&custom), 42);

        // Env override wins over config.
        unsafe { std::env::set_var(HEARTBEAT_INTERVAL_ENV, "3") };
        assert_eq!(resolve_interval_secs(&custom), 3);
        // A zero/garbage env value is ignored (falls back to config).
        unsafe { std::env::set_var(HEARTBEAT_INTERVAL_ENV, "0") };
        assert_eq!(resolve_interval_secs(&custom), 42);
        unsafe { std::env::set_var(HEARTBEAT_INTERVAL_ENV, "nonsense") };
        assert_eq!(resolve_interval_secs(&custom), 42);
        unsafe { std::env::remove_var(HEARTBEAT_INTERVAL_ENV) };
    }

    #[test]
    fn enabled_respects_env_override() {
        // SAFETY (edition 2024): serialized by ENV_LOCK; see `interval_respects_config`.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        unsafe { std::env::remove_var(HEARTBEAT_ENABLED_ENV) };

        let enabled_cfg = SellerHeartbeatConfig {
            enabled: true,
            interval_secs: 300,
            ..SellerHeartbeatConfig::default()
        };
        assert!(resolve_enabled(&enabled_cfg));
        unsafe { std::env::set_var(HEARTBEAT_ENABLED_ENV, "0") };
        assert!(!resolve_enabled(&enabled_cfg));
        unsafe { std::env::set_var(HEARTBEAT_ENABLED_ENV, "true") };
        assert!(resolve_enabled(&enabled_cfg));
        unsafe { std::env::remove_var(HEARTBEAT_ENABLED_ENV) };
    }

    #[test]
    fn stall_missed_intervals_respects_env_and_clamps() {
        // SAFETY (edition 2024): serialized by ENV_LOCK; see `interval_respects_config`.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        unsafe { std::env::remove_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV) };

        // Default is 3.
        let default_cfg = SellerHeartbeatConfig::default();
        assert_eq!(default_cfg.stall_missed_intervals, 3);
        assert_eq!(resolve_stall_missed_intervals(&default_cfg), 3);

        // Config override (no env) is honoured.
        let custom = SellerHeartbeatConfig {
            stall_missed_intervals: 5,
            ..SellerHeartbeatConfig::default()
        };
        assert_eq!(resolve_stall_missed_intervals(&custom), 5);

        // Env override wins over config.
        unsafe { std::env::set_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV, "2") };
        assert_eq!(resolve_stall_missed_intervals(&custom), 2);
        // Zero/garbage env falls back to config.
        unsafe { std::env::set_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV, "0") };
        assert_eq!(resolve_stall_missed_intervals(&custom), 5);
        unsafe { std::env::set_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV, "nonsense") };
        assert_eq!(resolve_stall_missed_intervals(&custom), 5);
        unsafe { std::env::remove_var(HEARTBEAT_STALL_MISSED_INTERVALS_ENV) };

        // A config of 0 is clamped up to 1 (never trips on the first tick).
        let zero = SellerHeartbeatConfig {
            stall_missed_intervals: 0,
            ..SellerHeartbeatConfig::default()
        };
        assert_eq!(resolve_stall_missed_intervals(&zero), 1);
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The WIRE round-trip: emit → sign → publish to a real in-process relay over a websocket →
    /// fetch by `(pubkey, kind, d)` → parse. Every other test in this file compares a draft to a
    /// parser in memory, which cannot fail on anything the relay or the tag encoding does.
    ///
    /// ⚠ **WHAT THIS DOES NOT PROVE, and the real bound is sharper than "it is only a unit test".**
    ///
    /// `parse_heartbeat` has NO Rust production caller — all 17 call sites are in this test module.
    /// **The readers that serve real users are JavaScript and TypeScript**: `web/network/js/parse.js`,
    /// `web/network/js/kinds.js`, `web/app/src/model/kinds.ts`, and `web/app/scripts/bake-snapshot.mjs`
    /// all read kind-30340 today.
    ///
    /// ⇒ **So this wire format has TWO INDEPENDENT PARSERS IN TWO LANGUAGES, and nothing tests them
    /// against each other.** This test pins Rust emitter ↔ Rust parser — the parser nobody ships. The
    /// JS suite pins JS parser ↔ a fixture a human typed, which is a CLAIM about what Rust emits
    /// rather than a reading of it. Neither proves the shipped parser agrees with the real emission.
    ///
    /// The golden artifact below is what closes that: set `MAXPLAYER_WRITE_GOLDEN_30340` to a path and
    /// this writes the exact signed JSON it published, for the JS suite to consume as a fixture. One
    /// emitter, one artifact, two parsers asserting on the same bytes. Off by default so CI is
    /// unaffected and no test writes outside its sandbox.
    ///
    /// The `Event` → [`EventDraft`] conversion below is TEST-ONLY scaffolding: production has none, so
    /// it is not a path anything ships.
    ///
    /// ⛔ Built on a bare `LocalRelay`, deliberately NOT on the `post_job_async` fixture the other
    /// relay tests in this crate use. Posting a job auto-awards, and that path resolves a fee floor
    /// at the home's real mint under `live-mints`. **The two patterns look interchangeable and only
    /// one of them spends money.**
    #[cfg(feature = "gateway")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stated_capability_survives_a_real_relay_round_trip() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, Filter, Keys, Kind};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let url = relay.url().await.to_string();

        let seat = Keys::generate();
        let client = Client::new(seat.clone());
        client.add_relay(&url).await.expect("add relay");
        client.connect().await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let emitted = draft(true, 0, 7)
            .with_agents(vec!["claude".to_owned()])
            .with_capability(full_capability())
            .to_event_draft();
        let event = crate::gateway::nostr::event_builder(&emitted)
            .expect("event builder")
            .sign_with_keys(&seat)
            .expect("sign");
        client.send_event(&event).await.expect("publish the announcement");

        // Resolve by (pubkey, kind, d) — never by event id. An addressable event is superseded in
        // place, so a by-id lookup on a replaced beat reads as "seller gone".
        let fetched = client
            .fetch_events(
                Filter::new()
                    .author(seat.public_key())
                    .kind(Kind::Custom(SELLER_HEARTBEAT_KIND))
                    .identifier(SELLER_HEARTBEAT_D),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("fetch");
        let back = fetched
            .first()
            .expect("the relay returns the announcement it stored");

        let read_back = EventDraft::new(
            back.kind.as_u16(),
            back.tags
                .iter()
                .map(|tag| TagSpec(tag.clone().to_vec()))
                .collect(),
            back.content.clone(),
        );
        let parsed = parse_heartbeat(&read_back).expect("a buyer parses the beat off the wire");

        // The whole capability compared as ONE value, not field by field: a per-field assertion
        // silently stops covering any field added later, which is the fixture-scoping trap the two
        // tag-set rows in this file exist to close.
        assert_eq!(
            parsed.capability,
            full_capability(),
            "every #784 field must survive sign → websocket → relay store → fetch → parse"
        );
        // The pre-#784 fields too, so a round-trip that silently dropped the base announcement while
        // carrying the new tags cannot pass.
        assert_eq!(parsed.agents, vec!["claude".to_owned()]);
        assert_eq!(parsed.accepted_mints, mints());
        assert_eq!(parsed.rate_sats, 7);
        assert!(parsed.accepting);

        // The cross-language artifact. Opt-in by env var: a test that wrote to a fixed path on every
        // run would fail wherever that path does not exist, and CI is one such place.
        //
        // ⚠ `id`, `sig`, `pubkey` and `created_at` vary per run — the keys are generated and the
        // timestamp is now. The TAGS are deterministic, and they are the whole subject: a consumer
        // asserting on tag names and values gets a stable fixture, one asserting on the event id gets
        // a test that fails every run.
        if let Ok(path) = std::env::var("MAXPLAYER_WRITE_GOLDEN_30340") {
            use nostr_sdk::JsonUtil as _;
            std::fs::write(&path, back.as_json()).expect("write the golden kind-30340");
        }
    }

    /// A fully-stated #784 capability: both filterable list fields, two paired models, and both
    /// display-only fields. One fixture, so the beat/claim comparison below compares the same input
    /// through two emitters rather than two hand-written tag sets made to agree.
    fn full_capability() -> SeatCapability {
        SeatCapability {
            harness_families: vec!["claude-code".to_owned(), "codex".to_owned()],
            models: vec![
                HarnessModel {
                    family: "claude-code".to_owned(),
                    model: "claude-opus-5".to_owned(),
                },
                HarnessModel {
                    family: "codex".to_owned(),
                    model: "gpt-5.6-sol[low]".to_owned(),
                },
            ],
            capabilities: vec!["rust".to_owned(), "node".to_owned()],
            harness_variant: Some("my-fork".to_owned()),
            hardware: Some("mac studio, 64GB".to_owned()),
        }
    }

    #[test]
    fn a_stated_capability_round_trips_through_the_beat() {
        let event = draft(true, 0, 5)
            .with_agents(vec!["claude".to_owned()])
            .with_capability(full_capability())
            .to_event_draft();
        let parsed = parse_heartbeat(&event).expect("a buyer parses the seat announcement");
        assert_eq!(
            parsed.capability,
            full_capability(),
            "every advertised field must survive the wire unchanged"
        );
    }

    #[test]
    fn a_seat_that_states_no_capability_emits_no_capability_tags() {
        // The compatibility property the no-v-bump rollout rests on: a seat that has not been taught
        // to fill SeatCapability publishes EXACTLY the tag set it always did. Asserted as an exact
        // set, not as "does not contain harness_family" — a per-tag check would pass while some
        // other new tag leaked in.
        let event = draft(true, 0, 5)
            .with_agents(vec!["claude".to_owned()])
            .to_event_draft();
        assert_eq!(
            tag_names(&event),
            vec!["accepted_mints", "accepting", "agents", "d", "queue_depth", "rate", "t", "v"],
            "an unstated capability must add nothing to the §4.2 tag set"
        );
        assert_eq!(parse_heartbeat(&event).expect("parses").capability, SeatCapability::default());
    }

    #[test]
    fn an_older_beat_without_capability_tags_still_parses() {
        // The other half of no-v-bump: a NEW reader must accept an OLD seat's beat. Absence parses to
        // unstated rather than failing, which is what lets emitters and readers ship in one release
        // without partitioning the fleet.
        let event = draft(true, 0, 5).to_event_draft();
        let parsed = parse_heartbeat(&event).expect("an old beat must still parse");
        assert!(parsed.capability.harness_families.is_empty());
        assert!(parsed.capability.models.is_empty());
        assert!(parsed.capability.capabilities.is_empty());
        assert_eq!(parsed.capability.harness_variant, None);
        assert_eq!(parsed.capability.hardware, None);
    }

    /// The capability a roster of these harness names implies, with nothing observed yet — what
    /// `Advertisement::capability()` produces for a seat that has advertised but not yet probed.
    fn cap(agents: &[&str]) -> SeatCapability {
        let names: Vec<String> = agents.iter().map(|name| (*name).to_owned()).collect();
        SeatCapability::from_roster(&names, &[])
    }

    fn observed(harness: &str, model: &str) -> RosterModel {
        RosterModel {
            harness: harness.to_owned(),
            model: model.to_owned(),
        }
    }

    #[test]
    fn an_observed_model_is_advertised_under_its_harnesss_wire_family() {
        // The roster speaks PRESET names; the wire speaks families. `claude` the preset must reach
        // the wire as `claude-code` the family, or a buyer filtering the documented family value
        // matches nothing and the field is decorative.
        let capability = SeatCapability::from_roster(
            &["claude".to_owned()],
            &[observed("claude", "claude-opus-5")],
        );
        assert_eq!(
            capability.models,
            vec![HarnessModel {
                family: "claude-code".to_owned(),
                model: "claude-opus-5".to_owned(),
            }]
        );
    }

    #[test]
    fn a_roster_name_with_no_wire_family_contributes_no_model() {
        // A custom preset still names itself in `agents` — we CAN dispatch it — but it has no family
        // in the spec vocabulary, so there is no key to hang its model on. Emitting it under a
        // guessed family would put a value on a filterable field that no seat actually advertises.
        let capability = SeatCapability::from_roster(
            &["my-fork".to_owned()],
            &[observed("my-fork", "some-model")],
        );
        assert!(capability.harness_families.is_empty());
        assert!(
            capability.models.is_empty(),
            "an unmappable name must drop its model, never guess a family: {:?}",
            capability.models
        );
        // And the tag surface agrees — the check that matters, since that is what a buyer reads.
        assert!(harness_model_tags(&capability.models).is_empty());
    }

    #[test]
    fn one_family_keeps_distinct_models_and_collapses_identical_ones() {
        // Two entries can alias ONE family. Different models are two true statements about two real
        // serving harnesses and both must stand — collapsing them would hide one from a filter.
        // Identical models are one statement said twice and add nothing.
        let distinct = SeatCapability::from_roster(
            &["claude".to_owned(), "claude".to_owned()],
            &[
                observed("claude", "claude-opus-5"),
                observed("claude", "claude-haiku-4-5"),
            ],
        );
        assert_eq!(distinct.models.len(), 2, "distinct models must both stand: {:?}", distinct.models);
        assert_eq!(
            distinct.harness_families,
            vec!["claude-code"],
            "the FAMILY list still dedupes — two aliases are one family"
        );

        let repeated = SeatCapability::from_roster(
            &["claude".to_owned(), "claude".to_owned()],
            &[
                observed("claude", "claude-opus-5"),
                observed("claude", "claude-opus-5"),
            ],
        );
        assert_eq!(repeated.models.len(), 1, "an identical pair says nothing extra: {:?}", repeated.models);
    }

    #[test]
    fn the_beat_carries_the_models_the_roster_observed() {
        // End to end through the emitter a seat actually calls, then read back off the EVENT rather
        // than off the struct — the struct being right proves nothing about what a buyer receives.
        let event = heartbeat_for_state(
            0,
            true,
            5,
            mints(),
            vec!["claude".to_owned(), "codex".to_owned()],
            SeatCapability::from_roster(
                &["claude".to_owned(), "codex".to_owned()],
                &[
                    observed("claude", "claude-opus-5"),
                    observed("codex", "gpt-5.6-terra[medium]"),
                ],
            ),
        )
        .to_event_draft();

        let read_back = harness_models_from_tags(&event.tags);
        assert_eq!(read_back.len(), 2, "both models must reach the wire: {read_back:?}");
        assert_eq!(
            read_back,
            vec![
                HarnessModel {
                    family: "claude-code".to_owned(),
                    model: "claude-opus-5".to_owned(),
                },
                HarnessModel {
                    family: "codex".to_owned(),
                    model: "gpt-5.6-terra[medium]".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn a_seat_that_observed_no_model_emits_no_model_tag() {
        // The default state of every seat before a probe has reported anything. Absent means
        // unstated, and nothing else on the beat shifts because of it.
        let event = heartbeat_for_state(0, true, 5, mints(), vec!["claude".to_owned()], cap(&["claude"]))
            .to_event_draft();
        assert!(harness_models_from_tags(&event.tags).is_empty());
        // The POSITIVE CONTROL for the assertion above: the same event still carries the family, so
        // the empty read is a real absence and not a beat that emitted no capability at all.
        assert_eq!(harness_families_from_tags(&event.tags), vec!["claude-code"]);
    }

    // ── Rocky blocker 4: the readers must enforce the whitespace contract the emitters already do ──
    //
    // §4.5.2 says a whitespace-only value is ABSENT. `single_value_tag` has always honoured that on
    // the way out, so every tag WE write is clean — and that is exactly why the readers could stay
    // wrong without any test noticing: a round-trip through our own emitter can never produce the
    // input that breaks them.
    //
    // So these tags are built by hand, as a hostile or merely sloppy third-party seat would put them
    // on the wire. All five #784 readers, three shapes each: whitespace-only, padded, and mixed.
    #[test]
    fn a_reader_treats_a_whitespace_only_wire_value_as_unstated() {
        let hostile = vec![
            TagSpec::new([HARNESS_FAMILY_TAG, "   ", " claude-code ", "\t\n"]),
            TagSpec::new([CAPABILITIES_TAG, " ", " rust ", "\t"]),
            TagSpec::new([HARNESS_VARIANT_TAG, "   "]),
            TagSpec::new([HARDWARE_TAG, " \t "]),
            TagSpec::new([HARNESS_MODEL_TAG, "  ", "claude-opus-5"]),
            TagSpec::new([HARNESS_MODEL_TAG, "claude-code", "   "]),
            TagSpec::new([HARNESS_MODEL_TAG, " codex ", " gpt-5 "]),
        ];

        // The list readers: blanks are dropped, padded values are TRIMMED rather than kept as-is.
        // Trimming is the half that matters for awards — a padded family that stayed padded would
        // never equal the family a buyer names, so the seat would advertise something unmatchable.
        assert_eq!(
            harness_families_from_tags(&hostile),
            vec!["claude-code"],
            "a blank family states nothing and a padded one must normalize to what a buyer names"
        );
        assert_eq!(
            capabilities_from_tags(&hostile),
            vec!["rust"],
            "capabilities are filterable and a buyer commits sats on them — a blank token would be \
             a capability no seat can be held to"
        );

        // The single-value readers: whitespace-only is `None`, never `Some("   ")`.
        assert_eq!(
            harness_variant_from_tags(&hostile),
            None,
            "a present-but-blank variant is not a state this field has"
        );
        assert_eq!(
            hardware_from_tags(&hostile),
            None,
            "never filtered, but a blank hardware string still renders as stated-and-empty"
        );

        // The PAIR reader. Either half blank makes the pair unpairable, and a stated model under a
        // blank family is not a partial answer worth salvaging.
        assert_eq!(
            harness_models_from_tags(&hostile),
            vec![HarnessModel {
                family: "codex".to_owned(),
                model: "gpt-5".to_owned(),
            }],
            "a blank family or a blank model drops the whole pair; the padded pair normalizes"
        );
    }

    #[test]
    fn a_whitespace_padded_advertisement_still_matches_the_buyer_that_names_it() {
        // The POSITIVE CONTROL for the test above, and the reason trimming is not cosmetic. Without
        // it every assertion above is satisfied by a reader that returns nothing at all, which would
        // refuse every award instead of just the blank ones.
        let padded = vec![
            TagSpec::new([HARNESS_FAMILY_TAG, " claude-code "]),
            TagSpec::new([CAPABILITIES_TAG, "  rust  ", " node "]),
            TagSpec::new([HARNESS_VARIANT_TAG, "  pro  "]),
            TagSpec::new([HARDWARE_TAG, " mac studio, 64GB "]),
        ];
        assert_eq!(harness_families_from_tags(&padded), vec!["claude-code"]);
        assert_eq!(capabilities_from_tags(&padded), vec!["rust", "node"]);
        assert_eq!(harness_variant_from_tags(&padded), Some("pro".to_owned()));
        assert_eq!(
            hardware_from_tags(&padded),
            Some("mac studio, 64GB".to_owned()),
            "interior whitespace is CONTENT and must survive — only the edges are noise"
        );
    }

    #[test]
    fn from_tags_is_the_exact_mirror_of_the_write_path() {
        // The read side of C2. `filterable_tags()` is the one writer; this asserts `from_tags()` is
        // the one reader that recovers exactly what it wrote, so the two halves cannot drift into
        // spelling the same field differently.
        let written = full_capability();
        let mut tags = written.filterable_tags();
        tags.extend(written.display_tags());
        assert_eq!(tags.len(), 6, "denominator: 4 filterable + 2 display: {tags:?}");

        assert_eq!(SeatCapability::from_tags(&tags), written);
    }

    #[test]
    fn from_tags_reads_a_claim_which_carries_no_display_fields() {
        // A claim emits filterable tags only. Reading all five off it must recover the filterable
        // three and leave the display two `None` — absent means unstated, with no per-kind rule.
        let written = full_capability();
        let claim_tags = written.filterable_tags();

        let read = SeatCapability::from_tags(&claim_tags);
        assert_eq!(read.harness_families, written.harness_families);
        assert_eq!(read.models, written.models);
        assert_eq!(read.capabilities, written.capabilities);
        assert_eq!(read.harness_variant, None, "a claim states no variant");
        assert_eq!(read.hardware, None, "a claim states no hardware");
        // POSITIVE CONTROL: the display fields really were set on the source, so the two `None`s
        // above are a genuine absence on the claim and not a fixture that never carried them.
        assert!(written.harness_variant.is_some() && written.hardware.is_some());
    }

    #[test]
    fn the_beat_and_the_claim_spell_every_filterable_field_identically() {
        // C2 as a test rather than a rule: the buyer's award filter reads the CLAIM while a seat
        // directory reads the BEAT, so a field spelled differently on the two would be filtered
        // differently from how it is displayed, and nothing would flag it. Both go through
        // `filterable_tags()`, and this asserts the consequence on the actual emitted events.
        let capability = full_capability();
        let beat = draft(true, 0, 5)
            .with_agents(vec!["claude".to_owned()])
            .with_capability(capability.clone())
            .to_event_draft();
        let claim = crate::gateway::claim_draft(
            "offer-id",
            "buyer-pubkey",
            "seller-pubkey",
            "creqA-test",
            &["claude".to_owned()],
            &capability,
        );

        // POSITIVE CONTROL FIRST. The loop below is `for tag in …` — over an empty list it passes
        // while asserting nothing, so the count is checked before the loop is trusted. Four tags:
        // harness_family, capabilities, and one harness_model per paired harness.
        let filterable = capability.filterable_tags();
        assert_eq!(filterable.len(), 4, "the fixture must produce filterable tags: {filterable:?}");
        for tag in filterable {
            assert!(beat.tags.contains(&tag), "beat is missing filterable tag {tag:?}");
            assert!(claim.tags.contains(&tag), "claim is missing filterable tag {tag:?}");
        }
        // And the join the filter actually performs: reading the two events back yields equal values.
        assert_eq!(
            harness_families_from_tags(&beat.tags),
            harness_families_from_tags(&claim.tags)
        );
        assert_eq!(
            harness_models_from_tags(&beat.tags),
            harness_models_from_tags(&claim.tags)
        );
        assert_eq!(
            capabilities_from_tags(&beat.tags),
            capabilities_from_tags(&claim.tags)
        );
    }

    #[test]
    fn display_only_fields_ride_the_beat_and_never_the_claim() {
        let capability = full_capability();
        let beat = draft(true, 0, 5).with_capability(capability.clone()).to_event_draft();
        let claim = crate::gateway::claim_draft(
            "offer-id",
            "buyer-pubkey",
            "seller-pubkey",
            "creqA-test",
            &[],
            &capability,
        );
        assert_eq!(hardware_from_tags(&beat.tags).as_deref(), Some("mac studio, 64GB"));
        assert_eq!(harness_variant_from_tags(&beat.tags).as_deref(), Some("my-fork"));
        assert_eq!(
            hardware_from_tags(&claim.tags),
            None,
            "hardware is display colour; the award decision never reads it, so it must not ride every claim"
        );
        assert_eq!(harness_variant_from_tags(&claim.tags), None);
    }

    #[test]
    fn hardware_is_unreachable_from_the_filterable_surface() {
        // #784: hardware "renders but never enters a filter predicate". Asserted against the ONE
        // function that defines the filterable surface, so this stays true as fields are added —
        // a test naming individual tags would silently stop covering a newly-added field.
        let capability = SeatCapability {
            hardware: Some("mac studio, 64GB".to_owned()),
            harness_variant: Some("my-fork".to_owned()),
            ..SeatCapability::default()
        };
        assert!(
            capability.filterable_tags().is_empty(),
            "a seat stating ONLY display-only fields must expose nothing filterable"
        );
    }

    #[test]
    fn a_harness_with_no_model_shifts_nothing_else() {
        // Why the pairing is per-tag rather than positional. Here the middle harness names no model;
        // under a positional encoding every pair after it would silently re-attribute. Each surviving
        // pair must still carry its OWN family.
        let capability = SeatCapability {
            harness_families: vec!["claude-code".to_owned(), "cursor".to_owned(), "codex".to_owned()],
            models: vec![
                HarnessModel { family: "claude-code".to_owned(), model: "claude-opus-5".to_owned() },
                HarnessModel { family: "codex".to_owned(), model: "gpt-5.6-sol[low]".to_owned() },
            ],
            ..SeatCapability::default()
        };
        let event = draft(true, 0, 5).with_capability(capability).to_event_draft();
        let read = harness_models_from_tags(&event.tags);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].family, "claude-code");
        assert_eq!(read[0].model, "claude-opus-5");
        assert_eq!(read[1].family, "codex", "cursor naming no model must not re-attribute codex's");
        assert_eq!(read[1].model, "gpt-5.6-sol[low]");
    }

    #[test]
    fn a_half_written_harness_model_tag_is_skipped_not_half_decoded() {
        // A pair missing either element cannot be attached to a harness, which is the unpairable
        // state the shape exists to rule out. Skipped, never defaulted to an empty family.
        let mut event = draft(true, 0, 5).to_event_draft();
        event.tags.push(TagSpec::new([HARNESS_MODEL_TAG, "claude-code"]));
        event.tags.push(TagSpec::new([HARNESS_MODEL_TAG, "", "orphan-model"]));
        event.tags.push(TagSpec::new([HARNESS_MODEL_TAG, "codex", ""]));
        event.tags.push(TagSpec::new([HARNESS_MODEL_TAG, "cursor", "real-model"]));
        let read = harness_models_from_tags(&event.tags);
        assert_eq!(read.len(), 1, "only the well-formed pair survives: {read:?}");
        assert_eq!(read[0].family, "cursor");
    }

    #[test]
    fn families_are_derived_from_the_advertised_roster() {
        // The families and the `agents` tag are built from ONE list inside `heartbeat_for_state`, so
        // they cannot describe different rosters. `claude` the preset becomes `claude-code` the
        // family; an unlabelled or custom preset contributes no family but still names itself in
        // `agents` — advertising a harness we can dispatch while stating no family for it is honest,
        // and stating a family we invented would not be.
        let event = heartbeat_for_state(
            0,
            true,
            5,
            mints(),
            vec!["claude".to_owned(), "my-fork".to_owned(), "codex".to_owned()],
            cap(&["claude", "my-fork", "codex"]),
        )
        .to_event_draft();
        assert_eq!(agents_from_tags(&event.tags), vec!["claude", "my-fork", "codex"]);
        assert_eq!(
            harness_families_from_tags(&event.tags),
            vec!["claude-code", "codex"],
            "my-fork has no spec family and must contribute none"
        );
    }
}
