//! The buyer daemon's trade logic: auto-award selection, the reserve→award and pay→convert
//! orderings, and the reconcile classification.
//!
//! The relay/wallet I/O lives in the RPC handlers ([`super`]); everything here is a PURE decision
//! over already-fetched truth, or a thin ordering seam over the store, so the money-load-bearing
//! rules are exhaustively testable without a relay or a mint:
//!
//! - [`select_awardable_claim`] — never auto-award a claim the buyer cannot pay (price/mint).
//! - [`award_with_reservation`] — reserve BEFORE publishing; a refused reservation publishes
//!   nothing and (by [`BuyerStore::reserve`]'s zero-write guarantee) leaves no row.
//! - [`settle_after_pay`] — flip `reserved → spent` ONLY after the budget append + wallet melt
//!   have landed (the #123/#126 ordering obligation on [`BuyerStore::convert_to_spent`]).
//! - [`classify_disposition`] — a reserved job's reconcile verdict; an ambiguous payment is kept,
//!   never auto-released.

use std::future::Future;

use cashu::{Amount, CurrencyUnit};

use crate::crossmint::plan_payment;
use crate::job_lifecycle::{
    AwardClaimOutcome, AwardPresence, JobLifecycleError, JobView, OfferView, PreparedAward,
    PresenceRead, SendOutcome,
};

use super::reservations::{Converted, JobDisposition, ReservationState, ReserveRefused};
use super::store::{AttemptState, AwardAttempt, AwardRecord, BeginAttempt, BuyerStore, StoreError};

/// Hard filters an awardable claim must pass (issue #126). Grounded in the wire the offer/claim
/// actually carry: the offer's signed `amount_sats` is the fixed price, the seller's claim `creq`
/// carries the payable terms + accepted mints, and the claim's `agents` tag carries the
/// harnesses the seller can run.
pub struct AwardFilters<'a> {
    /// The offer's signed amount — authority for the price. A claim whose `creq` quotes a
    /// different amount can never be accepted (the accept gate requires exact equality), so it
    /// cannot be paid and is skipped.
    pub offer_amount_sats: u64,
    /// The buyer's per-job ceiling. A claim priced above it is skipped (over budget).
    pub max_sats: u64,
    /// The buyer's own paying mint (config default). A claim whose `creq` lists no mint the buyer
    /// can settle at is skipped — the #126 mandatory guard: never auto-award what we cannot pay.
    pub buyer_mint: &'a str,
    /// Whether real (non-testnut) mints are permitted; gates the mint-compat check.
    pub allow_real_mints: bool,
    /// The harness the OFFER asked for, read back from the relay (never from award params — the
    /// signed offer is the authority for what the job requested). `None` ⇒ no preference and every
    /// claim passes this filter unchanged.
    pub requested_agent: Option<&'a str>,
    /// The harness FAMILY the offer asked for. `None` ⇒ no preference.
    ///
    /// Selects WHICH SEATS MAY CLAIM, and nothing more: a seat that does not advertise the family is
    /// refused, but a multi-harness seat that does advertise it still dispatches on its own preset.
    /// Only `requested_agent` reaches execution. A stated family must therefore AGREE with the
    /// preset when both are present — see
    /// [`CapabilityRefusal::HarnessFamilyContradictsPreset`].
    pub requested_harness_family: Option<&'a str>,
    /// The model the offer asked for. Only meaningful PAIRED with a preset, and refused rather than
    /// ignored when it arrives without one — see
    /// [`CapabilityRefusal::ModelWithoutHarnessPreset`]. When the family is absent it is DERIVED
    /// from the preset, so `agent` + `model` is a complete request.
    pub requested_model: Option<&'a str>,
    /// Capability tokens the offer requires (#784). Empty ⇒ no requirement. Every token is validated
    /// against [`crate::capability::CAPABILITIES`] before any claim is judged.
    pub required_capabilities: &'a [String],
}

/// Build the award filters for a job from its SIGNED OFFER — the ONE constructor both award paths
/// use (#897).
///
/// ★ THIS EXISTS SO THE TWO PATHS CANNOT DIVERGE, rather than so a test can detect that they have.
/// The manual RPC and `drive_auto_award` must apply identical filters — a request honoured on one and
/// dropped on the other is the bypass #866 was filed to close — and two hand-written literals make
/// that a property somebody has to keep noticing. One constructor makes it structural: a new request
/// axis is added here once and both paths have it.
///
/// It also removes a subtler hazard that had already bitten: while the two literals existed, the
/// tests needed a THIRD copy to build filters the way production does, and that copy silently drifted
/// from production for a revision — tests kept passing while asserting behaviour production did not
/// have. There is now nothing to drift from, which is why this is a constructor rather than a
/// stronger test.
///
/// Everything filterable comes from the OFFER; only the money context is passed in, because the
/// buyer's mint and its real-mint policy are properties of the buyer rather than of the job.
pub fn award_filters_for_offer<'a>(
    offer: &'a OfferView,
    max_sats: u64,
    buyer_mint: &'a str,
    allow_real_mints: bool,
) -> AwardFilters<'a> {
    AwardFilters {
        offer_amount_sats: offer.amount_sats,
        max_sats,
        buyer_mint,
        allow_real_mints,
        requested_agent: offer.requested_agent.as_deref(),
        requested_harness_family: offer.requested_harness_family.as_deref(),
        requested_model: offer.requested_model.as_deref(),
        required_capabilities: &offer.required_capabilities,
    }
}

/// Select the claim to auto-award: the first LIVE claim whose seller-authored `creq` passes every
/// hard filter. Pure — relay truth in, claim id out. Never invents a claim, and never returns one
/// the buyer cannot pay (price mismatch, over budget, or no mutually-payable mint).
pub fn select_awardable_claim(view: &JobView, filters: &AwardFilters) -> Option<String> {
    if filters.offer_amount_sats > filters.max_sats {
        return None;
    }
    view.claims
        .iter()
        .find(|claim| {
            claim.live
                && claim_serves_requested_agent(&claim.agents, filters.requested_agent)
                && claim_meets_capability_request(&claim.capability, filters).is_ok()
                && claim_is_payable(&view.job_id, claim.creq.as_deref(), filters)
        })
        .map(|claim| claim.claim_id.clone())
}

/// Why a claim's advertised capability did not satisfy the job's request (#784).
///
/// Each variant names what to FIX rather than restating the request, because the operator reading it
/// is deciding between waiting, changing the job, and adding a seat — three different actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityRefusal {
    /// The job named a harness family the claim does not advertise.
    HarnessFamily { requested: String },
    /// The job named a model the claim does not advertise FOR the requested family. Raised both when
    /// the claim advertises no model at all and when it advertises that model for a DIFFERENT
    /// family — a model is only meaningful paired to the harness that would run it.
    Model { family: String, requested: String },
    /// The job named a model but no harness PRESET. A buyer-side request defect, refused here as the
    /// fail-closed backstop so it can never be silently ignored on the money path.
    ///
    /// The preset is the anchor because it is the only requested axis the seller persists and
    /// dispatches on: `offer_row` stores `requested_agent` alone, `classify_offer` gates on it, and
    /// `execute_job` hands it to `SellerAgents::dispatch`. Family and model reach neither, so a model
    /// hung off a family would name a harness nothing selects — and `dispatch(None)` runs the seat's
    /// FIRST configured preset, which is how a multi-harness seat could accept a codex request and
    /// deterministically run Claude.
    ModelWithoutHarnessPreset { requested: String },
    /// The job named both a preset and a harness family, and they disagree. Dispatch honours the
    /// preset, so the family names a harness this job would not run on — the offer would be asking
    /// for one thing and executing another.
    HarnessFamilyContradictsPreset { preset: String, preset_family: String, requested: String },
    /// The job constrained the harness (a family or a model) while naming a preset with no family in
    /// [`crate::agent_presets::HARNESS_FAMILIES`] — a custom preset. Nothing can establish that what
    /// dispatch will run matches what the job asked for, so it refuses rather than assuming.
    PresetHasNoKnownFamily { preset: String },
    /// The job required capability tokens the claim does not advertise. Names the MISSING tokens
    /// rather than the whole request, so the refusal says what to fix.
    Capabilities { missing: Vec<String> },
    /// The job required a token outside [`crate::capability::CAPABILITIES`]. Deliberately distinct
    /// from `Capabilities`, because the two imply OPPOSITE operator actions: "no seat advertises
    /// this" means wait or add a seat, which is a correct and useful response, while "that is not a
    /// real token" means the request is wrong and no seat can ever satisfy it. Collapsing them would
    /// tell an operator to wait for a seat that cannot exist.
    UnknownCapabilityToken { token: String },
    /// The job requested a harness family outside [`crate::agent_presets::HARNESS_FAMILIES`].
    ///
    /// Distinct from `HarnessFamily` for the same reason `UnknownCapabilityToken` is distinct from
    /// `Capabilities`: the two imply OPPOSITE operator actions. "No seat advertises this family"
    /// means wait or add a seat; "that is not a real family" means the request is wrong and no seat
    /// can ever satisfy it.
    ///
    /// ⚠ THIS IS A WIRE-LEVEL GUARD, NOT A CONVENIENCE. The post-time vocabulary gate only sees
    /// offers built by OUR client. A foreign client can publish any string it likes, and neither
    /// reader filters vocabulary — `harness_families_from_tags` and the offer's param reader both
    /// only trim and drop blanks. So without this check a foreign offer requesting a bogus family,
    /// matched by a claim advertising THAT SAME bogus family, satisfied the predicate and both award
    /// paths could select it. Judged on the REQUEST, before the claim is consulted, so a nonsense
    /// request can never be laundered into a match by a claim that agrees with it.
    UnknownHarnessFamily { requested: String },
}

impl std::fmt::Display for CapabilityRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HarnessFamily { requested } => {
                write!(f, "claim does not advertise harness family {requested}")
            }
            Self::Model { family, requested } => {
                write!(f, "claim does not advertise model {requested} for family {family}")
            }
            Self::ModelWithoutHarnessPreset { requested } => {
                write!(f, "model {requested} requested without a harness preset (`agent`)")
            }
            Self::HarnessFamilyContradictsPreset { preset, preset_family, requested } => write!(
                f,
                "harness family {requested} contradicts preset {preset}, which dispatches {preset_family}"
            ),
            Self::PresetHasNoKnownFamily { preset } => {
                write!(f, "preset {preset} has no known harness family to check the request against")
            }
            Self::Capabilities { missing } => {
                write!(f, "claim is missing required capabilities: {}", missing.join(", "))
            }
            Self::UnknownCapabilityToken { token } => {
                write!(f, "{token} is not a known capability token")
            }
            Self::UnknownHarnessFamily { requested } => {
                write!(f, "{requested} is not a known harness family")
            }
        }
    }
}

/// Whether a claim's advertised capability satisfies the job's request (#784) — the ONE predicate,
/// consumed by both award paths so they cannot drift.
///
/// Reads the already-parsed [`crate::heartbeat::SeatCapability`] and never re-parses tags: the write
/// path is `filterable_tags()`, the read path is `from_tags()`, and this decides on what the reader
/// produced. A second parse here would be a second reading of the wire that could agree today and
/// diverge later.
///
/// Decides on the CLAIM alone. The seat's kind-30340 announcement is structurally absent from this
/// decision and no relay read happens inside it, so a seat whose beat says one thing and whose claim
/// says another is judged on the claim — the event it signed for THIS job.
///
/// Every filter is optional and an absent request passes every claim, so a job that asks for nothing
/// is awarded exactly as it is today. Present-but-unmatched and absent-on-the-claim both refuse, but
/// they are different states and the tests separate them.
///
/// ⚠ Matching is a decision about an ADVERTISEMENT, and an advertisement is a claim, not a proof. A
/// matched model may still differ from the `["model", name]` the seller stamps on the result — two
/// honest reads of the same ACP field at two times. Nothing on the pay path can catch that: the buyer
/// records that tag as its own `model_used`, which is a seller-claimed attribution outside the
/// receipt preimage and gates nothing (`docs/protocol-v1.md` §6.4). It can corroborate a divergence
/// afterwards; it can never have prevented one. A matched capability token promises the tool resolved
/// at probe time, never that the work succeeds. The award IS the payment decision, so nothing
/// downstream can revise it.
pub fn claim_meets_capability_request(
    advertised: &crate::heartbeat::SeatCapability,
    filters: &AwardFilters,
) -> Result<(), CapabilityRefusal> {
    // Validate the REQUEST before judging the claim. A malformed request judged against a claim
    // produces a claim-blaming refusal for a defect the claim had nothing to do with.
    if let Some(unknown) = filters
        .required_capabilities
        .iter()
        .find(|token| !crate::capability::CAPABILITIES.contains(&token.as_str()))
    {
        return Err(CapabilityRefusal::UnknownCapabilityToken { token: unknown.clone() });
    }
    // The STATED family must be a real one. Judged against the vocabulary, never against the claim:
    // a claim advertising the same bogus string would otherwise satisfy the filter, and the offer
    // and the seat would agree with each other about a harness that does not exist.
    //
    // Only the stated family needs this. A DERIVED one comes from `harness_family_for_preset`, whose
    // range is `HARNESS_FAMILIES` by construction — asserted in `agent_presets` — so validating it
    // here would be checking a value this crate produced rather than one the wire supplied.
    if let Some(requested) = filters.requested_harness_family {
        if !crate::agent_presets::HARNESS_FAMILIES.contains(&requested) {
            return Err(CapabilityRefusal::UnknownHarnessFamily {
                requested: requested.to_owned(),
            });
        }
    }
    // The request must be one DISPATCH can honour, before any of it is matched against a claim.
    //
    // Only `requested_agent` reaches execution: `offer_row` persists it alone, `classify_offer`
    // gates on it, and `execute_job` passes it to `SellerAgents::dispatch`, which runs the seat's
    // FIRST configured preset when it is absent. A family or model that disagrees with the preset
    // therefore describes a harness this job will not run on, and a multi-harness seat would satisfy
    // the filter and then execute something else. Refusing here is what keeps the offer's request and
    // the seat's dispatch the same statement.
    let preset = crate::seller_agents::normalize_request(filters.requested_agent);
    if let Some(requested_model) = filters.requested_model {
        if preset.is_none() {
            return Err(CapabilityRefusal::ModelWithoutHarnessPreset {
                requested: requested_model.to_owned(),
            });
        }
    }
    let preset_family =
        preset.as_deref().and_then(crate::agent_presets::harness_family_for_preset);
    if let Some(preset_name) = preset.as_deref() {
        // A bare preset constrains nothing about the family — `claim_serves_requested_agent` already
        // enforces it, and a seat naming that preset is by definition able to run it.
        if filters.requested_harness_family.is_some() || filters.requested_model.is_some() {
            let Some(preset_family) = preset_family else {
                return Err(CapabilityRefusal::PresetHasNoKnownFamily {
                    preset: preset_name.to_owned(),
                });
            };
            if let Some(requested) = filters.requested_harness_family {
                if requested != preset_family {
                    return Err(CapabilityRefusal::HarnessFamilyContradictsPreset {
                        preset: preset_name.to_owned(),
                        preset_family: preset_family.to_owned(),
                        requested: requested.to_owned(),
                    });
                }
            }
        }
    }
    // The family this request actually binds to: stated outright, or DERIVED from the preset when a
    // model needs one to pair against. Deriving rather than demanding both keeps `agent` + `model`
    // — the shape a buyer reaches for first — a valid request instead of a refusal.
    let effective_family: Option<&str> = match filters.requested_harness_family {
        Some(stated) => Some(stated),
        None if filters.requested_model.is_some() => preset_family,
        None => None,
    };
    // Family first: when a claim fails both, the family refusal is the actionable one — a model
    // refusal would send the operator chasing a model on a harness the seat never offered.
    if let Some(requested) = effective_family {
        if !advertised.harness_families.iter().any(|family| family == requested) {
            return Err(CapabilityRefusal::HarnessFamily { requested: requested.to_owned() });
        }
    }
    if let Some(requested_model) = filters.requested_model {
        // Some by construction: a model requires a preset, and a preset that constrains the harness
        // must map to a family or it was refused above.
        let Some(family) = effective_family else {
            return Err(CapabilityRefusal::ModelWithoutHarnessPreset {
                requested: requested_model.to_owned(),
            });
        };
        // The PAIR is the unit. Matching the model alone would award a seat that runs that model on
        // a different harness than the one this job asked to dispatch on.
        let paired = advertised
            .models
            .iter()
            .any(|advertised| advertised.family == family && advertised.model == requested_model);
        if !paired {
            return Err(CapabilityRefusal::Model {
                family: family.to_owned(),
                requested: requested_model.to_owned(),
            });
        }
    }
    let missing: Vec<String> = filters
        .required_capabilities
        .iter()
        .filter(|required| !advertised.capabilities.iter().any(|have| have == *required))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(CapabilityRefusal::Capabilities { missing });
    }
    Ok(())
}

/// Whether a capability request is UNSATISFIABLE BY CONSTRUCTION — no claim that could ever be
/// published can pass it — and if so, why (#897).
///
/// ★ DERIVED FROM THE PREDICATE, NOT A SECOND COPY OF ITS RULES. It synthesizes the claim that would
/// satisfy the request exactly — a seat advertising precisely what was asked for — and runs the REAL
/// [`claim_meets_capability_request`] against it. A request the perfect claim cannot pass is one no
/// claim can pass.
///
/// That construction is the whole point. A gate that restated "a model needs a preset" in its own
/// words would be a second copy of a rule owned elsewhere, and the two would rot apart silently:
/// #788 may later make a bare model valid, and on that day this gate must stop refusing WITHOUT
/// anyone remembering it exists. Because the verdict comes from the predicate, it will.
///
/// SCOPE, and it is a real limit rather than a caveat: this is a fail-fast for OUR posting surface,
/// where refusing costs the caller nothing and posting commits an offer. It is NOT the enforcement
/// boundary. A foreign client can publish a model-only offer straight to the relay, and for that
/// offer the award-time refusal and its park row are the wire-level truth. Both layers are tested and
/// neither makes the other unnecessary.
pub fn unsatisfiable_capability_request(
    requested_agent: Option<&str>,
    requested_harness_family: Option<&str>,
    requested_model: Option<&str>,
    required_capabilities: &[String],
) -> Option<CapabilityRefusal> {
    // The seat that advertises exactly what was asked for, along EVERY axis that could be asked
    // about. It advertises the stated family AND the preset's own family, and pairs the model to
    // both, so it satisfies whichever one the predicate decides is effective.
    //
    // Advertising both rather than working out which one applies is deliberate: computing the
    // effective family here would be a second copy of the derive rule, and the two would rot apart
    // exactly as a restated predicate would. A maximally-capable seat needs to know only which axes
    // were REQUESTED, never how they resolve — so what survives is a request no seat could satisfy,
    // which is the question this asks.
    let mut harness_families: Vec<String> = Vec::new();
    for family in [
        requested_harness_family,
        requested_agent.and_then(crate::agent_presets::harness_family_for_preset),
    ]
    .into_iter()
    .flatten()
    {
        if !harness_families.iter().any(|held| held == family) {
            harness_families.push(family.to_owned());
        }
    }
    let advertised = crate::heartbeat::SeatCapability {
        models: match requested_model {
            Some(model) => harness_families
                .iter()
                .map(|family| crate::heartbeat::HarnessModel {
                    family: family.clone(),
                    model: model.to_owned(),
                })
                .collect(),
            None => Vec::new(),
        },
        harness_families,
        capabilities: required_capabilities.to_vec(),
        ..crate::heartbeat::SeatCapability::default()
    };
    // Money fields are placeholders and are never read: `claim_meets_capability_request` consults
    // only the request axes. Zeroes rather than plausible amounts, so nothing here can be mistaken
    // for a price this function decides anything about.
    let filters = AwardFilters {
        offer_amount_sats: 0,
        max_sats: 0,
        buyer_mint: "",
        allow_real_mints: false,
        requested_agent,
        requested_harness_family,
        requested_model,
        required_capabilities,
    };
    claim_meets_capability_request(&advertised, &filters).err()
}

/// Why the job's capability request left nothing awardable, as an operator-facing clause — or `None`
/// when the capability request is not what stood in the way (#897).
///
/// DIAGNOSTICS ONLY. Nothing here decides an award; it explains one that already did not happen, so a
/// job that parks says what to fix instead of only that it parked.
///
/// `None` in three distinct cases, and each would otherwise produce a misleading line:
/// - No request ⇒ nothing to explain; the obstacle was price, mint, agent or an empty relay.
/// - No live claims ⇒ nothing was refused. "No seat advertises X" would be false where the truth is
///   that nobody claimed at all, and it would send an operator to fix a request that is fine.
/// - Some live claim SATISFIED the request ⇒ capability was not the obstacle. Naming it here would
///   blame the request for a price or mint failure and hide the real one. This is the case worth
///   being strict about: it is the one where a capability-shaped reason is most plausible and most
///   wrong.
///
/// Deliberately NOT a wire reason code. #821 adds `capability_missing` to the protocol and #859 makes
/// an undispatchable job carry that label; this is the local operator string that can land first
/// without pre-empting either, and it is what those two replace on the wire.
pub fn capability_park_reason(view: &JobView, filters: &AwardFilters) -> Option<String> {
    if filters.requested_harness_family.is_none()
        && filters.requested_model.is_none()
        && filters.required_capabilities.is_empty()
    {
        return None;
    }
    // Judged on the claims that were candidates AT THE DEADLINE, not on `view.claims` as they stand
    // now. A job reaches this diagnosis only after its deadline passed, and by then liveness has
    // been re-derived against the current clock and every ordinary claim is expired — so reading
    // `view.claims` here would find nothing live and stay silent in precisely the case this exists
    // to explain.
    let candidates = crate::job_lifecycle::claims_at_deadline(view);
    let mut refusals: Vec<String> = Vec::new();
    for claim in candidates.iter().filter(|claim| claim.live) {
        match claim_meets_capability_request(&claim.capability, filters) {
            // A live claim that passes means the request was satisfiable and something else refused
            // this job. Bail rather than report: a partial list reads as the whole reason.
            Ok(()) => return None,
            Err(refusal) => {
                let rendered = refusal.to_string();
                if !refusals.contains(&rendered) {
                    refusals.push(rendered);
                }
            }
        }
    }
    if refusals.is_empty() {
        return None;
    }
    Some(format!(
        "no live claim satisfied the job's capability request ({})",
        refusals.join("; ")
    ))
}

/// Whether a claim may be awarded a job that asked for a specific harness.
///
/// No request ⇒ every claim passes. A request ⇒ the claim must ADVERTISE that harness. A claim
/// that advertises nothing does not pass: silence is not a capability, and awarding it would be
/// paying a seller to run the job on whatever it happens to prefer. Matching is on the
/// canonicalised name so wire casing/whitespace cannot smuggle a mismatch past the filter.
pub fn claim_serves_requested_agent(claim_agents: &[String], requested: Option<&str>) -> bool {
    let Some(requested) = crate::seller_agents::normalize_request(requested) else {
        return true;
    };
    claim_agents
        .iter()
        .any(|advertised| advertised.trim().to_ascii_lowercase() == requested)
}

/// Why a specifically-named (manual) award was refused. The manual path names a `claim_id` instead
/// of auto-selecting, so it must apply the SAME hard filters `select_awardable_claim` applies —
/// otherwise `max_sats` and mint/price compatibility would be dead input on the manual path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedAwardRefused {
    /// The offer price exceeds the buyer's `max_sats` ceiling for this award.
    OverMax { offer_amount_sats: u64, max_sats: u64 },
    /// No claim with that id is on the relay for this job.
    NotFound { claim_id: String },
    /// The named claim is not live (expired / superseded / past deadline) — nothing to award.
    NotLive { claim_id: String },
    /// The named claim cannot be paid (missing/malformed creq, price ≠ offer amount, wrong unit, or
    /// no mutually-payable mint) — awarding it would commit to something the buyer cannot settle.
    Unpayable { claim_id: String },
    /// The job asked for a harness the named claim does not advertise — awarding it would buy work
    /// from a seller that never said it could do it this way.
    AgentMismatch { claim_id: String, requested: String },
    /// The named claim's advertised capability does not satisfy the job's request (#784). Carries
    /// the underlying [`CapabilityRefusal`] rather than flattening it: the auto path can already
    /// tell an operator WHICH axis refused, and collapsing that here would make the manual path the
    /// less informative of two paths that must decide identically.
    Capability { claim_id: String, refusal: CapabilityRefusal },
}

impl std::fmt::Display for NamedAwardRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverMax { offer_amount_sats, max_sats } => write!(
                formatter,
                "award refused: offer price {offer_amount_sats} sat exceeds max_sats {max_sats}"
            ),
            Self::NotFound { claim_id } => write!(formatter, "award refused: claim {claim_id} not found for this job"),
            Self::NotLive { claim_id } => write!(formatter, "award refused: claim {claim_id} is not live"),
            Self::Unpayable { claim_id } => write!(
                formatter,
                "award refused: claim {claim_id} is not payable (price/mint/creq incompatible — the buyer could not settle it)"
            ),
            Self::AgentMismatch { claim_id, requested } => write!(
                formatter,
                "award refused: job requested agent {requested:?}, which claim {claim_id} does not advertise"
            ),
            Self::Capability { claim_id, refusal } => {
                write!(formatter, "award refused: claim {claim_id}: {refusal}")
            }
        }
    }
}

impl std::error::Error for NamedAwardRefused {}

/// Verify a specifically-named claim is awardable under the hard filters — the manual-award
/// counterpart of [`select_awardable_claim`], so `max_sats`, mint/price compatibility AND the
/// capability request are applied on the manual path rather than ignored. Pure: relay truth +
/// filters in, verdict out.
///
/// Every filter `select_awardable_claim` applies is applied here, in the same order. That is a
/// property the tests hold, not a convention: naming a claim selects WHICH claim is judged, never
/// WHETHER it is judged, and the award IS the payment decision, so a filter skipped here is a
/// filter that never runs at all.
pub fn named_claim_awardable(
    view: &JobView,
    claim_id: &str,
    filters: &AwardFilters,
) -> Result<(), NamedAwardRefused> {
    if filters.offer_amount_sats > filters.max_sats {
        return Err(NamedAwardRefused::OverMax {
            offer_amount_sats: filters.offer_amount_sats,
            max_sats: filters.max_sats,
        });
    }
    let claim = view
        .claims
        .iter()
        .find(|claim| claim.claim_id == claim_id)
        .ok_or_else(|| NamedAwardRefused::NotFound { claim_id: claim_id.to_owned() })?;
    if !claim.live {
        return Err(NamedAwardRefused::NotLive { claim_id: claim_id.to_owned() });
    }
    if !claim_serves_requested_agent(&claim.agents, filters.requested_agent) {
        return Err(NamedAwardRefused::AgentMismatch {
            claim_id: claim_id.to_owned(),
            requested: filters.requested_agent.unwrap_or_default().to_owned(),
        });
    }
    // Same position the capability arm holds in `select_awardable_claim`, so the two chains stay
    // readable as mirrors: naming a claim chooses WHICH claim is judged, never WHETHER it is.
    if let Err(refusal) = claim_meets_capability_request(&claim.capability, filters) {
        return Err(NamedAwardRefused::Capability { claim_id: claim_id.to_owned(), refusal });
    }
    if !claim_is_payable(&view.job_id, claim.creq.as_deref(), filters) {
        return Err(NamedAwardRefused::Unpayable { claim_id: claim_id.to_owned() });
    }
    Ok(())
}

/// True when a claim's `creq` is present, well-formed, priced at the offer amount within the
/// budget ceiling, denominated in sats for this job, and quotes a mint the buyer can pay from.
fn claim_is_payable(job_id: &str, creq: Option<&str>, filters: &AwardFilters) -> bool {
    let Some(creq) = creq else { return false };
    let Ok(request) = crate::gateway::creq::parse_creq(creq) else {
        return false;
    };
    if request.payment_id.as_deref() != Some(job_id) {
        return false;
    }
    if request.unit.as_ref() != Some(&CurrencyUnit::Sat) {
        return false;
    }
    // The claim's price must equal the offer amount (else the accept gate refuses it → unpayable)
    // and must sit within the buyer's ceiling.
    if request.amount != Some(Amount::from(filters.offer_amount_sats)) {
        return false;
    }
    if filters.offer_amount_sats > filters.max_sats {
        return false;
    }
    // Mint compatibility: the buyer must have a route to a mint the seller listed — paying from its
    // own mint when the seller accepts it, otherwise hopping to one that the seller accepts and the
    // fence admits. This is the SAME planning the pay path performs, so a claim that passes here is
    // one the buyer can actually pay, by whichever of those two routes.
    let listed: Vec<String> = request.mints.iter().map(|mint| mint.to_string()).collect();
    plan_payment(filters.buyer_mint, &listed, filters.allow_real_mints).is_ok()
}

/// What [`award_with_reservation`] may do about a job, decided BEFORE any reserve, sign, or send.
///
/// The attempt row outranks the reservation because it is the only signal that knows whether
/// SIGNED BYTES may be on the wire: it is written before the first send by construction
/// ([`BuyerStore::begin_award_attempt`]), so "no attempt row" proves no award of ours was ever
/// transmitted — a proof the reservation ledger cannot offer (`Released` is written by the OLD
/// binary's publish-error path, where the event may have landed anyway; that misreading is #322).
/// The local `awards` row is consulted before either: it is written at this chokepoint, so
/// reading it either finds the award or proves it was never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwardStep {
    /// A local `awards` row exists: this buyer published this award and recorded it. Return it;
    /// send nothing.
    AlreadyAwarded,
    /// The attempt is confirmed public but the `awards` row is missing — the crash window between
    /// the relay's ack and `record_award`. Write the row FROM the attempt (which carries every
    /// field) and report `AlreadyAwarded`; send nothing.
    RepairFromAttempt,
    /// The attempt was refused by the relay — terminal. Nothing is public and nothing may be
    /// published for this job again; finish any half-done release and report the refusal.
    RefusedTerminal,
    /// A pinned attempt awaits its relay verdict. Re-hold the reservation (reconcile may have
    /// released it while the question was open) and re-send THE PINNED BYTES — never re-prepare,
    /// never re-select. The event id is a content hash, so however many sends this takes, the
    /// relay holds at most one award for the job.
    ResumeAttempt,
    /// No attempt row, but a reservation exists (`Reserved` OR `Released`) — a job last touched by
    /// a pre-attempt binary (or our own sub-second reserve→pin crash window, which is locally
    /// indistinguishable). Only the relay can say whether an award landed: probe fail-closed.
    /// Found → repair the row and re-hold its funds. CONFIRMED absent → release and terminalize
    /// (recovery: a new offer) — never a second claim selection, because "absent now" is not
    /// "never arrives" and the seller executes per-award, so a fresh event naming a different
    /// claim re-arms the #322 burn. Unverified → refuse, holding everything.
    ///
    /// `Released` taking this arm (instead of publishing straight away) is the #322 fix at its
    /// point of failure: the old binary released on any publish error, though a lost `OK` leaves
    /// the award public — so `Released` proves nothing and must ask, exactly like `Reserved`.
    ProbeLegacy,
    /// Nothing was ever started here (or the ledger says `Spent`, which `reserve` itself refuses
    /// downstream — one authority). Reserve, prepare + sign, PIN the attempt, then send.
    Fresh,
}

/// The award-step decision (#126/#127 invariant A: never award twice), local-first — pure over
/// the three local reads so the full state product is exhaustively testable.
///
/// `reservations.state` alone cannot make this call: `Reserved` is the state of a healthy
/// in-flight award AND of both crash windows, and `Released` is the state of a failed publish AND
/// of a landed-but-unacked one (#322). The discriminators are the JOIN against `awards` and the
/// attempt row's pinned verdict.
pub fn award_step(
    has_award_row: bool,
    attempt: Option<AttemptState>,
    reservation: Option<ReservationState>,
) -> AwardStep {
    if has_award_row {
        return AwardStep::AlreadyAwarded;
    }
    match attempt {
        Some(AttemptState::Confirmed) => AwardStep::RepairFromAttempt,
        Some(AttemptState::Refused) => AwardStep::RefusedTerminal,
        Some(AttemptState::Pending) => AwardStep::ResumeAttempt,
        None => match reservation {
            // Funds were committed by a binary that pinned no attempt — the award may or may not
            // be public, whichever of the two states the ledger shows. Ask the relay.
            Some(ReservationState::Reserved) | Some(ReservationState::Released) => {
                AwardStep::ProbeLegacy
            }
            // `Spent` (already paid) is decided downstream by `reserve` itself, which refuses
            // `AlreadySpent`. Falling through keeps ONE authority rather than a second opinion.
            Some(ReservationState::Spent) | None => AwardStep::Fresh,
        },
    }
}

/// Outcome of [`award_with_reservation`]: whether it published an award now, or found one this
/// buyer had already published.
///
/// These are separate variants because they know DIFFERENT things, and the difference is
/// money-relevant. [`AwardClaimOutcome`] carries `quoted_mints` — the mints the award commits the
/// buyer to paying at — and [`AwardRecord`] does not persist them. Collapsing both into
/// `AwardClaimOutcome` would mean handing back an empty mint list on the already-awarded path,
/// which is not a neutral placeholder: an empty `quoted_mints` already MEANS "the claim carried no
/// parseable `creq`". The type keeps "we don't store this" from being reported as "there were none".
#[derive(Debug)]
pub enum AwardOutcome {
    /// An award was published by THIS call; `quoted_mints` is live from the claim.
    Published(AwardClaimOutcome),
    /// This buyer had already published and recorded an award for the job; nothing was published
    /// now. The record carries no mint list (see the type note above).
    AlreadyAwarded(AwardRecord),
}

/// Failure of [`award_with_reservation`].
#[derive(Debug)]
pub enum AwardError {
    /// The reservation was refused — NOTHING was published and (by the store's zero-write
    /// guarantee on refusal) no reservation row was written.
    Reserve(ReserveRefused),
    /// An award for this job is ALREADY PUBLIC on the relay but missing from the local `awards`
    /// table, AND the row could not be repaired. Refused rather than published: a second 3405 would
    /// be a genuine duplicate award of real money. Nothing was published and the reservation was
    /// left untouched.
    ///
    /// Reaching this means repair was attempted and declined — `detail` says why (the award could
    /// not be parsed into a complete record, several awards made the choice ambiguous, or the write
    /// itself failed). Carries the relayed award's event id so the operator can act without
    /// re-querying. Repair succeeding is [`AwardOutcome::AlreadyAwarded`], not an error.
    PublishedButUnrecorded { job_id: String, award_event_id: String, detail: String },
    /// Funds are reserved for this job with no local award record, and the relay could not say
    /// whether an award is already public. Refused: publishing on an unverified absence is exactly
    /// the duplicate-award risk this gate exists to remove. Nothing was published; the reservation
    /// was left untouched.
    PresenceUnverified { job_id: String, detail: String },
    /// The local presence read failed. Refused rather than assumed absent — the whole point of
    /// reading local state first is that it is authoritative, so a failure to read it is not a
    /// licence to publish.
    Presence(StoreError),
    /// PREPARE (validate + sign) failed after the reservation was taken; the reservation was
    /// released. Releasing here is provably safe — the failure happened before anything signed
    /// was persisted or transmitted, so nothing of ours can be on the wire.
    Publish(JobLifecycleError),
    /// The relay explicitly rejected this job's pinned award event (`OK:false`), or the offer
    /// deadline passed with the award confirmed absent. TERMINAL for the job: nothing is public,
    /// the reservation was released, and no award may be published for this offer again — awards
    /// are write-once per offer, so the recovery is a new offer, never a second award.
    Refused { job_id: String, detail: String },
    /// The send got no verdict — the relay may or may not hold the event (a lost `OK` after a
    /// successful store is indistinguishable from a lost send, and the seller executes off the
    /// relay's copy either way; #322). The signed attempt stays PINNED and the reservation stays
    /// HELD. Retrying is safe and is the way forward: a retry re-sends the identical bytes, which
    /// the relay dedups.
    Unresolved { job_id: String, award_event_id: String, detail: String },
    /// Releasing the reservation after a refusal (or a prepare failure) itself failed.
    Store(StoreError),
}

impl std::fmt::Display for AwardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reserve(refused) => write!(formatter, "{refused}"),
            // An award refused for safety is only useful if the reader knows what happens next —
            // but "what happens next" is not always an operator action, and naming one that does
            // not exist is worse than naming none. `PublishedButUnrecorded` needs a human, because
            // nothing else will write the missing row. `PresenceUnverified` does not: reconcile
            // releases the reservation on its own once the claim stops being live, which re-arms
            // the award. Each message states its own recovery, and only that.
            Self::PublishedButUnrecorded { job_id, award_event_id, detail } => write!(
                formatter,
                "award {award_event_id} for job {job_id} is already published on the relay but has \
                 no local awards row, and the row could not be repaired ({detail}); refusing to \
                 publish a second award. Collect this job manually (`collect {job_id}`) — it will \
                 not auto-settle on delivery until the row exists",
            ),
            Self::PresenceUnverified { job_id, detail } => write!(
                formatter,
                "job {job_id} has funds reserved with no local awards row, and the relay could not \
                 confirm whether an award is already public ({detail}); refusing to publish rather \
                 than risk a duplicate award. No operator action is required: every award call for \
                 this job re-probes, and the auto path re-checks at the next daemon start; once \
                 the relay answers, the job either repairs its row (award found) or terminally \
                 parks with its funds returned (confirmed absence). If you want it settled sooner, \
                 check the relay for a 3405 on this job and, if one exists, run `collect {job_id}`",
            ),
            Self::Presence(error) => {
                write!(formatter, "could not read local award state: {error}")
            }
            Self::Publish(error) => write!(formatter, "{error}"),
            Self::Refused { job_id, detail } => write!(
                formatter,
                "the relay refused the award event for job {job_id} ({detail}); nothing is \
                 public and the reserved funds were returned. Awards are write-once per offer, \
                 so this job will not be awarded again — post a new offer to re-run the work",
            ),
            Self::Unresolved { job_id, award_event_id, detail } => write!(
                formatter,
                "the relay gave no verdict on award {award_event_id} for job {job_id} ({detail}); \
                 the signed award stays pinned and its funds stay reserved. Retry the award — a \
                 retry re-sends the SAME signed event (never a new one), so it can only converge",
            ),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for AwardError {}

/// Check that this job is not ALREADY awarded (or pinned), then reserve `amount`, pin the signed
/// award, and send it.
///
/// Four orderings are load-bearing here, in this order:
///
/// 1. **Presence before reserve** ([`award_step`]): an already-awarded job returns
///    [`AwardOutcome::AlreadyAwarded`] having sent nothing and reserved nothing — the original
///    award still holds its own reservation, so re-reserving would be a second commitment against
///    the same debt. A pinned attempt likewise short-circuits selection: whatever claim the caller
///    had in mind, the PINNED event is the only award this job may carry.
/// 2. **Reserve before sign**: a refused reservation returns [`AwardError::Reserve`] without ever
///    calling `prepare`, so an award the buyer cannot afford is never even signed and (by
///    [`BuyerStore::reserve`]) leaves no row.
/// 3. **Pin before send** ([`BuyerStore::begin_award_attempt`]): the signed bytes are durable
///    before the first transmission, so "bytes on the wire ⇒ an attempt row exists" holds by
///    construction and every later crash window is decidable from local state alone.
/// 4. **Send resolves the attempt, never the selection**: an acked send confirms + records; an
///    explicit relay refusal terminalizes + releases; an UNRESOLVED send (lost `OK`, timeout,
///    transport error) changes NOTHING — the reservation stays held, the attempt stays pending,
///    and the retry (this same function) re-sends the identical bytes. Concluding "nothing
///    landed" from an unresolved send — and then releasing + re-selecting — is how three seats
///    came to execute one offer (#322).
///
/// `award_present_on_relay` is the relay leg for PRE-ATTEMPT jobs only (rows written by a binary
/// older than the attempt table): with no pinned verdict to consult, only the relay can say
/// whether their award landed. It is fail-closed in every direction — `Unverified` refuses
/// holding everything, a found award repairs the row and re-holds its funds, and a CONFIRMED
/// absence releases and terminalizes (recovery: a new offer). A confirmed absence deliberately
/// does NOT proceed to a fresh selection: "absent now" is not "never arrives", the seller
/// executes per-award, and a fresh event naming a different claim would re-arm the #322 burn for
/// exactly the population this gate exists to protect.
///
/// ★ The recorded `amount_sats` comes from the job's own pinned attempt or RESERVATION, never
/// from this call's `amount`, on every non-fresh path. `amount` is what THIS call was asked to
/// spend, which may differ; the kind-3405 carries no amount tag, so the attempt/reservation is
/// the only artifact of the sum that was really awarded.
///
/// `balance` is the honest live-wallet snapshot the caller supplies — the wallet-ceiling input
/// [`BuyerStore::reserve`] guards against (issue #378 removed the budget ceiling's `total_cap`/`spent`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn award_with_reservation<P, PFut, R, RFut, S, SFut>(
    store: &BuyerStore,
    job_id: &str,
    amount: u64,
    balance: u64,
    now_unix: i64,
    award_present_on_relay: P,
    prepare: R,
    send: S,
    licensed_prior_sends: Option<u64>,
) -> Result<AwardOutcome, AwardError>
where
    P: FnOnce() -> PFut,
    PFut: Future<Output = Result<PresenceRead<AwardPresence>, JobLifecycleError>>,
    R: FnOnce() -> RFut,
    RFut: Future<Output = Result<PreparedAward, JobLifecycleError>>,
    S: FnOnce(String, String) -> SFut,
    SFut: Future<Output = SendOutcome>,
{
    // Presence before anything else. A local read failure REFUSES rather than falling through to
    // send: local state is the authority this gate rests on, so failing to read it is not a
    // licence to act as though it said "absent".
    let existing = store.award_record(job_id).map_err(AwardError::Presence)?;
    let attempt = store.award_attempt(job_id).map_err(AwardError::Presence)?;
    let reservation = store.reservation(job_id).map_err(AwardError::Presence)?;
    let reserved_amount = reservation.map(|(_, amount)| amount);
    let reservation = reservation.map(|(state, _)| state);

    match award_step(existing.is_some(), attempt.as_ref().map(|a| a.state), reservation) {
        AwardStep::AlreadyAwarded => {
            // Unwrap is sound by construction: `AlreadyAwarded` is returned only for
            // `existing.is_some()`.
            let record = existing.expect("AlreadyAwarded implies a local award record");
            // Close the pending-forever fixed point: if `drive_send`'s confirm write failed
            // while its `record_award` succeeded, the awards row (written ONLY on an ack or a
            // presence-verified repair) is itself the proof the event is public — land the
            // attempt's confirm here, the one arm every later call is guaranteed to reach.
            // No-op unless the attempt is still `pending`; advisory like the original write.
            let _ = store.mark_attempt_confirmed(job_id, now_unix);
            return Ok(AwardOutcome::AlreadyAwarded(record));
        }
        AwardStep::RepairFromAttempt => {
            let attempt = attempt.expect("RepairFromAttempt implies an attempt row");
            // Re-hold the funds first: a PUBLIC award must not sit recorded-but-unfunded.
            // `reserve` is idempotent for a live row and re-reserves a `Released` one; a `Spent`
            // row means the job was already paid (a manual collect can settle without the awards
            // row), and the row is still owed for history/attribution — skip the reserve, write it.
            match store.reserve(job_id, attempt.amount_sats, balance, now_unix) {
                Ok(_) | Err(ReserveRefused::AlreadySpent { .. }) => {}
                Err(refused) => {
                    return Err(AwardError::PublishedButUnrecorded {
                        job_id: job_id.to_owned(),
                        award_event_id: attempt.award_event_id,
                        detail: format!(
                            "the award is public but re-reserving its funds was refused ({refused})"
                        ),
                    });
                }
            }
            return record_and_read_back(store, job_id, &attempt, now_unix)
                .map(AwardOutcome::AlreadyAwarded);
        }
        AwardStep::RefusedTerminal => {
            let attempt = attempt.expect("RefusedTerminal implies an attempt row");
            // Finish a refusal a crash left half-done: funds must not stay committed to a job
            // that can never publish. (Refusal writes refused THEN releases, so this is the
            // recovery for a crash between the two.)
            if matches!(reservation, Some(ReservationState::Reserved)) {
                store.release(job_id, now_unix).map_err(AwardError::Store)?;
            }
            return Err(AwardError::Refused {
                job_id: job_id.to_owned(),
                detail: attempt
                    .detail
                    .unwrap_or_else(|| "the relay refused the award event".to_owned()),
            });
        }
        AwardStep::ResumeAttempt => {
            let attempt = attempt.expect("ResumeAttempt implies an attempt row");
            // Re-hold the reservation before re-sending: reconcile may have released it while the
            // verdict was open (it classifies by claim liveness, not by attempt state), and a
            // send must never race ahead of its funding. Idempotent when still `Reserved`. A
            // `Spent` row means the job was already paid (a manual collect can settle without
            // the awards row) — resolution is then pure bookkeeping the row is still owed for
            // (history, #261 attribution), so it proceeds unfunded exactly as the sibling arms
            // do.
            match store.reserve(job_id, attempt.amount_sats, balance, now_unix) {
                Ok(_) | Err(ReserveRefused::AlreadySpent { .. }) => {}
                Err(refused) => return Err(AwardError::Reserve(refused)),
            }
            return drive_send(store, job_id, attempt, send, now_unix, licensed_prior_sends)
                .await;
        }
        AwardStep::ProbeLegacy => {
            match award_present_on_relay().await {
                Ok(PresenceRead::Present(AwardPresence::Repairable(relayed))) => {
                    // Sound by construction: `ProbeLegacy` is returned only for an existing
                    // reservation row, whichever of the two states it is in.
                    let amount_sats = reserved_amount
                        .expect("ProbeLegacy implies a reservation row carrying an amount");
                    // The award is PUBLIC: re-hold its funds (the `Released` half of this arm is
                    // exactly the #322 ledger state — award public, funds returned) before
                    // repairing the row. A refused re-reserve leaves a public award unfunded,
                    // which is the loud PublishedButUnrecorded case, not a quiet skip.
                    match store.reserve(job_id, amount_sats, balance, now_unix) {
                        Ok(_) | Err(ReserveRefused::AlreadySpent { .. }) => {}
                        Err(refused) => {
                            return Err(AwardError::PublishedButUnrecorded {
                                job_id: job_id.to_owned(),
                                award_event_id: relayed.award_event_id,
                                detail: format!(
                                    "the award is public but re-reserving its funds was refused \
                                     ({refused})"
                                ),
                            });
                        }
                    }

                    // Repair through the SAME seam a fresh award records through, so the repaired
                    // row and a freshly-awarded one cannot drift in shape. A failed write refuses:
                    // the row really is still missing, and saying otherwise would strand the job
                    // as silently as before.
                    store
                        .record_award(
                            job_id,
                            &relayed.claim_id,
                            &relayed.award_event_id,
                            &relayed.seller_pubkey,
                            amount_sats,
                            now_unix,
                        )
                        .map_err(|error| AwardError::PublishedButUnrecorded {
                            job_id: job_id.to_owned(),
                            award_event_id: relayed.award_event_id.clone(),
                            detail: format!("repairing the missing awards row failed: {error}"),
                        })?;

                    // Read the row back instead of returning one assembled in memory. The caller
                    // reads `AlreadyAwarded` as proof the ledger now knows this job, and only a
                    // row that reads back is that proof — an in-memory copy would report the
                    // repair we INTENDED.
                    let record = store
                        .award_record(job_id)
                        .map_err(AwardError::Presence)?
                        .ok_or_else(|| AwardError::PublishedButUnrecorded {
                            job_id: job_id.to_owned(),
                            award_event_id: relayed.award_event_id.clone(),
                            detail: "the repaired awards row did not read back".to_owned(),
                        })?;
                    return Ok(AwardOutcome::AlreadyAwarded(record));
                }
                Ok(PresenceRead::Present(AwardPresence::Unrepairable { award_event_id, detail })) => {
                    return Err(AwardError::PublishedButUnrecorded {
                        job_id: job_id.to_owned(),
                        award_event_id,
                        detail,
                    });
                }
                // The relay ANSWERED (twice — the recheck read) and has no award. Even so, this
                // job never gets a SECOND selection: a pre-attempt binary may have transmitted
                // an award whose verdict was lost, "absent now" is not "never arrives", and the
                // seller side executes per-award — a fresh event naming a different claim is the
                // #322 burn re-armed, and `PresenceRead`'s own contract says re-selecting is not
                // idempotent against a late-materializing event. Fail closed: return the funds
                // and terminalize with the same recovery every refusal carries — a NEW offer.
                // (This also covers OUR OWN reserve→pin crash window, which is locally
                // indistinguishable from the legacy state; forfeiting that sub-second window's
                // offer is the price of never re-selecting.)
                Ok(PresenceRead::ConfirmedAbsent) => {
                    store.release(job_id, now_unix).map_err(AwardError::Store)?;
                    return Err(AwardError::Refused {
                        job_id: job_id.to_owned(),
                        detail: "this job predates the award attempt ledger and the relay \
                                 confirms no award is public; refusing to select a claim again \
                                 (awards are write-once per offer)"
                            .to_owned(),
                    });
                }
                Ok(PresenceRead::Unverified) => {
                    return Err(AwardError::PresenceUnverified {
                        job_id: job_id.to_owned(),
                        detail: "the relay did not demonstrate it was serving reads, so its empty \
                                 answer proves nothing"
                            .to_owned(),
                    });
                }
                Err(error) => {
                    return Err(AwardError::PresenceUnverified {
                        job_id: job_id.to_owned(),
                        detail: error.to_string(),
                    });
                }
            }
        }
        AwardStep::Fresh => {}
    }

    // ---- Fresh: reserve → prepare (validate + sign) → pin → send --------------------------------

    // Reserve before any signing: a refusal signs NOTHING (and writes no row).
    store
        .reserve(job_id, amount, balance, now_unix)
        .map_err(AwardError::Reserve)?;

    let prepared = match prepare().await {
        Ok(prepared) => prepared,
        Err(error) => {
            // Provably wire-free: prepare failed before anything signed was persisted or
            // transmitted (a signed-in-memory event that was never pinned was never sent — the
            // send below only ever transmits pinned bytes). Releasing cannot strand an award.
            store.release(job_id, now_unix).map_err(AwardError::Store)?;
            return Err(AwardError::Publish(error));
        }
    };

    let candidate = AwardAttempt {
        job_id: job_id.to_owned(),
        claim_id: prepared.claim_id,
        seller_pubkey: prepared.seller_pubkey,
        award_event_id: prepared.award_event_id,
        event_json: prepared.event_json,
        amount_sats: amount,
        quoted_mints_json: serde_json::to_string(&prepared.quoted_mints)
            .unwrap_or_else(|_| "[]".to_owned()),
        offer_deadline_unix: prepared.offer_deadline_unix,
        send_count: 0,
        relay_url: prepared.relay_url,
        state: AttemptState::Pending,
        detail: None,
    };
    let attempt = match store.begin_award_attempt(&candidate, now_unix) {
        Ok(BeginAttempt::Pinned) => candidate,
        // An attempt appeared between this call's read and its pin. Unreachable while award calls
        // serialize on the money lock and the daemon holds the home's run lock — so no money
        // moves here: the reservation this call just took stays held (a retry re-reserves
        // idempotently, or the Resume arm tolerates it), this call's candidate is discarded
        // unsent, and the caller is told the retry-safe truth. One conservative arm instead of
        // three unreachable money paths.
        Ok(BeginAttempt::Existing(existing)) => {
            return Err(AwardError::Unresolved {
                job_id: job_id.to_owned(),
                award_event_id: existing.award_event_id,
                detail: "an award attempt for this job appeared concurrently; retry to resolve \
                         the pinned attempt"
                    .to_owned(),
            });
        }
        Err(error) => {
            // The signed event was NOT persisted, so it must not be sent (pin before send).
            // Releasing is as safe as the prepare-failure arm: nothing of ours is on the wire.
            store.release(job_id, now_unix).map_err(AwardError::Store)?;
            return Err(AwardError::Presence(error));
        }
    };

    // A FRESH pin owns its own license: the caller cannot have taken one for bytes that did
    // not exist before this call.
    drive_send(store, job_id, attempt, send, now_unix, None).await
}

/// Transmit a pinned attempt's bytes once and fold the relay's verdict into durable state — the
/// shared tail of the fresh and resume paths, so the two cannot drift on what a verdict means.
///
/// The transmission is COUNTED (durably, before the socket write) because the meaning of an
/// explicit relay refusal depends on it: `OK:false` for the event's FIRST transmission proves
/// nothing is public and safely terminalizes; the same words for a re-send prove nothing about
/// the earlier transmissions whose verdicts were lost, so they hold everything instead — the
/// by-id probe terminalizes honestly after the pay window if the event really never landed.
async fn drive_send<S, SFut>(
    store: &BuyerStore,
    job_id: &str,
    attempt: super::store::AwardAttempt,
    send: S,
    now_unix: i64,
    licensed_prior_sends: Option<u64>,
) -> Result<AwardOutcome, AwardError>
where
    S: FnOnce(String, String) -> SFut,
    SFut: Future<Output = SendOutcome>,
{
    // `licensed_prior_sends` is `Some` when the CALLER already took the transmission license
    // (the sweep counts, then transmits outside the money lock, then replays the verdict here):
    // counting again would push a genuinely-first transmission to `prior == 1`, and a deliberate
    // relay refusal of it would then hold funds for the whole pay window instead of releasing at
    // once. `None` means this call owns the license and takes it now, before the socket write.
    //
    // ⚠ A CARRIED license is license-order truth, not WIRE-order truth, so it is reconciled
    // against the freshly-read row before it may license anything. The sweep transmits outside
    // the money lock, so another path can license AND transmit its own copy of these bytes
    // meanwhile — and that copy may have LANDED with its `OK` lost. `attempt` here is the
    // chokepoint's under-guard read (see the reads at the top of `award_with_reservation`), so
    // `attempt.send_count` counts every transmission ever started, including that one. Taking the
    // max means a stale `prior == 0` can never license a terminal refusal for bytes a concurrent
    // sender may have made public (round-5 review): the verdict folds to a hold instead.
    let prior_sends = match licensed_prior_sends {
        Some(prior) => {
            let observed = prior.max(attempt.send_count.saturating_sub(1));
            if observed != prior {
                crate::opline!(
                    "buyer: award {} for {job_id} was transmitted concurrently (licensed after \
                     {prior} starts, {} recorded now); this verdict judges one copy only and \
                     cannot be terminal",
                    attempt.award_event_id, attempt.send_count
                );
            }
            observed
        }
        None => store
            .record_attempt_send(job_id, now_unix)
            .map_err(AwardError::Presence)?,
    };
    match send(attempt.event_json.clone(), attempt.award_event_id.clone()).await {
        SendOutcome::Acked => {
            // Confirm THEN record. Neither write failing un-acks the relay, so neither may fail
            // the award: a failed confirm leaves the attempt pending with its awards row present,
            // and the `AlreadyAwarded` arm re-confirms it on the next award call or sweep pass; a
            // failed record leaves a confirmed attempt without its row — the boot heal writes it
            // from the attempt. Both converge without an operator; say what happened either way.
            if let Err(error) = store.mark_attempt_confirmed(job_id, now_unix) {
                crate::opline!(
                    "buyer: award for {job_id} acked but confirming the attempt failed ({error}); \
                     the next award call re-confirms it from the recorded award"
                );
            }
            if let Err(error) = store.record_award(
                job_id,
                &attempt.claim_id,
                &attempt.award_event_id,
                &attempt.seller_pubkey,
                attempt.amount_sats,
                now_unix,
            ) {
                crate::opline!(
                    "buyer: award for {job_id} confirmed ({}) but recording it failed ({error}); \
                     the boot heal will write the awards row from the pinned attempt",
                    attempt.award_event_id
                );
            }
            Ok(AwardOutcome::Published(AwardClaimOutcome {
                award_event_id: attempt.award_event_id,
                job_id: job_id.to_owned(),
                claim_id: attempt.claim_id,
                seller_pubkey: attempt.seller_pubkey,
                quoted_mints: serde_json::from_str(&attempt.quoted_mints_json).unwrap_or_default(),
            }))
        }
        SendOutcome::Refused { detail } => {
            // An explicit relay refusal proves "nothing is public" ONLY for the event's first
            // transmission. On a re-send it judges THIS transmission alone — the pending state
            // exists precisely because an earlier send's verdict was lost, and that send may
            // have landed (relay policy drift, membership churn, an aged `created_at` bouncing
            // off a timestamp bound). Releasing on it would repudiate a possibly-executing
            // seller; hold instead, and let the by-id probe terminalize after the pay window.
            if prior_sends > 0 {
                return Err(AwardError::Unresolved {
                    job_id: job_id.to_owned(),
                    award_event_id: attempt.award_event_id,
                    detail: format!(
                        "the relay refused a RE-send ({detail}); an earlier transmission's \
                         verdict was lost and may have landed — holding funds; the probe \
                         resolves this after the pay window"
                    ),
                });
            }
            // Refused THEN released, so a crash between the two leaves refused+reserved — the
            // state `RefusedTerminal` finishes — and never released+pending (which would read as
            // a resumable attempt whose funds are gone). The release is licensed by WINNING the
            // transition: `false` here means another resolver terminalized the attempt while our
            // send was in flight (all drive_send callers hold the money lock, so this is
            // belt-and-braces — but the check costs nothing and the release must never rest on
            // an assumption when the store can state the fact).
            if !store
                .mark_attempt_refused(job_id, &detail, now_unix)
                .map_err(AwardError::Presence)?
            {
                return Err(AwardError::Unresolved {
                    job_id: job_id.to_owned(),
                    award_event_id: attempt.award_event_id,
                    detail: format!(
                        "the relay refused this transmission ({detail}), but the attempt was \
                         resolved concurrently — leaving the other resolver's verdict in place"
                    ),
                });
            }
            store.release(job_id, now_unix).map_err(AwardError::Store)?;
            Err(AwardError::Refused { job_id: job_id.to_owned(), detail })
        }
        // No verdict: hold EVERYTHING. The reservation stays, the attempt stays pending, and the
        // caller is told retrying is safe. This arm writing nothing is the #322 fix in one line —
        // the old path released here and re-selected a claim on the next call.
        SendOutcome::Unresolved { detail } => Err(AwardError::Unresolved {
            job_id: job_id.to_owned(),
            award_event_id: attempt.award_event_id,
            detail,
        }),
    }
}

/// Write the `awards` row from a CONFIRMED attempt through the same `record_award` seam a fresh
/// award uses, and read it back — only a row that reads back proves the ledger now knows the job.
fn record_and_read_back(
    store: &BuyerStore,
    job_id: &str,
    attempt: &super::store::AwardAttempt,
    now_unix: i64,
) -> Result<AwardRecord, AwardError> {
    store
        .record_award(
            job_id,
            &attempt.claim_id,
            &attempt.award_event_id,
            &attempt.seller_pubkey,
            attempt.amount_sats,
            now_unix,
        )
        .map_err(|error| AwardError::PublishedButUnrecorded {
            job_id: job_id.to_owned(),
            award_event_id: attempt.award_event_id.clone(),
            detail: format!("writing the awards row from the confirmed attempt failed: {error}"),
        })?;
    store
        .award_record(job_id)
        .map_err(AwardError::Presence)?
        .ok_or_else(|| AwardError::PublishedButUnrecorded {
            job_id: job_id.to_owned(),
            award_event_id: attempt.award_event_id.clone(),
            detail: "the awards row written from the confirmed attempt did not read back".to_owned(),
        })
}

/// Failure of [`settle_after_pay`].
#[derive(Debug)]
pub enum SettleError<E> {
    /// The pay leg (budget append + wallet melt) failed; the reservation was left untouched
    /// (still `reserved`), so no funds were dropped from either ceiling.
    Pay(E),
    /// The pay leg succeeded but the reserved→spent flip failed. The budget append + melt already
    /// landed (conservative: `available` under-stated by `amount`); reconcile's `Paid` disposition
    /// converges the dangling reservation on the next start.
    Store(StoreError),
}

/// Convert `job_id`'s reservation `reserved → spent` — but ONLY after `pay` succeeds.
///
/// `pay` MUST perform the two effects that take the amount up elsewhere — the budget-ledger append
/// (`crate::budget`) AND the wallet melt — before this flips it out of `reserved`. Sequenced this
/// way, the amount is never counted in NEITHER term: it stays in `reserved` until `pay` has moved
/// it into `spent` + melted, then the flip closes the handoff (see the ordering obligation on
/// [`BuyerStore::convert_to_spent`]). This is the ordering the #123 reservation ledger documented
/// and the #126 wiring must honor.
///
/// If `pay` fails, the flip is NOT reached, so a failed/incomplete payment can never drop the
/// reservation and over-state `available`. `amount_of` reads the settled amount off the pay
/// outcome (it only affects the flip when the job had no prior reservation — an externally-accepted
/// job — in which case a `spent` row is inserted for that amount).
pub async fn settle_after_pay<T, E, F, Fut>(
    store: &BuyerStore,
    job_id: &str,
    now_unix: i64,
    pay: F,
    amount_of: impl FnOnce(&T) -> u64,
) -> Result<(T, Converted), SettleError<E>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    // Pay FIRST: the budget append + wallet melt both take the amount up before the flip below
    // takes it out of `reserved`.
    let paid = pay().await.map_err(SettleError::Pay)?;
    let converted = store
        .convert_to_spent(job_id, amount_of(&paid), now_unix)
        .map_err(SettleError::Store)?;
    Ok((paid, converted))
}

/// A reserved job's payment progress, folded from its payment journal, as reconcile sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentProgress {
    /// A payment attempt reached `Closed` — the budget append + melt are durable, the receipt is
    /// published. The dangling reservation must become `spent`.
    Closed,
    /// A payment attempt reached `Sent`/`ReceiptPublished` but not `Closed` — ambiguous
    /// (PAYMENT_UNCERTAIN): the ecash may already have left. Must NOT auto-release; the phase-3
    /// payment saga (#127) resolves it.
    Uncertain,
    /// A payment attempt exists but never left funds — the journal folded to `Intent`/`Locked`.
    ///
    /// Distinct from [`Self::None`] because "a debt being retried" and "an award nothing ever
    /// attempted to pay" are different facts, and only the second is a leaked reservation. The
    /// journal already records the difference; folding both into one variant discarded it one
    /// step before the decision that needs it.
    Attempted,
    /// No payment attempt exists at all — this job has no payment journal.
    None,
}

/// Whether re-arming a pending auto-award should re-attempt the reserve-then-award, or skip because
/// the job is already awarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RearmAction {
    /// The job is already awarded — do not award again.
    Skip,
    /// No award exists yet — run the reserve-then-award path.
    Attempt,
}

/// The CHEAP first pass of the idempotent re-arm decision (#126/#127 invariant A: never award
/// twice): skip without any further work when the relay already shows our 3405, or the reservation
/// is already `Spent` (collect paid it).
///
/// ⚠ **`Attempt` does not mean "publish" — it means "go to the chokepoint and let it decide."**
/// [`award_with_reservation`] holds the actual guard. This distinction is load-bearing because
/// `award_on_relay == false` conflates three different things (no award; a relay error; a read that
/// timed out, which [`crate::job_lifecycle::award_presence_async`] also reports as no award), and
/// only one of them means the job is unawarded.
///
/// In particular a `Reserved` row with no relay award is NOT necessarily the crash window between
/// reserve and publish. It is equally the window where `publish` succeeded and `record_award` then
/// failed — a state [`award_with_reservation`] itself creates and logs. Those two want opposite
/// actions, and reserve-idempotency does not separate them: `reserve` returning
/// [`super::reservations::Reserved::Idempotent`] protects the BUDGET from a second commitment, and
/// does nothing to stop `publish` from putting a second 3405 on the relay. Deciding here would
/// therefore be deciding on a signal that cannot tell the healthy case from the duplicate; the
/// chokepoint decides instead, against the local `awards` row, which can.
pub fn plan_rearm(award_on_relay: bool, reservation: Option<ReservationState>) -> RearmAction {
    if award_on_relay {
        return RearmAction::Skip;
    }
    if matches!(reservation, Some(ReservationState::Spent)) {
        return RearmAction::Skip;
    }
    RearmAction::Attempt
}

/// What the auto-award loop does when the job view carries no offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MissingOfferAction {
    /// The relay answered and had no offer for this job. Absence is established; park is honest.
    ParkOfferAbsent,
    /// The read went unanswered and the retry budget remains. Read again — conclude nothing.
    Retry,
    /// The read never got answered and the budget is spent. Park, but on what we OBSERVED
    /// (reads that went unanswered), never on the offer, whose presence we never established.
    ParkUnreadable { unanswered_reads: u32 },
}

/// Decide what an empty offer read means (#291).
///
/// `fetch_events` resolves `Ok(empty)` on timeout, so "the relay has no offer" and "we stopped
/// waiting" arrive as identical bytes. [`JobView::read_confirmed`] is the discriminator, obtained
/// one layer out by asking the relay for an `EOSE` it owes us. This function is the only place that
/// turns those two facts into an action, so the two cannot drift apart again.
///
/// ⚠ **The park is terminal** — the driver never retries a parked row — so `ParkOfferAbsent` may be
/// returned only on a confirmed read. Before #291 this decision did not exist: an empty read parked
/// a live, claimed, real-money job with 5.8 hours left on its deadline, under a reason that was
/// false in every clause.
///
/// ⚠ **`ParkUnreadable` is a floor, not a fallback to the old behaviour.** Refusing forever is an
/// infinite loop, so the budget has to end somewhere — but it ends on a statement about the READ.
/// A bound that parked with "offer no longer on the relay" would have reintroduced the whole defect
/// on a delay.
pub fn plan_missing_offer(
    read_confirmed: bool,
    unanswered_reads: u32,
    max_unanswered_reads: u32,
) -> MissingOfferAction {
    if read_confirmed {
        return MissingOfferAction::ParkOfferAbsent;
    }
    if unanswered_reads < max_unanswered_reads {
        return MissingOfferAction::Retry;
    }
    MissingOfferAction::ParkUnreadable { unanswered_reads }
}

/// Park reason for [`MissingOfferAction::ParkOfferAbsent`]. Says the relay ANSWERED, because that
/// is what makes the emptiness mean anything.
pub const PARK_REASON_OFFER_ABSENT: &str =
    "relay answered our read and returned no offer for this job";

/// Park reason for [`MissingOfferAction::ParkUnreadable`].
///
/// The wording lives beside the decision, not at the call site, so the reason a row carries and the
/// evidence that produced it cannot drift. It states the observation and explicitly declines the
/// conclusion — #291 was filed because a row asserted the offer was gone on evidence that could not
/// establish it.
pub fn park_reason_unreadable(unanswered_reads: u32) -> String {
    format!(
        "{unanswered_reads} consecutive job reads went unanswered by the relay; the offer's \
         presence was never established either way"
    )
}

/// Park reason for an auto-award whose offer deadline passed with nothing awardable.
///
/// `capability` is the clause from [`capability_park_reason`], or `None` when the capability request
/// was not the obstacle. With `None` the wording is exactly what it was before capability requests
/// existed, so a job that asked for nothing parks with an unchanged row.
///
/// The wording lives beside the decision rather than at the call site, matching
/// [`park_reason_unreadable`] — the reason a row carries and the evidence that produced it cannot
/// drift, and it was previously the one park reason spelled as a duplicated literal at its call site.
///
/// The deadline is still stated when a capability clause is present, because both facts are true and
/// they imply different actions: the deadline says this job is over, the clause says what would have
/// to change for the next one to succeed.
pub fn park_reason_deadline_passed(capability: Option<&str>) -> String {
    match capability {
        Some(clause) => {
            format!("offer deadline passed before an awardable claim appeared — {clause}")
        }
        None => "offer deadline passed before an awardable claim appeared".to_owned(),
    }
}

/// Classify a reserved job for [`BuyerStore::reconcile`] from its payment progress + relay
/// liveness. The payment journal is authoritative over relay liveness: a `Closed` payment is
/// `Paid` regardless of whether the claim still looks live, and an ambiguous payment is KEPT
/// (`Payable`) even if the claim looks dead — the funds may have moved, so only the phase-3 saga
/// may resolve it. A job with no payment is `Dead` only when it is no longer payable on the relay.
///
/// [`PaymentProgress::Attempted`] and [`PaymentProgress::None`] classify identically here: both are
/// "no funds have left", so both are `Dead` exactly when the relay says the claim is gone. The two
/// are separate variants so a *later* rule can tell them apart — an unattempted award past its
/// deadline is a leak, whereas a retried-and-refused debt is correctly held — without this
/// classifier changing behaviour today.
pub fn classify_disposition(payment: PaymentProgress, claim_payable: bool) -> JobDisposition {
    match payment {
        PaymentProgress::Closed => JobDisposition::Paid,
        PaymentProgress::Uncertain => JobDisposition::Payable,
        PaymentProgress::Attempted | PaymentProgress::None if claim_payable => {
            JobDisposition::Payable
        }
        PaymentProgress::Attempted | PaymentProgress::None => JobDisposition::Dead,
    }
}

/// The local-clock floor's policy, read from `[buyer_reservation_floor]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnattemptedFloor {
    /// Ships **false**. While false this is the identity function, unconditionally.
    pub enabled: bool,
    /// Seconds past `reservations.created_at_unix` before a release is permitted.
    pub grace_secs: u64,
}

/// Upgrade `Payable → Dead` for a reservation nothing ever attempted to pay, once it is older than
/// the floor — WITHOUT consulting the relay.
///
/// This exists because every other exit from `reserved` needs the relay to supply something: the
/// offer, so a claim can be derived expired, or the claim's disappearance. When neither arrives the
/// funds stay committed with no local recourse. `created_at_unix` cannot become unreachable.
///
/// Four properties hold it narrow, each one a way this could wrongly free money:
/// - **Disabled is the identity.** No age, payment state, or verdict can produce a release.
/// - **[`PaymentProgress::None`] only.** [`PaymentProgress::Attempted`] is a debt being retried and
///   is indistinguishable from a leak in the reservation row alone — that collapse is exactly what
///   this change split apart, and releasing it would free money the buyer genuinely owes.
/// - **`Payable → Dead` only.** `Paid` is untouched, so reconcile's `Paid` arm remains the sole
///   converger for a pay whose `reserved → spent` flip failed.
/// - **An unknown age never releases.** `None` means the row was not read, and absence of evidence
///   is not evidence of a leak.
pub fn apply_unattempted_floor(
    verdict: JobDisposition,
    payment: PaymentProgress,
    age_secs: Option<u64>,
    floor: UnattemptedFloor,
) -> JobDisposition {
    if !floor.enabled {
        return verdict;
    }
    match (verdict, payment, age_secs) {
        (JobDisposition::Payable, PaymentProgress::None, Some(age)) if age > floor.grace_secs => {
            JobDisposition::Dead
        }
        (other, _, _) => other,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetGate;
    use crate::gateway::creq::build_seller_creq;
    use crate::home::{self, DEFAULT_MINT_URL};
    use crate::job_lifecycle::{ClaimView, OfferView, RelayedAward};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-buyer-lifecycle-{label}-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    fn fresh_store(label: &str) -> (BuyerStore, std::path::PathBuf) {
        let path = temp_db(label);
        let _ = std::fs::remove_file(&path);
        (BuyerStore::open(&path).expect("open"), path)
    }

    const SELLER_HEX: &str = "aa1e5f8c9d3b6a2f4e7c1d0b8a5f3e2c1d0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f";

    fn offer_view(job_id: &str, amount: u64) -> OfferView {
        OfferView {
            event_id: job_id.to_owned(),
            created_at: 0,
            author_pubkey: "b".repeat(64),
            author_display_name: None,
            task: "t".into(),
            output: "o".into(),
            amount_sats: amount,
            deadline_unix: 1_900_000_000,
            seller_pubkey: Some(SELLER_HEX.to_owned()),
            seller_display_name: None,
            targeted: true,
            repo: None,
            branch: None,
            job_class: None,
            contribution: None,
            // The NEUTRAL request — a job that asks for nothing. The default on purpose: every test
            // built on this fixture asserts today's award behaviour, and that behaviour must be
            // byte-unchanged by #897. Tests that exercise a request set these fields explicitly.
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: Vec::new(),
        }
    }

    fn claim(job_id: &str, live: bool, creq_amount: u64, mints: &[String]) -> ClaimView {
        let creq = build_seller_creq(job_id, creq_amount, "sat", mints, SELLER_HEX).expect("creq");
        ClaimView {
            // The UNSTATED capability — a seat advertising nothing. Set explicitly because `ClaimView`
            // has no `Default` derive and must not gain one: a default `SandboxConfig` is a meaningful
            // object, but a default `ClaimView` is a claim that never existed.
            capability: crate::heartbeat::SeatCapability::default(),
            claim_id: "c".repeat(64),
            created_at: 1,
            seller_pubkey: SELLER_HEX.to_owned(),
            display_name: None,
            status: "processing".into(),
            live,
            creq: Some(creq),
            agents: Vec::new(),
        }
    }

    fn view_with(job_id: &str, amount: u64, claims: Vec<ClaimView>) -> JobView {
        JobView {
            job_id: job_id.to_owned(),
            offer: Some(offer_view(job_id, amount)),
            claims,
            results: Vec::new(),
            live_claim_id: None,
            accepted: None,
            pending: false,
            read_confirmed: true,
        }
    }

    fn filters<'a>(offer_amount: u64, max_sats: u64) -> AwardFilters<'a> {
        AwardFilters {
            offer_amount_sats: offer_amount,
            max_sats,
            buyer_mint: DEFAULT_MINT_URL,
            allow_real_mints: false,
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: &[],
        }
    }

    // A live claim priced at the offer amount, quoting the buyer's default mint, is selected.
    #[test]
    fn select_picks_live_payable_claim() {
        let job = "a".repeat(64);
        let view = view_with(&job, 10, vec![claim(&job, true, 10, &[DEFAULT_MINT_URL.into()])]);
        let selected = select_awardable_claim(&view, &filters(10, 100));
        assert_eq!(selected.as_deref(), Some("c".repeat(64).as_str()));
    }

    // TOOTH (charter invariant 5, the strong one) — a job that asked for a harness is never
    // awarded to a claim that does not advertise it, on EITHER award path. Everything else about
    // these claims is payable: same price, same mint, live. Bite: drop the
    // `claim_serves_requested_agent` arm from `select_awardable_claim` and the codex-only claim
    // below wins a claude job; drop it from `named_claim_awardable` and the manual path pays it.
    #[test]
    fn a_job_requesting_a_harness_is_never_awarded_to_a_claim_without_it() {
        let job = "a".repeat(64);
        let mut codex_only = claim(&job, true, 10, &[DEFAULT_MINT_URL.into()]);
        codex_only.agents = vec!["codex".to_owned()];
        let mut silent = claim(&job, true, 10, &[DEFAULT_MINT_URL.into()]);
        silent.claim_id = "d".repeat(64);
        let view = view_with(&job, 10, vec![codex_only, silent]);

        let mut wants_claude = filters(10, 100);
        wants_claude.requested_agent = Some("claude");
        assert_eq!(
            select_awardable_claim(&view, &wants_claude),
            None,
            "neither a wrong-harness claim nor a silent one may win a claude job"
        );
        // …and the manual path refuses by NAME, with the reason on the error.
        let refused = named_claim_awardable(&view, &"c".repeat(64), &wants_claude)
            .expect_err("manual award must apply the same filter");
        assert!(
            matches!(refused, NamedAwardRefused::AgentMismatch { .. }),
            "unexpected refusal: {refused:?}"
        );
        assert!(refused.to_string().contains("claude"), "{refused}");
        assert!(
            matches!(
                named_claim_awardable(&view, &"d".repeat(64), &wants_claude),
                Err(NamedAwardRefused::AgentMismatch { .. })
            ),
            "a claim advertising nothing does not satisfy a request either"
        );

        // The claim that DOES advertise it is awarded, so the filter selects rather than blocks.
        let mut wants_codex = filters(10, 100);
        wants_codex.requested_agent = Some("codex");
        assert_eq!(
            select_awardable_claim(&view, &wants_codex).as_deref(),
            Some("c".repeat(64).as_str())
        );
        assert!(named_claim_awardable(&view, &"c".repeat(64), &wants_codex).is_ok());
    }

    // TOOTH — the capability predicate governs EVERY award-selection entry point, not just the one
    // that happens to call it. This is the tripwire for a BYPASS, so its red condition is the
    // PROPERTY (a violating claim is refused via both entry points), never the shape of a struct
    // literal: an inert-field counter passes happily while a whole path skips the predicate.
    //
    // Bite, per path: delete the `claim_meets_capability_request` arm from `named_claim_awardable`
    // and every `manual …` assertion below goes red; delete it from `select_awardable_claim` and
    // every `auto …` assertion goes red. Each axis is asserted on BOTH paths before the next axis
    // begins, so the failure names the axis AND the path rather than stopping at the first one.
    #[test]
    fn every_award_entry_point_applies_the_capability_predicate() {
        let job = "a".repeat(64);
        let mut named = claim(&job, true, 10, &[DEFAULT_MINT_URL.into()]);
        named.capability = crate::heartbeat::SeatCapability {
            harness_families: vec!["codex".to_owned()],
            models: vec![crate::heartbeat::HarnessModel {
                family: "codex".to_owned(),
                model: "sonnet".to_owned(),
            }],
            capabilities: vec!["python".to_owned()],
            harness_variant: None,
            hardware: None,
        };
        // The seat serves the `codex` PRESET as well as the family. A model axis now has to name a
        // preset, because dispatch reads nothing else — so without this the model case would be
        // refused for an unserved preset and would stop exercising the model rule at all.
        named.agents = vec!["codex".to_owned()];
        let view = view_with(&job, 10, vec![named]);
        let id = "c".repeat(64);

        // Every axis is COLLECTED, not asserted in place, and the verdict is taken once at the end.
        // An assert fires on the first axis and silences the rest, so a bite that breaks all three
        // would look identical to one that breaks only the first — and the reader would fix one.
        let mut failures: Vec<String> = Vec::new();

        // Axis 1 — harness family the seat does not advertise.
        let mut wants_family = filters(10, 100);
        wants_family.requested_harness_family = Some("claude-code");
        if select_awardable_claim(&view, &wants_family).is_some() {
            failures.push("auto family: a codex-only seat won a claude-code job".to_owned());
        }
        match named_claim_awardable(&view, &id, &wants_family) {
            Err(NamedAwardRefused::Capability { refusal: CapabilityRefusal::HarnessFamily { .. }, .. }) => {}
            other => failures.push(format!("manual family: expected a family refusal, got {other:?}")),
        }

        // Axis 2 — right family, model the seat does not advertise. The PAIR is the unit.
        let mut wants_model = filters(10, 100);
        wants_model.requested_agent = Some("codex");
        wants_model.requested_harness_family = Some("codex");
        wants_model.requested_model = Some("opus");
        if select_awardable_claim(&view, &wants_model).is_some() {
            failures.push("auto model: a family match alone won a job naming a model".to_owned());
        }
        match named_claim_awardable(&view, &id, &wants_model) {
            Err(NamedAwardRefused::Capability { refusal: CapabilityRefusal::Model { .. }, .. }) => {}
            other => failures.push(format!("manual model: expected a model refusal, got {other:?}")),
        }

        // Axis 3 — capability token the seat does not advertise.
        let needs_rust = vec!["rust".to_owned()];
        let mut wants_caps = filters(10, 100);
        wants_caps.required_capabilities = &needs_rust;
        if select_awardable_claim(&view, &wants_caps).is_some() {
            failures.push("auto capabilities: a python-only seat won a rust job".to_owned());
        }
        match named_claim_awardable(&view, &id, &wants_caps) {
            Err(NamedAwardRefused::Capability { refusal: CapabilityRefusal::Capabilities { .. }, .. }) => {}
            other => failures.push(format!("manual capabilities: expected a token refusal, got {other:?}")),
        }

        // POSITIVE CONTROL — a predicate that refused EVERYTHING would satisfy all six checks
        // above. That is a different bug wearing the same green, and only this catches it.
        let conforming_caps = vec!["python".to_owned()];
        let mut conforming = filters(10, 100);
        conforming.requested_agent = Some("codex");
        conforming.requested_harness_family = Some("codex");
        conforming.requested_model = Some("sonnet");
        conforming.required_capabilities = &conforming_caps;
        if select_awardable_claim(&view, &conforming).as_deref() != Some(id.as_str()) {
            failures.push("auto control: a claim meeting all three axes was NOT awarded".to_owned());
        }
        if named_claim_awardable(&view, &id, &conforming) != Ok(()) {
            failures.push("manual control: a claim meeting all three axes was NOT awarded".to_owned());
        }

        assert!(
            failures.is_empty(),
            "the capability predicate is not applied at every award entry point:\n  {}",
            failures.join("\n  ")
        );
    }

    // Compat: with no harness requested the award path behaves exactly as before — a claim that
    // advertises nothing is still awardable, so existing sellers keep winning existing jobs.
    #[test]
    fn no_harness_request_awards_exactly_as_before() {
        let job = "a".repeat(64);
        let view = view_with(&job, 10, vec![claim(&job, true, 10, &[DEFAULT_MINT_URL.into()])]);
        let unfiltered = filters(10, 100);
        assert!(unfiltered.requested_agent.is_none());
        assert_eq!(
            select_awardable_claim(&view, &unfiltered).as_deref(),
            Some("c".repeat(64).as_str())
        );
        // The explicit "any" is the same case as no request at all.
        let mut any = filters(10, 100);
        any.requested_agent = Some("any");
        assert_eq!(
            select_awardable_claim(&view, &any).as_deref(),
            Some("c".repeat(64).as_str())
        );
    }

    // A non-live claim is never selected (nothing to award yet).
    #[test]
    fn select_skips_non_live_claim() {
        let job = "a".repeat(64);
        let view = view_with(&job, 10, vec![claim(&job, false, 10, &[DEFAULT_MINT_URL.into()])]);
        assert_eq!(select_awardable_claim(&view, &filters(10, 100)), None);
    }

    // Mint compatibility is a HARD filter: a live claim quoting only a mint the buyer cannot pay
    // from is skipped — the buyer must never auto-award a claim it cannot settle.
    #[test]
    fn select_skips_claim_with_no_payable_mint() {
        let job = "a".repeat(64);
        // The seller lists only a foreign testnut mint; the buyer's default mint is not among it.
        let view = view_with(
            &job,
            10,
            vec![claim(&job, true, 10, &["https://foreign.testnut.example".into()])],
        );
        assert_eq!(select_awardable_claim(&view, &filters(10, 100)), None);
    }

    // Over the buyer's ceiling: an offer amount above max_sats yields no selection.
    #[test]
    fn select_skips_when_offer_over_max_sats() {
        let job = "a".repeat(64);
        let view = view_with(&job, 50, vec![claim(&job, true, 50, &[DEFAULT_MINT_URL.into()])]);
        assert_eq!(select_awardable_claim(&view, &filters(50, 40)), None);
    }

    // A claim whose creq price diverges from the offer amount can never be accepted, so it is not
    // payable and must be skipped.
    #[test]
    fn select_skips_claim_priced_off_the_offer() {
        let job = "a".repeat(64);
        let view = view_with(&job, 10, vec![claim(&job, true, 11, &[DEFAULT_MINT_URL.into()])]);
        assert_eq!(select_awardable_claim(&view, &filters(10, 100)), None);
    }

    // MANUAL-AWARD max_sats tooth: a named claim applies the SAME hard filters as auto-award, so
    // max_sats is enforced (not dead input) on the manual path.
    #[test]
    fn named_claim_awardable_accepts_live_payable_within_max() {
        let job = "a".repeat(64);
        let claim_id = "c".repeat(64);
        let view = view_with(&job, 10, vec![claim(&job, true, 10, &[DEFAULT_MINT_URL.into()])]);
        assert_eq!(named_claim_awardable(&view, &claim_id, &filters(10, 100)), Ok(()));
    }

    // Over the ceiling: a manual award of a claim whose offer price exceeds max_sats is refused —
    // the check that was missing (max_sats was ignored) on the manual path.
    #[test]
    fn named_claim_over_max_sats_refused() {
        let job = "a".repeat(64);
        let claim_id = "c".repeat(64);
        let view = view_with(&job, 50, vec![claim(&job, true, 50, &[DEFAULT_MINT_URL.into()])]);
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters(50, 40)),
            Err(NamedAwardRefused::OverMax { offer_amount_sats: 50, max_sats: 40 })
        );
    }

    // A named claim that is not on the relay is refused as NotFound.
    #[test]
    fn named_claim_not_found_refused() {
        let job = "a".repeat(64);
        let view = view_with(&job, 10, vec![claim(&job, true, 10, &[DEFAULT_MINT_URL.into()])]);
        let missing = "d".repeat(64);
        assert_eq!(
            named_claim_awardable(&view, &missing, &filters(10, 100)),
            Err(NamedAwardRefused::NotFound { claim_id: missing })
        );
    }

    // A named but non-live claim is refused (nothing live to award).
    #[test]
    fn named_claim_not_live_refused() {
        let job = "a".repeat(64);
        let claim_id = "c".repeat(64);
        let view = view_with(&job, 10, vec![claim(&job, false, 10, &[DEFAULT_MINT_URL.into()])]);
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters(10, 100)),
            Err(NamedAwardRefused::NotLive { claim_id })
        );
    }

    // A named claim quoting only a mint the buyer cannot settle at is refused as Unpayable — the
    // manual path never awards a claim it cannot pay.
    #[test]
    fn named_claim_unpayable_mint_refused() {
        let job = "a".repeat(64);
        let claim_id = "c".repeat(64);
        let view = view_with(
            &job,
            10,
            vec![claim(&job, true, 10, &["https://foreign.testnut.example".into()])],
        );
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters(10, 100)),
            Err(NamedAwardRefused::Unpayable { claim_id })
        );
    }

    // AWARD-REFUSED tooth: when the reservation is refused, `prepare` is NEVER called (nothing is
    // even signed) and NO reservation row is written. Red-on-revert: reserving AFTER the prepare
    // would fire the prepare closure here (the flag flips), failing the "must not sign" assertion.
    #[tokio::test(flavor = "current_thread")]
    async fn award_refused_signs_nothing_and_writes_no_row() {
        let (store, path) = fresh_store("award-refused");
        let job_a = "a".repeat(64);
        let job_b = "b".repeat(64);
        // Reserve the whole balance against job_a so job_b cannot fit.
        store.reserve(&job_a, 100, 100, 1).expect("first reserve");

        let prepared = AtomicBool::new(false);
        let error = award_with_reservation(
            &store,
            &job_b,
            40,
            100,            2,
            no_relay,
            || {
                prepared.store(true, Ordering::SeqCst);
                async { unreachable!("prepare must not run when the reservation is refused") }
            },
            no_send,
            None,
        )
        .await
        .expect_err("over-available award must refuse");

        assert!(matches!(error, AwardError::Reserve(ReserveRefused::InsufficientAvailable { .. })));
        assert!(!prepared.load(Ordering::SeqCst), "a refused reservation must sign NOTHING");
        assert!(store.reservation(&job_b).expect("read").is_none(), "refused award writes NO row");
        assert!(store.award_attempt(&job_b).expect("read").is_none(), "and pins NO attempt");
        assert_eq!(store.reserved_in_flight().expect("r"), 100, "only job_a's reserve stands");
        let _ = std::fs::remove_file(&path);
    }

    // A PREPARE failure (validate/sign, before anything is pinned or sent) after a successful
    // reservation RELEASES it, so the funds return to available rather than stranding against a
    // job with no live award. This release is provably safe — the failure precedes the pin, so
    // nothing of ours can be on the wire. Contrast the SEND-failure test below, where releasing
    // is exactly the #322 bug.
    #[tokio::test(flavor = "current_thread")]
    async fn award_prepare_failure_releases_the_reservation() {
        let (store, path) = fresh_store("award-prepare-fail");
        let job = "a".repeat(64);
        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            1,
            no_relay,
            || async { Err(JobLifecycleError::Relay("claim vanished from the relay".into())) },
            no_send,
            None,
        )
        .await
        .expect_err("prepare failed");
        assert!(matches!(error, AwardError::Publish(_)));
        assert_eq!(store.reserved_in_flight().expect("r"), 0, "prepare failure reclaimed the reserve");
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::super::reservations::ReservationState::Released)
        );
        // Nothing was signed or pinned, so nothing may be recorded as awarded — otherwise the
        // delivery watcher would sweep a job that has no award on the relay.
        assert!(
            store.award_record(&job).expect("read").is_none(),
            "a failed prepare must record no award"
        );
        assert!(
            store.award_attempt(&job).expect("read").is_none(),
            "a failed prepare must pin no attempt — the job stays freely re-plannable"
        );
        let _ = std::fs::remove_file(&path);
    }

    // The published award is recorded at the reserve-then-award SEAM, not at the call sites. Both
    // the manual `award` RPC and the background auto-award reach the relay through this one
    // function, so recording here is what makes the delivery watcher able to see EITHER — and what
    // stops the two paths from drifting apart. Red-on-revert: drop the `record_award` call and the
    // job becomes invisible to the watcher (`awarded_unsettled_job_ids` returns empty), which is
    // exactly the "awarded but never auto-settled" failure.
    #[tokio::test(flavor = "current_thread")]
    async fn a_published_award_is_recorded_at_the_single_seam() {
        let (store, path) = fresh_store("award-recorded");
        let job = "a".repeat(64);
        let claim = "c".repeat(64);
        let award_event = "e".repeat(64);

        let outcome = award_with_reservation(
            &store,
            &job,
            40,
            100,            7,
            no_relay,
            || {
                let job = job.clone();
                async move { Ok(fake_prepared(&job)) }
            },
            send_acked,
            None,
        )
        .await
        .expect("award");
        let AwardOutcome::Published(outcome) = outcome else {
            panic!("a fresh job must PUBLISH, not report an existing award");
        };
        assert_eq!(outcome.award_event_id, award_event);

        let recorded = store.award_record(&job).expect("read").expect("award recorded");
        assert_eq!(recorded.award_event_id, award_event);
        assert_eq!(recorded.claim_id, claim);
        assert_eq!(recorded.seller_pubkey, SELLER_HEX);
        assert_eq!(recorded.amount_sats, 40, "the RESERVED amount, not a seller-quoted one");
        assert_eq!(recorded.awarded_at_unix, 7);

        // The attempt is pinned AND confirmed: durable proof of exactly which bytes carry this
        // job's award, in the state that says the relay holds them.
        let attempt = store.award_attempt(&job).expect("read").expect("attempt pinned");
        assert_eq!(attempt.state, super::super::store::AttemptState::Confirmed);
        assert_eq!(attempt.award_event_id, award_event);

        // And the job is now the delivery watcher's work — awarded, reservation still held.
        assert_eq!(
            store.awarded_unsettled_job_ids().expect("awarded"),
            vec![job],
            "a published award must put the job in the watcher's work set"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ── §1 AWARD-PRESENCE CHOKEPOINT (never award twice) ────────────────────────────────────────

    // The pure decision table, exhaustive over the FULL product (awards row × attempt state ×
    // reservation state) = 2×4×4 = 32 cells. Every cell is pinned individually because the gate's
    // whole value is which cells may touch the relay, which may send, and which are terminal —
    // a table this central should be pinned cell by cell rather than sampled.
    #[test]
    fn award_step_decides_every_cell_of_the_state_product() {
        use super::super::store::AttemptState::{Confirmed, Pending, Refused};
        use AwardStep::*;
        use ReservationState::{Released, Reserved, Spent};

        let reservations =
            [None, Some(Reserved), Some(Released), Some(Spent)];

        for reservation in reservations {
            // A local awards row settles it, whatever else says — read first because it is the one
            // signal that cannot lie by omission.
            for attempt in [None, Some(Pending), Some(Confirmed), Some(Refused)] {
                assert_eq!(
                    award_step(true, attempt, reservation),
                    AlreadyAwarded,
                    "awards row present must always be AlreadyAwarded ({attempt:?}, {reservation:?})"
                );
            }
            // No row: the pinned attempt outranks the reservation — it is the only signal that
            // knows whether signed bytes may be on the wire.
            assert_eq!(
                award_step(false, Some(Confirmed), reservation),
                RepairFromAttempt,
                "confirmed attempt without its row must repair ({reservation:?})"
            );
            assert_eq!(
                award_step(false, Some(Refused), reservation),
                RefusedTerminal,
                "refused attempt is terminal whatever the ledger says ({reservation:?})"
            );
            assert_eq!(
                award_step(false, Some(Pending), reservation),
                ResumeAttempt,
                "pending attempt must resume THE PINNED BYTES ({reservation:?})"
            );
        }

        // No row, no attempt: the reservation is all there is, and it can only say "ask" or "go".
        // Nothing was ever started → fresh.
        assert_eq!(award_step(false, None, None), Fresh);
        // Spent is adjudicated by `reserve` itself downstream (AlreadySpent) — one authority.
        assert_eq!(award_step(false, None, Some(Spent)), Fresh);
        // Money committed, no award recorded, no pinned verdict: only the relay knows.
        assert_eq!(award_step(false, None, Some(Reserved)), ProbeLegacy);
        // ★ THE #322 CELL. The old table mapped Released → Publish, on the theory that a publish
        // error means nothing landed. A lost OK lands anyway, so Released proves nothing and must
        // ask the relay exactly like Reserved. Red-on-revert: map this cell back to Fresh and
        // the legacy-released repair test below fails by publishing a duplicate.
        assert_eq!(award_step(false, None, Some(Released)), ProbeLegacy);
    }

    // A job this buyer has already awarded must not publish again — and must not RESERVE again
    // either, because the original award still holds its own reservation. Red-on-revert: the publish
    // closure is `unreachable!`, so deleting the precheck panics here.
    #[tokio::test(flavor = "current_thread")]
    async fn a_recorded_award_publishes_nothing_and_reserves_nothing() {
        let (store, path) = fresh_store("award-already-recorded");
        let job = "a".repeat(64);
        store.reserve(&job, 40, 100, 1).expect("reserve");
        store
            .record_award(&job, &"c".repeat(64), &"e".repeat(64), SELLER_HEX, 40, 7)
            .expect("record");

        let outcome = award_with_reservation(
            &store,
            &job,
            40,
            100,            9,
            no_relay,
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect("an already-awarded job is not an error");

        let AwardOutcome::AlreadyAwarded(record) = outcome else {
            panic!("expected AlreadyAwarded for a job with a local awards row");
        };
        // The RECORDED award is handed back, not a fabricated one. `AwardRecord` has no mint list,
        // and the enum is what keeps "we don't store this" from being reported as "there were none".
        assert_eq!(record.job_id, job);
        assert_eq!(record.award_event_id, "e".repeat(64));
        assert_eq!(record.awarded_at_unix, 7, "the ORIGINAL award time, not this call's");
        assert_eq!(
            store.reserved_in_flight().expect("reserved"),
            40,
            "a re-award must not commit a second time"
        );
        let _ = std::fs::remove_file(&path);
    }

    // The duplicate-award hazard itself: funds reserved, no local row, and the relay says an award
    // IS already public. Publishing here would put a SECOND 3405 on the relay for money already
    // committed — so `publish` stays `unreachable!` and the missing row is rebuilt from the award.
    //
    // ★ The reservation (40) and this call's `amount` (99) DISAGREE on purpose. Equal values would
    // make the amount assertion collinear: it would pass whichever source the code read, and prove
    // neither. Only a disagreeing pair can show the repair records what was actually committed.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unrecorded_public_award_is_repaired_rather_than_published_again() {
        let (store, path) = fresh_store("award-public-unrecorded");
        let job = "a".repeat(64);
        let award = "e".repeat(64);
        let claim = "c".repeat(64);
        // Exactly the `record_award`-failed window: reserved, published, no row.
        store.reserve(&job, 40, 100, 1).expect("reserve");

        let relayed = AwardPresence::Repairable(RelayedAward {
            award_event_id: award.clone(),
            claim_id: claim.clone(),
            seller_pubkey: SELLER_HEX.to_owned(),
        });
        let outcome = award_with_reservation(
            &store,
            &job,
            99,
            100,            9,
            || async move { Ok(PresenceRead::Present(relayed)) },
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect("a parseable public award is repaired, not an error");

        let AwardOutcome::AlreadyAwarded(record) = outcome else {
            panic!("expected AlreadyAwarded once the row is repaired");
        };
        // Identity fields come from the award that is actually on the relay.
        assert_eq!(record.job_id, job);
        assert_eq!(record.award_event_id, award);
        assert_eq!(record.claim_id, claim);
        assert_eq!(record.seller_pubkey, SELLER_HEX);
        // The amount does NOT: the 3405 carries no amount tag, so it comes from the reservation —
        // what the original award committed — and never from the 99 this call was asked to spend.
        assert_eq!(
            record.amount_sats, 40,
            "the repair must record the RESERVED amount, not this call's"
        );

        // Returned from the store, not assembled in memory: read it back independently.
        let persisted = store.award_record(&job).expect("read").expect("a repaired row exists");
        assert_eq!(persisted.award_event_id, award);
        assert_eq!(persisted.amount_sats, 40);
        assert_eq!(
            store.reserved_in_flight().expect("reserved"),
            40,
            "a repair must not commit a second time"
        );
        let _ = std::fs::remove_file(&path);
    }

    // RED LEG. The relay has an award, but it cannot be parsed into a complete, unambiguous record.
    // Repair must decline and leave the §1 refusal exactly as it was: a partial or inferred money
    // row is WORSE than a missing one, because the delivery watcher would then settle against it.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unrepairable_public_award_refuses_and_writes_nothing() {
        let (store, path) = fresh_store("award-public-unrepairable");
        let job = "a".repeat(64);
        let award = "e".repeat(64);
        store.reserve(&job, 40, 100, 1).expect("reserve");

        let found = AwardPresence::Unrepairable {
            award_event_id: award.clone(),
            detail: "award has no `p` tag other than this buyer's own".to_owned(),
        };
        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            9,
            || async move { Ok(PresenceRead::Present(found)) },
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect_err("an unrepairable public award must refuse");

        assert!(
            matches!(&error, AwardError::PublishedButUnrecorded { award_event_id, .. } if *award_event_id == award),
            "expected PublishedButUnrecorded naming the found award, got: {error}"
        );
        // The refusal is only useful if it says what to do — and now also WHY repair declined,
        // otherwise an operator cannot tell a self-healing failure from a gate that never tried.
        let message = error.to_string();
        assert!(message.contains(&award), "the refusal must NAME the award it found: {message}");
        assert!(message.contains("collect"), "the refusal must name the operator action: {message}");
        assert!(
            message.contains("other than this buyer's own"),
            "the refusal must carry WHY repair declined: {message}"
        );
        // Nothing moved: the reservation belongs to the award that IS public.
        assert_eq!(
            store.reserved_in_flight().expect("reserved"),
            40,
            "a presence refusal must not disturb the reservation"
        );
        assert!(
            store.award_record(&job).expect("read").is_none(),
            "a refused repair must write NOTHING — a partial row is worse than none"
        );
        let _ = std::fs::remove_file(&path);
    }

    // The uninformative relay answer. An UNVERIFIED read is not "no award exists" — the relay
    // never demonstrated it was answering at all. Both it and an outright error must refuse:
    // publishing on an unverified absence is the duplicate. (A CONFIRMED absence now proceeds —
    // that is the separate `a_released_legacy_reservation_with_confirmed_absence…` test.)
    #[tokio::test(flavor = "current_thread")]
    async fn an_unverifiable_presence_refuses_rather_than_publishing() {
        for (label, probe_result) in [
            ("unverified answer", Ok(PresenceRead::Unverified)),
            ("relay error", Err(JobLifecycleError::Relay("relay down".into()))),
        ] {
            let (store, path) = fresh_store(&format!("award-unverified-{}", label.replace(' ', "-")));
            let job = "a".repeat(64);
            store.reserve(&job, 40, 100, 1).expect("reserve");

            let error = award_with_reservation(
                &store,
                &job,
                40,
                100,                9,
                || async move { probe_result },
                no_prepare,
                no_send,
                None,
            )
            .await
            .expect_err("an unverifiable presence must refuse");

            assert!(
                matches!(error, AwardError::PresenceUnverified { .. }),
                "{label}: expected PresenceUnverified, got: {error}"
            );
            // The refusal must name a recovery that EXISTS. The original wording told the operator
            // to "release the reservation" — there is no such command anywhere in the CLI or MCP
            // surface; recovery actually rides the reconcile pass. Naming an unreachable action is
            // worse than naming none, because it sends the reader looking for it.
            //
            // Asserting on absence as well as presence is deliberate: the previous version of this
            // test checked `contains("release")`, which a message saying "reconcile RELEASES this
            // reservation" still satisfies — the substring survives a change that inverts the
            // meaning, so the test would have passed on wording it was written to reject.
            let message = error.to_string();
            assert!(
                message.contains(&format!("collect {job}")),
                "{label}: the refusal must name the one reachable command, `collect <job>`: {message}"
            );
            assert!(
                message.contains("No operator action is required"),
                "{label}: the refusal must say recovery is automatic, or the reader will hunt for a \
                 command to run: {message}"
            );
            assert!(
                !message.contains("release the reservation"),
                "{label}: this names an operator action that does not exist (#290): {message}"
            );
            assert_eq!(
                store.reserved_in_flight().expect("reserved"),
                40,
                "{label}: an unverified presence must leave the reservation alone"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    // ── #322: the award attempt outbox ──────────────────────────────────────────────────────────

    // ★ THE #322 REGRESSION, send half. A send with no verdict (lost OK / timeout) must change
    // NOTHING: the reservation stays held, the attempt stays pending — and the retry re-sends the
    // IDENTICAL bytes without re-preparing, so no retry can ever name a different claim or mint a
    // different event id. Red-on-revert: release on the unresolved send (the old behaviour) and
    // the reservation assert fails; re-run prepare on the retry and `no_prepare` panics.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unresolved_send_holds_funds_and_a_retry_resends_the_identical_bytes() {
        use std::sync::Mutex;

        let (store, path) = fresh_store("attempt-unresolved-retry");
        let job = "a".repeat(64);
        let sent: Mutex<Vec<String>> = Mutex::new(Vec::new());

        // First call: fresh prepare, send gets no verdict.
        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            1,
            no_relay,
            || {
                let job = job.clone();
                async move { Ok(fake_prepared(&job)) }
            },
            |bytes: String, _event_id: String| {
                sent.lock().unwrap().push(bytes);
                async { SendOutcome::Unresolved { detail: "OK never arrived".to_owned() } }
            },
            None,
        )
        .await
        .expect_err("an unresolved send is not success");
        assert!(
            matches!(&error, AwardError::Unresolved { .. }),
            "expected Unresolved, got: {error}"
        );
        // The refusal must teach the retry: same bytes, safe to repeat.
        let message = error.to_string();
        assert!(message.contains("SAME signed event"), "must say the retry is same-bytes: {message}");

        // NOTHING moved: funds held, attempt pending, no award recorded.
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::super::reservations::ReservationState::Reserved),
            "an unresolved send must HOLD the reservation — releasing here is #322"
        );
        let attempt = store.award_attempt(&job).expect("read").expect("attempt pinned");
        assert_eq!(attempt.state, super::super::store::AttemptState::Pending);
        assert!(store.award_record(&job).expect("read").is_none(), "no verdict, no row");

        // Second call: the retry. Prepare must NOT run (no_prepare panics); the pinned bytes go
        // out again; this time the relay acks.
        let outcome = award_with_reservation(
            &store,
            &job,
            40,
            100,            2,
            no_relay,
            no_prepare,
            |bytes: String, _event_id: String| {
                sent.lock().unwrap().push(bytes);
                async { SendOutcome::Acked }
            },
            None,
        )
        .await
        .expect("the retry converges");

        let sent = sent.into_inner().unwrap();
        assert_eq!(sent.len(), 2, "one transmission per call");
        assert_eq!(sent[0], sent[1], "the retry must transmit BYTE-IDENTICAL event JSON");

        let AwardOutcome::Published(published) = outcome else {
            panic!("the acked retry reports the publish");
        };
        assert_eq!(published.award_event_id, "e".repeat(64));
        assert_eq!(published.claim_id, "c".repeat(64), "the pinned claim, never a re-selection");
        let recorded = store.award_record(&job).expect("read").expect("recorded on ack");
        assert_eq!(recorded.award_event_id, published.award_event_id);
        assert_eq!(
            store.award_attempt(&job).expect("read").expect("attempt").state,
            super::super::store::AttemptState::Confirmed
        );
        assert_eq!(store.reserved_in_flight().expect("r"), 40, "still exactly one commitment");
        let _ = std::fs::remove_file(&path);
    }

    // An explicit relay refusal (OK:false) is the one send verdict that releases — nothing is
    // public — and it is TERMINAL: the job can never publish again, however many times the caller
    // retries, because awards are write-once per offer. Red-on-revert: let the refused state fall
    // through to Fresh and `no_prepare` panics on the second call.
    #[tokio::test(flavor = "current_thread")]
    async fn a_relay_refusal_is_terminal_and_releases_the_funds() {
        let (store, path) = fresh_store("attempt-refused-terminal");
        let job = "a".repeat(64);

        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            1,
            no_relay,
            || {
                let job = job.clone();
                async move { Ok(fake_prepared(&job)) }
            },
            |_bytes: String, _event_id: String| async {
                SendOutcome::Refused { detail: "blocked: policy".to_owned() }
            },
            None,
        )
        .await
        .expect_err("a refused event is an error");
        assert!(
            matches!(&error, AwardError::Refused { detail, .. } if detail == "blocked: policy"),
            "expected Refused carrying the relay's words, got: {error}"
        );
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::super::reservations::ReservationState::Released),
            "an EXPLICIT refusal is the verdict that safely releases"
        );
        assert_eq!(
            store.award_attempt(&job).expect("read").expect("attempt").state,
            super::super::store::AttemptState::Refused
        );

        // Any later call — probe, prepare, send all unreachable — reports the refusal again.
        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            2,
            no_relay,
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect_err("a refused job stays refused");
        let message = error.to_string();
        assert!(
            matches!(&error, AwardError::Refused { .. }),
            "terminal refusal must be reported as Refused, got: {error}"
        );
        assert!(
            message.contains("new offer"),
            "the refusal must name the real recovery (a NEW offer): {message}"
        );
        assert!(store.award_record(&job).expect("read").is_none(), "nothing ever recorded");
        let _ = std::fs::remove_file(&path);
    }

    // ★ THE #322 REGRESSION, ledger half — the exact field chain. The OLD binary published an
    // award, took a publish error for it (lost OK), concluded "nothing landed", and RELEASED —
    // leaving reservation=Released, no awards row, award public. Its next award call then sailed
    // through the precheck (Released mapped to Publish) and PUBLISHED A SECOND AWARD to a freshly
    // selected claim: two seats executing one offer. The fixed table sends Released to the probe;
    // when the probe finds the award, the row is repaired, the funds are RE-HELD, and nothing is
    // prepared or sent.
    #[tokio::test(flavor = "current_thread")]
    async fn a_released_legacy_reservation_probes_and_repairs_rather_than_republishing() {
        let (store, path) = fresh_store("legacy-released-repair");
        let job = "a".repeat(64);
        let award = "e".repeat(64);
        let claim = "c".repeat(64);

        // The #322 ledger state, verbatim: reserve → (publish, lost OK) → release.
        store.reserve(&job, 40, 100, 1).expect("reserve");
        store.release(&job, 2).expect("the old binary released on the publish error");
        assert!(store.award_attempt(&job).expect("read").is_none(), "pre-attempt-era job");

        let relayed = AwardPresence::Repairable(RelayedAward {
            award_event_id: award.clone(),
            claim_id: claim.clone(),
            seller_pubkey: SELLER_HEX.to_owned(),
        });
        let outcome = award_with_reservation(
            &store,
            &job,
            40,
            100,            3,
            || async move { Ok(PresenceRead::Present(relayed)) },
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect("the public award is repaired, not duplicated");

        let AwardOutcome::AlreadyAwarded(record) = outcome else {
            panic!("expected AlreadyAwarded from the repair");
        };
        assert_eq!(record.award_event_id, award);
        assert_eq!(record.claim_id, claim, "the claim the RELAY names, never a re-selection");
        // The funds are committed again: a public award must not sit unfunded.
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::super::reservations::ReservationState::Reserved),
            "repairing a Released legacy row must re-hold its funds"
        );
        assert_eq!(store.reserved_in_flight().expect("r"), 40);
        let _ = std::fs::remove_file(&path);
    }

    // A legacy row whose relay CONFIRMS no award still never re-selects: "absent now" is not
    // "never arrives", the seller executes per-award, and the store's own contract says a fresh
    // selection is not idempotent against the legacy event materializing. Both legacy states
    // terminalize with the funds returned — the recovery, as for every refusal, is a NEW offer.
    // Red-on-revert: fall through to Fresh here (the first version of this fix did) and the
    // prepare closure panics.
    #[tokio::test(flavor = "current_thread")]
    async fn a_legacy_reservation_with_confirmed_absence_refuses_and_releases() {
        for seed_released in [true, false] {
            let (store, path) = fresh_store(&format!("legacy-absent-{seed_released}"));
            let job = "a".repeat(64);
            store.reserve(&job, 40, 100, 1).expect("reserve");
            if seed_released {
                store.release(&job, 2).expect("release");
            }

            let error = award_with_reservation(
                &store,
                &job,
                40,
                100,                3,
                || async { Ok(PresenceRead::ConfirmedAbsent) },
                no_prepare,
                no_send,
                None,
            )
            .await
            .expect_err("a confirmed absence on a legacy row is terminal");

            assert!(
                matches!(&error, AwardError::Refused { .. }),
                "seed_released={seed_released}: expected Refused, got: {error}"
            );
            let message = error.to_string();
            assert!(
                message.contains("new offer"),
                "the refusal names the real recovery: {message}"
            );
            assert_eq!(
                store.reservation(&job).expect("read").map(|(state, _)| state),
                Some(super::super::reservations::ReservationState::Released),
                "seed_released={seed_released}: the funds come back"
            );
            assert!(
                store.award_attempt(&job).expect("read").is_none(),
                "no attempt is fabricated for a job that never pinned one"
            );
            assert!(store.award_record(&job).expect("read").is_none(), "and no award row");
            let _ = std::fs::remove_file(&path);
        }
    }

    // The crash between `mark_attempt_refused` and `release` leaves refused+Reserved — the
    // RefusedTerminal arm's documented recovery. It must finish the release, not just repeat the
    // refusal. Red-on-revert: drop the release from the arm and the funds stay committed forever
    // to a job that can never publish.
    #[tokio::test(flavor = "current_thread")]
    async fn a_crashed_refusal_releases_its_reservation_on_the_next_call() {
        let (store, path) = fresh_store("refused-reserved-recovery");
        let job = "a".repeat(64);
        // Build the crash state directly: pinned, refused, reservation still held.
        store.reserve(&job, 40, 100, 1).expect("reserve");
        let prepared = fake_prepared(&job);
        store
            .begin_award_attempt(
                &AwardAttempt {
                    job_id: job.clone(),
                    claim_id: prepared.claim_id.clone(),
                    seller_pubkey: prepared.seller_pubkey.clone(),
                    award_event_id: prepared.award_event_id.clone(),
                    event_json: prepared.event_json.clone(),
                    amount_sats: 40,
                    quoted_mints_json: "[]".to_owned(),
                    offer_deadline_unix: prepared.offer_deadline_unix,
                    send_count: 1,
                    relay_url: prepared.relay_url.clone(),
                    state: AttemptState::Pending,
                    detail: None,
                },
                1,
            )
            .expect("pin");
        store.mark_attempt_refused(&job, "blocked: policy", 2).expect("refuse");
        assert_eq!(store.reserved_in_flight().expect("r"), 40, "the crash: still reserved");

        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            3,
            no_relay,
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect_err("refused is terminal");
        assert!(matches!(&error, AwardError::Refused { .. }), "got: {error}");
        assert_eq!(
            store.reserved_in_flight().expect("r"),
            0,
            "the recovery arm must finish the crashed release"
        );
        let _ = std::fs::remove_file(&path);
    }

    // A pending attempt on an already-PAID job (manual collect settled it while the verdict was
    // open) must still resolve: the reserve refuses AlreadySpent, the arm tolerates it exactly
    // like its two siblings, and the acked resolution finally writes the awards row the history
    // and the #261 attribution heal need. Red-on-revert: narrow the arm's tolerance to Ok(_) and
    // this errors out before the send.
    #[tokio::test(flavor = "current_thread")]
    async fn a_pending_attempt_on_a_spent_job_still_resolves_and_lands_its_row() {
        let (store, path) = fresh_store("attempt-spent-resume");
        let job = "a".repeat(64);
        store.reserve(&job, 40, 100, 1).expect("reserve");
        let prepared = fake_prepared(&job);
        store
            .begin_award_attempt(
                &AwardAttempt {
                    job_id: job.clone(),
                    claim_id: prepared.claim_id.clone(),
                    seller_pubkey: prepared.seller_pubkey.clone(),
                    award_event_id: prepared.award_event_id.clone(),
                    event_json: prepared.event_json.clone(),
                    amount_sats: 40,
                    quoted_mints_json: "[]".to_owned(),
                    offer_deadline_unix: prepared.offer_deadline_unix,
                    send_count: 1,
                    relay_url: prepared.relay_url.clone(),
                    state: AttemptState::Pending,
                    detail: None,
                },
                1,
            )
            .expect("pin");
        // The manual collect: reserved → spent with no awards row.
        store.convert_to_spent(&job, 40, 2).expect("spent");
        assert!(store.award_record(&job).expect("read").is_none());

        let outcome = award_with_reservation(
            &store,
            &job,
            40,
            100,            3,
            no_relay,
            no_prepare,
            send_acked,
            None,
        )
        .await
        .expect("a spent job's attempt resolves as bookkeeping");
        assert!(matches!(outcome, AwardOutcome::Published(_)));
        assert!(store.award_record(&job).expect("read").is_some(), "the row lands at last");
        assert_eq!(
            store.award_attempt(&job).expect("read").expect("attempt").state,
            AttemptState::Confirmed
        );
        let _ = std::fs::remove_file(&path);
    }

    // The pending-forever fixed point: awards row present (record succeeded) while the attempt's
    // confirm write failed. Every later call short-circuits at AlreadyAwarded — which is exactly
    // why THAT arm must land the confirm. Red-on-revert: drop the mark from the arm and the
    // attempt stays pending for the life of the store.
    #[tokio::test(flavor = "current_thread")]
    async fn already_awarded_confirms_a_stranded_pending_attempt() {
        let (store, path) = fresh_store("already-awarded-confirms");
        let job = "a".repeat(64);
        store.reserve(&job, 40, 100, 1).expect("reserve");
        let prepared = fake_prepared(&job);
        store
            .begin_award_attempt(
                &AwardAttempt {
                    job_id: job.clone(),
                    claim_id: prepared.claim_id.clone(),
                    seller_pubkey: prepared.seller_pubkey.clone(),
                    award_event_id: prepared.award_event_id.clone(),
                    event_json: prepared.event_json.clone(),
                    amount_sats: 40,
                    quoted_mints_json: "[]".to_owned(),
                    offer_deadline_unix: prepared.offer_deadline_unix,
                    send_count: 1,
                    relay_url: prepared.relay_url.clone(),
                    state: AttemptState::Pending,
                    detail: None,
                },
                1,
            )
            .expect("pin");
        // The crash: record_award landed, mark_attempt_confirmed did not.
        store
            .record_award(&job, &prepared.claim_id, &prepared.award_event_id, SELLER_HEX, 40, 2)
            .expect("record");

        let outcome = award_with_reservation(
            &store,
            &job,
            40,
            100,            3,
            no_relay,
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect("already awarded");
        assert!(matches!(outcome, AwardOutcome::AlreadyAwarded(_)));
        assert_eq!(
            store.award_attempt(&job).expect("read").expect("attempt").state,
            AttemptState::Confirmed,
            "the awards row is the proof; the arm must land the confirm it implies"
        );
        assert!(
            store.pending_award_attempts().expect("pending").is_empty(),
            "the sweep set drains — no per-boot re-processing forever"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ★ #322 round 4: a caller that already took the transmission license (the sweep counts, then
    // transmits outside the money lock, then replays the verdict) must hand its PRIOR count in —
    // re-counting inside drive_send would push a genuinely-first transmission to prior==1, and a
    // deliberate relay refusal of it would then hold the funds for the whole pay window instead
    // of releasing at once. Red-on-revert: ignore `licensed_prior_sends` and take a fresh count,
    // and the release assertion below fails.
    #[tokio::test(flavor = "current_thread")]
    async fn a_carried_license_keeps_the_first_transmission_refusal_immediate() {
        let (store, path) = fresh_store("carried-license");
        let job = "a".repeat(64);
        store.reserve(&job, 40, 100, 1).expect("reserve");
        let prepared = fake_prepared(&job);
        store
            .begin_award_attempt(
                &AwardAttempt {
                    job_id: job.clone(),
                    claim_id: prepared.claim_id.clone(),
                    seller_pubkey: prepared.seller_pubkey.clone(),
                    award_event_id: prepared.award_event_id.clone(),
                    event_json: prepared.event_json.clone(),
                    amount_sats: 40,
                    quoted_mints_json: "[]".to_owned(),
                    offer_deadline_unix: prepared.offer_deadline_unix,
                    send_count: 0,
                    relay_url: prepared.relay_url.clone(),
                    state: AttemptState::Pending,
                    detail: None,
                },
                1,
            )
            .expect("pin");
        // The caller's license: this IS the first transmission (prior == 0).
        let prior = store.record_attempt_send(&job, 2).expect("license");
        assert_eq!(prior, 0, "the sweep observes a never-transmitted attempt");

        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            3,
            no_relay,
            no_prepare,
            |_bytes: String, _event_id: String| async {
                SendOutcome::Refused { detail: "blocked: policy".to_owned() }
            },
            Some(prior),
        )
        .await
        .expect_err("a deliberate refusal of the FIRST transmission is terminal");
        assert!(
            matches!(&error, AwardError::Refused { .. }),
            "the carried prior==0 must keep the terminal license, got: {error}"
        );
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::super::reservations::ReservationState::Released),
            "and the funds come back at once, not after the 7-day pay window"
        );
        assert_eq!(
            store.award_attempt(&job).expect("read").expect("row").send_count,
            1,
            "the license was counted ONCE, not twice"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ★ #322 round 5: a carried license is license-order truth, not WIRE-order truth. The sweep
    // licenses (prior=0), drops the money lock, and transmits; meanwhile an RPC retry licenses
    // AND transmits its own copy under the lock, and that copy may have LANDED with its OK lost.
    // If the sweep's copy is then deliberately refused (policy flip mid-window), a naive
    // prior==0 would terminalize and release funds for an award that is public. The freshly-read
    // send_count states the truth: it exceeds licensed_prior+1, so the verdict must HOLD.
    // Red-on-revert: in drive_send, take the carried `prior` verbatim instead of reconciling it
    // against the fresh row (`prior.max(send_count - 1)`), and this releases.
    #[tokio::test(flavor = "current_thread")]
    async fn a_stale_carried_license_cannot_terminalize_after_a_concurrent_transmission() {
        let (store, path) = fresh_store("stale-carried-license");
        let job = "a".repeat(64);
        store.reserve(&job, 40, 100, 1).expect("reserve");
        let prepared = fake_prepared(&job);
        store
            .begin_award_attempt(
                &AwardAttempt {
                    job_id: job.clone(),
                    claim_id: prepared.claim_id.clone(),
                    seller_pubkey: prepared.seller_pubkey.clone(),
                    award_event_id: prepared.award_event_id.clone(),
                    event_json: prepared.event_json.clone(),
                    amount_sats: 40,
                    quoted_mints_json: "[]".to_owned(),
                    offer_deadline_unix: prepared.offer_deadline_unix,
                    send_count: 0,
                    relay_url: prepared.relay_url.clone(),
                    state: AttemptState::Pending,
                    detail: None,
                },
                1,
            )
            .expect("pin");
        // The sweep's license: this looked like the first transmission.
        let prior = store.record_attempt_send(&job, 2).expect("sweep license");
        assert_eq!(prior, 0);
        // ...and while the sweep's copy was in flight OUTSIDE the lock, another path licensed and
        // transmitted its own copy (whose OK was lost, so nothing was recorded).
        let _ = store.record_attempt_send(&job, 3).expect("concurrent license");

        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            4,
            no_relay,
            no_prepare,
            |_bytes: String, _event_id: String| async {
                SendOutcome::Refused { detail: "restricted: members only".to_owned() }
            },
            Some(prior),
        )
        .await
        .expect_err("the refusal is reported, not applied");
        assert!(
            matches!(&error, AwardError::Unresolved { .. }),
            "a STALE carried license must fold to Unresolved, not terminalize: {error}"
        );
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::super::reservations::ReservationState::Reserved),
            "the funds stay held — the concurrent copy may be public"
        );
        assert_eq!(
            store.award_attempt(&job).expect("read").expect("row").state,
            AttemptState::Pending,
            "and the attempt stays resolvable"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ★ A refusal on a RE-send releases nothing: the pending state exists precisely because an
    // earlier transmission's verdict was lost, and that send may have landed — OK:false for
    // transmission N is not evidence about transmission 1. Only a FIRST transmission's refusal
    // (send_count 0 at entry) terminalizes. Red-on-revert: drop the prior_sends gate in
    // drive_send's Refused arm and the funds release + the attempt terminalizes here.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_on_a_resend_holds_everything() {
        let (store, path) = fresh_store("resend-refusal-holds");
        let job = "a".repeat(64);

        // First call: fresh pin, send gets no verdict (transmission 1 counted).
        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            1,
            no_relay,
            || {
                let job = job.clone();
                async move { Ok(fake_prepared(&job)) }
            },
            |_bytes: String, _event_id: String| async {
                SendOutcome::Unresolved { detail: "OK never arrived".to_owned() }
            },
            None,
        )
        .await
        .expect_err("unresolved");
        assert!(matches!(error, AwardError::Unresolved { .. }));

        // Retry: the relay now REFUSES the re-send (policy drift). That judges transmission 2
        // only — everything holds.
        let error = award_with_reservation(
            &store,
            &job,
            40,
            100,            2,
            no_relay,
            no_prepare,
            |_bytes: String, _event_id: String| async {
                SendOutcome::Refused { detail: "restricted: members only".to_owned() }
            },
            None,
        )
        .await
        .expect_err("held, not refused");
        assert!(
            matches!(&error, AwardError::Unresolved { .. }),
            "a re-send refusal must HOLD, got: {error}"
        );
        let attempt = store.award_attempt(&job).expect("read").expect("attempt");
        assert_eq!(attempt.state, AttemptState::Pending, "not terminalized");
        assert_eq!(attempt.send_count, 2, "both transmissions counted");
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::super::reservations::ReservationState::Reserved),
            "funds stay held — releasing on a re-send refusal is the #322 shape again"
        );
        let _ = std::fs::remove_file(&path);
    }

    // The crash window between the relay's ack and `record_award`: attempt confirmed, row missing.
    // The next call repairs from the ATTEMPT — every field, amount included, no network at all
    // (probe, prepare and send are all unreachable).
    #[tokio::test(flavor = "current_thread")]
    async fn a_confirmed_attempt_missing_its_row_repairs_without_any_network() {
        let (store, path) = fresh_store("attempt-confirmed-repair");
        let job = "a".repeat(64);

        // Build the crash state: pinned + confirmed, reservation held, no awards row.
        store.reserve(&job, 40, 100, 1).expect("reserve");
        let prepared = fake_prepared(&job);
        let candidate = AwardAttempt {
            job_id: job.clone(),
            claim_id: prepared.claim_id.clone(),
            seller_pubkey: prepared.seller_pubkey.clone(),
            award_event_id: prepared.award_event_id.clone(),
            event_json: prepared.event_json.clone(),
            amount_sats: 40,
            quoted_mints_json: "[]".to_owned(),
            offer_deadline_unix: prepared.offer_deadline_unix,
            send_count: 0,
            relay_url: prepared.relay_url.clone(),
            state: AttemptState::Pending,
            detail: None,
        };
        assert!(matches!(
            store.begin_award_attempt(&candidate, 1).expect("pin"),
            BeginAttempt::Pinned
        ));
        store.mark_attempt_confirmed(&job, 2).expect("confirm");
        assert!(store.award_record(&job).expect("read").is_none(), "the crash: no row");

        let outcome = award_with_reservation(
            &store,
            &job,
            40,
            100,            3,
            no_relay,
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect("repair from the attempt");

        let AwardOutcome::AlreadyAwarded(record) = outcome else {
            panic!("expected AlreadyAwarded from the attempt repair");
        };
        assert_eq!(record.award_event_id, prepared.award_event_id);
        assert_eq!(record.amount_sats, 40, "the attempt carries the committed amount");
        assert!(store.award_record(&job).expect("read").is_some(), "the row exists now");
        let _ = std::fs::remove_file(&path);
    }

    // ORDERING tooth (red-on-revert). `settle_after_pay` must run `pay` (budget append + melt)
    // BEFORE the reserved→spent flip. A pay that FAILS must leave the reservation intact so
    // `available` is never over-stated. Red-on-revert: move `convert_to_spent` before `pay()` in
    // `settle_after_pay` and the failed pay would already have flipped the row to `spent`, dropping
    // it from `reserved` — this test's "still reserved / available unchanged" asserts then fail.
    #[tokio::test(flavor = "current_thread")]
    async fn settle_flips_only_after_pay_succeeds() {
        let (store, path) = fresh_store("settle-ordering");
        let job = "a".repeat(64);
        store.reserve(&job, 40, 100, 1).expect("reserve");
        assert_eq!(store.available(100).expect("avail"), 60);

        // A pay that fails must NOT flip the reservation.
        let result: Result<(u64, Converted), SettleError<&str>> =
            settle_after_pay(&store, &job, 2, || async { Err("melt failed") }, |amount| *amount).await;
        assert!(matches!(result, Err(SettleError::Pay("melt failed"))));
        assert_eq!(
            store.reservation(&job).expect("read"),
            Some((super::super::reservations::ReservationState::Reserved, 40)),
            "a failed pay must leave the reservation reserved (funds still committed)"
        );
        assert_eq!(store.reserved_in_flight().expect("r"), 40, "reserved unchanged after failed pay");
        assert_eq!(store.available(100).expect("avail"), 60, "available NOT over-stated");

        // A pay that succeeds flips reserved → spent exactly once.
        let (_, converted) =
            settle_after_pay(&store, &job, 3, || async { Ok::<u64, &str>(40) }, |amount| *amount)
                .await
                .expect("settle");
        assert_eq!(converted, Converted::FromReserved);
        assert_eq!(store.reserved_in_flight().expect("r"), 0, "spent leaves the reserved term");
        let _ = std::fs::remove_file(&path);
    }

    // CRASH-RECOVERY tooth (the #123→#126 obligation). Simulate a crash BETWEEN the budget append +
    // melt and the reserved→spent flip: the budget spend is durable and the wallet has melted, but
    // the reservation is still `reserved`. Throughout that window `available` is only ever
    // UNDER-stated (the amount is counted twice in the wallet ceiling — the melt already dropped the
    // balance AND reserved still holds it) — never over-stated. On restart, reconcile with a `Paid`
    // disposition converges the dangling reservation to `spent`, and `available` returns to the
    // correct post-settle value. Uses the REAL durable store + the REAL durable BudgetGate
    // (spent.jsonl) for the audit spend, not a model. Issue #378 removed the budget ceiling, so the
    // wallet ceiling is the sole ceiling under test here.
    #[test]
    fn crash_between_pay_and_flip_never_overstates_available_and_reconcile_converges() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-buyer-lifecycle-crash-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        home.config.per_job_budget_sats = 100;
        let db = root.join("buyer.sqlite");
        let job = "a".repeat(64);

        let starting_balance = 100u64;
        let amount = 40u64;

        // True post-settle available if the flip HAD happened: the wallet ceiling balance-amount
        // (issue #378 removed the budget ceiling, so the wallet is the sole ceiling).
        let true_available_after = starting_balance - amount;

        {
            let store = BuyerStore::open(&db).expect("open");
            store
                .reserve(&job, amount, starting_balance, 1)
                .expect("reserve");

            // PAY: budget append (durable) + melt. The melt drops the live wallet balance by
            // `amount`; we model that post-melt balance below. Crucially we DO NOT flip here.
            let mut gate = BudgetGate::from_home(&home).expect("gate");
            gate.authorize_and_commit(amount).expect("budget append");
            assert_eq!(gate.spent(), amount, "budget spend is durable pre-flip");
            let melted_balance = starting_balance - amount;

            // WINDOW (crash before the flip): the reservation is still `reserved`, budget spent is
            // `amount` (durable for audit), the wallet has melted. available must be conservative —
            // the wallet ceiling counts the amount twice (balance already dropped AND reserved still
            // holds it) — hence UNDER-stated, never over.
            let windowed = store.available(melted_balance).expect("windowed avail");
            assert!(
                windowed <= true_available_after,
                "available in the crash window ({windowed}) must never exceed the true \
                 post-settle available ({true_available_after}) — no over-commit window"
            );
        } // "crash": drop the store + gate; only the durable DB + spent.jsonl survive.

        // RESTART: the reservation is still on disk, the budget spend folded back from spent.jsonl.
        let store = BuyerStore::open(&db).expect("restart open");
        assert_eq!(store.reserved_in_flight().expect("r"), amount, "reservation survived the crash");
        let reloaded = BudgetGate::from_home(&home).expect("reload gate");
        assert_eq!(reloaded.spent(), amount, "budget spend survived the crash");
        let melted_balance = starting_balance - amount;

        // Reconcile: the payment journal shows this attempt Closed ⇒ Paid ⇒ convert the dangling
        // reservation. classify + the store reconcile are the live path (relay/journal I/O aside).
        assert_eq!(classify_disposition(PaymentProgress::Closed, false), JobDisposition::Paid);
        let mut dispositions = super::super::reservations::Dispositions::new();
        dispositions.insert(job.clone(), JobDisposition::Paid);
        let report = store.reconcile(&dispositions, 10).expect("reconcile");
        assert_eq!(report.converted, vec![job.clone()]);
        assert_eq!(store.reserved_in_flight().expect("r"), 0, "dangling reservation converged to spent");

        // CONVERGED: available now equals the true post-settle value (reserved cleared, wallet
        // already melted; the budget ledger still records the spend for audit but no longer bounds
        // available). Neither over- nor under-stated.
        assert_eq!(
            store.available(melted_balance).expect("avail"),
            true_available_after,
            "post-reconcile available is exactly the true settled value"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // PAYMENT_UNCERTAIN tooth: an ambiguous payment (Sent-but-not-Closed) is classified `Payable`
    // — KEPT — even when the claim looks dead on the relay. reconcile must leave the reservation
    // intact (the funds may have moved; only the phase-3 saga may resolve it), never release it.
    #[test]
    fn payment_uncertain_is_kept_not_released() {
        // Pure classification: uncertain payment never becomes Dead, regardless of liveness.
        assert_eq!(classify_disposition(PaymentProgress::Uncertain, false), JobDisposition::Payable);
        assert_eq!(classify_disposition(PaymentProgress::Uncertain, true), JobDisposition::Payable);

        // And the store honours a `Payable` verdict by KEEPING the reserved row.
        let (store, path) = fresh_store("uncertain-kept");
        let job = "a".repeat(64);
        store.reserve(&job, 30, 100, 1).expect("reserve");
        let mut dispositions = super::super::reservations::Dispositions::new();
        dispositions.insert(job.clone(), classify_disposition(PaymentProgress::Uncertain, false));
        let report = store.reconcile(&dispositions, 2).expect("reconcile");
        assert_eq!(report.kept, vec![job.clone()], "uncertain payment's reservation is kept");
        assert!(report.released.is_empty(), "PAYMENT_UNCERTAIN must NOT release");
        assert_eq!(store.reserved_in_flight().expect("r"), 30, "funds stay committed");
        let _ = std::fs::remove_file(&path);
    }

    // DEAD-JOB release through the reconcile path: a reserved job with no payment that is no longer
    // payable on the relay is classified `Dead`, and reconcile releases it — funds reclaimed.
    #[test]
    fn dead_job_releases_through_reconcile() {
        assert_eq!(classify_disposition(PaymentProgress::None, false), JobDisposition::Dead);
        assert_eq!(classify_disposition(PaymentProgress::None, true), JobDisposition::Payable);

        let (store, path) = fresh_store("dead-release");
        let job = "a".repeat(64);
        store.reserve(&job, 100, 100, 1).expect("reserve");
        assert_eq!(store.available(100).expect("avail"), 0, "all funds committed");

        let mut dispositions = super::super::reservations::Dispositions::new();
        dispositions.insert(job.clone(), classify_disposition(PaymentProgress::None, false));
        let report = store.reconcile(&dispositions, 2).expect("reconcile");
        assert_eq!(report.released, vec![job.clone()]);
        assert_eq!(store.available(100).expect("avail"), 100, "dead job's funds reclaimed");
        let _ = std::fs::remove_file(&path);
    }

    // IDEMPOTENT RE-ARM tooth (invariant A): never award twice. An award already on the relay ⇒
    // Skip regardless of local state; an already-`Spent` reservation ⇒ Skip. No relay award ⇒
    // Attempt — including the `Reserved`-but-not-published crash window (republish) and the
    // Released/None cases.
    #[test]
    fn plan_rearm_skips_only_when_already_awarded() {
        // Relay award present ⇒ Skip regardless of local reservation state.
        assert_eq!(plan_rearm(true, None), RearmAction::Skip);
        assert_eq!(plan_rearm(true, Some(ReservationState::Reserved)), RearmAction::Skip);
        assert_eq!(plan_rearm(true, Some(ReservationState::Spent)), RearmAction::Skip);
        // Already paid ⇒ Skip.
        assert_eq!(plan_rearm(false, Some(ReservationState::Spent)), RearmAction::Skip);
        // Crash window: reserved but no relay award ⇒ Attempt (republish, reserve is idempotent).
        assert_eq!(plan_rearm(false, Some(ReservationState::Reserved)), RearmAction::Attempt);
        // Released or never reserved, no relay award ⇒ Attempt.
        assert_eq!(plan_rearm(false, Some(ReservationState::Released)), RearmAction::Attempt);
        assert_eq!(plan_rearm(false, None), RearmAction::Attempt);
    }

    // ── CONCURRENCY test helpers ────────────────────────────────────────────────────────────────

    /// A home with the given per-job cap, on a scratch dir. `per_job` is set equal to `cap` so the
    /// per-job ceiling never masks the wallet ceiling these tests exercise (issue #378 removed the
    /// rolling total ceiling; the wallet balance is the sole shared limit).
    fn conc_home(label: &str, cap: u64) -> (crate::home::MaxplayerHome, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-buyer-lifecycle-conc-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        home.config.per_job_budget_sats = cap;
        (home, root)
    }

    /// The relay-presence probe for tests whose outcome must be decided from LOCAL state alone.
    ///
    /// It panics if awaited, which makes "the local-first decision needs no network" an assertion
    /// rather than a comment: a first award (no row, no reservation), a re-award of a recorded
    /// award (row present), and every attempt-era state (the pinned verdict outranks the relay)
    /// must all resolve without it. Only the legacy reserved/released-but-unrecorded states may
    /// reach the relay, and the tests that exercise those pass an explicit probe instead of this.
    async fn no_relay() -> Result<PresenceRead<AwardPresence>, JobLifecycleError> {
        unreachable!("presence must be decided from local state — the relay must not be consulted")
    }

    /// A stand-in prepared (signed) award for the `award_with_reservation` prepare closure (these
    /// tests never touch a relay — the money accounting is what is under test, not the wire).
    /// `event_json` is distinct per award id so a byte-identity assertion means something.
    fn fake_prepared(job_id: &str) -> PreparedAward {
        PreparedAward {
            award_event_id: "e".repeat(64),
            event_json: format!("{{\"id\":\"{}\",\"job\":\"{job_id}\"}}", "e".repeat(64)),
            job_id: job_id.to_owned(),
            claim_id: "c".repeat(64),
            seller_pubkey: SELLER_HEX.to_owned(),
            quoted_mints: Vec::new(),
            offer_deadline_unix: 9_999,
            relay_url: "ws://relay.test".to_owned(),
        }
    }

    /// A send that acks — the happy wire for tests about the money accounting around it.
    async fn send_acked(_bytes: String, _event_id: String) -> SendOutcome {
        SendOutcome::Acked
    }

    /// A send that must never run: pinned/refused/repaired paths transmit nothing.
    async fn no_send(_bytes: String, _event_id: String) -> SendOutcome {
        unreachable!("nothing may be sent on this path")
    }

    /// A prepare that must never run: any path with an existing attempt or award never re-selects.
    async fn no_prepare() -> Result<PreparedAward, JobLifecycleError> {
        unreachable!("nothing may be prepared (signed) on this path")
    }

    // ★ N-AGENT NO-OVERSPEND TOOTH. The buyer daemon serves N MCP agents that all draw the SAME
    // wallet (gudnuf's product decision: one wallet, N equal agents, no per-agent caps). This is the
    // assembled-money invariant at that scale: no matter how the awards interleave, the funds
    // committed across every agent can never exceed the live wallet balance (issue #378 removed the
    // rolling budget ceiling, so the wallet is the sole shared limit).
    //
    // It composes the SAME seam the daemon's `award` RPC uses — a balance snapshot then
    // `award_with_reservation` — serialized behind the SAME kind of async money lock the daemon
    // holds (`BuyerContext::money_lock`), over the REAL durable store. Five agents each try to
    // reserve 30 against a shared wallet of 100: at most three fit (90), the other two must get a
    // clean insufficient-available refusal, and the total reserved must land at exactly 90.
    //
    // The reserved-accumulation race itself is closed one layer down by the store's `BEGIN
    // IMMEDIATE` (store `tooth2`); this proves the assembled award path — snapshot + reserve seam —
    // enforces the wallet ceiling under many concurrent agents, and that idempotent re-awards never
    // inflate the committed total.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn n_equal_agents_cannot_overspend_the_shared_budget() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let cap = 100u64; // per-job cap (via conc_home); slack here so the wallet ceiling binds
        let balance = 100u64; // modeled wallet ecash; no settle in this test, so it never moves
        let amount = 30u64; // wallet balance 100 ⇒ exactly 3 fit (90), 2 must be refused
        let agents = 5usize;

        let (_home, root) = conc_home("n-agents", cap);
        let store = Arc::new(BuyerStore::open(root.join("buyer.sqlite")).expect("store"));
        let money_lock = Arc::new(Mutex::new(()));

        let mut set = tokio::task::JoinSet::new();
        for agent in 0..agents {
            let (store, money_lock) = (store.clone(), money_lock.clone());
            // A distinct 64-hex job id per agent.
            let job = format!("{agent:064x}");
            set.spawn(async move {
                // The daemon's money lock, held across the snapshot AND the reserve — exactly as
                // `award` composes it, so no agent's snapshot races another's commit.
                let _guard = money_lock.lock().await;
                let job_out = job.clone();
                award_with_reservation(
                    &store,
                    &job,
                    amount,
                    balance,
                    1,
                    no_relay,
                    || async move { Ok(fake_prepared(&job_out)) },
                    send_acked,
                    None,
                )
                .await
                .map_err(|error| {
                    // A refused agent must be a clean insufficient-available refusal, never a panic
                    // or a partial write.
                    assert!(
                        matches!(
                            error,
                            AwardError::Reserve(ReserveRefused::InsufficientAvailable { .. })
                        ),
                        "unexpected award failure: {error}"
                    );
                })
                .ok()
                // Report WHICH agent won, not merely that one did: which three of the five win is
                // decided by the order they acquire the money lock, and nothing pins that order.
                .map(|_| job)
            });
        }

        let mut winners: Vec<String> = Vec::new();
        let mut refusals = 0usize;
        while let Some(result) = set.join_next().await {
            match result.expect("join") {
                Some(job) => winners.push(job),
                None => refusals += 1,
            }
        }

        assert_eq!(winners.len(), 3, "exactly three 30-sat awards fit under a shared 100");
        assert_eq!(refusals, 2, "the other two agents must get a clean refusal, not an overspend");

        let reserved = store.reserved_in_flight().expect("reserved");
        assert_eq!(reserved, 90, "total committed is exactly the three winners' 90");
        assert!(
            reserved <= balance,
            "committed {reserved} must never exceed the wallet balance {balance}"
        );

        // Idempotency under concurrency: a winning agent re-awarding its own job (a client retry)
        // must not commit a second time — and must not PUBLISH a second time either.
        //
        // Re-award an agent that ACTUALLY won, taken from the join results (#287). Naming a fixed
        // agent here is what made this leg flaky: the winners are whichever three reached the money
        // lock first, so a scheduling change (one more test in the binary is enough) can leave the
        // named agent among the two refused. Then this is not a re-award at all — it is a fresh
        // reserve of 30 against the 10 the winners left, which `reserve` correctly refuses, and the
        // refusal reads as an idempotency failure when the code did exactly the right thing.
        //
        // The prepare/send closures are `unreachable!`, which makes this red-on-revert for the
        // presence gate: remove the gate and the retry republishes — a duplicate award of real
        // money — and this test panics instead of quietly passing on the reserved total alone.
        // Note the two failures are independent: #287 fixed WHICH job is re-awarded, this asserts
        // WHAT a re-award may do. A winner is required for either to mean anything.
        let winner = winners[0].clone();
        let outcome = award_with_reservation(
            &store,
            &winner,
            amount,
            balance,
            2,
            no_relay,
            no_prepare,
            no_send,
            None,
        )
        .await
        .expect("idempotent re-award");
        assert!(
            matches!(outcome, AwardOutcome::AlreadyAwarded(ref record) if record.job_id == winner),
            "a re-award must report the EXISTING award, not a new publish"
        );
        assert_eq!(
            store.reserved_in_flight().expect("reserved"),
            90,
            "an idempotent re-award must not inflate the committed total"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ★ THE MONEY-LOCK-IS-LOAD-BEARING TOOTH (the red-without-serialization proof).
    //
    // The store's atomic reserve cannot, by itself, stop an overspend: `reserve` trusts the
    // `balance`/`spent` SNAPSHOT the caller passes in. If an award reads that snapshot while a
    // concurrent settle is melting — balance read BEFORE the melt drops it, but the reserve landing
    // AFTER the settle's `reserved → spent` flip clears the paid job — the award commits against
    // ecash that has already left the wallet. `BuyerContext::money_lock`, held across BOTH the
    // award's snapshot+reserve and the settle's pay+flip, is the ONE thing that closes this window;
    // nothing in the store or the budget ledger can.
    //
    // This test reproduces that exact interleave deterministically over the REAL store + REAL budget
    // ledger + a modeled wallet balance (the melt drops it; a real melt needs a mint we must not
    // touch), and shows both directions:
    //   • UNSERIALIZED — the stale-snapshot interleave over-commits: reserved + spent exceeds the
    //     wallet's starting ecash. This is the bug that returns the moment the daemon's money_lock
    //     is narrowed or dropped around `award`/`settle_job`.
    //   • SERIALIZED — the same two operations behind one async money lock can interleave in either
    //     order, and neither over-commits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_money_lock_closes_the_award_snapshot_vs_settle_melt_race() {
        use std::sync::atomic::AtomicU64;
        use std::sync::Arc;
        use tokio::sync::{Mutex, Notify};

        let start_balance = 100u64; // wallet ecash on hand
        let cap = 1_000u64; // per-job cap (via conc_home), slack — the wallet is the binding ceiling
        let paid = 60u64; // the in-flight job being settled
        let award = 60u64; // the new award racing it; 60 + 60 = 120 > 100 ecash if both commit

        let job_x = "a".repeat(64); // settled (melt + flip)
        let job_y = "b".repeat(64); // newly awarded during the settle

        // ---- UNSERIALIZED: the stale-snapshot interleave over-commits the wallet ----
        {
            let (home, root) = conc_home("race-unserial", cap);
            let store = Arc::new(BuyerStore::open(root.join("buyer.sqlite")).expect("store"));
            let balance = Arc::new(AtomicU64::new(start_balance));
            store
                .reserve(&job_x, paid, start_balance, 1)
                .expect("pre-reserve the in-flight job");

            let snapshot_taken = Arc::new(Notify::new());
            let settle_done = Arc::new(Notify::new());

            let settle = {
                let (store, balance) = (store.clone(), balance.clone());
                let (home, job_x) = (home.clone(), job_x.clone());
                let (snapshot_taken, settle_done) = (snapshot_taken.clone(), settle_done.clone());
                tokio::spawn(async move {
                    // Wait until the award has read the pre-melt balance, then pay + flip.
                    snapshot_taken.notified().await;
                    settle_after_pay(
                        &store,
                        &job_x,
                        3,
                        || async {
                            let mut gate = BudgetGate::from_home(&home).expect("gate");
                            gate.authorize_and_commit(paid).expect("budget append"); // durable spent
                            balance.fetch_sub(paid, Ordering::SeqCst); // models the wallet melt
                            Ok::<(), &str>(())
                        },
                        |_| paid,
                    )
                    .await
                    .expect("settle");
                    settle_done.notify_one();
                })
            };

            let awarding = {
                let (store, balance) = (store.clone(), balance.clone());
                let job_y = job_y.clone();
                let (snapshot_taken, settle_done) = (snapshot_taken.clone(), settle_done.clone());
                tokio::spawn(async move {
                    // Read the balance snapshot BEFORE the settle melts it…
                    let stale_balance = balance.load(Ordering::SeqCst);
                    snapshot_taken.notify_one();
                    // …then reserve AFTER the settle's flip has cleared job_x from `reserved`.
                    settle_done.notified().await;
                    let job_out = job_y.clone();
                    award_with_reservation(
                        &store,
                        &job_y,
                        award,
                        stale_balance,
                        4,
                        no_relay,
                        || async move { Ok(fake_prepared(&job_out)) },
                        send_acked,
                        None,
                    )
                    .await
                })
            };

            settle.await.expect("settle task");
            let awarded = awarding.await.expect("award task");
            assert!(awarded.is_ok(), "the stale-snapshot award slips through unserialized");

            let committed = store.reserved_in_flight().expect("reserved")
                + BudgetGate::from_home(&home).expect("gate").spent();
            assert!(
                committed > start_balance,
                "the race must over-commit the wallet without the money lock — committed \
                 {committed} sat against only {start_balance} sat of starting ecash; this is the \
                 window BuyerContext::money_lock exists to close"
            );
            let _ = std::fs::remove_dir_all(&root);
        }

        // ---- SERIALIZED: the same two ops behind one money lock never over-commit ----
        {
            let (home, root) = conc_home("race-serial", cap);
            let home = Arc::new(home);
            let store = Arc::new(BuyerStore::open(root.join("buyer.sqlite")).expect("store"));
            let balance = Arc::new(AtomicU64::new(start_balance));
            let money_lock = Arc::new(Mutex::new(()));
            store
                .reserve(&job_x, paid, start_balance, 1)
                .expect("pre-reserve the in-flight job");

            let settle = {
                let (store, balance, money_lock) =
                    (store.clone(), balance.clone(), money_lock.clone());
                let (home, job_x) = (home.clone(), job_x.clone());
                tokio::spawn(async move {
                    // The daemon's `settle_job` holds money_lock across pay + flip.
                    let _guard = money_lock.lock().await;
                    settle_after_pay(
                        &store,
                        &job_x,
                        3,
                        || async {
                            let mut gate = BudgetGate::from_home(&home).expect("gate");
                            gate.authorize_and_commit(paid).expect("budget append");
                            balance.fetch_sub(paid, Ordering::SeqCst);
                            Ok::<(), &str>(())
                        },
                        |_| paid,
                    )
                    .await
                    .expect("settle");
                })
            };

            let awarding = {
                let (store, balance, money_lock) =
                    (store.clone(), balance.clone(), money_lock.clone());
                let job_y = job_y.clone();
                tokio::spawn(async move {
                    // The daemon's `award` holds money_lock across snapshot + reserve.
                    let _guard = money_lock.lock().await;
                    let snap = balance.load(Ordering::SeqCst);
                    let job_out = job_y.clone();
                    award_with_reservation(
                        &store,
                        &job_y,
                        award,
                        snap,
                        4,
                        no_relay,
                        || async move { Ok(fake_prepared(&job_out)) },
                        send_acked,
                        None,
                    )
                    .await
                })
            };

            let _ = settle.await.expect("settle task");
            let _ = awarding.await.expect("award task");

            let committed = store.reserved_in_flight().expect("reserved")
                + BudgetGate::from_home(&home).expect("gate").spent();
            assert!(
                committed <= start_balance,
                "under the money lock the wallet is never over-committed — committed {committed} \
                 sat against {start_balance} sat of ecash, in EITHER interleave order"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    // ── #291: an empty offer read is not proof the offer is gone.
    //
    // The branches are tested together, and the SECOND one is the point. It would be trivial to
    // satisfy "a timed-out read must not park" by never parking, and a suite that only tested the
    // timeout leg would pass on that. The confirmed-empty leg is the positive control: it shows the
    // guard DISCRIMINATES rather than disables.

    const MAX_UNANSWERED: u32 = 12;

    #[test]
    fn an_unanswered_read_does_not_park_the_job() {
        let action = plan_missing_offer(false, 1, MAX_UNANSWERED);
        assert_eq!(
            action,
            MissingOfferAction::Retry,
            "a read the relay never answered is not evidence the offer is gone; parking is \
             terminal, so it must not happen on an unconfirmed read (#291)"
        );
    }

    #[test]
    fn a_confirmed_empty_read_still_parks() {
        // RED LEG. If this ever returns Retry the fix has stopped being a fix and become an
        // infinite poll on every genuinely-expired offer.
        let action = plan_missing_offer(true, 0, MAX_UNANSWERED);
        assert_eq!(
            action,
            MissingOfferAction::ParkOfferAbsent,
            "the relay answered and had no offer: absence IS established here and park is correct"
        );
    }

    #[test]
    fn confirmed_reads_park_regardless_of_earlier_unanswered_ones() {
        // The budget must not outrank the evidence: one answered read settles the question even
        // after a long unreadable stretch.
        let action = plan_missing_offer(true, MAX_UNANSWERED + 5, MAX_UNANSWERED);
        assert_eq!(action, MissingOfferAction::ParkOfferAbsent);
    }

    #[test]
    fn an_exhausted_retry_budget_parks_on_the_read_not_the_offer() {
        let action = plan_missing_offer(false, MAX_UNANSWERED, MAX_UNANSWERED);
        assert_eq!(
            action,
            MissingOfferAction::ParkUnreadable {
                unanswered_reads: MAX_UNANSWERED
            },
            "refusing forever is an infinite loop, so the budget ends — but on a statement about \
             the READ, and it has to carry the count that justifies it"
        );
    }

    #[test]
    fn the_unreadable_park_reason_never_asserts_the_offer_is_gone() {
        // ANCHORED ON ABSENCE, because the defect in #291 was a reason asserting something the
        // evidence could not support. A test that only checked for the presence of good words
        // would pass on a string that also contained the bad ones.
        let reason = park_reason_unreadable(12);
        for forbidden in ["no longer on the relay", "no offer", "offer is gone", "expired"] {
            assert!(
                !reason.contains(forbidden),
                "the unreadable park reason claims something the read never established \
                 ({forbidden:?}): {reason}"
            );
        }
        assert!(
            reason.contains("unanswered"),
            "the reason must say what was OBSERVED — reads that went unanswered: {reason}"
        );
        assert!(
            reason.contains("12"),
            "the reason must carry the count that justifies giving up: {reason}"
        );
    }

    #[test]
    fn the_offer_absent_park_reason_names_the_answer_it_rests_on() {
        // Mirror of the test above: this reason MAY assert absence, but only because it also states
        // the thing that licenses the assertion — that the relay answered.
        assert!(
            PARK_REASON_OFFER_ABSENT.contains("answered"),
            "a park on absence has to name the answered read that established it: \
             {PARK_REASON_OFFER_ABSENT}"
        );
    }

    // ---- #784 capability request predicate -------------------------------------------------

    fn seat(families: &[&str], models: &[(&str, &str)], capabilities: &[&str]) -> crate::heartbeat::SeatCapability {
        crate::heartbeat::SeatCapability {
            harness_families: families.iter().map(|f| (*f).to_owned()).collect(),
            models: models
                .iter()
                .map(|(family, model)| crate::heartbeat::HarnessModel {
                    family: (*family).to_owned(),
                    model: (*model).to_owned(),
                })
                .collect(),
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            harness_variant: None,
            hardware: None,
        }
    }

    // The property the whole feature rests on: a job that asks for nothing is awarded exactly as it
    // was before #784. An UNSTATED seat is the hardest case for this, because it is what every seat
    // on the network advertises until it upgrades.
    #[test]
    fn no_capability_request_awards_exactly_as_before() {
        assert_eq!(
            claim_meets_capability_request(&crate::heartbeat::SeatCapability::default(), &filters(10, 10)),
            Ok(()),
            "an unstated seat must pass a request that asks for nothing"
        );
        assert_eq!(
            claim_meets_capability_request(
                &seat(&["claude-code"], &[("claude-code", "opus")], &["rust"]),
                &filters(10, 10),
            ),
            Ok(()),
            "a fully-stated seat must also pass a request that asks for nothing"
        );
    }

    #[test]
    fn a_family_request_matches_only_a_seat_that_advertises_it() {
        let mut wants = filters(10, 10);
        wants.requested_harness_family = Some("claude-code");
        assert_eq!(
            claim_meets_capability_request(&seat(&["claude-code", "codex"], &[], &[]), &wants),
            Ok(())
        );
        assert_eq!(
            claim_meets_capability_request(&seat(&["codex"], &[], &[]), &wants),
            Err(CapabilityRefusal::HarnessFamily { requested: "claude-code".into() })
        );
    }

    // An unstated seat and a seat stating the WRONG family both refuse, and they are different
    // states — the refusal must not claim the seat advertised something else.
    #[test]
    fn an_unstated_seat_cannot_satisfy_a_family_request() {
        let mut wants = filters(10, 10);
        wants.requested_harness_family = Some("claude-code");
        assert_eq!(
            claim_meets_capability_request(&crate::heartbeat::SeatCapability::default(), &wants),
            Err(CapabilityRefusal::HarnessFamily { requested: "claude-code".into() })
        );
    }

    // The PAIR is the unit. A seat that runs the model on a different harness than the job asked to
    // dispatch on is refused, because matching the model alone would award the wrong dispatch.
    #[test]
    fn a_model_matches_only_when_paired_with_its_own_family() {
        let mut wants = filters(10, 10);
        // A model request names the PRESET too — dispatch reads nothing else. `claude` maps to the
        // requested family, so the two agree and this stays a test about model pairing.
        wants.requested_agent = Some("claude");
        wants.requested_harness_family = Some("claude-code");
        wants.requested_model = Some("opus");
        assert_eq!(
            claim_meets_capability_request(
                &seat(&["claude-code"], &[("claude-code", "opus")], &[]),
                &wants,
            ),
            Ok(())
        );
        assert_eq!(
            claim_meets_capability_request(
                &seat(&["claude-code", "codex"], &[("codex", "opus")], &[]),
                &wants,
            ),
            Err(CapabilityRefusal::Model {
                family: "claude-code".into(),
                requested: "opus".into(),
            }),
            "the same model advertised for a DIFFERENT family must not satisfy the pair"
        );
        assert_eq!(
            claim_meets_capability_request(&seat(&["claude-code"], &[], &[]), &wants),
            Err(CapabilityRefusal::Model {
                family: "claude-code".into(),
                requested: "opus".into(),
            }),
            "a seat stating the family but NO model does not satisfy a model request"
        );
    }

    // A model without a PRESET is a defect in the REQUEST. Refused rather than ignored: ignoring it
    // would award on a weaker filter than the buyer asked for, silently, on the money path.
    //
    // The preset is the anchor because it is the only axis dispatch reads. A model hung off a family
    // alone would pass this filter and then run on whatever preset the seat happens to list first.
    #[test]
    fn a_model_request_without_a_harness_preset_is_refused_not_ignored() {
        let mut wants = filters(10, 10);
        wants.requested_model = Some("opus");
        wants.requested_harness_family = Some("claude-code");
        assert_eq!(
            claim_meets_capability_request(
                &seat(&["claude-code"], &[("claude-code", "opus")], &[]),
                &wants,
            ),
            Err(CapabilityRefusal::ModelWithoutHarnessPreset { requested: "opus".into() }),
            "a seat that DOES advertise the model, for the requested family, must STILL be refused: \
             without a preset nothing binds which harness dispatch would actually start"
        );
    }

    // The counterexample from review: a preset and a family that name different harnesses. Dispatch
    // honours the preset, so awarding this would run Claude on a job that asked for codex — and a
    // multi-harness seat advertises BOTH families, so every other filter passes.
    #[test]
    fn a_family_that_contradicts_the_preset_is_refused_on_a_seat_that_advertises_both() {
        let mut wants = filters(10, 10);
        wants.requested_agent = Some("claude");
        wants.requested_harness_family = Some("codex");
        let multi_harness =
            seat(&["claude-code", "codex"], &[("claude-code", "opus"), ("codex", "gpt-5.6")], &[]);
        assert_eq!(
            claim_meets_capability_request(&multi_harness, &wants),
            Err(CapabilityRefusal::HarnessFamilyContradictsPreset {
                preset: "claude".into(),
                preset_family: "claude-code".into(),
                requested: "codex".into(),
            }),
            "the seat can genuinely serve both, so nothing about the CLAIM refuses this — the \
             request itself is unrunnable and only a request-level check can see it"
        );
    }

    // The shape a buyer reaches for first: name the preset, name the model, say nothing about the
    // family. The family is DERIVED from the preset rather than demanded, so this is a valid request.
    #[test]
    fn a_preset_and_a_model_derive_the_family_instead_of_refusing() {
        let mut wants = filters(10, 10);
        wants.requested_agent = Some("claude");
        wants.requested_model = Some("opus");
        assert_eq!(
            claim_meets_capability_request(
                &seat(&["claude-code"], &[("claude-code", "opus")], &[]),
                &wants,
            ),
            Ok(()),
            "agent=claude implies family=claude-code, so the model has a family to pair against"
        );
        assert_eq!(
            claim_meets_capability_request(
                &seat(&["claude-code"], &[("claude-code", "sonnet")], &[]),
                &wants,
            ),
            Err(CapabilityRefusal::Model {
                family: "claude-code".into(),
                requested: "opus".into()
            }),
            "and the derived family is a real filter, not a formality"
        );
    }

    // The refusal names the MISSING tokens, not the whole request, so it says what to fix.
    #[test]
    fn missing_capabilities_are_named_individually() {
        let required = vec!["rust".to_owned(), "python".to_owned()];
        let mut wants = filters(10, 10);
        wants.required_capabilities = &required;
        assert_eq!(
            claim_meets_capability_request(&seat(&[], &[], &["rust", "python"]), &wants),
            Ok(())
        );
        assert_eq!(
            claim_meets_capability_request(&seat(&[], &[], &["rust"]), &wants),
            Err(CapabilityRefusal::Capabilities { missing: vec!["python".to_owned()] }),
            "only the absent token is named, not the satisfied one"
        );
    }

    // Two refusals that imply OPPOSITE operator actions must not collapse: "no seat advertises this"
    // means wait or add a seat; "that is not a real token" means no seat can ever satisfy it.
    #[test]
    fn an_unknown_token_is_a_different_refusal_from_a_missing_one() {
        let bogus = vec!["kubernetes".to_owned()];
        let mut wants = filters(10, 10);
        wants.required_capabilities = &bogus;
        assert_eq!(
            claim_meets_capability_request(&seat(&[], &[], &["rust"]), &wants),
            Err(CapabilityRefusal::UnknownCapabilityToken { token: "kubernetes".into() })
        );
        // Validated BEFORE the claim is judged: a seat advertising everything real is still refused,
        // so the refusal can never be mistaken for a property of the seat.
        assert_eq!(
            claim_meets_capability_request(&seat(&[], &[], &["node", "python", "rust"]), &wants),
            Err(CapabilityRefusal::UnknownCapabilityToken { token: "kubernetes".into() })
        );
    }

    // The same rule for the FAMILY axis, which had the token axis's guard and not its own.
    //
    // Neither reader filters vocabulary — `harness_families_from_tags` and the offer's param reader
    // both only trim and drop blanks — so a foreign offer can carry any string, and a claim can
    // advertise the same one.
    #[test]
    fn an_unknown_harness_family_is_refused_even_when_the_claim_advertises_it() {
        let mut wants = filters(10, 10);
        wants.requested_harness_family = Some("not-a-family");

        // ⛔ THE ADVERSARIAL CASE IS THE ONLY ONE THAT PROVES ANYTHING, AND IT IS EASY TO GET WRONG.
        // The claim advertises THE SAME unknown value. Test this against an ORDINARY claim instead
        // and it goes red on a plain family mismatch — which it would do with the vocabulary check
        // deleted entirely. Such a test passes without the guard and certifies nothing.
        assert_eq!(
            claim_meets_capability_request(&seat(&["not-a-family"], &[], &[]), &wants),
            Err(CapabilityRefusal::UnknownHarnessFamily { requested: "not-a-family".into() }),
            "a claim that AGREES with a nonsense request must not launder it into a match"
        );

        // Judged on the REQUEST, before the claim: a seat advertising every real family is refused
        // identically, so the refusal can never be read as a property of the seat.
        assert_eq!(
            claim_meets_capability_request(
                &seat(&["claude-code", "codex", "cursor", "goose"], &[], &[]),
                &wants,
            ),
            Err(CapabilityRefusal::UnknownHarnessFamily { requested: "not-a-family".into() }),
        );

        // And it narrows nothing real: every family in the vocabulary still passes on a seat that
        // serves it. Without this the guard could be a blanket refusal and the cases above would not
        // notice.
        for family in crate::agent_presets::HARNESS_FAMILIES {
            wants.requested_harness_family = Some(family);
            assert_eq!(
                claim_meets_capability_request(&seat(&[family], &[], &[]), &wants),
                Ok(()),
                "{family} is a real family and must still pass"
            );
        }
    }

    // The wire-level case this exists for: an offer we did not build, matched by a claim that agrees
    // with it. The post-time vocabulary gate never sees this offer, so if the predicate does not
    // refuse it, nothing does — and the comment on `unsatisfiable_capability_request` promises that
    // the award-time refusal IS the backstop for foreign offers.
    #[test]
    fn a_foreign_offer_naming_a_bogus_family_is_refused_on_both_paths_and_parks_blaming_the_request()
    {
        let job = "a".repeat(64);
        let mints = vec![DEFAULT_MINT_URL.to_owned()];
        let claim_id = "c".repeat(64);

        // A claim that is payable, live, and advertises exactly the bogus family requested. Every
        // other filter passes; only the vocabulary check stands between this and an award.
        let mut colluding = claim(&job, true, 10, &mints);
        colluding.capability = seat(&["not-a-family"], &[], &[]);

        let bogus = offer_requesting(&job, 10, Some("not-a-family"), &[]);
        let view =
            JobView { offer: Some(bogus.clone()), ..view_with(&job, 10, vec![colluding]) };
        let filters = filters_from_offer(&bogus, 10);

        assert_eq!(
            select_awardable_claim(&view, &filters),
            None,
            "auto path: a family outside the vocabulary must never award, however well it matches"
        );
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters),
            Err(NamedAwardRefused::Capability {
                claim_id: claim_id.clone(),
                refusal: CapabilityRefusal::UnknownHarnessFamily {
                    requested: "not-a-family".into()
                },
            }),
            "manual path: naming the claim must not bypass the vocabulary check either"
        );

        let reason = capability_park_reason(&view, &filters)
            .expect("a bogus family must produce a park reason");
        assert!(
            reason.contains("not a known harness family"),
            "the row must blame the REQUEST. Blaming the claim would send an operator to find \
             another seat for a family that does not exist: {reason}"
        );
    }

    // A family refusal beats a model refusal when a claim fails both, because the family one is the
    // actionable half — a model refusal would send an operator chasing a model on a harness the seat
    // never offered.
    #[test]
    fn a_claim_failing_both_reports_the_family_refusal() {
        let mut wants = filters(10, 10);
        wants.requested_agent = Some("claude");
        wants.requested_harness_family = Some("claude-code");
        wants.requested_model = Some("opus");
        assert_eq!(
            claim_meets_capability_request(&seat(&["codex"], &[("codex", "sonnet")], &[]), &wants),
            Err(CapabilityRefusal::HarnessFamily { requested: "claude-code".into() })
        );
    }

    /// An offer carrying a capability request, and the filters built from it EXACTLY as both
    /// production award sites build them (`buyer/mod.rs`).
    ///
    /// The existing predicate tests set these fields on hand-built filters, which proves the
    /// predicate decides correctly. It cannot prove the OFFER reaches the predicate — the gap #897
    /// exists to close. This builds the request the way production does, so the two tests together
    /// cover the predicate and the wiring rather than the predicate twice.
    fn offer_requesting(
        job_id: &str,
        amount: u64,
        family: Option<&str>,
        capabilities: &[&str],
    ) -> OfferView {
        OfferView {
            requested_harness_family: family.map(str::to_owned),
            required_capabilities: capabilities.iter().map(|token| (*token).to_owned()).collect(),
            ..offer_view(job_id, amount)
        }
    }

    /// The filters production would build for this offer — THE PRODUCTION CONSTRUCTOR ITSELF, not a
    /// copy of it.
    ///
    /// This was briefly a hand-written mirror, and the mirror drifted: it carried
    /// `requested_model: None` for a revision while production read the offer, so two model tests
    /// passed while asserting the opposite of the intended behaviour. A test that restates production
    /// wiring is a second copy subject to exactly the same drift rule as a gate that restates a
    /// predicate, and the fix in both cases is to call the real thing rather than to test the copy
    /// harder.
    fn filters_from_offer<'a>(offer: &'a OfferView, max_sats: u64) -> AwardFilters<'a> {
        award_filters_for_offer(offer, max_sats, DEFAULT_MINT_URL, false)
    }

    // THE ACCEPTANCE TEST FOR #897, both axes through BOTH selection entry points.
    //
    // A request honoured on one path and dropped on the other is precisely the bypass #866 was filed
    // to close, so a test that checked only `select_awardable_claim` would leave the manual path — the
    // one an operator drives by hand, naming a claim — free to award what the auto path refuses.
    //
    // The payable claim is deliberately otherwise-perfect: live, priced at the offer amount, quoting
    // the buyer's mint. Only capability separates award from refusal, so a green here cannot be a
    // price or mint failure wearing a capability costume.
    #[test]
    fn an_offer_sourced_request_refuses_a_non_matching_claim_on_both_paths() {
        let job = "a".repeat(64);
        let mints = vec![DEFAULT_MINT_URL.to_owned()];
        let claim_id = "c".repeat(64);
        let mut payable = claim(&job, true, 10, &mints);
        payable.capability = seat(&["codex"], &[], &["rust"]);
        // The seat also serves the `codex` PRESET, so the model case below can name one. A model
        // request without a preset is refused as a request defect and would never reach the model
        // comparison this test exists to make.
        payable.agents = vec!["codex".to_owned()];

        // Control FIRST: the same claim, the same paths, no request. A refusal below means nothing
        // unless this passes — otherwise the test proves only that the claim was unawardable.
        let indifferent = offer_requesting(&job, 10, None, &[]);
        let view = JobView { offer: Some(indifferent.clone()), ..view_with(&job, 10, vec![payable]) };
        assert_eq!(
            select_awardable_claim(&view, &filters_from_offer(&indifferent, 10)),
            Some(claim_id.clone()),
            "control: with no request on the offer the claim must be awarded as before"
        );
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters_from_offer(&indifferent, 10)),
            Ok(()),
            "control: the manual path must also award it with no request"
        );

        // FAMILY the claim does not advertise.
        let wants_family = offer_requesting(&job, 10, Some("claude-code"), &[]);
        let view = JobView { offer: Some(wants_family.clone()), ..view.clone() };
        assert_eq!(
            select_awardable_claim(&view, &filters_from_offer(&wants_family, 10)),
            None,
            "auto path: a claim failing the offer's family request must not be selected"
        );
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters_from_offer(&wants_family, 10)),
            Err(NamedAwardRefused::Capability {
                claim_id: claim_id.clone(),
                refusal: CapabilityRefusal::HarnessFamily { requested: "claude-code".into() },
            }),
            "manual path: naming the claim must NOT bypass the offer's family request"
        );

        // CAPABILITY the claim does not advertise. Asserted separately because a wiring that carried
        // only the family would leave this axis silently inert while the family assertions pass.
        let wants_python = offer_requesting(&job, 10, None, &["python"]);
        let view = JobView { offer: Some(wants_python.clone()), ..view.clone() };
        assert_eq!(
            select_awardable_claim(&view, &filters_from_offer(&wants_python, 10)),
            None,
            "auto path: a claim missing a required capability must not be selected"
        );
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters_from_offer(&wants_python, 10)),
            Err(NamedAwardRefused::Capability {
                claim_id: claim_id.clone(),
                refusal: CapabilityRefusal::Capabilities { missing: vec!["python".to_owned()] },
            }),
            "manual path: naming the claim must NOT bypass the offer's capability request"
        );

        // MODEL the claim does not advertise, paired with a family it DOES. The pair is the unit, so
        // this is the case that separates a real model filter from one that matches on family alone.
        let wants_model = OfferView {
            requested_agent: Some("codex".to_owned()),
            requested_model: Some("opus".to_owned()),
            ..offer_requesting(&job, 10, Some("codex"), &[])
        };
        let view = JobView { offer: Some(wants_model.clone()), ..view.clone() };
        assert_eq!(
            select_awardable_claim(&view, &filters_from_offer(&wants_model, 10)),
            None,
            "auto path: a claim advertising the family but not the model must not be selected"
        );
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters_from_offer(&wants_model, 10)),
            Err(NamedAwardRefused::Capability {
                claim_id: claim_id.clone(),
                refusal: CapabilityRefusal::Model {
                    family: "codex".into(),
                    requested: "opus".into(),
                },
            }),
            "manual path: naming the claim must NOT bypass the offer's model request"
        );

        // And a request the claim DOES satisfy still awards — otherwise the refusals above are
        // equally explained by a filter that refuses everything.
        let satisfied = offer_requesting(&job, 10, Some("codex"), &["rust"]);
        let view = JobView { offer: Some(satisfied.clone()), ..view.clone() };
        assert_eq!(
            select_awardable_claim(&view, &filters_from_offer(&satisfied, 10)),
            Some(claim_id.clone()),
            "a matching request must still award — the filter narrows, it does not block"
        );
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters_from_offer(&satisfied, 10)),
            Ok(()),
            "the manual path must award a claim that satisfies the request"
        );
    }

    // THE COUPLING TEST: the post-time gate refuses EXACTLY the requests the award predicate can
    // never pass — no more, no less.
    //
    // This is the assertion that keeps the two from rotting apart. The gate exists only to surface a
    // predicate consequence earlier, so any request where the two disagree is a bug in one of them:
    // a gate that refuses MORE blocks posts that would have been awarded, and a gate that refuses
    // LESS is the silent park the gate was added to prevent.
    //
    // Both sides are computed here rather than asserted against a hand-written expectation. A table
    // of "these shapes must be refused" would be a third copy of the rule and would need editing on
    // the day #788 makes a bare model valid; this needs none — on that day the predicate changes and
    // both sides of the comparison move together.
    #[test]
    fn the_post_time_gate_refuses_exactly_what_the_predicate_can_never_pass() {
        let rust = vec!["rust".to_owned()];
        let bogus = vec!["kubernetes".to_owned()];
        let mixed = vec!["rust".to_owned(), "kubernetes".to_owned()];
        let shapes: Vec<(Option<&str>, Option<&str>, Option<&str>, &[String])> = vec![
            // Satisfiable: absent, single-axis, and fully-specified requests.
            (None, None, None, &[]),
            (None, Some("codex"), None, &[]),
            (Some("codex"), None, None, &[]),
            (Some("codex"), Some("codex"), Some("gpt-5.6-sol[low]"), &[]),
            // The family DERIVED from the preset rather than stated.
            (Some("codex"), None, Some("gpt-5.6-sol[low]"), &[]),
            (None, None, None, &rust),
            (Some("codex"), Some("codex"), Some("gpt-5.6-sol[low]"), &rust),
            // Unsatisfiable: a model with no preset, so nothing binds the harness that would run it.
            // A family does not rescue it — dispatch never reads one.
            (None, None, Some("opus"), &[]),
            (None, Some("claude-code"), Some("opus"), &[]),
            (None, Some("claude-code"), Some("opus"), &rust),
            // Unsatisfiable: a family naming a harness the preset would not dispatch.
            (Some("claude"), Some("codex"), None, &[]),
            (Some("claude"), Some("codex"), Some("opus"), &[]),
            // Unsatisfiable: a custom preset has no family, so nothing can establish that what
            // dispatch runs is what was asked for.
            (Some("my-custom-harness"), Some("codex"), None, &[]),
            (Some("my-custom-harness"), None, Some("opus"), &[]),
            // But a custom preset ALONE constrains no harness and stays satisfiable — the preset
            // filter already binds dispatch exactly.
            (Some("my-custom-harness"), None, None, &[]),
            // Out-of-vocabulary capability token. Included because the token rule is the ONE rule
            // both post-time gates can see: the vocabulary gate checks it directly, and the
            // predicate checks it too, so the satisfiability gate surfaces it as well. Both read the
            // same `CAPABILITIES` constant, so adding a token cannot make them disagree — but the
            // rule being stated twice is worth a row here rather than an argument in a comment.
            (None, None, None, &bogus),
            (None, Some("codex"), None, &mixed),
            // Out-of-vocabulary FAMILY, the axis that had no vocabulary rule until review found it.
            // Alone, and paired with each other axis, because the check runs on the REQUEST and must
            // not be reachable only on some paths through the predicate.
            (None, Some("not-a-family"), None, &[]),
            (None, Some("not-a-family"), Some("opus"), &[]),
            (Some("codex"), Some("not-a-family"), None, &[]),
            (None, Some("not-a-family"), None, &rust),
        ];

        for (agent, family, model, capabilities) in shapes {
            let gate = unsatisfiable_capability_request(agent, family, model, capabilities);

            // What the predicate says about the claim that advertises EXACTLY this request. If even
            // that claim is refused, no publishable claim can pass.
            //
            // Built here rather than reused from the gate, so the two are a CROSS-CHECK instead of
            // one artifact agreeing with itself. It advertises the stated family and the preset's
            // family both, because either can be the one the predicate binds to and this oracle
            // must not have to know which — knowing would make it a copy of the rule under test.
            let mut families: Vec<&str> = Vec::new();
            for candidate in
                [family, agent.and_then(crate::agent_presets::harness_family_for_preset)]
                    .into_iter()
                    .flatten()
            {
                if !families.contains(&candidate) {
                    families.push(candidate);
                }
            }
            let pairs: Vec<(&str, &str)> = match model {
                Some(model) => families.iter().map(|family| (*family, model)).collect(),
                None => Vec::new(),
            };
            let perfect =
                seat(&families, &pairs, &capabilities.iter().map(String::as_str).collect::<Vec<_>>());
            let mut filters = filters(10, 10);
            filters.requested_agent = agent;
            filters.requested_harness_family = family;
            filters.requested_model = model;
            filters.required_capabilities = capabilities;
            let predicate = claim_meets_capability_request(&perfect, &filters).err();

            assert_eq!(
                gate, predicate,
                "gate and predicate disagree on {agent:?}/{family:?}/{model:?}/{capabilities:?}. The \
                 gate must refuse exactly what the predicate can never pass: refusing more blocks \
                 posts that would have been awarded, refusing less restores the silent park the gate \
                 exists to prevent."
            );
        }

        // ⛔ THESE CONTROLS ARE THE ONLY REAL COVERAGE THIS TEST HAS AGAINST A FAULT IN THE
        // PREDICATE. THE LOOP ABOVE IS NOT. Do not read that loop as protecting the predicate, and
        // do not weaken these on the grounds that the loop already compares the two sides.
        //
        // The oracle is COMPUTED FROM THE SUBJECT: both sides of that comparison end in
        // `claim_meets_capability_request`. So when the PREDICATE is what breaks, both sides move
        // together, the comparison still holds, and the loop reports a pass THROUGH the exact fault
        // it looks like it is watching for. That is measured, not feared — deleting the
        // family-contradicts-preset check, and separately the model-requires-preset check, each left
        // the loop GREEN, and only these controls went red.
        //
        // The general form, because it is not special to this test: whenever an oracle is derived
        // from the thing under test, the controls carry the whole of the coverage. Agreement between
        // two things computed from one broken source is not evidence about the source.
        //
        // ⇒ ADDING A RULE TO THE PREDICATE MEANS ADDING A CONTROL HERE. The loop will not notice.
        // One control per rule the table exercises; they pin actual verdicts rather than agreement,
        // so an oracle wrong in the same way as the gate still fails here.
        assert!(
            unsatisfiable_capability_request(None, None, Some("opus"), &[]).is_some(),
            "control: a model with no harness preset must be refused by the gate"
        );
        assert!(
            unsatisfiable_capability_request(None, Some("claude-code"), Some("opus"), &[]).is_some(),
            "control: a family does NOT substitute for the preset — dispatch never reads one"
        );
        assert!(
            unsatisfiable_capability_request(Some("claude"), Some("codex"), None, &[]).is_some(),
            "control: a family contradicting the preset must be refused by the gate"
        );
        assert!(
            unsatisfiable_capability_request(Some("codex"), Some("codex"), Some("opus"), &[])
                .is_none(),
            "control: a fully-specified, self-consistent request must NOT be refused by the gate"
        );
        assert!(
            unsatisfiable_capability_request(Some("codex"), None, Some("opus"), &[]).is_none(),
            "control: the family may be DERIVED from the preset rather than stated"
        );
        assert!(
            unsatisfiable_capability_request(None, Some("codex"), None, &[]).is_none(),
            "control: a family-only request stays valid as a SEAT filter"
        );
        assert!(
            unsatisfiable_capability_request(None, Some("not-a-family"), None, &[]).is_some(),
            "control: a family outside the vocabulary must be refused by the gate — the synthesized \
             claim advertises exactly that family, so this passes only if the REQUEST is judged \
             before the claim"
        );
    }

    // A request that matches no live claim PARKS with a reason naming what to fix, rather than
    // awarding anyway or failing silently (#897 acceptance).
    #[test]
    fn a_request_matching_no_claim_parks_with_an_actionable_reason() {
        let job = "a".repeat(64);
        let mints = vec![DEFAULT_MINT_URL.to_owned()];
        let mut payable = claim(&job, true, 10, &mints);
        payable.capability = seat(&["codex"], &[], &[]);

        let wants = offer_requesting(&job, 10, Some("claude-code"), &[]);
        let view = JobView { offer: Some(wants.clone()), ..view_with(&job, 10, vec![payable]) };
        let reason = capability_park_reason(&view, &filters_from_offer(&wants, 10))
            .expect("an unsatisfiable request must produce a reason");
        assert!(
            reason.contains("claude-code"),
            "the reason must name the REQUEST that refused, so an operator knows what to change: \
             {reason}"
        );

        // The park row an operator actually reads states BOTH facts: the job is over, and this is
        // what would have to change. Neither alone is actionable.
        let row = park_reason_deadline_passed(Some(&reason));
        assert!(row.contains("offer deadline passed"), "{row}");
        assert!(row.contains("claude-code"), "{row}");

        // And with no capability obstacle the row is EXACTLY what it was before #897 — a job that
        // asked for nothing must not acquire new wording.
        assert_eq!(
            park_reason_deadline_passed(None),
            "offer deadline passed before an awardable claim appeared"
        );
    }

    // THE WIRE-LEVEL LAYER, and the reason the post-time gate is not the whole answer.
    //
    // Our posting surface refuses a model-only request before signing. A FOREIGN client is under no
    // such obligation: it can publish a model-only offer straight to the relay, and our buyer will
    // read it. For that offer the award-time refusal and this park row are the only truth there is.
    //
    // So this is deliberately NOT reachable through `post_job` — it constructs the offer the way the
    // relay would hand it to us. A test that went through our own posting path could not express this
    // case at all, because the gate would refuse it first, and the layer would look tested when the
    // only tested thing was the gate.
    #[test]
    fn a_foreign_model_only_offer_is_refused_at_award_and_parks_saying_why() {
        let job = "a".repeat(64);
        let mints = vec![DEFAULT_MINT_URL.to_owned()];
        let claim_id = "c".repeat(64);
        // A seat that genuinely advertises the model, under a family. Even this claim must be refused:
        // the defect is in the REQUEST, and a claim-blaming refusal would send an operator to fix a
        // seat that is doing everything right.
        let mut advertising = claim(&job, true, 10, &mints);
        advertising.capability = seat(&["codex"], &[("codex", "opus")], &[]);

        let model_only = offer_requesting(&job, 10, None, &[]);
        let model_only = OfferView { requested_model: Some("opus".to_owned()), ..model_only };
        let view = JobView {
            offer: Some(model_only.clone()),
            ..view_with(&job, 10, vec![advertising])
        };
        let filters = filters_from_offer(&model_only, 10);

        assert_eq!(
            select_awardable_claim(&view, &filters),
            None,
            "auto path: a model with no preset must refuse, not be ignored"
        );
        assert_eq!(
            named_claim_awardable(&view, &claim_id, &filters),
            Err(NamedAwardRefused::Capability {
                claim_id: claim_id.clone(),
                refusal: CapabilityRefusal::ModelWithoutHarnessPreset { requested: "opus".into() },
            }),
            "manual path: naming the claim must not bypass the malformed request either"
        );

        let reason = capability_park_reason(&view, &filters)
            .expect("a model-only request must produce a park reason");
        assert!(
            reason.contains("without a harness preset"),
            "the row must name the REQUEST defect, not blame the claim — an operator reading it has \
             to know to add the `agent` preset rather than to go find another seat: {reason}"
        );
    }

    // The park reason must stay SILENT unless the capability request is genuinely the obstacle.
    // This is the half that makes the reason worth trusting: a clause that appears whenever a request
    // is present would blame the request for every price and mint failure, and it would be most
    // convincing exactly when it was wrong.
    #[test]
    fn the_park_reason_declines_to_blame_capability_when_it_was_not_the_obstacle() {
        let job = "a".repeat(64);
        let mints = vec![DEFAULT_MINT_URL.to_owned()];
        let mut matching = claim(&job, true, 10, &mints);
        matching.capability = seat(&["codex"], &[], &[]);

        // No request ⇒ nothing to explain.
        let indifferent = offer_requesting(&job, 10, None, &[]);
        let view = JobView {
            offer: Some(indifferent.clone()),
            ..view_with(&job, 10, vec![matching.clone()])
        };
        assert_eq!(capability_park_reason(&view, &filters_from_offer(&indifferent, 10)), None);

        // A request that SOME live claim satisfies ⇒ capability was not the obstacle. The claim here
        // matches the request, so whatever stopped the award was price, mint or budget.
        let satisfied = offer_requesting(&job, 10, Some("codex"), &[]);
        let view = JobView { offer: Some(satisfied.clone()), ..view.clone() };
        assert_eq!(
            capability_park_reason(&view, &filters_from_offer(&satisfied, 10)),
            None,
            "a satisfied request must never appear in a park reason"
        );

        // No LIVE claims ⇒ nothing was refused. "No seat advertises X" would be false where the truth
        // is that nobody claimed, and it would send an operator to fix a request that is fine.
        let wants = offer_requesting(&job, 10, Some("claude-code"), &[]);
        // NO CLAIMS AT ALL, which is what "an empty relay" means. A claim carrying `live: false`
        // would NOT do: the diagnosis re-derives liveness from status, so a claim still saying
        // "processing" is a candidate however the flag was set, and an `expired` one was a
        // candidate at the deadline by definition — that status is written only by the demotion.
        // Hand-setting the flag would assert against a state production cannot hold.
        let view = JobView { offer: Some(wants.clone()), ..view_with(&job, 10, Vec::new()) };
        assert_eq!(
            capability_park_reason(&view, &filters_from_offer(&wants, 10)),
            None,
            "an empty relay is not a capability failure"
        );
    }

    // THE PRODUCTION SHAPE OF THE DEADLINE PARK — the state `drive_auto_award` actually holds when
    // it writes the row, rather than one assembled to suit the diagnosis.
    //
    // The earlier park tests hand-built `live: true` and called the helpers directly. That is a state
    // the real branch CANNOT receive: a job parks for a passed deadline only after liveness has been
    // re-derived against the current clock, and `derive_claim_liveness` demotes every `processing`
    // claim to expired the moment `now > deadline`. Reading `live` at that point finds nothing, so
    // the diagnosis returned `None` and the generic deadline row stood — a green test over a code
    // path production could never reach.
    //
    // So this drives PRODUCTION'S OWN liveness step at `now = deadline + 1` BEFORE forming the row.
    #[test]
    fn the_capability_clause_survives_the_real_deadline_demotion() {
        let job = "a".repeat(64);
        let mints = vec![DEFAULT_MINT_URL.to_owned()];
        let deadline = 1_000_u64;

        let park_row_for = |capability: crate::heartbeat::SeatCapability, requested| {
            let mut claimed = claim(&job, true, 10, &mints);
            claimed.capability = capability;
            let offer = OfferView {
                deadline_unix: deadline,
                ..offer_requesting(&job, 10, requested, &[])
            };
            let mut claims = vec![claimed];
            // PRODUCTION'S OWN STEP, at the clock the park branch runs on.
            crate::job_lifecycle::derive_claim_liveness(
                &mut claims,
                &[],
                Some(deadline),
                deadline + 1,
            );
            assert!(
                claims.iter().all(|claim| !claim.live),
                "precondition: past the deadline production holds NO live claim — if this ever \
                 stops being true the regression has stopped reproducing the bug it guards"
            );
            let view = JobView {
                offer: Some(offer.clone()),
                claims,
                ..view_with(&job, 10, Vec::new())
            };
            park_reason_deadline_passed(
                capability_park_reason(&view, &filters_from_offer(&offer, 10)).as_deref(),
            )
        };

        let refused = park_row_for(seat(&["codex"], &[], &[]), Some("claude-code"));
        assert!(
            refused.contains("claude-code"),
            "the row an operator reads must name the request that refused every claim, not only \
             that time ran out: {refused}"
        );

        // SILENT when capability was not the obstacle — the case where a capability-shaped reason is
        // most plausible and most wrong.
        let satisfied = park_row_for(seat(&["claude-code"], &[], &[]), Some("claude-code"));
        assert_eq!(
            satisfied,
            park_reason_deadline_passed(None),
            "a claim that SATISFIED the request means price, mint or budget stopped the award; \
             blaming capability would send the operator to fix a request that is fine"
        );

        // SILENT with no claims at all — nothing was refused, so there is nothing to explain.
        let wants = OfferView {
            deadline_unix: deadline,
            ..offer_requesting(&job, 10, Some("claude-code"), &[])
        };
        let empty = JobView {
            offer: Some(wants.clone()),
            ..view_with(&job, 10, Vec::new())
        };
        assert_eq!(
            capability_park_reason(&empty, &filters_from_offer(&wants, 10)),
            None,
            "an empty relay is not a capability failure"
        );
    }

    // #897 opens a NEW filter surface — the offer's request params — and the display-only fields must
    // not reach it. `hardware_is_unreachable_from_the_filterable_surface` guards the seat's
    // ADVERTISEMENT; nothing guarded the REQUEST side, because until now there was no request.
    //
    // Asserted against the filter struct itself rather than against a list of param names: a test
    // naming params would keep passing if a display-only axis were added straight to `AwardFilters`,
    // which is the one place a filter can actually read.
    #[test]
    fn display_only_fields_never_reach_the_award_filter() {
        // FIELD NAMES, not raw text. Searching the declaration's text would fire on a doc comment
        // that merely MENTIONS hardware — and the natural comment to write beside these fields is
        // exactly "hardware is never filterable", so the obvious implementation is a false-alarm
        // generator aimed at the one sentence a careful author would add.
        fn filter_field_names(source: &str) -> Vec<&str> {
            source
                .split_once("pub struct AwardFilters<'a> {")
                .expect("AwardFilters declaration")
                .1
                .split_once("\n}")
                .expect("end of AwardFilters declaration")
                .0
                .lines()
                .filter_map(|line| line.trim().strip_prefix("pub "))
                .filter_map(|field| field.split(':').next())
                .collect()
        }

        let display_only = [crate::heartbeat::HARNESS_VARIANT_TAG, crate::heartbeat::HARDWARE_TAG];

        // POSITIVE CONTROL ON THE DETECTOR ITSELF, against a synthetic declaration that IS bad.
        // Without it, a `filter_field_names` that returned an empty list — a renamed struct, a
        // changed brace style, a `pub(crate)` field — would pass the real assertion below while
        // inspecting nothing, and the pass would look identical to a correct one.
        let planted = filter_field_names(
            "pub struct AwardFilters<'a> {\n    pub max_sats: u64,\n    pub hardware: Option<&'a str>,\n}",
        );
        assert!(
            planted.contains(&"hardware"),
            "control: the detector must FIND a display-only field when one is present, else its \
             verdict on the real declaration means nothing. Found: {planted:?}"
        );

        let fields = filter_field_names(include_str!("lifecycle.rs"));
        assert!(
            fields.contains(&"required_capabilities"),
            "control: the real declaration must parse into recognizable fields. Found: {fields:?}"
        );
        for banned in display_only {
            assert!(
                !fields.contains(&banned),
                "{banned} is display-only and must NEVER be filterable \
                 (docs/protocol-v1.md 4.5.1) — it is operator-declared free text that nothing can \
                 contradict, so filtering on it would decide money on an unfalsifiable claim. It is \
                 a field of AwardFilters. Fields: {fields:?}"
            );
        }
    }

    // ALL THREE axes of the capability request are read OFF THE SIGNED OFFER, at BOTH award sites
    // (#897, replacing the all-inert tripwire #866 left behind).
    //
    // The property is not "the fields are populated" but "they come from the OFFER". A site reading
    // any of them from award params, config, or a local would let the request a buyer is held to
    // differ from the one it signed and published — and that is the whole reason the filter reads the
    // relay rather than its own inputs.
    #[test]
    fn both_award_paths_read_the_capability_request_off_the_offer() {
        let award_paths = include_str!("mod.rs");
        // Occurrences on LIVE lines only. A commented-out field name holds a whole-file count at the
        // expected value while the live code says something else — a comment manufacturing the very
        // occurrence that answers the probe. Line comments are the only comment form in this file.
        let live = |needle: &str| {
            award_paths
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .map(|line| line.matches(needle).count())
                .sum::<usize>()
        };

        // The two production award sites: the manual `award_claim` RPC and `drive_auto_award`.
        const AWARD_SITES: usize = 2;

        // Every axis, spelled as the offer-sourced form. Enumerated as a table rather than as three
        // hand-written assertions so that adding a fourth request field to `AwardFilters` and wiring
        // it is the only way to satisfy this — a new axis left inert has to be added here to pass,
        // which is the moment its author reads what that costs.
        // Both award sites must go through `award_filters_for_offer`, the ONE constructor. That
        // function reads every axis off the offer, so this single property replaces the per-axis
        // string-scraping this test used to do — and it replaces it with something stronger, because
        // a new request axis added to the constructor reaches both paths with no edit here.
        let shared = live("lifecycle::award_filters_for_offer(");
        assert_eq!(
            shared, AWARD_SITES,
            "expected both award sites in buyer/mod.rs to build filters through \
             `lifecycle::award_filters_for_offer`, found {shared}. That constructor is what makes \
             'both paths filter identically' structural rather than a convention: a site building \
             its own AwardFilters can silently drop a request axis, which is exactly the bypass #866 \
             was filed to close."
        );

        // AND NO SITE MAY HAND-ROLL ITS OWN. Without this the assertion above passes while a third
        // site quietly constructs a literal beside the two that behave — the counts would confirm the
        // good paths and say nothing about the bad one.
        let hand_rolled = live("AwardFilters {");
        assert_eq!(
            hand_rolled, 0,
            "found {hand_rolled} hand-written `AwardFilters` literal(s) in buyer/mod.rs. Every award \
             site must use `award_filters_for_offer` so a new request axis cannot reach one path and \
             miss the other. If a site genuinely needs different filters, that is a money-path \
             decision and belongs in the constructor with a reason, not in a local literal."
        );
    }

    // The wire-in, asserted separately from the predicate: `select_awardable_claim` must CONSULT it.
    // Bite for the red-prove: drop the `claim_meets_capability_request` arm from the `find` chain and
    // the second assertion below returns the claim id instead of `None`.
    #[test]
    fn select_awardable_claim_consults_the_capability_predicate() {
        let job = "a".repeat(64);
        let mints = vec![DEFAULT_MINT_URL.to_owned()];
        let mut payable = claim(&job, true, 10, &mints);
        payable.capability = seat(&["codex"], &[], &[]);
        let view = view_with(&job, 10, vec![payable]);

        assert_eq!(
            select_awardable_claim(&view, &filters(10, 10)),
            Some("c".repeat(64)),
            "with no capability request the claim is awarded as before"
        );

        let mut wants = filters(10, 10);
        wants.requested_harness_family = Some("claude-code");
        assert_eq!(
            select_awardable_claim(&view, &wants),
            None,
            "a payable claim whose capability fails the request must NOT be selected"
        );
    }
}
