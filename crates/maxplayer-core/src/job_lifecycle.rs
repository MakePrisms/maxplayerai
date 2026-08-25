//! Buyer job lifecycle over the maxplayer relay (kinds offer / feedback / result).
//!
//! - [`post_job`] publishes a real offer-kind offer (targeted p-tag = documented default).
//! - [`get_job`] reads claim/result state from relay events (not local invent).
//! - [`accept_claim`] records a local pay-bind for
//!   [`authorize_pay`](crate::authorize_pay) (seller / result / commit) — written BEFORE the
//!   publish, so a crash cannot leave a public accept with no bind — then publishes an
//!   `accepted` ACCEPT (kind-3406 via [`accept_draft`]). [`prepare_award_async`] signs the
//!   SELECTION as a kind-3405 AWARD — separate kinds, so a reader can tell a pay-bind from a
//!   choice of seller. Claims/results themselves remain relay-truth.
//!
//! Local bind under `~/.maxplayer/jobs/<job_id>.json` is accept-state only.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gateway::{
    self, accept_draft, award_draft, parse_git_result_delivery, parse_offer, EventDraft, OfferDraft,
    TagSpec, JOB_AWARD_KIND, JOB_CLAIM_KIND, JOB_FEEDBACK_KIND, JOB_OFFER_KIND, JOB_RESULT_KIND,
};
use crate::home::{self, HomeError, MaxplayerHome};
#[cfg(feature = "wallet")]
use crate::{buyer_fund, payment_wallet};

const JOBS_DIR: &str = "jobs";
/// Per-relay-fetch budget. Kept well under [`WAIT_FOR_CAP_SECS`] / MCP tool deadline.
const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 5;
/// Re-exported so this module's own callers (and `buyer`) keep their existing path. The constant
/// itself lives in an UNGATED module because the MCP tool table needs it without `wallet`.
pub use crate::long_poll::WAIT_FOR_CAP_SECS;
const DEFAULT_DEADLINE_SECS: u64 = 3_600;
/// Derived claim status surfaced when a `processing` claim is past its offer deadline and its
/// seller has published NO payable delivery (or the delivery's pay window has itself lapsed).
/// Never a relay status value — it is computed by [`derive_claim_liveness`] from `now`.
pub const CLAIM_STATUS_EXPIRED: &str = "expired";
/// Derived claim status surfaced when a `processing` claim is past its offer deadline BUT its
/// seller has published a delivery that is still inside the pay window. The offer deadline is a
/// scheduling clock (stop accepting new work); it must not invalidate work already delivered, so
/// such a claim stays payable — [`accept_claim`] treats `delivered` exactly like `processing`.
/// Never a relay status value — computed by [`derive_claim_liveness`] from `now` + the results.
pub const CLAIM_STATUS_DELIVERED: &str = "delivered";
/// How long after a delivery the buyer may still accept + pay for it, measured from the delivery
/// event's `created_at`. Decoupled from — and far more generous than — the offer deadline
/// ([`DEFAULT_DEADLINE_SECS`]): a gate-verified delivery proves the work happened, so a slow buyer
/// must not be able to strand it on the short scheduling clock. The relaxation is ONLY about not
/// rejecting a proven delivery on a timer; every money gate (creq/cosig/tip-match/budget/
/// single-redeem) still fires downstream in [`accept_claim`] / [`crate::authorize_pay`].
pub(crate) const DELIVERY_PAY_WINDOW_SECS: u64 = 7 * 24 * 3_600;

/// Inputs for posting a offer-kind offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostJobRequest {
    pub task: String,
    pub output: String,
    pub amount_sats: u64,
    /// Targeted seller hex pubkey. Required unless `untargeted` is true.
    pub seller_pubkey: Option<String>,
    /// When true, omit the p-tag (open offer). Documented default is targeted.
    pub untargeted: bool,
    pub deadline_unix: Option<u64>,
    /// Optional git delivery bind tags on the offer (repo + branch).
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// Job class, explicit at the type level. `FromScratch` emits no contribution tags
    /// (byte-identical to a non-contribution offer); `Contribution` carries the required target +
    /// base pins. See [`JobKind`].
    pub job: JobKind,
    /// Ask for a specific agent harness (`claude`, `codex`, …). `None` or `"any"` ⇒ no
    /// preference, and the offer is byte-identical to one posted before harness selection existed.
    /// A requested harness narrows the market: only sellers advertising it may be awarded.
    pub requested_agent: Option<String>,
    /// Ask for a harness FAMILY (#897). `None` ⇒ no preference. Enforced as a hard award filter on
    /// both award paths, so only a seller advertising that family may be awarded.
    pub requested_harness_family: Option<String>,
    /// Ask for a MODEL (#897). `None` ⇒ no preference. Enforced as a hard award filter on both award
    /// paths, matched against the family/model PAIR a seat advertises.
    ///
    /// ⚠ Requires `requested_harness_family`: a model with no family refuses every claim (#788), so
    /// posting one alone stops awards rather than narrowing them.
    pub requested_model: Option<String>,
    /// Capability tokens the job REQUIRES (#897). Empty ⇒ no requirement, and the offer is
    /// byte-identical to one posted before capability requests existed. Every token must be in
    /// [`crate::capability::CAPABILITIES`]; the posting path refuses the request otherwise, before
    /// any event is signed.
    pub required_capabilities: Vec<String>,
}

/// Job class of a posted offer. Making this an enum (rather than an all-or-nothing cluster of
/// `Option`s) makes a partial contribution spec unrepresentable: the flat MCP tool args are
/// validated into this at the tool layer, so the core never sees a half-specified contribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// No contribution pins — a plain offer, byte-identical to a non-contribution offer.
    FromScratch,
    /// A `job-class=contribution` offer carrying the required target + base pins.
    Contribution(ContributionSpec),
}

/// Required contribution-offer pins. The four target/base fields are REQUIRED (a partial set is
/// unrepresentable); `accepts` is optional and defaults to `["fork"]` — fork is the only supported
/// delivery, so a supplied `accepts` MUST include `"fork"`. The owner/branch/oid formats and the
/// clone-URL transport allowlist are validated by [`contribution_offer_from_spec`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContributionSpec {
    pub target_repo_owner: String,
    pub target_repo_url: String,
    pub base_branch: String,
    pub base_oid: String,
    pub accepts: Option<Vec<String>>,
}

/// Outcome of a successful `post_job`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PostJobOutcome {
    pub job_id: String,
    pub job_hash: String,
    pub offer_kind: u16,
    pub targeted: bool,
    pub seller_pubkey: Option<String>,
    pub amount_sats: u64,
    pub relay_url: String,
    pub task: String,
    pub output: String,
}

/// Inputs for reading job state from the relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetJobRequest {
    pub job_id: String,
    /// Optional long-poll: `claim` or `result`. Preference — not required for freeze.
    pub wait_for: Option<WaitFor>,
    pub timeout_secs: Option<u64>,
    /// Opt in to cosmetic kind-0 display-name enrichment. Disabled by default at tool/RPC edges.
    pub include_display_names: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitFor {
    Claim,
    Result,
}

/// Whether a fetched view satisfies the caller's `wait_for` condition.
///
/// The ONE definition of "ready", shared by every waiter. The buyer daemon waits by subscription
/// and the CLI waits by poll — the mechanisms differ deliberately, but if the two ever disagreed
/// about what they were waiting FOR, the same job would read complete to one and pending to the
/// other. Extracting the predicate rather than the loop is what makes that disagreement impossible
/// to write.
pub fn view_is_ready(view: &JobView, wait_for: WaitFor) -> bool {
    match wait_for {
        WaitFor::Claim => view.live_claim_id.is_some(),
        WaitFor::Result => !view.results.is_empty(),
    }
}

impl WaitFor {
    pub fn parse(raw: &str) -> Result<Self, JobLifecycleError> {
        match raw {
            "claim" => Ok(Self::Claim),
            "result" => Ok(Self::Result),
            other => Err(JobLifecycleError::Input(format!(
                "wait_for must be claim|result, got {other:?}"
            ))),
        }
    }
}

/// Relay-truth view of a job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobView {
    pub job_id: String,
    pub offer: Option<OfferView>,
    pub claims: Vec<ClaimView>,
    pub results: Vec<ResultView>,
    pub live_claim_id: Option<String>,
    pub accepted: Option<AcceptedBind>,
    /// True when `wait_for` was set and the wait cap hit before the condition —
    /// buyer should re-poll (PENDING), not treat as failure.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    /// Whether the relay ANSWERED the read this view was built from.
    ///
    /// `fetch_events` resolves `Ok(empty)` on timeout, so an empty view cannot by itself tell
    /// "the relay holds nothing for this job" from "we stopped waiting". Those are the same bytes
    /// and the discriminator is one layer out, so it has to be asked for: when every filter comes
    /// back empty we send one trivial `limit(0)` REQ and wait for the `EOSE` the relay OWES us.
    /// Served ⇒ the emptiness is a fact about the relay. Unserved ⇒ it is a fact about our patience.
    ///
    /// ⚠ Any caller about to take an IRREVERSIBLE action on absence must check this first. It is
    /// `false` on every view not built by a confirmed read, so the unsafe direction is the one you
    /// have to opt into.
    pub read_confirmed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OfferView {
    pub event_id: String,
    pub created_at: u64,
    pub author_pubkey: String,
    /// Cosmetic kind-0 `name` for `author_pubkey` (untrusted; never replaces hex).
    pub author_display_name: Option<String>,
    pub task: String,
    pub output: String,
    pub amount_sats: u64,
    pub deadline_unix: u64,
    pub seller_pubkey: Option<String>,
    /// Cosmetic kind-0 `name` for targeted `seller_pubkey` (untrusted; never replaces hex).
    pub seller_display_name: Option<String>,
    pub targeted: bool,
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// Raw `job-class` tag value. `Some("contribution")` ⇒ a contribution offer; absent
    /// ⇒ from-scratch. Carried raw so a `contribution`-class offer whose pins failed to parse is
    /// visible as `job_class=Some, contribution=None` and REFUSED at accept (never run from-scratch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_class: Option<String>,
    /// Parsed, well-formed contribution pins (target + base + accepts). `None` when not a
    /// contribution OR when a `contribution`-class offer's pins were malformed (fail-closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<ContributionOfferView>,
    /// The harness this job requested (`["param", "agent", …]`), canonicalised. `None` ⇒ any.
    /// The award filter reads it from HERE — the signed offer on the relay — never from award
    /// parameters, so the request cannot be changed after the fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_agent: Option<String>,
    /// The harness FAMILY this job requested (`["param", "harness_family", …]`), #897. `None` ⇒ any.
    /// Read from the signed offer for the same reason `requested_agent` is: the request a buyer is
    /// held to must be the one it published, not one supplied at award time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_harness_family: Option<String>,
    /// The model this job requested (`["param", "harness_model", …]`), #897. `None` ⇒ any.
    ///
    /// ⚠ Only meaningful PAIRED with `requested_harness_family`: a model with no family is REFUSED
    /// rather than ignored (#788), so it stops awards instead of narrowing them. And it matches a
    /// seat's LAST-OBSERVED self-report, so it narrows who is considered without pinning what runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    /// Capability tokens this job requires (`["param", "capability", …]`), #897. Empty ⇒ none, and
    /// every claim passes this filter unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
}

/// Serializable view of a well-formed contribution offer's pins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContributionOfferView {
    pub target_owner_pubkey: String,
    pub target_clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
    pub accepts: Vec<String>,
}

/// Serializable view of a seller result's contribution echo + authorship signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContributionResultView {
    pub target_owner_pubkey: String,
    pub target_clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
    /// Seller schnorr signature (hex) over the signed-result authorship tuple (`sig/seller-contribution`).
    pub tuple_signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClaimView {
    pub claim_id: String,
    pub created_at: u64,
    pub seller_pubkey: String,
    /// Cosmetic kind-0 `name` for this claim's `seller_pubkey` (untrusted).
    pub display_name: Option<String>,
    pub status: String,
    pub live: bool,
    /// The seller-authored NUT-18 payment request (`creqA…`) string read from the
    /// claim's `["creq", …]` tag, when present. `None` for a claim that carries none — the
    /// accept-bind then records no `creq_hash` and binding behaves identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creq: Option<String>,
    /// Harnesses this seller advertised on the claim (`["agents", …]`), preference order.
    /// Empty ⇒ the claim stated none, which never satisfies a job that asked for one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// The #784 capability this seller advertised ON THIS CLAIM, read through the one
    /// [`crate::heartbeat::SeatCapability::from_tags`] path.
    ///
    /// ⚠ The award filter decides on the CLAIM and never reads the seat announcement, so this is
    /// the ONLY place a capability filter can get its facts. A claim carries filterable fields only;
    /// the display fields parse to `None` and are never filtered on.
    ///
    /// All-default ⇒ the claim stated nothing — which is NOT the same as stating a non-matching
    /// value, even though both refuse an award. Any test over this field has to separate the two,
    /// or it stays green against a parser that reads nothing at all.
    #[serde(default, skip_serializing_if = "crate::heartbeat::SeatCapability::is_unstated")]
    pub capability: crate::heartbeat::SeatCapability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResultView {
    pub result_id: String,
    pub created_at: u64,
    pub seller_pubkey: String,
    /// Cosmetic kind-0 `name` for this result's `seller_pubkey` (untrusted).
    pub display_name: Option<String>,
    pub job_hash: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub commit_oid: Option<String>,
    pub amount_sats: Option<u64>,
    /// Seller schnorr signature (hex) from the result's `["sig","seller",..]` tag — the
    /// buyer counter-signs the same receipt preimage to co-sign the kind-3400.
    pub seller_signature: Option<String>,
    /// Harness the seller claims RAN this result, read from the result's seller-claimed
    /// exec-metadata `["harness", …]` tag (`metadata_trust=seller-claimed`). An attribution of
    /// what executed — never the buyer's requested harness echoed back (a request is not an
    /// attribution). `None` when the result carries no metadata block (pre-metadata sellers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Model the harness self-reported for the run (`["model", …]`), driver-surfaced only.
    /// Same trust class as `harness`: seller-claimed, absent-stays-absent (#233: a model string
    /// is a claim, never a verification).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Contribution echo + authorship signature. `Some` iff the result carries a
    /// well-formed `job-class=contribution` echo AND a `sig/seller-contribution` tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<ContributionResultView>,
}

/// Local accept-bind recorded by [`accept_claim`] for authorize_pay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedBind {
    pub job_id: String,
    pub claim_id: String,
    pub result_id: String,
    pub seller_pubkey: String,
    pub commit_oid: String,
    pub repo: String,
    pub branch: String,
    pub job_hash: String,
    pub amount_sats: u64,
    pub accept_event_id: String,
    pub accepted_at: u64,
    /// Seller schnorr signature (hex) over the receipt preimage, captured from the
    /// accepted result's `sig/seller` tag. Accept refuses a missing/empty sig so it never
    /// occupies the single-settlement slot (issue #93); empty only appears on legacy binds.
    #[serde(default)]
    pub seller_signature: String,
    /// SHA-256 hex of the accepted claim's seller-authored `creq` (`creqA…`) string,
    /// recorded at accept so authorize_pay binds the attempt id + receipt preimage to it. `None`
    /// for a claim that carries no `creq` — binding then behaves byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creq_hash: Option<String>,
    /// The accepted claim's `creq` accepted-mint list (`m`), recorded at accept so
    /// the buyer pay path chooses the realized mint from it. Empty for a claim with no `creq`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_mints: Vec<String>,
    /// The buyer's FUNDING (source) mint for this job — the mint whose proofs are spent — SELECTED and
    /// frozen at accept from the buyer's then-configured default (or a pre-funded cross-mint balance),
    /// validated against `accepted_mints` + the real-mint fence. The pay path derives the paying mint
    /// from THIS on every attempt — including retries — so a config-default change between attempts can
    /// never shift the mint and mint a second [`crate::payment::AttemptId`] (double-pay). On a
    /// cross-mint hop this is the SOURCE the buyer melts, NOT the mint the seller is paid in (that is
    /// `delivery_mint`). `None` only on a legacy bind serialized before this field existed; the pay
    /// path then falls back to the live config default (legacy behavior). Serialized as `funding_mint`;
    /// the `realized_mint` alias keeps binds written before the #495 rename readable — they carry the
    /// same funding value under the old (misleading) name.
    #[serde(default, alias = "realized_mint", skip_serializing_if = "Option::is_none")]
    pub funding_mint: Option<String>,
    /// The DELIVERY (realized) mint — the mint the seller is actually paid in — recorded at accept for
    /// reporting (#495). On a direct payment this equals `funding_mint`; on a cross-mint hop it is the
    /// hop TARGET (an entry of `accepted_mints`), which differs from the funding source. Advisory
    /// record only: the pay path re-derives the realized mint from `funding_mint` + `accepted_mints`
    /// and never reads this field, so it gates nothing and can never shift a spend. `None` on a legacy
    /// bind serialized before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_mint: Option<String>,
    /// Harness the seller claimed RAN the accepted result (its exec-metadata `["harness", …]`
    /// tag), captured at accept so settlement can attribute the payment to the worker that earned
    /// it (#261). Truth-only: this is what the seller says EXECUTED — never the buyer's requested
    /// harness written upfront (a request is not an attribution). `None` when the result carried
    /// no metadata (legacy sellers). Advisory record only — NOT in the receipt preimage, gates
    /// nothing on the pay path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_used: Option<String>,
    /// Model the harness self-reported for the run (`["model", …]`). Same trust class as
    /// `agent_used`: seller-claimed attribution, absent when the driver surfaced none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
    /// Contribution binds, recorded at accept when the OFFER is a contribution (authority
    /// = the buyer's signed offer; the result echo is equality-checked, never trusted). Absent ⇒
    /// from-scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<AcceptedContribution>,
}

/// Contribution binds captured in the accept-bind. `target_*` / `base_*` come from the
/// buyer's SIGNED offer (authority); `tuple_signature` is the seller's signed-result authorship sig
/// from the accepted result; `store_ref` is the buyer-controlled ref the fork tip is
/// retained under (buyer-store retention; merge uses THIS, never the live fork branch).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedContribution {
    pub target_owner_pubkey: String,
    pub target_clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
    pub tuple_signature: String,
    pub store_ref: String,
}

/// Inputs for accepting a seller claim (and binding the matching result).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptClaimRequest {
    pub job_id: String,
    pub claim_id: String,
    /// Optional explicit result id; otherwise the newest git result from the claim seller.
    pub result_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AcceptClaimOutcome {
    pub accept_event_id: String,
    pub bind: AcceptedBind,
}

pub struct AwardClaimRequest {
    pub job_id: String,
    pub claim_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AwardClaimOutcome {
    pub award_event_id: String,
    pub job_id: String,
    pub claim_id: String,
    pub seller_pubkey: String,
    /// The accepted mints the awarded claim's `creq` quotes. Surfaced so the buyer sees which mints
    /// the award commits it to paying at — awarding a claim it cannot settle is visible here, not a
    /// surprise at pay time. Empty when the claim carried no parseable `creq`.
    pub quoted_mints: Vec<String>,
}

/// An award validated and SIGNED but not yet sent (#322). `event_json` is the signed kind-3405
/// verbatim; its `award_event_id` is the content hash, fixed the moment this struct exists. The
/// caller persists it before the first send ([`crate::buyer::store::BuyerStore::begin_award_attempt`])
/// and every send — first or retry — transmits these bytes unmodified, so no publish ambiguity can
/// ever mint a second award for the job. Signing happens here (not at send time) precisely because
/// a re-signed draft gets a fresh `created_at` and therefore a fresh id: that near-identical
/// second event is how #322's three seats all came to execute one offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedAward {
    pub job_id: String,
    pub claim_id: String,
    pub seller_pubkey: String,
    pub award_event_id: String,
    pub event_json: String,
    pub quoted_mints: Vec<String>,
    /// The offer's deadline at prepare time. Past it a still-unresolved attempt is settled by
    /// probe, never by re-send — re-sending would knowingly inject a late award.
    pub offer_deadline_unix: i64,
    /// The relay these bytes are for, frozen from config now: every send and every presence
    /// probe of this award targets THIS url, so a config change mid-attempt cannot make the
    /// resolution interrogate a relay the bytes never went to.
    pub relay_url: String,
}

/// The relay's verdict on one transmission of a signed event. The three-way split is the point:
/// only an explicit `OK` moves money state, in either direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// The relay acked the event (`OK:true`, or `OK:false duplicate:` — it already holds it).
    /// This is the relay's word that it accepted the event; durability past that word is the
    /// relay's business, and nothing stronger exists on the wire.
    Acked,
    /// The relay explicitly rejected the event with a DELIBERATE, understood refusal
    /// (`blocked:`/`invalid:`/`pow:`/`restricted:`/`unsupported:`). It examined the event and
    /// refused storage: nothing from THIS transmission is public. Whether that licenses
    /// releasing funds is the caller's question — it does only when this was the event's FIRST
    /// transmission ever (see the attempt row's `send_count`).
    Refused { detail: String },
    /// Everything else — transport error, timeout waiting for the OK, connection lost mid-send,
    /// `rate-limited:`, `auth-required:`, `error:`, or words we don't understand. The event MAY
    /// be public (a lost OK after a successful store is indistinguishable from a lost send), so
    /// the caller must hold state and retry the same bytes, never conclude "nothing landed"
    /// (#322).
    Unresolved { detail: String },
}

#[derive(Debug)]
pub enum JobLifecycleError {
    Input(String),
    Home(HomeError),
    Relay(String),
    NotFound(String),
    Targeting(String),
    Io(String),
}

impl fmt::Display for JobLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "job lifecycle input: {message}"),
            Self::Home(error) => write!(formatter, "{error}"),
            Self::Relay(message) => write!(formatter, "job lifecycle relay: {message}"),
            Self::NotFound(message) => write!(formatter, "job lifecycle not found: {message}"),
            Self::Targeting(message) => write!(formatter, "job lifecycle targeting: {message}"),
            Self::Io(message) => write!(formatter, "job lifecycle io: {message}"),
        }
    }
}

impl std::error::Error for JobLifecycleError {}

impl From<HomeError> for JobLifecycleError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}

/// Publish a offer-kind offer to the configured relay. Returns the offer event id as `job_id`.
/// Sync entry for CLI/tests — nested call fails fast; MCP uses [`post_job_async`].
pub fn post_job(home: &MaxplayerHome, request: PostJobRequest) -> Result<PostJobOutcome, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("post_job")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(post_job_async(home, request))
}

/// Async `post_job` for callers already on a Tokio runtime (MCP dispatch).
/// Avoids nested `block_on` when publishing the offer over the relay.
pub async fn post_job_async(
    home: &MaxplayerHome,
    request: PostJobRequest,
) -> Result<PostJobOutcome, JobLifecycleError> {
    if request.task.trim().is_empty() {
        return Err(JobLifecycleError::Input("task must be non-empty".into()));
    }
    if request.output.trim().is_empty() {
        return Err(JobLifecycleError::Input("output must be non-empty".into()));
    }
    if request.untargeted && request.seller_pubkey.is_some() {
        return Err(JobLifecycleError::Input(
            "untargeted=true cannot also set seller_pubkey".into(),
        ));
    }
    if !request.untargeted && request.seller_pubkey.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true)
    {
        return Err(JobLifecycleError::Input(
            "post_job requires seller_pubkey (targeted default) or untargeted=true".into(),
        ));
    }
    match (&request.repo, &request.branch) {
        (Some(_), None) | (None, Some(_)) => {
            return Err(JobLifecycleError::Input(
                "repo and branch must be supplied together".into(),
            ));
        }
        _ => {}
    }
    // Validate the contribution spec up front (fail-closed, before the wallet opens). From-scratch
    // ⇒ `None` (no additive tags). Emission happens in `build_offer_draft`.
    let contribution = match &request.job {
        JobKind::FromScratch => None,
        JobKind::Contribution(spec) => Some(contribution_offer_from_spec(spec)?),
    };
    let deadline_unix = resolve_post_deadline(request.deadline_unix, now_unix_secs()?)?;

    // Refuse a post whose amount exceeds the per-job budget cap AT POST — a job you
    // can post but can never pay (authorize_pay refuses at the SAME cap) is a UX trap. Read the
    // cap from the SAME config the budget gate uses (`home.config.per_job_budget_sats`).
    assert_amount_within_budget_cap(request.amount_sats, home.config.per_job_budget_sats)?;

    // Dust guard: live keyset N=1 floor, fail-closed (no hardcoded fee=1). Bounded
    // and mint-unreachable-aware: a dead mint degrades to the cached
    // keyset fee floor, and only refuses (fast, `mint_unreachable`) when no fee can
    // be read at all — posting needs no funds, so it must not hang on a dead mint.
    #[cfg(feature = "wallet")]
    {
        let wallet = buyer_fund::open_wallet_async(home)
            .await
            .map_err(|error| JobLifecycleError::Input(error.to_string()))?;
        payment_wallet::require_fee_safe_amount_for_post(
            &wallet,
            cashu::Amount::from(request.amount_sats),
        )
        .await
        .map_err(|error| JobLifecycleError::Input(error.to_string()))?;
    }

    let draft = build_offer_draft(&request, deadline_unix, contribution.as_ref())?;

    let keys = buyer_keys(home)?;
    let event_id = publish_draft_async(home, &keys, &draft).await?;
    let job_hash = job_hash_for_offer(&event_id, &request.task, request.amount_sats);

    Ok(PostJobOutcome {
        job_id: event_id,
        job_hash,
        offer_kind: JOB_OFFER_KIND,
        targeted: !request.untargeted,
        seller_pubkey: request.seller_pubkey,
        amount_sats: request.amount_sats,
        relay_url: home.config.relay_url.clone(),
        task: request.task,
        output: request.output,
    })
}

fn now_unix_secs() -> Result<u64, JobLifecycleError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| JobLifecycleError::Input(format!("current unix time unavailable: {error}")))
}

fn resolve_post_deadline(
    deadline_unix: Option<u64>,
    now_unix: u64,
) -> Result<u64, JobLifecycleError> {
    match deadline_unix {
        Some(given) if given <= now_unix => Err(JobLifecycleError::Input(format!(
            "post_job refused: deadline_unix must be greater than current unix time; given={given}, current={now_unix}"
        ))),
        Some(given) => Ok(given),
        None => Ok(now_unix.saturating_add(DEFAULT_DEADLINE_SECS)),
    }
}

/// Refuse a post whose `amount_sats` exceeds the per-job budget cap. Mirrors the
/// budget gate's refuse condition (`amount > per_job_cap`; see [`crate::budget`]) at POST time so a
/// buyer never posts a job that can never be paid. At-cap and under-cap pass unchanged. The message
/// NAMES the config key + both numbers + the remedy; it never auto-raises — the cap is a safety
/// control.
fn assert_amount_within_budget_cap(
    amount_sats: u64,
    per_job_cap: u64,
) -> Result<(), JobLifecycleError> {
    if amount_sats > per_job_cap {
        return Err(JobLifecycleError::Input(format!(
            "post_job refused: amount {amount_sats} sat exceeds the per-job budget cap \
             {per_job_cap} sat (config key `per_job_budget_sats`). A job posted over the cap can \
             never be paid — authorize_pay refuses at the same cap. Raise `per_job_budget_sats` in \
             config.toml and RESTART the process (config is read at startup); the cap is a safety \
             control and is never auto-raised."
        )));
    }
    Ok(())
}

/// Build the offer-kind offer event draft. The optional git-delivery tags **and** the
/// contribution tags are emitted HERE so the post path and its round-trip test share ONE
/// tag-emission seam (pure — no publish, no wallet). `contribution` is the pre-validated canonical
/// offer from [`contribution_offer_from_spec`]; `None` ⇒ from-scratch, so NO additive
/// contribution tags are emitted (byte-identical to a non-contribution offer).
fn build_offer_draft(
    request: &PostJobRequest,
    deadline_unix: u64,
    contribution: Option<&crate::contribution::ContributionOffer>,
) -> Result<EventDraft, JobLifecycleError> {
    let offer = if request.untargeted {
        OfferDraft::untargeted(
            request.task.clone(),
            request.output.clone(),
            request.amount_sats,
            deadline_unix,
        )
    } else {
        OfferDraft::new(
            request.task.clone(),
            request.output.clone(),
            request.amount_sats,
            deadline_unix,
            request.seller_pubkey.clone().ok_or_else(|| {
                JobLifecycleError::Input(
                    "post_job requires seller_pubkey (targeted default) or untargeted=true".into(),
                )
            })?,
        )
    }
    .requesting_agent(request.requested_agent.as_deref())
    .requiring_capability(
        request.requested_harness_family.as_deref(),
        request.requested_model.as_deref(),
        &request.required_capabilities,
    );

    // #897 — TWO GATES, on the NORMALIZED request rather than the raw one. Validating what was handed
    // in would refuse a padded `" rust "` for a defect the builder just fixed, and would pass a value
    // the builder had dropped. What reaches the wire is what must be judged.
    //
    // WHY GATE AT POST AT ALL: posting commits. `post_job` arms the auto-award and puts a signed offer
    // on the relay with its deadline running, so a request nothing can satisfy converts a caller's
    // mistake into a committed offer plus a guaranteed park. Post time is the cheapest moment the
    // mistake can surface, and refusing here costs the caller nothing.
    //
    // The division of labour matters and is not a duplication:
    //   · The VOCABULARY gate knows the closed lists — a family or token no seat can ever advertise.
    //     That is a fact about the vocabularies, not about matching.
    //   · The SATISFIABILITY gate owns nothing: it asks the AWARD PREDICATE whether the perfect claim
    //     would pass. The predicate keeps sole ownership of matching semantics; this only surfaces a
    //     consequence of them earlier.
    //
    // Neither is the enforcement boundary — a foreign client can publish either shape straight to the
    // relay, where the award-time refusal is what holds. These make our own surface fail fast.
    crate::capability::validate_capability_request(
        offer.requested_harness_family.as_deref(),
        &offer.required_capabilities,
    )
    .map_err(|defect| JobLifecycleError::Input(format!("post_job refused: {defect}")))?;
    if let Some(refusal) = crate::buyer::lifecycle::unsatisfiable_capability_request(
        offer.requested_agent.as_deref(),
        offer.requested_harness_family.as_deref(),
        offer.requested_model.as_deref(),
        &offer.required_capabilities,
    ) {
        return Err(JobLifecycleError::Input(format!(
            "post_job refused: no seat could ever satisfy this request — {refusal}"
        )));
    }

    let mut draft = offer.to_event_draft();
    if let (Some(repo), Some(branch)) = (&request.repo, &request.branch) {
        draft.tags.push(TagSpec::new(["delivery", "git"]));
        draft.tags.push(TagSpec::new(["repo", repo]));
        draft.tags.push(TagSpec::new(["branch", branch]));
    }
    // Emit contribution tags via the CANONICAL constructor (never hand-rolled) — the buyer offer
    // and the seller echo therefore serialize the same shape.
    if let Some(contribution) = contribution {
        for tag in crate::contribution::contribution_offer_tags(contribution) {
            draft.tags.push(tag);
        }
    }
    Ok(draft)
}

/// Validate a [`ContributionSpec`] and build the canonical
/// [`ContributionOffer`](crate::contribution::ContributionOffer).
///
/// owner (64-hex) + branch/oid are validated by the canonical constructors
/// ([`TargetRepoPin`](crate::contribution::TargetRepoPin) /
/// [`ContributionBase`](crate::contribution::ContributionBase)), and the clone URL additionally
/// passes the SAME transport allowlist the pay path fetches under — `ext::`/file/ssh are refused
/// at POST time so a buyer never publishes an offer nobody can safely verify. `accepts` defaults
/// to `["fork"]` and MUST include `"fork"` (fork is the only supported delivery).
fn contribution_offer_from_spec(
    spec: &ContributionSpec,
) -> Result<crate::contribution::ContributionOffer, JobLifecycleError> {
    use crate::contribution::{ContributionBase, ContributionOffer, TargetRepoPin, ACCEPTS_FORK};

    let owner = spec.target_repo_owner.trim().to_owned();
    let url = spec.target_repo_url.trim().to_owned();
    let branch = spec.base_branch.trim().to_owned();
    // Keep the supplied oid byte-for-byte so ContributionBase can enforce its canonical shape.
    // Trimming here would let a non-exact value through the POST-time validation gate.
    let oid = spec.base_oid.clone();
    let accepts: Vec<String> = spec
        .accepts
        .as_ref()
        .map(|values| {
            values
                .iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // Transport allowlist at POST time (https + relay-git only; ext::/file/ssh/local refused) —
    // don't let a buyer publish an offer nobody can safely verify. The pay-path verifier re-checks
    // under the SAME allowlist (defense in depth).
    crate::delivery_transport::assert_allowed_repo_locator(&url).map_err(|refusal| {
        JobLifecycleError::Input(format!("contribution target_repo_url refused: {refusal}"))
    })?;

    let target =
        TargetRepoPin::new(owner, url).map_err(|e| JobLifecycleError::Input(e.to_string()))?;
    let base =
        ContributionBase::new(branch, oid).map_err(|e| JobLifecycleError::Input(e.to_string()))?;
    let accepts = if accepts.is_empty() {
        vec![ACCEPTS_FORK.to_owned()]
    } else {
        accepts
    };
    if !accepts.iter().any(|a| a == ACCEPTS_FORK) {
        return Err(JobLifecycleError::Input(format!(
            "contribution accepts must include \"fork\" (fork is the only supported delivery); got {accepts:?}"
        )));
    }

    Ok(ContributionOffer {
        target,
        base,
        accepts,
    })
}

/// Read offer / claims / results from the relay. Local accept-bind is attached if present.
/// Sync entry for CLI/tests — nested call fails fast; MCP uses [`get_job_async`].
pub fn get_job(home: &MaxplayerHome, request: GetJobRequest) -> Result<JobView, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("get_job")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(get_job_async(home, request))
}

/// Async `get_job` for callers already on a Tokio runtime (MCP dispatch).
///
/// `wait_for` is capped at [`WAIT_FOR_CAP_SECS`]. Cap-hit with condition unmet returns
/// `pending: true` (re-poll) — never an error.
pub async fn get_job_async(
    home: &MaxplayerHome,
    request: GetJobRequest,
) -> Result<JobView, JobLifecycleError> {
    let keys = buyer_keys(home)?;
    let fetch_timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);

    let Some(wait_for) = request.wait_for else {
        let mut view =
            fetch_job_view_async(home, &keys, &request.job_id, fetch_timeout, now_unix()).await?;
        view.pending = false;
        maybe_attach_display_names_async(home, &mut view, request.include_display_names).await;
        return Ok(view);
    };

    let wait_cap_secs = request
        .timeout_secs
        .unwrap_or(WAIT_FOR_CAP_SECS)
        .min(WAIT_FOR_CAP_SECS);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_cap_secs);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let mut view =
                fetch_job_view_async(home, &keys, &request.job_id, fetch_timeout, now_unix())
                    .await?;
            view.pending = !view_is_ready(&view, wait_for);
            maybe_attach_display_names_async(home, &mut view, request.include_display_names).await;
            return Ok(view);
        }
        let this_fetch = fetch_timeout.min(remaining);
        let mut view =
            fetch_job_view_async(home, &keys, &request.job_id, this_fetch, now_unix()).await?;
        if view_is_ready(&view, wait_for) {
            view.pending = false;
            maybe_attach_display_names_async(home, &mut view, request.include_display_names).await;
            return Ok(view);
        }
        if tokio::time::Instant::now() >= deadline {
            view.pending = true;
            maybe_attach_display_names_async(home, &mut view, request.include_display_names).await;
            return Ok(view);
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// How long a subscription-driven wait will sit before re-reading the view anyway.
///
/// This is a BACKSTOP, not the mechanism. Events resolve the wait in milliseconds; this only bounds
/// how long a wait can be wrong if an event never arrives — a relay that CLOSED our subscription, a
/// fan-out we fell behind on, an event filtered away upstream. Without it, a single missed event
/// costs the caller its whole remaining deadline.
const SAFETY_RECHECK: Duration = Duration::from_secs(3);

/// `get_job` for a caller that holds a live subscription to this buyer's job events.
///
/// Same contract and same readiness rule as [`get_job_async`] — [`view_is_ready`] decides for both
/// — but it WAKES on event arrival instead of sleeping 400ms between full reconnect-and-refetch
/// cycles. The poll variant stays exactly as it is for callers with no persistent session (the CLI);
/// this is not a replacement, it is the daemon's path.
///
/// THE SUBSCRIPTION IS THE WAKE; THE FETCH IS THE TRUTH. An arriving event means only "something
/// changed for this job" — the view is then re-read from the relay. Assembling the view out of the
/// event stream would duplicate the assembly logic and force trusting a stream that may be partial;
/// this way a missed event costs a re-check, never a wrong answer.
///
/// The caller must have subscribed BEFORE calling: `events` is passed in already-live precisely so
/// the subscribe cannot land after the first fetch and lose an event in the gap.
pub async fn get_job_awaiting_events_async(
    home: &MaxplayerHome,
    request: GetJobRequest,
    mut events: tokio::sync::broadcast::Receiver<std::sync::Arc<nostr_sdk::Event>>,
) -> Result<JobView, JobLifecycleError> {
    let keys = buyer_keys(home)?;
    let fetch_timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);

    let Some(wait_for) = request.wait_for else {
        let mut view =
            fetch_job_view_async(home, &keys, &request.job_id, fetch_timeout, now_unix()).await?;
        view.pending = false;
        maybe_attach_display_names_async(home, &mut view, request.include_display_names).await;
        return Ok(view);
    };

    let wait_cap_secs = request
        .timeout_secs
        .unwrap_or(WAIT_FOR_CAP_SECS)
        .min(WAIT_FOR_CAP_SECS);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_cap_secs);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let this_fetch = fetch_timeout.min(remaining.max(Duration::from_millis(1)));
        let mut view =
            fetch_job_view_async(home, &keys, &request.job_id, this_fetch, now_unix()).await?;
        if view_is_ready(&view, wait_for) {
            view.pending = false;
            maybe_attach_display_names_async(home, &mut view, request.include_display_names).await;
            return Ok(view);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            view.pending = true;
            maybe_attach_display_names_async(home, &mut view, request.include_display_names).await;
            return Ok(view);
        }
        await_job_event(&mut events, &request.job_id, remaining.min(SAFETY_RECHECK)).await;
    }
}

/// Wait until an event referencing `job_id` arrives, the window elapses, or the fan-out reports we
/// fell behind. Returns in all three cases — the caller's next act is the same either way: re-read
/// the view.
///
/// `Lagged` is deliberately NOT an error. It says this receiver missed events, which means
/// something happened; returning here re-checks, whereas treating it as a failure would turn a busy
/// relay into a spurious timeout. `Closed` also returns rather than erroring: the session is gone,
/// and the caller's deadline plus its next fetch is what surfaces that honestly.
async fn await_job_event(
    events: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<nostr_sdk::Event>>,
    job_id: &str,
    window: Duration,
) {
    let _ = tokio::time::timeout(window, async {
        loop {
            match events.recv().await {
                Ok(event) if event_references_job(&event, job_id) => return,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
    .await;
}

/// True when `event` carries an `e` tag naming this job's offer — the tag every claim, result and
/// feedback roots itself on.
pub(crate) fn event_references_job(event: &nostr_sdk::Event, job_id: &str) -> bool {
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.first().map(String::as_str) == Some("e") && parts.get(1).map(String::as_str) == Some(job_id)
    })
}

/// Accept a live claim: persist the pay-bind, then publish the `accepted` ACCEPT (kind-3406).
/// Sync entry for CLI/tests — nested call fails fast; MCP uses [`accept_claim_async`].
pub fn accept_claim(
    home: &MaxplayerHome,
    request: AcceptClaimRequest,
) -> Result<AcceptClaimOutcome, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("accept_claim")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(accept_claim_async(home, request))
}

/// Validate and SIGN the buyer's kind-award AWARD selecting `claim_id` for `job_id` — without
/// sending it. The awarded seller executes; every other claimant releases its claim without
/// spending compute.
///
/// This is the pre-work counterpart to [`accept_claim_async`], which runs AFTER delivery to bind
/// payment to a verified result. The award carries no pay-bind — it only names the winning claim.
/// The claim must be present and still `processing`, and (for a targeted offer) authored by the
/// targeted seller; otherwise the award is refused.
///
/// Prepare and send are split (#322) so the signed bytes can be PERSISTED before the first
/// transmission: `award_with_reservation` pins the [`PreparedAward`] as a durable attempt, then
/// drives [`send_signed_award_async`] against it — first send and every retry alike — so a publish
/// whose `OK` never arrives is retried with the identical event instead of a re-selected claim
/// and a fresh id. A failure HERE (validation or signing) is provably wire-free: nothing signed
/// has been persisted or transmitted, so the caller may safely release and re-plan.
pub async fn prepare_award_async(
    home: &MaxplayerHome,
    request: AwardClaimRequest,
) -> Result<PreparedAward, JobLifecycleError> {
    let timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);
    let keys = buyer_keys(home)?;
    // Injected `now` derives claim liveness — a claim past the offer deadline surfaces as
    // non-processing and is refused below.
    let view = fetch_job_view_async(home, &keys, &request.job_id, timeout, now_unix()).await?;
    let offer = view
        .offer
        .as_ref()
        .ok_or_else(|| JobLifecycleError::NotFound(format!("offer {}", request.job_id)))?;
    let claim = view
        .claims
        .iter()
        .find(|claim| claim.claim_id == request.claim_id)
        .ok_or_else(|| JobLifecycleError::NotFound(format!("claim {}", request.claim_id)))?;
    if claim.status != "processing" {
        return Err(JobLifecycleError::Input(format!(
            "claim {} status is {}, expected processing",
            claim.claim_id, claim.status
        )));
    }
    if let Some(target) = &offer.seller_pubkey {
        if target != &claim.seller_pubkey {
            return Err(JobLifecycleError::Targeting(format!(
                "offer targets seller {target}, claim seller is {}",
                claim.seller_pubkey
            )));
        }
    }

    // Surface the mints the claim's creq quotes so an incompatible award is visible before the
    // buyer commits (informational — the pay path's mint checks remain the settlement authority).
    let quoted_mints = claim
        .creq
        .as_deref()
        .and_then(|creq| crate::gateway::creq::parse_creq(creq).ok())
        .map(|request| request.mints.iter().map(|mint| mint.to_string()).collect())
        .unwrap_or_default();

    let buyer_pubkey = keys.public_key().to_hex();
    let draft = award_draft(
        &request.job_id,
        &request.claim_id,
        &buyer_pubkey,
        &claim.seller_pubkey,
    );
    // Sign NOW: from here the event id is fixed, and only these bytes may ever carry this job's
    // award. (`sign_with_keys` stamps `created_at`, so signing at send time would mint a new id
    // per retry — the exact duplication this function exists to prevent.)
    let event = gateway::nostr::event_builder(&draft)
        .map_err(|error| JobLifecycleError::Relay(format!("event builder: {error}")))?
        .sign_with_keys(&keys)
        .map_err(|error| JobLifecycleError::Relay(format!("sign award: {error}")))?;
    use nostr_sdk::JsonUtil;
    Ok(PreparedAward {
        award_event_id: event.id.to_hex(),
        event_json: event.as_json(),
        job_id: request.job_id,
        claim_id: request.claim_id,
        seller_pubkey: claim.seller_pubkey.clone(),
        quoted_mints,
        offer_deadline_unix: offer.deadline_unix as i64,
        relay_url: home.config.relay_url.clone(),
    })
}

/// How long one send waits for the relay's verdict before reporting [`SendOutcome::Unresolved`].
/// The SDK's real write-path worst case is WAIT_FOR_OK(10s) + WAIT_FOR_AUTHENTICATION(7s) +
/// WAIT_FOR_OK(10s) = 27s when the relay NIP-42-gates writes and the event is resent after auth —
/// the same arithmetic [`crate::buyer::relay`] documents for its own `PUBLISH_TIMEOUT` (45s).
/// Matching it here keeps a slow auth round-trip from reading as an eternal `Unresolved`.
const SEND_AWARD_TIMEOUT: Duration = Duration::from_secs(45);

/// How long to wait for the WebSocket to actually come up before sending / fetching —
/// `connect()` only SPAWNS the connection task. Mirrors [`crate::buyer::relay`]'s CONNECT_WAIT.
const RELAY_CONNECT_WAIT: Duration = Duration::from_secs(20);

/// Transmit a pinned, signed award — [`PreparedAward::event_json`] / a stored attempt's bytes —
/// to the relay the attempt was pinned for, and report the relay's verdict as a three-way
/// [`SendOutcome`].
///
/// Never errors: every failure mode is a verdict. In particular a transport failure or a lost
/// `OK` reports `Unresolved`, because the relay may hold (and be fanning out) the event even
/// though we never heard back — the seller executes off the relay's copy, not off our ack
/// (#322). Only an explicit `OK` moves anything: `true` (or `duplicate:`) → `Acked`, and a
/// deliberate refusal → `Refused`. `rate-limited:` / `auth-required:` / `error:` stay
/// `Unresolved` — verdicts about this transmission or session, not about the event.
///
/// `expected_event_id` re-derives nothing: the stored bytes are verified (id + signature) to BE
/// the pinned event before anything is transmitted, so confirm/record/probe — all keyed on the
/// pinned id — can never chase an event these bytes don't carry. A mismatch or verification
/// failure is local corruption and reports `Unresolved` (concluding "never landed" from local
/// damage would be #322 again); the by-id probe resolves it against the relay's copy.
pub async fn send_signed_award_async(
    keys: &nostr_sdk::Keys,
    relay_url: &str,
    expected_event_id: &str,
    event_json: &str,
) -> SendOutcome {
    use nostr_sdk::prelude::{Client, Event, JsonUtil};

    let event = match Event::from_json(event_json) {
        Ok(event) => event,
        Err(error) => {
            return SendOutcome::Unresolved {
                detail: format!("pinned award event does not parse ({error}); probe will resolve"),
            };
        }
    };
    if event.id.to_hex() != expected_event_id {
        return SendOutcome::Unresolved {
            detail: format!(
                "pinned bytes carry event {} but the attempt is keyed on {expected_event_id}; \
                 refusing to transmit them — probe will resolve",
                event.id.to_hex()
            ),
        };
    }
    if let Err(error) = event.verify() {
        return SendOutcome::Unresolved {
            detail: format!("pinned award event fails verification ({error}); probe will resolve"),
        };
    }
    let client = Client::new(keys.clone());
    // Explicit, not a default we hope for: the NIP-42 resend after auth fails SILENTLY when
    // auto-auth is off — the drift guard buyer/relay.rs pins for its own client.
    client.automatic_authentication(true);
    if let Err(error) = client.add_relay(relay_url).await {
        return SendOutcome::Unresolved { detail: format!("add relay: {error}") };
    }
    client.connect().await;
    let outcome = match client.relay(relay_url).await {
        Err(error) => SendOutcome::Unresolved { detail: format!("relay handle: {error}") },
        Ok(relay) => {
            // ONE wall-clock budget for connect + send: `connect()` only SPAWNS the connection
            // task, so the wait keeps the send from failing on a handshake race — but it must
            // live INSIDE the timeout, or the worst case is their sum (65s) held under the
            // caller's money lock rather than the stated 45s.
            let attempt = async {
                relay.wait_for_connection(RELAY_CONNECT_WAIT).await;
                relay.send_event(&event).await
            };
            match tokio::time::timeout(SEND_AWARD_TIMEOUT, attempt).await {
                Err(_) => SendOutcome::Unresolved {
                    detail: "timed out waiting for the relay's OK".to_owned(),
                },
                Ok(Ok(_)) => SendOutcome::Acked,
                Ok(Err(error)) => classify_send_error(error),
            }
        }
    };
    client.disconnect().await;
    outcome
}

/// Classify one send's typed failure into a [`SendOutcome`]. Only [`RelayMessage`] — the relay's
/// own explicit `OK:false` — can ever produce `Refused`; every other variant (timeout, transport,
/// not-connected, …) says nothing about whether the event landed and stays `Unresolved`.
///
/// [`RelayMessage`]: nostr_sdk::pool::relay::Error::RelayMessage
fn classify_send_error(error: nostr_sdk::pool::relay::Error) -> SendOutcome {
    match error {
        nostr_sdk::pool::relay::Error::RelayMessage(message) => classify_ok_false(&message),
        other => SendOutcome::Unresolved { detail: other.to_string() },
    }
}

/// Classify the relay's `OK:false` message by its NIP-01 machine-readable prefix. Pure, so the
/// mapping — the one place a relay's words become a money decision — is unit-testable.
///
/// - `duplicate:` → [`SendOutcome::Acked`]: the relay already HOLDS the event; that is a
///   confirmation wearing an error's clothes (and exactly what a successful retry looks like).
/// - `rate-limited:` / `auth-required:` → [`SendOutcome::Unresolved`]: verdicts about this
///   TRANSMISSION or this session, not about the event — the same bytes are expected to succeed
///   later. (These are also exactly the two CLOSED reasons the SDK itself treats as
///   non-removing; the write side mirrors that split.)
/// - `error:` → [`SendOutcome::Unresolved`]: the NIP-01 catch-all relays use for transient
///   backend/storage failures. Terminalizing it would release funds over a hiccup.
/// - an UNPREFIXED message → [`SendOutcome::Unresolved`]: words we do not understand never
///   release funds. A relay that refuses forever in nonstandard language keeps the attempt
///   pending until the pay window passes and the by-id probe terminalizes it honestly.
/// - `blocked:` / `invalid:` / `pow:` / `restricted:` / `unsupported:` →
///   [`SendOutcome::Refused`]: the relay examined the event and DELIBERATELY declined to store
///   it. Nothing from this transmission is public.
fn classify_ok_false(message: &str) -> SendOutcome {
    use nostr_sdk::prelude::MachineReadablePrefix;
    match MachineReadablePrefix::parse(message) {
        Some(MachineReadablePrefix::Duplicate) => SendOutcome::Acked,
        Some(
            MachineReadablePrefix::RateLimited
            | MachineReadablePrefix::AuthRequired
            | MachineReadablePrefix::Error,
        )
        | None => SendOutcome::Unresolved { detail: message.to_owned() },
        Some(_) => SendOutcome::Refused { detail: message.to_owned() },
    }
}

/// Seal the FUNDING (source) and DELIVERY (realized) mints for the accept-bind from ONE payment
/// plan (#495). Deriving both from the same plan is what keeps them consistent: `funding` is the
/// source the buyer melts (sealed for pay-path attempt-id stability), `delivery` is the mint the
/// seller is realized at (recorded for reporting). Equal on a direct payment; on a cross-mint hop
/// `funding` is the source and `delivery` is the target, so they differ — which is exactly the case
/// the old single `realized_mint` field mis-reported (it carried the source under a delivery name).
fn seal_bind_mints(plan: &crate::crossmint::PayPlan) -> (String, String) {
    (plan.source_mint().to_string(), plan.realized_mint().to_string())
}

/// Async `accept_claim` for callers already on a Tokio runtime (MCP dispatch).
pub async fn accept_claim_async(
    home: &MaxplayerHome,
    request: AcceptClaimRequest,
) -> Result<AcceptClaimOutcome, JobLifecycleError> {
    let timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);
    let keys = buyer_keys(home)?;
    // Injected `now` derives the claim status: past the offer deadline a claim reads `expired`
    // (refused below) UNLESS its seller delivered inside the pay window, in which case it reads
    // `delivered` and stays acceptable — the offer deadline is a scheduling clock and must not
    // strand a completed, gate-verifiable delivery. Every money gate still fires below.
    let view = fetch_job_view_async(home, &keys, &request.job_id, timeout, now_unix()).await?;
    let offer = view
        .offer
        .as_ref()
        .ok_or_else(|| JobLifecycleError::NotFound(format!("offer {}", request.job_id)))?;

    let claim = view
        .claims
        .iter()
        .find(|claim| claim.claim_id == request.claim_id)
        .ok_or_else(|| JobLifecycleError::NotFound(format!("claim {}", request.claim_id)))?;
    if claim.status != "processing" && claim.status != CLAIM_STATUS_DELIVERED {
        return Err(JobLifecycleError::Input(format!(
            "claim {} status is {}, expected processing or delivered",
            claim.claim_id, claim.status
        )));
    }

    if let Some(target) = &offer.seller_pubkey {
        if target != &claim.seller_pubkey {
            return Err(JobLifecycleError::Targeting(format!(
                "offer targets seller {target}, claim seller is {}",
                claim.seller_pubkey
            )));
        }
    }

    let result = select_result(&view.results, &claim.seller_pubkey, request.result_id.as_deref())?;

    // Finding W: hold a per-job advisory lock across the single-settlement check→durable-bind-write
    // so two concurrent accepts for DIFFERENT results of one job cannot both observe "no bind" and
    // both write (the unlocked TOCTOU that would let two distinct AttemptIds each become payable).
    // Same pattern as the budget flock (`budget.rs::acquire_lock`): blocks until any other accept on
    // THIS job releases. Held until the function returns (past the pending + finalized bind writes),
    // so the loser re-reads the winner's bind and refuses at `assert_single_settlement`.
    let _job_lock = acquire_job_lock(home, &request.job_id)?;

    // Issue #93: refuse a missing/empty seller co-signature BEFORE any durable bind write so an
    // incomplete result never occupies the single-settlement slot. A later result (or re-publish)
    // carrying a valid `sig/seller` can then bind. Checked before the single-settlement guard so
    // empty presentation is a pure no-op on the bind file.
    let seller_signature = require_seller_signature(&result.seller_signature)?;

    // Interim single-settlement guard (P): a job binds at most ONE result. If an accept-bind
    // already exists for this job pinned to a DIFFERENT result, refuse — a second/different
    // result_id must not mint a second buyer attempt/payment for one job. Re-accepting the SAME
    // result stays idempotent (the durability re-publish/finalize below rewrites the same bind).
    // TEMPORARY: the full job-scoped settlement-key refactor (which would let a job legitimately
    // re-bind a corrected result) is tracked separately; until then one settlement per job.
    assert_single_settlement(
        load_accepted_bind(home, &request.job_id)?.as_ref(),
        &request.job_id,
        &result.result_id,
    )?;

    let repo = result
        .repo
        .clone()
        .ok_or_else(|| JobLifecycleError::Input("result missing repo".into()))?;
    let branch = result
        .branch
        .clone()
        .ok_or_else(|| JobLifecycleError::Input("result missing branch".into()))?;
    let commit_oid = result
        .commit_oid
        .clone()
        .ok_or_else(|| JobLifecycleError::Input("result missing commit_oid".into()))?;
    // Bind the OFFER's amount (buyer-signed authority) — NEVER the seller-authored result
    // amount, which a malicious seller could inflate.
    let amount_sats = offer.amount_sats;
    // Recompute the canonical job-hash from the buyer's signed offer and REQUIRE the seller's
    // result to echo it exactly; never trust the result's self-authored job-hash. The seller
    // co-signs the receipt preimage over THIS hash, so an offer-derived hash that the result
    // does not match means the result quoted a different task/amount — refuse.
    let expected_job_hash = job_hash_for_offer(&request.job_id, &offer.task, offer.amount_sats);
    let result_job_hash = result
        .job_hash
        .clone()
        .ok_or_else(|| JobLifecycleError::Input("result missing job-hash".into()))?;
    if result_job_hash != expected_job_hash {
        return Err(JobLifecycleError::Input(format!(
            "result job-hash {result_job_hash} does not match offer-derived job-hash \
             {expected_job_hash} (seller quoted a different task/amount) — refused"
        )));
    }
    let job_hash = expected_job_hash;

    // Resolve contribution binds from the buyer's SIGNED OFFER (authority), refusing a
    // malformed contribution offer and equality-checking the seller echo — never trusting
    // the echo. From-scratch offers (no `job-class=contribution`) leave `contribution = None`.
    let contribution = resolve_accepted_contribution(offer, result, &commit_oid)?;

    // Fail-closed STRICT verification of the accepted claim's seller-authored creq: creq is
    // REQUIRED and its payment-terms (payment_id, amount, unit, mints) are verified field by
    // field before the bind is written (see [`verify_accepted_claim_creq`]).
    let accepted_mints =
        verify_accepted_claim_creq(claim.creq.as_deref(), &request.job_id, offer.amount_sats)?;

    // FREEZE the buyer's paying mint at accept from its then-configured default, validated by
    // planning the payment against the accepted set + the real-mint fence. Sealing the SELECTION
    // here — not just the accepted SET (finding V) — is what makes the pay-path attempt id stable
    // across retries: a config-default change after accept can no longer shift the mint into a
    // different attempt id and mint a second payment for one job (double-pay). A buyer with no
    // payable route to this claim is refused HERE (fail-closed), never accepted into an unpayable
    // bind.
    //
    // The seal is the buyer's own funded mint, which is also the mint a direct payment realizes at.
    // A payment that reaches the seller's mint by hopping realizes at the TARGET, but sealing that
    // would re-plan at pay time as a direct payment from a mint the buyer holds nothing at; the
    // target is re-derived there from this seal plus the accepted set frozen alongside it, so it
    // stays deterministic without being stored twice.
    // Prefer sourcing from a mint the buyer already holds a covering balance at (spends a
    // pre-funded cross-mint balance directly instead of hopping from the default and paying a melt
    // fee); fall back to the configured default. Balances are a local sqlite read (no network). The
    // CHOICE is sealed below and re-derived at pay, so it stays deterministic — a later balance or
    // config-default change can never shift a sealed mint (the pays-once attempt-id invariant).
    let source_seed = match crate::wallet_ops::balances_async(home).await {
        Ok(balances) => crate::crossmint::select_source_mint(
            home.config.default_mint(),
            &accepted_mints,
            home.config.allow_real_mints,
            &balances,
            offer.amount_sats,
        ),
        // Best-effort: a balance-read failure falls back to today's behavior (the default mint)
        // rather than blocking an otherwise-plannable payment. Logged, never silent.
        Err(error) => {
            crate::opline!("accept: mint balance read failed ({error}); sourcing from the default mint");
            home.config.default_mint().to_string()
        }
    };
    // Plan the payment ONCE and seal BOTH mints from that single decision (#495): the funding SOURCE
    // the pay path spends from (frozen for attempt-id stability), and the DELIVERY mint the seller is
    // realized at (reporting only — the pay path re-derives it and never reads the stored value). On a
    // direct payment the two are equal; on a hop the delivery mint is the target, not the source.
    let plan = crate::crossmint::plan_payment(
        &source_seed,
        &accepted_mints,
        home.config.allow_real_mints,
    )
    .map_err(|error| JobLifecycleError::Input(error.to_string()))?;
    let (funding_mint, delivery_mint) = seal_bind_mints(&plan);

    let buyer_pubkey = keys.public_key().to_hex();
    // ACCEPT is its own kind: this is the pay-bind, not the selection — `prepare_award_async` owns
    // the selection and signs `award_draft`. Distinct kinds are what let a reader tell the two
    // apart, because a count of same-kind events cannot: a repeat of one is shaped like the other.
    let draft = accept_draft(
        &request.job_id,
        &request.claim_id,
        &buyer_pubkey,
        &claim.seller_pubkey,
    );

    // Durability ordering: write the pay-bind BEFORE publishing the accept, so a crash between
    // publish and bind-write can never leave a PUBLIC accepted state on the relay with NO local
    // bind (which would strand the buyer — unable to pay, at risk of re-accepting). The pending
    // bind carries an empty `accept_event_id` (the pay path never reads that field — it is a
    // record only), then we publish and finalize the bind with the real id + timestamp.
    let mut bind = AcceptedBind {
        job_id: request.job_id.clone(),
        claim_id: request.claim_id.clone(),
        result_id: result.result_id.clone(),
        seller_pubkey: claim.seller_pubkey.clone(),
        commit_oid,
        repo,
        branch,
        job_hash,
        amount_sats,
        // Pending marker until the accept is published + finalized below.
        accept_event_id: String::new(),
        accepted_at: 0,
        // Capture non-empty sig/seller (required above) so authorize_pay can co-sign the receipt.
        seller_signature,
        // Hash the seller-authored creq from the accepted claim (when present) so the
        // pay path binds the attempt + receipt to the exact request the seller quoted. A claim
        // that carries no creq ⇒ `None` ⇒ binding behaves identically.
        creq_hash: claim.creq.as_deref().map(crate::gateway::creq_hash_hex),
        // The creq's accepted-mint list (validated + parsed above, fail-closed).
        accepted_mints,
        // The FUNDING (source) mint SELECTED for this job, frozen above. Sealing the choice makes the
        // pay-path attempt id stable across retries. On a hop this is the source, NOT the delivery mint.
        funding_mint: Some(funding_mint),
        // The DELIVERY (realized) mint the seller is paid in — equals the funding mint on a direct
        // payment, the hop target on a cross-mint hop. Recorded for reporting (#495); read by no
        // pay-path code.
        delivery_mint: Some(delivery_mint),
        // Attribution of the worker that produced THIS result (seller-claimed exec-metadata),
        // frozen with the bind so settlement records who earned the payment (#261).
        agent_used: result.harness.clone(),
        model_used: result.model.clone(),
        contribution,
    };
    write_accepted_bind(home, &bind)?;

    let accept_event_id = publish_draft_async(home, &keys, &draft).await?;
    let accepted_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Finalize: stamp the published id + timestamp and re-persist. A crash before this leaves the
    // pending bind on disk (accept may or may not be public); a subsequent accept re-publishes
    // idempotently and re-finalizes.
    bind.accept_event_id = accept_event_id.clone();
    bind.accepted_at = accepted_at;
    write_accepted_bind(home, &bind)?;

    Ok(AcceptClaimOutcome {
        accept_event_id,
        bind,
    })
}

/// Resolve + accept the delivered claim for a one-call collect on `job_id`, returning the recorded
/// pay-bind. This is the accept step [`collect`](crate::collect) runs ITSELF when no accept-bind
/// exists yet, so a buyer with an awarded job and a delivered result can collect once (no separate
/// accept_claim call). It fetches relay truth, resolves the buyer's own durable AWARD (kind-3405)
/// and accepts THAT awarded claim's delivery — against the award, never the live-claim set, so later
/// claim churn cannot strand it (#540) — and runs the SAME [`accept_claim_async`] path, publishing
/// the buyer accept and recording
/// the co-signed pay-bind (seller / result / commit / repo / branch / job-hash / creq_hash, all
/// accept-time money gates: single-settlement, creq verify, realized-mint freeze). It adds NO money
/// authority — it only moves WHERE the bind is created.
///
/// Fail-closed with no bind written when the award cannot be resolved (absent, unverified, or
/// unrepairable on the relay) or the awarded delivery is missing or past its pay window; each error
/// names the remedy (`accept_claim`, or wait for delivery).
pub async fn accept_for_collect_async(
    home: &MaxplayerHome,
    job_id: &str,
) -> Result<AcceptedBind, JobLifecycleError> {
    let timeout = Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS);
    let keys = buyer_keys(home)?;
    let view = fetch_job_view_async(home, &keys, job_id, timeout, now_unix()).await?;
    // Resolve the winner from the buyer's DURABLE AWARD (its own kind-3405), never the live-claim
    // set: the award is the payment decision, and later claim churn — a newer, non-awarded claim
    // becoming the single `live_claim_id` — must not strand a delivered, still-payable awarded claim
    // (#540). A relay-truth read (restart-safe, no local ledger); its Unverified / Absent /
    // Unrepairable cases each refuse fail-closed rather than guess a claim.
    let award = match award_presence_async(home, &keys, job_id, timeout).await? {
        PresenceRead::Present(AwardPresence::Repairable(award)) => award,
        PresenceRead::Present(AwardPresence::Unrepairable { detail, .. }) => {
            return Err(JobLifecycleError::Input(format!(
                "collect: the award for job {job_id} is not a single unambiguous claim ({detail}); \
                 resolve it with accept_claim before collecting"
            )));
        }
        PresenceRead::ConfirmedAbsent => {
            return Err(JobLifecycleError::NotFound(format!(
                "collect: no award on the relay for job {job_id} — award a seller and wait for \
                 delivery before collecting"
            )));
        }
        PresenceRead::Unverified => {
            return Err(JobLifecycleError::Relay(format!(
                "collect: could not confirm the award for job {job_id} (relay unverified) — retry"
            )));
        }
    };
    let claim_id = select_deliverable_claim(&view, &award)?;
    // Reuse accept_claim_async unchanged: it re-fetches relay truth and re-runs every accept-time
    // gate (targeting, single-settlement, job-hash echo, creq verify, realized-mint freeze) before
    // writing the bind. The extra fetch is the price of not duplicating the money machinery.
    let outcome = accept_claim_async(
        home,
        AcceptClaimRequest {
            job_id: job_id.to_owned(),
            claim_id,
            result_id: None,
        },
    )
    .await?;
    Ok(outcome.bind)
}

/// Select the claim to auto-accept for a one-call collect: the AWARDED claim (the buyer's own
/// kind-3405 established the winner — the payment decision), verifying its seller has delivered a
/// git result. Resolves against the durable AWARD passed in, NOT the exclusive live claim (#540):
/// once an award exists, later claim churn — a newer, non-awarded claim becoming the single
/// `live_claim_id` — is status-only and must never overturn it, so a delivered, still-payable
/// awarded claim stays collectable through that churn.
///
/// The pay WINDOW is retained: the awarded claim must be payable — `processing` or
/// [`CLAIM_STATUS_DELIVERED`], the SAME per-claim predicate [`derive_claim_liveness`] admits a live
/// claim by — so a [`CLAIM_STATUS_EXPIRED`] award (past its pay window) still never pays. Only the
/// EXCLUSIVE "is THE live claim" test is dropped. Relay truth only; never invents a claim.
/// Fail-closed and named on every refusal (award not on the relay, past the pay window, not yet
/// delivered), writing no bind.
fn select_deliverable_claim(
    view: &JobView,
    award: &RelayedAward,
) -> Result<String, JobLifecycleError> {
    let awarded = view
        .claims
        .iter()
        .find(|claim| claim.claim_id == award.claim_id)
        .ok_or_else(|| {
            JobLifecycleError::NotFound(format!(
                "collect: the awarded claim {} for job {} is not on the relay yet — cannot collect \
                 (retry once the claim is readable)",
                award.claim_id, view.job_id
            ))
        })?;
    if awarded.status != "processing" && awarded.status != CLAIM_STATUS_DELIVERED {
        return Err(JobLifecycleError::NotFound(format!(
            "collect: the awarded claim {} for job {} is {} (past its pay window) — the awarded \
             delivery is no longer collectable",
            award.claim_id, view.job_id, awarded.status
        )));
    }
    let delivered = view
        .results
        .iter()
        .any(|result| result.seller_pubkey == awarded.seller_pubkey && result.commit_oid.is_some());
    if !delivered {
        return Err(JobLifecycleError::NotFound(format!(
            "collect: the awarded seller for job {} has not delivered a git result yet — wait for \
             delivery (get_job wait_for=result) before collecting",
            view.job_id
        )));
    }
    Ok(award.claim_id.clone())
}

/// Whether the awarded `claim_id` and `seller_pubkey` are currently ready for this job's one
/// settlement: no result has already occupied the job-wide accept bind, the awarded claim is
/// present and still payable, its seller matches the stored award, and that seller has delivered.
/// Kept independent from [`select_deliverable_claim`] so its pinned named-refusal strings do not
/// change while `get_job` (#544) and its wire-contract test share one production predicate.
pub(crate) fn awarded_delivery_pending(
    view: &JobView,
    claim_id: &str,
    seller_pubkey: &str,
) -> bool {
    if view.accepted.is_some() {
        return false;
    }
    let Some(claim) = view.claims.iter().find(|c| c.claim_id == claim_id) else {
        return false;
    };
    if claim.seller_pubkey != seller_pubkey
        || (claim.status != "processing" && claim.status != CLAIM_STATUS_DELIVERED)
    {
        return false;
    }
    view.results
        .iter()
        .any(|result| result.seller_pubkey == seller_pubkey && result.commit_oid.is_some())
}

/// Accept-time contribution resolution. Authority is the buyer's SIGNED OFFER:
/// - not a contribution offer (`job_class != contribution`) ⇒ `Ok(None)` (from-scratch);
/// - a `contribution`-class offer whose pins failed to parse ⇒ REFUSE (fail-closed — never
///   silently run from-scratch);
/// - a contribution offer whose accepted result carries no valid echo+sig ⇒ REFUSE;
/// - the seller-echoed `{target_repo, base_oid}` are EQUALITY-CHECKED against the offer (a
///   cross-check input, never authority) — a mismatch REFUSES.
///
/// The recorded binds (`target_*`, `base_*`) come from the OFFER; the fork is the result's
/// repo/branch; `store_ref` is derived from the fork-tip `commit_oid`.
/// Fail-closed STRICT verification of the accepted claim's seller-authored NUT-18 `creq`,
/// mirroring the seller-side field-by-field creq rebind. The buyer must not accept-then-pay a claim whose payment
/// terms it did not fully verify, so before the accept-bind is written this requires:
///
/// - a creq is PRESENT (a claim carrying none is refused — every seller claim authors one);
/// - the creq PARSES as a NUT-18 payment request;
/// - `payment_id == job_id`;
/// - `amount == offer.amount_sats` (the buyer-signed offer amount, never the result's echo);
/// - `unit == sat`;
/// - the accepted-mint list (`m`) is NON-EMPTY.
///
/// Returns the normalized accepted-mint list (`m`) on success. Any failure REFUSES the accept.
fn verify_accepted_claim_creq(
    creq: Option<&str>,
    job_id: &str,
    offer_amount_sats: u64,
) -> Result<Vec<String>, JobLifecycleError> {
    let creq = creq.ok_or_else(|| {
        JobLifecycleError::Input(
            "accepted claim carries no creq (cannot verify payment terms) — refused".into(),
        )
    })?;
    let request = crate::gateway::creq::parse_creq(creq).map_err(|error| {
        JobLifecycleError::Input(format!(
            "accepted claim carries an unparseable creq (refusing accept, fail-closed): {error}"
        ))
    })?;
    if request.payment_id.as_deref() != Some(job_id) {
        return Err(JobLifecycleError::Input(format!(
            "accepted claim creq payment id {:?} != job_id {job_id} — refused",
            request.payment_id
        )));
    }
    if request.amount != Some(cashu::Amount::from(offer_amount_sats)) {
        return Err(JobLifecycleError::Input(format!(
            "accepted claim creq amount {:?} != offer amount {offer_amount_sats} — refused",
            request.amount
        )));
    }
    if request.unit.as_ref() != Some(&cashu::CurrencyUnit::Sat) {
        return Err(JobLifecycleError::Input(format!(
            "accepted claim creq unit {:?} != sat — refused",
            request.unit
        )));
    }
    if request.mints.is_empty() {
        return Err(JobLifecycleError::Input(
            "accepted claim creq carries no accepted mints (m) — refused".into(),
        ));
    }
    Ok(request.mints.iter().map(|mint| mint.to_string()).collect())
}

fn resolve_accepted_contribution(
    offer: &OfferView,
    result: &ResultView,
    commit_oid: &str,
) -> Result<Option<AcceptedContribution>, JobLifecycleError> {
    use crate::contribution::JOB_CLASS_CONTRIBUTION;
    let offer_contribution = match &offer.contribution {
        Some(c) => c,
        None => {
            // Fail-closed: a contribution-class offer whose pins didn't parse must NOT run as
            // from-scratch. Only a genuinely non-contribution offer resolves to None.
            if offer.job_class.as_deref() == Some(JOB_CLASS_CONTRIBUTION) {
                return Err(JobLifecycleError::Input(
                    "offer is job-class=contribution but its target-repo/base pins are malformed — \
                     refused (a malformed contribution offer is never run as from-scratch)"
                        .into(),
                ));
            }
            return Ok(None);
        }
    };
    let echo = result.contribution.as_ref().ok_or_else(|| {
        JobLifecycleError::Input(
            "contribution offer requires a contribution result (job-class echo + \
             sig/seller-contribution); the accepted result carries none — refused"
                .into(),
        )
    })?;
    // Equality-check: seller-echoed target/base MUST equal the buyer's signed offer.
    if echo.target_owner_pubkey != offer_contribution.target_owner_pubkey
        || echo.target_clone_url != offer_contribution.target_clone_url
    {
        return Err(JobLifecycleError::Targeting(format!(
            "contribution result echoes target-repo (owner {}, {}) but the signed offer pins \
             (owner {}, {}) — echo mismatch refused (base/target resolved from the PIN, never the echo)",
            echo.target_owner_pubkey,
            echo.target_clone_url,
            offer_contribution.target_owner_pubkey,
            offer_contribution.target_clone_url
        )));
    }
    if echo.base_branch != offer_contribution.base_branch
        || echo.base_oid != offer_contribution.base_oid
    {
        return Err(JobLifecycleError::Targeting(format!(
            "contribution result echoes base ({}, {}) but the signed offer pins ({}, {}) — echo \
             mismatch refused",
            echo.base_branch, echo.base_oid, offer_contribution.base_branch, offer_contribution.base_oid
        )));
    }
    Ok(Some(AcceptedContribution {
        // Authority = the OFFER (buyer-signed), never the echo.
        target_owner_pubkey: offer_contribution.target_owner_pubkey.clone(),
        target_clone_url: offer_contribution.target_clone_url.clone(),
        base_branch: offer_contribution.base_branch.clone(),
        base_oid: offer_contribution.base_oid.clone(),
        tuple_signature: echo.tuple_signature.clone(),
        store_ref: crate::delivery_git::PayPathDeliveryVerifier::store_ref_for(commit_oid),
    }))
}

/// Load the local accept-bind for a job, if any.
pub fn load_accepted_bind(
    home: &MaxplayerHome,
    job_id: &str,
) -> Result<Option<AcceptedBind>, JobLifecycleError> {
    let path = bind_path(home, job_id);
    if !path.is_file() {
        return Ok(None);
    }
    let mut file = File::open(&path).map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    let bind: AcceptedBind = serde_json::from_str(&raw)
        .map_err(|error| JobLifecycleError::Io(format!("accept bind parse: {error}")))?;
    Ok(Some(bind))
}

/// Refuse authorize_pay fields that disagree with a recorded accept-bind.
pub fn assert_authorize_matches_bind(
    bind: &AcceptedBind,
    seller_pubkey: &str,
    result_id: &str,
    commit_oid: &str,
) -> Result<(), JobLifecycleError> {
    if seller_pubkey != bind.seller_pubkey {
        return Err(JobLifecycleError::Targeting(format!(
            "authorize_pay seller_pubkey {} does not match accepted seller {}",
            seller_pubkey, bind.seller_pubkey
        )));
    }
    if result_id != bind.result_id {
        return Err(JobLifecycleError::Targeting(format!(
            "authorize_pay result_id {} does not match accepted result {}",
            result_id, bind.result_id
        )));
    }
    if commit_oid != bind.commit_oid {
        return Err(JobLifecycleError::Targeting(format!(
            "authorize_pay commit_oid {} does not match accepted commit {}",
            commit_oid, bind.commit_oid
        )));
    }
    Ok(())
}

/// Interim single-settlement guard (P): a job binds at most one result. An existing accept-bind
/// pinned to a DIFFERENT result refuses (a second/different result_id must not mint a second
/// buyer attempt/payment for one job); re-accepting the SAME result is idempotent (the durability
/// re-publish/finalize rewrites the same bind). No existing bind ⇒ allowed. Non-secret error.
fn assert_single_settlement(
    existing: Option<&AcceptedBind>,
    job_id: &str,
    new_result_id: &str,
) -> Result<(), JobLifecycleError> {
    if let Some(existing) = existing {
        if existing.result_id != new_result_id {
            return Err(JobLifecycleError::Input(format!(
                "job {job_id} already accepted result {}; refusing to bind a different result \
                 {new_result_id} (one settlement per job)",
                existing.result_id
            )));
        }
    }
    Ok(())
}

/// Require a non-empty seller co-signature (`sig/seller`) at accept-time (issue #93).
///
/// A missing or empty signature must NOT be recorded as `""` on the accept-bind: that would
/// permanently occupy the single-settlement slot and block a later valid sig for the same claim.
/// Refuse instead so the bind file is never written and the slot stays free.
fn require_seller_signature(
    seller_signature: &Option<String>,
) -> Result<String, JobLifecycleError> {
    match seller_signature {
        Some(sig) if !sig.trim().is_empty() => Ok(sig.clone()),
        _ => Err(JobLifecycleError::Input(
            "result missing non-empty seller signature (sig/seller); refusing to bind so the \
             single-settlement slot stays free for a later valid sig"
                .into(),
        )),
    }
}

/// Build an [`AuthorizePayRequest`](crate::authorize_pay::AuthorizePayRequest) from the
/// accept-bind + buyer-supplied tip-match.
///
/// Rules:
/// - `delivery_integrity_hash` is a **required** buyer arg (never defaulted/derived from
///   claim feedback or result oid).
/// - Compare it to the seller's advertised `commit_oid` and **refuse on mismatch**.
/// - Matching is fine when the buyer independently tip-matched the same oid; auto-fill
///   from the seller advertisement is the circular-bind failure mode.
pub fn authorize_request_from_bind(
    bind: &AcceptedBind,
    amount_sats: u64,
    delivery_integrity_hash: String,
) -> Result<crate::authorize_pay::AuthorizePayRequest, JobLifecycleError> {
    if delivery_integrity_hash.trim().is_empty() {
        return Err(JobLifecycleError::Input(
            "authorize_pay(job_id) requires buyer-supplied delivery_integrity_hash (tip-match); never auto-filled from claim oid".into(),
        ));
    }
    if delivery_integrity_hash != bind.commit_oid {
        return Err(JobLifecycleError::Targeting(format!(
            "authorize_pay(job_id) delivery_integrity_hash {} does not match accepted seller commit_oid {} (buyer tip-match required; refuse mismatch)",
            delivery_integrity_hash, bind.commit_oid
        )));
    }
    // The caller-supplied amount must equal the accept-bind amount (which was itself bound from
    // the buyer's signed offer at accept). Refuse any drift so the pay amount can never diverge
    // from the amount the buyer authorized.
    if amount_sats != bind.amount_sats {
        return Err(JobLifecycleError::Input(format!(
            "authorize_pay(job_id) amount_sats {amount_sats} does not match accepted bind amount {} — refused",
            bind.amount_sats
        )));
    }
    Ok(crate::authorize_pay::AuthorizePayRequest {
        job_id: bind.job_id.clone(),
        result_id: bind.result_id.clone(),
        // Sound because accept is fail-closed: a contribution-class offer with malformed pins is
        // refused at accept, so `bind.contribution == None` iff the offer was from-scratch.
        job_class: if bind.contribution.is_some() {
            crate::authorize_pay::JobClass::Contribution
        } else {
            crate::authorize_pay::JobClass::FromScratch
        },
        delivery_integrity_hash,
        job_hash: bind.job_hash.clone(),
        seller_pubkey: bind.seller_pubkey.clone(),
        amount_sats,
        repo: bind.repo.clone(),
        branch: bind.branch.clone(),
        commit_oid: bind.commit_oid.clone(),
        seller_signature: bind.seller_signature.clone(),
        // Thread the recorded creq hash so the attempt + receipt bind the seller's
        // request. `None` ⇒ a claim with no creq.
        creq_hash: bind.creq_hash.clone(),
        // Thread the creq's accepted-mint list so the buyer chooses the realized
        // mint. Empty ⇒ a claim with no creq (pay from the pinned default mint).
        accepted_mints: bind.accepted_mints.clone(),
        // Thread the SEALED funding-mint selection so the pay path derives the paying mint from the
        // bind, not the live config default — stable attempt id across retries. `None` ⇒ legacy bind.
        realized_mint: bind.funding_mint.clone(),
        // Thread the contribution binds so authorize_pay runs the contribution
        // verify-path + authorship seam. `None` ⇒ from-scratch.
        contribution: bind.contribution.as_ref().map(|c| {
            crate::authorize_pay::ContributionPayBinds {
                target_owner_pubkey: c.target_owner_pubkey.clone(),
                target_clone_url: c.target_clone_url.clone(),
                base_branch: c.base_branch.clone(),
                base_oid: c.base_oid.clone(),
                tuple_signature: c.tuple_signature.clone(),
            }
        }),
    })
}

/// Fill an explicit-form [`AuthorizePayRequest`](crate::authorize_pay::AuthorizePayRequest) from
/// the accept-bind so it builds the SAME co-signed receipt preimage as
/// [`authorize_request_from_bind`]. Call AFTER [`assert_authorize_matches_bind`].
///
/// `job_hash` feeds the receipt preimage and is taken from the bind unconditionally: it was
/// recorded at accept from the seller's own result, so the bind is authoritative. It is the only
/// preimage field neither asserted against nor otherwise filled from the bind, so a caller whose
/// `job_hash` diverged from the bind would build a preimage the seller never co-signed and the pay
/// would refuse "pre-pay seller co-signature invalid"; sourcing it from the bind keeps the explicit
/// form byte-identical to the bind-first form. `seller_signature`, `creq_hash`,
/// and the contribution binds fill from the bind only when the explicit caller left them
/// empty/absent.
///
/// `accepted_mints` is taken from the bind UNCONDITIONALLY, overwriting any caller-supplied value
/// (finding V). The co-signed receipt preimage binds only `creq_hash` (which pins the accepted SET),
/// not the realized mint, so the seller cosig does not pin `accepted_mints`; a caller-supplied list
/// could otherwise substitute a mint outside the bound set. Deriving it solely from the sealed bind
/// closes that: payment planning ([`crate::crossmint::plan_payment`]) realizes only at a mint drawn
/// from the bound set — the buyer's own when the seller accepts it, otherwise a hop target taken
/// from that same set.
pub fn fill_explicit_request_from_bind(
    request: &mut crate::authorize_pay::AuthorizePayRequest,
    bind: &AcceptedBind,
) {
    request.job_hash = bind.job_hash.clone();
    // The bind is authoritative for the job class (resolved from the signed offer at accept), so
    // the explicit form matches the bind form and cannot declare a divergent class.
    request.job_class = if bind.contribution.is_some() {
        crate::authorize_pay::JobClass::Contribution
    } else {
        crate::authorize_pay::JobClass::FromScratch
    };
    if request.seller_signature.is_empty() {
        request.seller_signature = bind.seller_signature.clone();
    }
    if request.creq_hash.is_none() {
        request.creq_hash = bind.creq_hash.clone();
    }
    // Finding V: derive the accepted-mint set SOLELY from the sealed bind, overwriting any
    // caller-supplied value. The seller cosig does not pin the realized mint (the preimage binds
    // only creq_hash), so a caller list must never be trusted to select the paying mint.
    request.accepted_mints = bind.accepted_mints.clone();
    // Finding CC: same seal for the funding-mint SELECTION — derived SOLELY from the sealed bind,
    // overwriting any caller value. A caller must not be able to pick the paying mint (which would
    // shift the attempt id); the frozen selection makes retries dedup.
    request.realized_mint = bind.funding_mint.clone();
    // Same seal for the delivery locator: repo/branch identify WHERE the paid commit is fetched
    // from, so they must come from the sealed bind (which `authorize_request_from_bind` already
    // does), never caller input — a caller must not be able to redirect the fetch to a different
    // locator for the accepted commit.
    request.repo = bind.repo.clone();
    request.branch = bind.branch.clone();
    if request.contribution.is_none() {
        request.contribution = bind.contribution.as_ref().map(|c| {
            crate::authorize_pay::ContributionPayBinds {
                target_owner_pubkey: c.target_owner_pubkey.clone(),
                target_clone_url: c.target_clone_url.clone(),
                base_branch: c.base_branch.clone(),
                base_oid: c.base_oid.clone(),
                tuple_signature: c.tuple_signature.clone(),
            }
        });
    }
}

fn write_accepted_bind(home: &MaxplayerHome, bind: &AcceptedBind) -> Result<(), JobLifecycleError> {
    let dir = home.root.join(JOBS_DIR);
    fs::create_dir_all(&dir).map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    let path = bind_path(home, &bind.job_id);
    let raw = serde_json::to_string_pretty(bind)
        .map_err(|error| JobLifecycleError::Io(format!("accept bind encode: {error}")))?;
    // Crash-atomic rewrite (temp → sync → rename → dir-fsync): a crash between the relay publish and
    // the bind-write can never leave a truncated/empty bind, which would let the daemon re-accept a
    // job and pay a SECOND time. Serialized per-job by `acquire_job_lock`.
    crate::durable::write_atomic(&dir, &path, raw.as_bytes())
        .map_err(|error| JobLifecycleError::Io(error.to_string()))
}

fn bind_path(home: &MaxplayerHome, job_id: &str) -> PathBuf {
    // Event ids are hex — safe as a single path segment.
    home.root.join(JOBS_DIR).join(format!("{job_id}.json"))
}

/// Acquire the per-job exclusive advisory lock guarding the accept-bind check→write section
/// (finding W). Blocks until any other accept holding this job's lock releases; the returned
/// handle holds the `flock` until it drops. Separate lock file per job (`<job_id>.lock`) so
/// accepts on DIFFERENT jobs never contend. Mirrors `budget.rs::acquire_lock`.
fn acquire_job_lock(home: &MaxplayerHome, job_id: &str) -> Result<File, JobLifecycleError> {
    let dir = home.root.join(JOBS_DIR);
    fs::create_dir_all(&dir).map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    // Event ids are hex — safe as a single path segment.
    let lock_path = dir.join(format!("{job_id}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    // Blocks until any other accept process/task holding this job's lock releases it.
    file.lock()
        .map_err(|error| JobLifecycleError::Io(error.to_string()))?;
    Ok(file)
}

/// Canonical job-hash for offer/result signing (buyer + seller share this).
pub fn job_hash_for_offer(job_id: &str, task: &str, amount_sats: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(job_id.as_bytes());
    hasher.update(b"|");
    hasher.update(task.as_bytes());
    hasher.update(b"|");
    hasher.update(amount_sats.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

fn buyer_keys(home: &MaxplayerHome) -> Result<nostr_sdk::Keys, JobLifecycleError> {
    let secret = home::read_secret_key_hex(home)?;
    nostr_sdk::Keys::parse(&secret)
        .map_err(|error| JobLifecycleError::Home(HomeError::Key(format!("buyer key parse: {error}"))))
}

#[allow(dead_code)] // guarded sync twin for non-async callers; MCP uses `_async`
fn publish_draft(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    draft: &EventDraft,
) -> Result<String, JobLifecycleError> {
    crate::runtime_guard::refuse_nested_block_on("publish_draft")
        .map_err(JobLifecycleError::Relay)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JobLifecycleError::Relay(error.to_string()))?;
    runtime.block_on(publish_draft_async(home, keys, draft))
}

async fn publish_draft_async(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    draft: &EventDraft,
) -> Result<String, JobLifecycleError> {
    use nostr_sdk::prelude::{Client, Kind};

    let builder = gateway::nostr::event_builder(draft)
        .map_err(|error| JobLifecycleError::Relay(format!("event builder: {error}")))?;
    let event = builder
        .sign_with_keys(keys)
        .map_err(|error| JobLifecycleError::Relay(format!("sign offer: {error}")))?;
    // Keep Kind::Custom visible for readers of the draft path.
    let _ = Kind::Custom(draft.kind);

    let client = Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let output = client
        .send_event_to([&home.config.relay_url], &event)
        .await;
    client.disconnect().await;
    let output = output.map_err(|error| JobLifecycleError::Relay(format!("send event: {error}")))?;
    if output.success.is_empty() {
        let failed: Vec<String> = output
            .failed
            .into_iter()
            .map(|(url, err)| format!("{url}: {err}"))
            .collect();
        return Err(JobLifecycleError::Relay(format!(
            "no relay accepted event ({})",
            failed.join("; ")
        )));
    }
    Ok(output.val.to_hex())
}

/// Current unix time (seconds). Wall-clock lives ONLY at call sites; the derivation
/// ([`derive_claim_liveness`]) takes `now` as input so the pure path stays testable.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Latest instant a delivery from `seller_pubkey` is still payable: the newest matching result's
/// `created_at` plus [`DELIVERY_PAY_WINDOW_SECS`]. A "delivery" here is the cheap relay-truth
/// signal — a result the seller authored that carries a `commit_oid`; the FULL gate-verify
/// (git tip-match / cosig / tip-match / budget) stays downstream in the pay path and is never
/// skipped. `None` when the seller has published no such result (nothing delivered to pay for).
fn delivery_pay_deadline(results: &[ResultView], seller_pubkey: &str) -> Option<u64> {
    results
        .iter()
        .filter(|result| result.seller_pubkey == seller_pubkey && result.commit_oid.is_some())
        .map(|result| result.created_at)
        .max()
        .map(|created_at| created_at.saturating_add(DELIVERY_PAY_WINDOW_SECS))
}

/// Derive claim liveness from status + offer deadline + published results + the injected `now`.
///
/// A `processing` claim whose offer deadline has passed (`now > deadline`) is reclassified:
/// - if its seller has a delivery still inside the pay window ([`delivery_pay_deadline`]), it
///   surfaces as [`CLAIM_STATUS_DELIVERED`] — payable, and counted as the live claim, because the
///   offer deadline is a scheduling clock and must not invalidate work already delivered;
/// - otherwise (no delivery, or its pay window has also lapsed) it surfaces as
///   [`CLAIM_STATUS_EXPIRED`], `live = false`, excluded from `live_claim_id`.
///
/// Both reclassifications are DERIVED — never stored, never read from the wall clock inside this
/// function (tests pass a fixed `now`). `claims` must be pre-sorted newest-first; the newest claim
/// that is still `processing` or `delivered` becomes the live one.
///
/// `offer_deadline_unix == None` (offer not yet on the relay) means expiry cannot be derived,
/// so status-based liveness is preserved unchanged.
pub(crate) fn derive_claim_liveness(
    claims: &mut [ClaimView],
    results: &[ResultView],
    offer_deadline_unix: Option<u64>,
    now: u64,
) -> Option<String> {
    if let Some(deadline) = offer_deadline_unix {
        for claim in claims.iter_mut() {
            if claim.status == "processing" && now > deadline {
                claim.status = match delivery_pay_deadline(results, &claim.seller_pubkey) {
                    Some(pay_deadline) if now <= pay_deadline => CLAIM_STATUS_DELIVERED.to_string(),
                    _ => CLAIM_STATUS_EXPIRED.to_string(),
                };
            }
        }
    }
    let live_claim_id = claims
        .iter()
        .find(|claim| claim.status == "processing" || claim.status == CLAIM_STATUS_DELIVERED)
        .map(|claim| claim.claim_id.clone());
    for claim in claims.iter_mut() {
        claim.live = live_claim_id.as_deref() == Some(claim.claim_id.as_str());
    }
    live_claim_id
}

/// The claims that were award CANDIDATES at the instant the offer's deadline struck (#897).
///
/// Diagnosing a park means reasoning about the claims that were in the running, and a job parks for
/// a passed deadline only once [`derive_claim_liveness`] has already demoted every `processing`
/// claim to expired. At that point `live` is false on all of them, so a diagnosis that reads `live`
/// sees an empty field in exactly the case where a seat did claim — and reports nothing, which reads
/// as "capability was not the problem" rather than as "this question was never asked".
///
/// Re-derives with the SAME function at `now = deadline` instead of restating which statuses count
/// as candidates. At that instant the demotion branch (`now > deadline`) is inert and
/// `delivery_pay_deadline` is never reached, so this reproduces the liveness any pre-deadline
/// evaluation would have produced. It owns no rule of its own and so cannot drift from the one it
/// is asking about.
pub(crate) fn claims_at_deadline(view: &JobView) -> Vec<ClaimView> {
    let mut claims = view.claims.clone();
    // The demotion is DESTRUCTIVE — it overwrites `status` — so re-deriving at the deadline is not
    // enough on its own: the information the diagnosis needs was already spent. Reverse the marker
    // first.
    //
    // [`CLAIM_STATUS_EXPIRED`] is never a relay value. `derive_claim_liveness` is the only thing that
    // writes it, and only for a claim that was `processing` when the deadline passed, so restoring it
    // reads that function's own marker rather than guessing at a status. The round trip is asserted
    // by `the_capability_clause_survives_the_real_deadline_demotion`, which demotes at `deadline + 1`
    // and requires the claim to be a candidate again here — so if the demotion ever writes a
    // different status, that test fails rather than this quietly seeing nothing.
    for claim in claims.iter_mut() {
        if claim.status == CLAIM_STATUS_EXPIRED {
            claim.status = "processing".to_owned();
        }
    }
    if let Some(deadline) = view.offer.as_ref().map(|offer| offer.deadline_unix) {
        derive_claim_liveness(&mut claims, &view.results, Some(deadline), deadline);
    }
    claims
}

/// A buyer AWARD found on the relay, parsed into exactly the fields an `awards` row needs and
/// nothing else. Constructed ONLY when every field was read off the event itself, so a caller
/// holding one can repair the local ledger without inferring or defaulting anything.
///
/// `amount_sats` is deliberately absent: the kind-3405 carries no amount tag (see
/// [`gateway::award_draft`]), so the sum a repair records must come from the buyer's own
/// reservation, not from this event. Leaving the field out makes that impossible to get wrong by
/// accident — there is no plausible-looking zero here to reach for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelayedAward {
    pub award_event_id: String,
    pub claim_id: String,
    pub seller_pubkey: String,
}

/// What the relay had to say about an award for a job. `Some(..)` of either variant means an award
/// IS public; they differ only in whether it can be trusted to rebuild a money row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AwardPresence {
    /// Complete and unambiguous — enough to repair the ledger from.
    Repairable(RelayedAward),
    /// An award exists but cannot be turned into a complete, unambiguous record. Refuse and leave
    /// the row missing; `detail` says which property failed so an operator is not left guessing.
    Unrepairable { award_event_id: String, detail: String },
}

impl AwardPresence {
    /// The id of the award the relay returned, whichever variant. Both variants know it — they
    /// differ on whether the REST of the event could be trusted, not on which event it was.
    ///
    /// Test-only, and deliberately kept rather than deleted. Its one caller is the live-relay
    /// red-prove, which asserts the probe found the KNOWN award and not merely SOME award for the
    /// job — an identity check a bare `is_some()` would pass while pointing at the wrong event.
    /// Deleting the accessor would force that assertion down to presence, which is the weaker
    /// property the red-prove exists to rule out. `#[cfg(test)]` retires the dead-code warning
    /// without retiring the check.
    #[cfg(test)]
    pub(crate) fn award_event_id(&self) -> &str {
        match self {
            Self::Repairable(relayed) => &relayed.award_event_id,
            Self::Unrepairable { award_event_id, .. } => award_event_id,
        }
    }
}

/// A three-way relay read: what the relay had to say, distinguishing an ANSWERED emptiness from a
/// read that merely went unanswered. `fetch_events` resolves `Ok(empty)` on timeout, so emptiness
/// alone proves nothing — [`ConfirmedAbsent`](PresenceRead::ConfirmedAbsent) is returned only when
/// the relay demonstrably served the session (an `EOSE` it owed us), the same discipline
/// [`JobView::read_confirmed`] applies to offer reads (#291).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PresenceRead<T> {
    /// The relay returned the thing.
    Present(T),
    /// The relay answered and does not have it. Still a statement about NOW — an event in flight
    /// this instant lands after the answer — so a caller acting on this must be idempotent
    /// against that event materializing (re-sending pinned bytes is; re-selecting a claim is not).
    ConfirmedAbsent,
    /// The read went unanswered — a slow or unreachable relay. Concluding absence here is #322.
    Unverified,
}

/// The buyer AWARD (kind-3405) authored by this buyer for `job_id`, if the relay returns one — the
/// relay half of the idempotent re-arm check (a 3405 may have published before a crash, so the
/// local ledger alone is insufficient). A relay error propagates so the caller treats it as
/// "unknown" and does not falsely mark the intent awarded.
///
/// Emptiness is disambiguated before it is reported: an empty read is `ConfirmedAbsent` only once
/// the relay proves it is serving this session's REQs, and `Unverified` otherwise — callers that
/// spend money on the answer refuse on `Unverified`
/// (see [`crate::buyer::lifecycle::award_with_reservation`]).
///
/// Returns the parsed award rather than a bare id so a caller can both NAME the award it found and
/// repair the missing row from it. Parsing happens here, against the real event, so
/// `buyer::lifecycle` stays free of nostr types and remains unit-testable with a plain closure.
pub(crate) async fn award_presence_async(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    job_id: &str,
    timeout: Duration,
) -> Result<PresenceRead<AwardPresence>, JobLifecycleError> {
    use nostr_sdk::prelude::{Client, EventId, Filter, Kind};

    let offer_id = EventId::from_hex(job_id)
        .map_err(|error| JobLifecycleError::Input(format!("job_id: {error}")))?;
    let client = Client::new(keys.clone());
    // Same discipline as the send path: auto-auth on (NIP-42-gated reads re-issue silently only
    // when it is), and WAIT for the socket — `connect()` only spawns, and a fetch racing the
    // handshake burns its whole window and reads as empty.
    client.automatic_authentication(true);
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let relay = client
        .relay(&home.config.relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("relay handle: {error}")))?;
    relay.wait_for_connection(RELAY_CONNECT_WAIT).await;

    let filter = Filter::new()
        .kind(Kind::Custom(JOB_AWARD_KIND))
        .author(keys.public_key())
        .event(offer_id)
        .hashtag(gateway::MAXPLAYER_TAG);
    // Fetch through the single-relay API, not the pool: the pool SWALLOWS per-relay stream
    // errors into `Ok(empty)`, so a relay REFUSING this REQ (CLOSED with a reason, auth failure)
    // would read as emptiness. Here a refusal surfaces as `Err` → the caller's Unverified — a
    // refused read is not an answered one.
    use nostr_sdk::pool::relay::ReqExitPolicy;
    let mut events = relay
        .fetch_events(filter.clone(), timeout, ReqExitPolicy::ExitOnEOSE)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("fetch award: {error}")))?;

    if events.is_empty() {
        // Emptiness means nothing until the relay shows it is answering us at all — and the
        // proof must PRECEDE the read it vouches for. The first fetch may have spent its window
        // on connect/auth, so a probe answered afterwards says only "the session works NOW".
        // Absence is therefore concluded exclusively from a SECOND read taken after the probe's
        // EOSE.
        let confirmed =
            crate::buyer::relay::probe_relay_serves_our_reqs(&client, keys.public_key(), timeout)
                .await;
        if !confirmed {
            client.disconnect().await;
            return Ok(PresenceRead::Unverified);
        }
        events = relay
            .fetch_events(filter, timeout, ReqExitPolicy::ExitOnEOSE)
            .await
            .map_err(|error| JobLifecycleError::Relay(format!("fetch award (recheck): {error}")))?;
        if events.is_empty() {
            client.disconnect().await;
            return Ok(PresenceRead::ConfirmedAbsent);
        }
    }

    // This filter asks for AWARDs alone (the accept is kind-3406), so every event here is a
    // selection. One is the intended state: the award is signed once and persisted before the
    // first send, and every retry re-transmits those exact bytes, so the relay dedups them by id.
    //
    // This read must not DEPEND on that. It exists to report what the relay actually holds for a
    // ledger that may be missing a row, so its soundness cannot rest on the very emit discipline
    // it is checking — and refusing on multiplicity ALONE would refuse to repair a row precisely
    // when that row is most likely absent.
    //
    // Multiplicity is only AMBIGUOUS if the events disagree about what to write. So parse them all
    // and compare the fields that land in the row: agreement means there is nothing to choose
    // between, and disagreement is the only case where picking one would launder a guess into a
    // money row.
    let buyer_pubkey_hex = keys.public_key().to_hex();
    let mut ordered: Vec<_> = events.iter().collect();
    // Oldest first — a repair records the selection that has been public longest, never whichever
    // the relay happened to send first. The id is a tiebreak, so the choice is total and stable.
    ordered.sort_by_key(|event| (event.created_at, event.id.to_hex()));

    let mut parsed = Vec::with_capacity(ordered.len());
    for event in &ordered {
        match parse_relayed_award(event, &buyer_pubkey_hex, job_id) {
            Ok(award) => parsed.push(award),
            // One unparseable event condemns the set: it may be the very one that disagrees, and
            // we cannot know that without parsing it.
            Err(detail) => {
                client.disconnect().await;
                return Ok(PresenceRead::Present(AwardPresence::Unrepairable {
                    award_event_id: event.id.to_hex(),
                    detail,
                }));
            }
        }
    }

    client.disconnect().await;
    Ok(PresenceRead::Present(reduce_parsed_awards(parsed)))
}

/// Whether the exact event `event_id_hex` is on `relay_url` — the by-id probe that settles a
/// pinned attempt which must not be re-sent (past the offer deadline). Because the id names one
/// specific event, this read cannot be confused by event multiplicity at all (#268) — the kind of
/// ambiguity that makes a COUNT unreliable: the answer is about THIS event or no event. Targets the
/// attempt's PINNED relay, never live config — the question is about the relay the bytes went to.
///
/// Same emptiness discipline as [`award_presence_async`]: absence is concluded only from a read
/// taken AFTER the relay proved it serves this session's REQs.
pub(crate) async fn event_present_async(
    keys: &nostr_sdk::Keys,
    relay_url: &str,
    event_id_hex: &str,
    timeout: Duration,
) -> Result<PresenceRead<()>, JobLifecycleError> {
    use nostr_sdk::prelude::{EventId, Filter};

    let event_id = EventId::from_hex(event_id_hex)
        .map_err(|error| JobLifecycleError::Input(format!("event id: {error}")))?;
    let filter = Filter::new().id(event_id);
    presence_of_filter(keys, relay_url, filter, timeout).await
}

/// Whether a kind-3403 RESULT by `seller_pubkey` — the PINNED, awarded seller — exists for
/// `job_id` on `relay_url`. Positive evidence that seller executed, which (for a pinned attempt)
/// means our award almost certainly WAS public and has merely aged out of the probe's view.
/// Consulted before the pay-window termination: refusing an attempt whose awarded seller
/// delivered would repudiate work that happened.
///
/// The author filter is load-bearing, not an optimisation: this probe's `Present` verdict HOLDS
/// the terminalization (and therefore the refund) indefinitely, and without the filter any
/// pubkey could publish one junk 3403 e-tagging the job and permanently pin the buyer's funds —
/// a griefing vector with no exit, since a forged result can never be collected (round-3
/// review). Only the awarded seller's delivery is evidence OUR award was public.
pub(crate) async fn job_has_results_async(
    keys: &nostr_sdk::Keys,
    relay_url: &str,
    job_id: &str,
    seller_pubkey: &str,
    timeout: Duration,
) -> Result<PresenceRead<()>, JobLifecycleError> {
    use nostr_sdk::prelude::{EventId, Filter, Kind, PublicKey};

    let offer_id = EventId::from_hex(job_id)
        .map_err(|error| JobLifecycleError::Input(format!("job_id: {error}")))?;
    let seller = PublicKey::from_hex(seller_pubkey)
        .map_err(|error| JobLifecycleError::Input(format!("seller pubkey: {error}")))?;
    let filter = Filter::new()
        .kind(Kind::Custom(JOB_RESULT_KIND))
        .author(seller)
        .event(offer_id)
        .hashtag(gateway::MAXPLAYER_TAG);
    presence_of_filter(keys, relay_url, filter, timeout).await
}

/// The shared presence read: one filter against one relay, with the full emptiness discipline —
/// single-relay fetches (a refused REQ surfaces as `Err`, never as emptiness), connection wait,
/// auto-auth, and absence concluded only from a SECOND read taken after the relay's EOSE proof.
async fn presence_of_filter(
    keys: &nostr_sdk::Keys,
    relay_url: &str,
    filter: nostr_sdk::prelude::Filter,
    timeout: Duration,
) -> Result<PresenceRead<()>, JobLifecycleError> {
    use nostr_sdk::pool::relay::ReqExitPolicy;
    use nostr_sdk::prelude::Client;

    let client = Client::new(keys.clone());
    client.automatic_authentication(true);
    client
        .add_relay(relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let relay = client
        .relay(relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("relay handle: {error}")))?;
    relay.wait_for_connection(RELAY_CONNECT_WAIT).await;

    let mut events = relay
        .fetch_events(filter.clone(), timeout, ReqExitPolicy::ExitOnEOSE)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("fetch: {error}")))?;
    if events.is_empty() {
        let confirmed =
            crate::buyer::relay::probe_relay_serves_our_reqs(&client, keys.public_key(), timeout)
                .await;
        if !confirmed {
            client.disconnect().await;
            return Ok(PresenceRead::Unverified);
        }
        events = relay
            .fetch_events(filter, timeout, ReqExitPolicy::ExitOnEOSE)
            .await
            .map_err(|error| JobLifecycleError::Relay(format!("fetch (recheck): {error}")))?;
    }

    let read = if events.is_empty() {
        PresenceRead::ConfirmedAbsent
    } else {
        PresenceRead::Present(())
    };
    client.disconnect().await;
    Ok(read)
}

/// Reduce a job's parsed awards — **oldest first** — to a single presence.
///
/// Split out as a pure function because the agreement rule is the part worth testing and the fetch
/// around it needs a live relay. Ordering is the caller's job: this takes the first element as the
/// award and never re-sorts.
///
/// Since #329 moved ACCEPT to its own kind (3406), a 3405 multiplicity is no longer the routine
/// award+accept pair it used to be — the probe's filter now returns SELECTIONS only. So the
/// agreement rule below got strictly stronger without changing: two 3405s that disagree on claim
/// or seller are now a genuine duplicate award (#322's harm), not a normal lifecycle artifact,
/// and refusing to pick between them is exactly right.
fn reduce_parsed_awards(parsed: Vec<RelayedAward>) -> AwardPresence {
    // Sound by construction: every caller checks the event set is non-empty before parsing it.
    let earliest = parsed.first().expect("a non-empty award set").clone();
    if let Some(other) = parsed.iter().find(|award| {
        award.claim_id != earliest.claim_id || award.seller_pubkey != earliest.seller_pubkey
    }) {
        return AwardPresence::Unrepairable {
            award_event_id: earliest.award_event_id,
            detail: format!(
                "{} awards for this job disagree on what to record (claim {} / seller {} against \
                 claim {} / seller {}); refusing to pick one",
                parsed.len(),
                earliest.claim_id,
                earliest.seller_pubkey,
                other.claim_id,
                other.seller_pubkey
            ),
        };
    }
    AwardPresence::Repairable(earliest)
}

/// Parse a single award event into a [`RelayedAward`], or `Err(reason)` when any field the ledger
/// needs is missing, malformed, or ambiguous. Every failure is a refusal — this never substitutes a
/// default, because each field it reads goes straight into a money row.
fn parse_relayed_award(
    event: &nostr_sdk::Event,
    buyer_pubkey_hex: &str,
    job_id: &str,
) -> Result<RelayedAward, String> {
    let draft = event_to_draft(event);
    let parsed = gateway::parse_award(&draft)
        .ok_or_else(|| "award has no root `e` (offer) and non-root `e` (claim) tag pair".to_owned())?;

    // The award we fetched was filtered by offer id, but verify it anyway: the filter is the
    // relay's word for what it sent, and this is the last point where a mismatched event could be
    // written into the ledger under the wrong job.
    if !parsed.offer_id.eq_ignore_ascii_case(job_id) {
        return Err(format!(
            "award roots on offer {} but was read for job {job_id}",
            parsed.offer_id
        ));
    }

    // An award carries TWO `p` tags — this buyer and the seller. The seller is identified by NOT
    // being us, never by tag order: ordering is a property of how `award_draft` happens to build
    // the event today, and reading it positionally would silently record the buyer as the seller
    // if that order ever changed.
    let mut others = draft
        .tags
        .iter()
        .filter(|tag| tag.first() == Some("p"))
        .filter_map(|tag| tag.value())
        .filter(|pubkey| !pubkey.eq_ignore_ascii_case(buyer_pubkey_hex));
    let seller_pubkey = others
        .next()
        .ok_or_else(|| "award has no `p` tag other than this buyer's own".to_owned())?
        .to_owned();
    if others.next().is_some() {
        return Err("award names more than one non-buyer `p`; seller is ambiguous".to_owned());
    }

    Ok(RelayedAward {
        award_event_id: event.id.to_hex(),
        claim_id: parsed.claim_id,
        seller_pubkey,
    })
}

/// Whether the OFFER read for a job was ANSWERED — the sole discriminator [`JobView::read_confirmed`]
/// carries, and the input [`crate::buyer::lifecycle::plan_missing_offer`] turns into a terminal park.
///
/// Rests on the offer filter's OWN evidence: `offer_present` (the relay returned our offer) or
/// `offer_probe_confirmed` (the `EOSE` the relay owes us for the offer REQ, re-proven by a second
/// offer fetch). A claim or result event is the relay answering a DIFFERENT filter — it proves the
/// session is alive but says nothing about whether the offer subscription was served, so it can
/// never certify offer-absence. #602 was exactly that substitution. Feedback/result are deliberately
/// NOT parameters here so the blend cannot even type-check.
/// Everything a claim event's TAGS say, as the view the award path reads. Pure, so what a buyer
/// takes off a seller's claim is testable without a relay.
///
/// Extracted for exactly that reason: the caller is an async relay fetch, and a parse that can only
/// be exercised through one has no test that could notice it silently reading nothing. Every
/// tag-derived field lands here; `live` is decided later against the offer, not by the tags.
fn claim_view_from_tags(
    claim_id: String,
    created_at: u64,
    seller_pubkey: String,
    status: String,
    tags: &[crate::gateway::TagSpec],
) -> ClaimView {
    ClaimView {
        claim_id,
        created_at,
        seller_pubkey,
        display_name: None,
        status,
        live: false,
        // Capture the seller-authored creq tag; absent on claims with no creq.
        creq: first_tag_value(tags, "creq").map(str::to_owned),
        agents: crate::heartbeat::agents_from_tags(tags),
        capability: crate::heartbeat::SeatCapability::from_tags(tags),
    }
}

fn offer_read_answered(offer_present: bool, offer_probe_confirmed: bool) -> bool {
    offer_present || offer_probe_confirmed
}

/// Read one job's offer + claims + results from the relay, with claim liveness derived
/// against `now` (a `processing` claim past the offer deadline is EXPIRED, not live). Exposed
/// `pub(crate)` so the seller daemon can run the backfill money-safety pre-claim check
/// (already-delivered / live-claimed-by-another) without duplicating the relay read.

pub(crate) async fn fetch_job_view_async(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    job_id: &str,
    timeout: Duration,
    now: u64,
) -> Result<JobView, JobLifecycleError> {
    use nostr_sdk::prelude::{Client, EventId, Filter, Kind};

    let offer_id = EventId::from_hex(job_id)
        .map_err(|error| JobLifecycleError::Input(format!("job_id: {error}")))?;

    let client = Client::new(keys.clone());
    // Same discipline as `award_presence_async` / `presence_of_filter`: auto-auth ON so a
    // NIP-42-gated read re-issues after the handshake, and WAIT for the socket before the first
    // fetch — `connect()` only spawns, and a fetch racing the handshake burns its whole window and
    // reads as empty.
    client.automatic_authentication(true);
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let relay = client
        .relay(&home.config.relay_url)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("relay handle: {error}")))?;
    relay.wait_for_connection(RELAY_CONNECT_WAIT).await;

    // Every fetch filter carries the `#t=maxplayer` namespace guard so a foreign event squatting a
    // maxplayer kind is never returned.
    let offer_filter = Filter::new()
        .id(offer_id)
        .kind(Kind::Custom(JOB_OFFER_KIND))
        .hashtag(gateway::MAXPLAYER_TAG);

    // ── The OFFER read is the SOLE evidence that may certify offer-ABSENCE (#291, #602).
    //
    // Read it through the SINGLE-RELAY api with `ExitOnEOSE`, not the pool: the pool SWALLOWS a
    // per-relay stream error (a `CLOSED auth-required:`, a refused REQ) into `Ok(empty)`, so a
    // refusal would read as absence. Here a refusal surfaces as `Err` and the caller treats the
    // read as unknown — the discipline `award_presence_async` already relies on.
    use nostr_sdk::pool::relay::ReqExitPolicy;
    let offer_read_started = tokio::time::Instant::now();
    let mut offer_events = relay
        .fetch_events(offer_filter.clone(), timeout, ReqExitPolicy::ExitOnEOSE)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("fetch offer: {error}")))?;

    // `read_confirmed` is derived from the OFFER read ALONE. A claim or result event is the relay
    // answering a DIFFERENT filter — it proves the session is alive but says NOTHING about whether
    // the offer subscription was served. #602 was exactly that substitution: a non-empty claims
    // read certifying an empty offer read as absence, terminally parking a retryable offer. The
    // claim/result reads below are therefore taken AFTER this decision and cannot feed it —
    // reintroducing the blend here would reference bindings that do not yet exist.
    //
    // The by-id filter means a compliant relay returns only our offer; we still assert `id ==
    // offer_id` at every point we consult the set, so a misbehaving relay's foreign event can
    // neither seed the view nor count as presence (matches the award path's rigor).
    let offer_present = offer_events.iter().any(|event| event.id == offer_id);
    let offer_probe_confirmed = if offer_present {
        // The offer is in hand; there is nothing to disambiguate and the relay owes no round trip.
        false
    } else {
        // Empty offer read: pay the round trip the relay OWES us (a `limit(0)` REQ's `EOSE`) and,
        // only on that proof, RE-FETCH the offer once more before concluding absence — the proof
        // must PRECEDE the read it vouches for, and a second read may land what a window-starved
        // first one missed. Deliberately a RESPONSE, not a broadcast we merely hope to receive.
        let confirmed =
            crate::buyer::relay::probe_relay_serves_our_reqs(&client, keys.public_key(), timeout)
                .await;
        if confirmed {
            offer_events = relay
                .fetch_events(offer_filter, timeout, ReqExitPolicy::ExitOnEOSE)
                .await
                .map_err(|error| {
                    JobLifecycleError::Relay(format!("fetch offer (recheck): {error}"))
                })?;
        }
        // Make the why-empty question observable next time (the report the tree lacked): whether
        // the relay answered our REQ, whether the recheck found the offer, and how long it took.
        crate::opline!(
            "buyer offer-read empty job={job_id} relay_answered={confirmed} offer_on_recheck={} \
             elapsed_ms={} (#602)",
            offer_events.iter().any(|event| event.id == offer_id),
            offer_read_started.elapsed().as_millis()
        );
        confirmed
    };
    let read_confirmed = offer_read_answered(offer_present, offer_probe_confirmed);

    // Claims (processing) and feedback (error) are distinct kinds — fetch both so the claim view
    // surfaces both. These reads are informational for the view (liveness, accept, delivery); they
    // run AFTER `read_confirmed` above precisely so they can never certify offer-absence (#602).
    let feedback_filter = Filter::new()
        .kinds([Kind::Custom(JOB_CLAIM_KIND), Kind::Custom(JOB_FEEDBACK_KIND)])
        .hashtag(gateway::MAXPLAYER_TAG)
        .event(offer_id);
    let result_filter = Filter::new()
        .kind(Kind::Custom(JOB_RESULT_KIND))
        .hashtag(gateway::MAXPLAYER_TAG)
        .event(offer_id);
    let feedback_events = client
        .fetch_events(feedback_filter, timeout)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("fetch feedback: {error}")))?;
    let result_events = client
        .fetch_events(result_filter, timeout)
        .await
        .map_err(|error| JobLifecycleError::Relay(format!("fetch results: {error}")))?;

    client.disconnect().await;

    let offer = offer_events.into_iter().find(|event| event.id == offer_id).map(|event| {
        let draft = event_to_draft(&event);
        let parsed = parse_offer(&draft).ok();
        OfferView {
            event_id: event.id.to_hex(),
            created_at: event.created_at.as_secs(),
            author_pubkey: event.pubkey.to_hex(),
            author_display_name: None,
            task: parsed
                .as_ref()
                .map(|p| p.task.clone())
                .unwrap_or_default(),
            output: parsed
                .as_ref()
                .map(|p| p.output.clone())
                .unwrap_or_default(),
            amount_sats: parsed.as_ref().map(|p| p.amount).unwrap_or(0),
            deadline_unix: parsed.as_ref().map(|p| p.deadline_unix).unwrap_or(0),
            seller_pubkey: parsed.as_ref().and_then(|p| p.seller_pubkey.clone()),
            seller_display_name: None,
            targeted: parsed.as_ref().map(|p| p.is_targeted()).unwrap_or(false),
            repo: first_tag_value(&draft.tags, "repo").map(str::to_owned),
            branch: first_tag_value(&draft.tags, "branch").map(str::to_owned),
            // Raw job-class + parsed pins. A malformed contribution offer parses to
            // `contribution=None` while `job_class=Some("contribution")` — accept refuses it
            // (fail-closed; never silently from-scratch).
            job_class: first_tag_value(&draft.tags, crate::contribution::TAG_JOB_CLASS)
                .map(str::to_owned),
            contribution: contribution_offer_view(&draft.tags),
            requested_agent: parsed.as_ref().and_then(|p| p.requested_agent.clone()),
            requested_harness_family: parsed
                .as_ref()
                .and_then(|p| p.requested_harness_family.clone()),
            requested_model: parsed.as_ref().and_then(|p| p.requested_model.clone()),
            required_capabilities: parsed
                .as_ref()
                .map(|p| p.required_capabilities.clone())
                .unwrap_or_default(),
        }
    });

    let mut claims = Vec::new();
    for event in feedback_events {
        let draft = event_to_draft(&event);
        let status = first_tag_value(&draft.tags, "status")
            .unwrap_or("")
            .to_owned();
        if status != "processing" && status != "error" {
            // accepts are buyer-authored; skip for claim list
            continue;
        }
        claims.push(claim_view_from_tags(
            event.id.to_hex(),
            event.created_at.as_secs(),
            event.pubkey.to_hex(),
            status,
            &draft.tags,
        ));
    }
    claims.sort_by_key(|c| std::cmp::Reverse(c.created_at));

    let mut results = Vec::new();
    for event in result_events {
        let draft = event_to_draft(&event);
        let delivery = parse_git_result_delivery(&draft).ok();
        let amount_sats = first_tag(&draft.tags, "amount")
            .and_then(|tag| tag.0.get(1))
            .and_then(|value| value.parse().ok());
        let (harness, model) = result_attribution(&draft.tags);
        results.push(ResultView {
            result_id: event.id.to_hex(),
            created_at: event.created_at.as_secs(),
            seller_pubkey: event.pubkey.to_hex(),
            display_name: None,
            job_hash: first_tag_value(&draft.tags, "job-hash").map(str::to_owned),
            repo: delivery.as_ref().map(|d| d.repo().to_owned()),
            branch: delivery.as_ref().map(|d| d.branch().to_owned()),
            commit_oid: delivery
                .as_ref()
                .map(|d| d.commit_oid().as_str().to_owned()),
            amount_sats,
            seller_signature: sig_seller_value(&draft.tags),
            harness,
            model,
            contribution: contribution_result_view(&draft.tags),
        });
    }
    results.sort_by_key(|r| std::cmp::Reverse(r.created_at));

    // Liveness is DERIVED from `now` vs the offer deadline AND the published deliveries: a
    // processing claim past its deadline reads EXPIRED unless its seller delivered inside the pay
    // window, in which case it stays payable (DELIVERED). Derived after results so the pay-window
    // decision can see them.
    let offer_deadline_unix = offer.as_ref().map(|o| o.deadline_unix);
    let live_claim_id = derive_claim_liveness(&mut claims, &results, offer_deadline_unix, now);

    let accepted = load_accepted_bind(home, job_id)?;

    let view = JobView {
        job_id: job_id.to_owned(),
        offer,
        claims,
        results,
        live_claim_id,
        accepted,
        pending: false,
        read_confirmed,
    };
    Ok(view)
}

/// Collect hex pubkeys for cosmetic kind-0 enrichment (never for pay/targeting).
fn display_name_pubkeys(view: &JobView) -> Vec<String> {
    let mut pubkeys: Vec<String> = Vec::new();
    if let Some(offer) = &view.offer {
        pubkeys.push(offer.author_pubkey.clone());
        if let Some(seller) = &offer.seller_pubkey {
            pubkeys.push(seller.clone());
        }
    }
    for claim in &view.claims {
        pubkeys.push(claim.seller_pubkey.clone());
    }
    for result in &view.results {
        pubkeys.push(result.seller_pubkey.clone());
    }
    pubkeys
}

fn apply_display_names(view: &mut JobView, names: &std::collections::HashMap<String, Option<String>>) {
    let lookup = |hex: &str| -> Option<String> {
        names
            .get(&hex.to_ascii_lowercase())
            .and_then(|value| value.clone())
    };

    if let Some(offer) = &mut view.offer {
        offer.author_display_name = lookup(&offer.author_pubkey);
        offer.seller_display_name = offer
            .seller_pubkey
            .as_ref()
            .and_then(|seller| lookup(seller));
    }
    for claim in &mut view.claims {
        claim.display_name = lookup(&claim.seller_pubkey);
    }
    for result in &mut view.results {
        result.display_name = lookup(&result.seller_pubkey);
    }
}

/// Cosmetic kind-0 enrichment only — never feeds accept-bind / targeting / pay.
/// Async so `get_job`'s existing runtime does not nest `block_on` (panic).
async fn maybe_attach_display_names_async(
    home: &MaxplayerHome,
    view: &mut JobView,
    include_display_names: bool,
) {
    if include_display_names {
        attach_display_names_async(home, view).await;
    }
}

async fn attach_display_names_async(home: &MaxplayerHome, view: &mut JobView) {
    let pubkeys = display_name_pubkeys(view);
    let mut unique = std::collections::HashSet::new();
    for key in pubkeys {
        let hex = key.trim().to_ascii_lowercase();
        if hex.len() == 64 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            unique.insert(hex);
        }
    }
    if unique.is_empty() {
        return;
    }
    let names = match crate::profile::fetch_names_async(home, &unique).await {
        Ok(map) => map,
        Err(_) => unique.into_iter().map(|k| (k, None)).collect(),
    };
    apply_display_names(view, &names);
}

fn select_result<'a>(
    results: &'a [ResultView],
    seller_pubkey: &str,
    result_id: Option<&str>,
) -> Result<&'a ResultView, JobLifecycleError> {
    if let Some(id) = result_id {
        let result = results
            .iter()
            .find(|result| result.result_id == id)
            .ok_or_else(|| JobLifecycleError::NotFound(format!("result {id}")))?;
        // CROSS-BIND TOOTH: a result is bindable to this claim ONLY if the result's author
        // (its result-kind event pubkey) IS the claim seller. NEVER trust an operator-supplied
        // `result_id` to override this — accepting seller A's claim with seller B's result is
        // the live 21-sat cross-bind (the buyer pays A, who is p2pk-locked into the token, for
        // B's artifact). The `result_id == None` branch below already author-filters; this
        // closes the explicit-id hole. Refuse naming BOTH public keys (public keys only).
        if result.seller_pubkey != seller_pubkey {
            return Err(JobLifecycleError::Targeting(format!(
                "result {id} is authored by seller {} but the accepted claim's seller is {} — \
                 cross-authored result refused (the buyer must not pay one seller for another \
                 seller's result)",
                result.seller_pubkey, seller_pubkey
            )));
        }
        return Ok(result);
    }
    results
        .iter()
        .find(|result| result.seller_pubkey == seller_pubkey && result.commit_oid.is_some())
        .ok_or_else(|| {
            JobLifecycleError::NotFound(format!(
                "no git result from seller {seller_pubkey} for this job"
            ))
        })
}

/// Convert a relay event into an [`EventDraft`] (tag/content only — no secrets).
pub fn event_to_draft(event: &nostr_sdk::Event) -> EventDraft {
    let tags = event
        .tags
        .iter()
        .map(|tag| TagSpec(tag.as_slice().to_vec()))
        .collect();
    EventDraft::new(event.kind.as_u16(), tags, event.content.clone())
}

fn first_tag<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a TagSpec> {
    tags.iter()
        .find(|tag| tag.0.first().map(String::as_str) == Some(name))
}

fn first_tag_value<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a str> {
    first_tag(tags, name).and_then(TagSpec::value)
}

/// Parse a well-formed contribution offer's pins into a serializable view. A malformed
/// `contribution`-class offer yields `None` (surfaced as `job_class=Some, contribution=None`, which
/// accept refuses — fail-closed, never run from-scratch).
fn contribution_offer_view(tags: &[TagSpec]) -> Option<ContributionOfferView> {
    match crate::contribution::parse_contribution_offer(tags) {
        Ok(Some(offer)) => Some(ContributionOfferView {
            target_owner_pubkey: offer.target.owner_pubkey().to_owned(),
            target_clone_url: offer.target.clone_url().to_owned(),
            base_branch: offer.base.branch().to_owned(),
            base_oid: offer.base.oid().to_owned(),
            accepts: offer.accepts,
        }),
        _ => None,
    }
}

/// Parse a seller result's contribution echo + authorship signature into a serializable view.
fn contribution_result_view(tags: &[TagSpec]) -> Option<ContributionResultView> {
    match crate::contribution::parse_contribution_result_echo(tags) {
        Ok(Some((echo, tuple_signature))) => Some(ContributionResultView {
            target_owner_pubkey: echo.target.owner_pubkey().to_owned(),
            target_clone_url: echo.target.clone_url().to_owned(),
            base_branch: echo.base.branch().to_owned(),
            base_oid: echo.base.oid().to_owned(),
            tuple_signature,
        }),
        _ => None,
    }
}

/// Value of the `["sig","seller",<hex>]` tag, if present (co-signature capture).
fn sig_seller_value(tags: &[TagSpec]) -> Option<String> {
    tags.iter()
        .find(|tag| {
            tag.0.first().map(String::as_str) == Some("sig")
                && tag.0.get(1).map(String::as_str) == Some("seller")
        })
        .and_then(|tag| tag.0.get(2))
        .map(String::to_owned)
}

/// Seller-claimed exec-metadata attribution off a result's tags: `(harness, model)` from the
/// `["harness", …]` / `["model", …]` tags `seller_exec_metadata` stamps on the kind-3403 (#261).
/// Absent-stays-absent: a result with no metadata block yields `(None, None)` — never a
/// fabricated attribution, and never the buyer's requested harness echoed back.
fn result_attribution(tags: &[TagSpec]) -> (Option<String>, Option<String>) {
    (
        first_tag_value(tags, "harness").map(str::to_owned),
        first_tag_value(tags, "model").map(str::to_owned),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home;

    // #602: offer-ABSENCE is certified from the offer read ALONE. The bug was
    // `read_confirmed = offer || feedback || result || probe`, which let a non-empty claims (or
    // results) read stand in for the offer's own answer and terminally park a retryable offer. The
    // contract is pinned here by SIGNATURE as much as by value: `offer_read_answered` takes only the
    // offer's own evidence, so no future edit can feed a claim/result into it without a loud,
    // reviewable change. (The derivation site enforces the same by ORDER — the claim/result reads
    // run after this decision, so reintroducing the blend there is a compile error.)
    #[test]
    fn claims_and_results_are_not_inputs_to_offer_absence_certification() {
        // Offer in hand → answered, no probe needed.
        assert!(offer_read_answered(true, false));
        // Offer empty but the relay proved it served the OFFER REQ (EOSE + re-fetch) → answered.
        assert!(offer_read_answered(false, true));
        // Offer empty AND its own read unconfirmed → NOT answered. No claim or result can flip this,
        // because neither is an input: the driver must RETRY, never terminally park (#291/#602).
        assert!(!offer_read_answered(false, false));
    }

    // Finding A: accept-side creq verification is STRICT and fail-closed. A well-formed creq
    // whose payment terms match the job+offer yields its `m` mints; every other shape refuses.
    #[test]
    fn accept_verify_creq_is_strict_and_fail_closed() {
        let seller = nostr_sdk::Keys::generate().public_key().to_hex();
        let mint = "https://testnut.cashudevkit.org".to_string();
        let good = crate::gateway::creq::build_seller_creq("job", 7, "sat", &[mint.clone()], &seller)
            .expect("build creq");

        // Matching terms → accepted, returns the m list.
        assert_eq!(
            verify_accepted_claim_creq(Some(&good), "job", 7).unwrap(),
            vec![mint.clone()]
        );

        // No creq at all → REFUSE (was previously the silent empty/default path).
        let err = verify_accepted_claim_creq(None, "job", 7).unwrap_err();
        assert!(err.to_string().contains("no creq"), "got: {err}");

        // Present but garbage → REFUSE.
        let err = verify_accepted_claim_creq(Some("creqAnot-valid-cbor"), "job", 7).unwrap_err();
        assert!(err.to_string().contains("unparseable creq"), "got: {err}");

        // payment_id != job_id → REFUSE.
        let err = verify_accepted_claim_creq(Some(&good), "other-job", 7).unwrap_err();
        assert!(err.to_string().contains("payment id"), "got: {err}");

        // amount != offer amount → REFUSE (the load-bearing money check).
        let err = verify_accepted_claim_creq(Some(&good), "job", 8).unwrap_err();
        assert!(err.to_string().contains("amount"), "got: {err}");

        // Non-sat unit → REFUSE.
        let usd = crate::gateway::creq::build_seller_creq("job", 7, "usd", &[mint.clone()], &seller)
            .expect("build usd creq");
        let err = verify_accepted_claim_creq(Some(&usd), "job", 7).unwrap_err();
        assert!(err.to_string().contains("unit"), "got: {err}");

        // Empty accepted-mint list → REFUSE.
        let no_mints = crate::gateway::creq::build_seller_creq("job", 7, "sat", &[], &seller)
            .expect("build no-mint creq");
        let err = verify_accepted_claim_creq(Some(&no_mints), "job", 7).unwrap_err();
        assert!(err.to_string().contains("accepted mints"), "got: {err}");
    }

    // Finding B: authorize_request_from_bind refuses a caller amount that drifts from the bind.
    #[test]
    fn authorize_from_bind_refuses_amount_drift() {
        let bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 5,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        // Drifted amount → refuse.
        let err = authorize_request_from_bind(&bind, 6, bind.commit_oid.clone())
            .expect_err("amount drift");
        assert!(
            err.to_string().contains("does not match accepted bind amount"),
            "got: {err}"
        );
        // Matching amount → ok.
        let req = authorize_request_from_bind(&bind, 5, bind.commit_oid.clone()).expect("ok");
        assert_eq!(req.amount_sats, 5);
    }

    // Fix P — interim single-settlement guard: a second accept binding a DIFFERENT result for a
    // job that already has an accept-bind is refused, so a second result_id cannot mint a second
    // buyer attempt/payment for one job. Re-accepting the SAME result is idempotent; no prior bind
    // is allowed.
    #[test]
    fn single_settlement_refuses_different_result_and_is_idempotent_on_same() {
        let existing = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "res-A".into(),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 5,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        // A different result for the already-bound job → refused (one settlement per job).
        let err = assert_single_settlement(Some(&existing), &existing.job_id, "res-B")
            .expect_err("different result refused");
        assert!(
            err.to_string().contains("already accepted result res-A")
                && err.to_string().contains("res-B"),
            "unexpected error: {err}"
        );
        // Re-accepting the SAME result is idempotent (durability re-publish/finalize).
        assert_single_settlement(Some(&existing), &existing.job_id, "res-A")
            .expect("same result idempotent");
        // No prior bind → allowed.
        assert_single_settlement(None, &existing.job_id, "res-B").expect("first accept allowed");
    }

    // Issue #93: an empty/missing seller co-signature must be refused at accept-time and must NOT
    // write an accept-bind (which would permanently occupy the single-settlement slot with `""`).
    // After empty presentation, a later VALID sig for the same claim must still be able to bind.
    #[test]
    fn empty_seller_sig_does_not_occupy_settlement_slot_later_valid_binds() {
        // Missing / empty / whitespace-only → refuse (no bind written).
        for bad in [
            None,
            Some(String::new()),
            Some("   ".to_owned()),
            Some("\t\n".to_owned()),
        ] {
            let err = require_seller_signature(&bad).expect_err("empty/missing seller sig must refuse");
            let message = err.to_string();
            assert!(
                message.contains("seller signature") && message.contains("sig/seller"),
                "refusal must name the missing sig/seller gate: {message}"
            );
            assert!(
                message.contains("single-settlement") || message.contains("later valid"),
                "refusal must explain the slot stays free: {message}"
            );
        }

        let root = std::env::temp_dir().join(format!(
            "maxplayer-jobs-empty-sig-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let job_id = "aa".repeat(32);

        // Model accept after empty presentation: refuse ⇒ do NOT write. Slot stays free.
        let empty_presented = require_seller_signature(&None);
        assert!(empty_presented.is_err(), "empty presentation refuses");
        assert!(
            load_accepted_bind(&home, &job_id)
                .expect("load")
                .is_none(),
            "empty presentation must leave no accept-bind on disk"
        );
        assert_single_settlement(None, &job_id, "res-valid")
            .expect("slot free after empty presentation");

        // Later VALID sig for the same claim → bind successfully.
        let valid_sig =
            require_seller_signature(&Some("ab".repeat(64))).expect("valid non-empty sig accepted");
        assert_eq!(valid_sig, "ab".repeat(64));
        let bind = AcceptedBind {
            job_id: job_id.clone(),
            claim_id: "bb".repeat(32),
            result_id: "res-valid".into(),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 5,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: valid_sig.clone(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        // Single-settlement still free (no prior bind), then durable write of the valid sig.
        assert_single_settlement(
            load_accepted_bind(&home, &job_id).expect("load").as_ref(),
            &job_id,
            &bind.result_id,
        )
        .expect("valid sig may bind after empty was presented");
        write_accepted_bind(&home, &bind).expect("write valid bind");
        let loaded = load_accepted_bind(&home, &job_id)
            .expect("load")
            .expect("valid bind present");
        assert_eq!(loaded.seller_signature, valid_sig);
        assert!(!loaded.seller_signature.is_empty());
        assert_eq!(loaded.result_id, "res-valid");
        // Slot is now occupied by the valid result (different result refused).
        let err = assert_single_settlement(Some(&loaded), &job_id, "res-other")
            .expect_err("after valid bind, different result refused");
        assert!(
            err.to_string().contains("already accepted result res-valid"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Load-bearing: the explicit 9-field form (with bind-fill applied) and the
    // bind-first form must build the IDENTICAL AuthorizePayRequest over the same accept-bind —
    // identical requests ⇒ identical PaymentKey ⇒ identical co-signed receipt preimage/digest, so
    // both forms pass the seller's pre-pay cosig. `fill_explicit_request_from_bind` sources
    // `job_hash` from the bind so the explicit form does not keep a caller's divergent job_hash.
    #[test]
    fn explicit_and_bind_forms_build_identical_request() {
        let bind = AcceptedBind {
            job_id: "2a195bece5f6".into(),
            claim_id: "0a8bbc5284e8".into(),
            result_id: "058886d7b19e".into(),
            seller_pubkey: "aa".repeat(32),
            commit_oid: "bb".repeat(20),
            repo: "https://example.invalid/repo.git".into(),
            branch: "maxplayer/job".into(),
            job_hash: "cc".repeat(32),
            amount_sats: 5,
            accept_event_id: "accept-x".into(),
            accepted_at: 1,
            seller_signature: "dd".repeat(64),
            creq_hash: Some("2ad9b34cbf8c".to_string()),
            accepted_mints: vec!["https://mint.minibits.cash/Bitcoin".into()],
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };

        // The bind-first request (always sources preimage fields from the bind).
        let from_bind =
            authorize_request_from_bind(&bind, 5, bind.commit_oid.clone()).expect("bind form");

        // The explicit request as mcp.rs builds it, with a job_hash that DIVERGES from the bind
        // (the real-trade failure) and creq_hash/accepted_mints/seller_signature left for the fill.
        let mut explicit = crate::authorize_pay::AuthorizePayRequest {
            job_id: bind.job_id.clone(),
            result_id: bind.result_id.clone(),
            job_class: crate::authorize_pay::JobClass::FromScratch,
            delivery_integrity_hash: bind.commit_oid.clone(),
            job_hash: "ff".repeat(32), // caller-supplied, DIFFERENT from bind.job_hash
            seller_pubkey: bind.seller_pubkey.clone(),
            amount_sats: 5,
            repo: bind.repo.clone(),
            branch: bind.branch.clone(),
            commit_oid: bind.commit_oid.clone(),
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        fill_explicit_request_from_bind(&mut explicit, &bind);

        assert_eq!(
            explicit, from_bind,
            "explicit-form and bind-form requests must be identical so both build the same \
             co-signed receipt preimage"
        );
    }

    // Finding BB: repo/branch identify WHERE the paid commit is fetched, so the explicit form must
    // seal them from the bind and never keep a caller's divergent locator.
    #[test]
    fn explicit_form_seals_repo_and_branch_from_bind() {
        let bind = AcceptedBind {
            job_id: "2a195bece5f6".into(),
            claim_id: "0a8bbc5284e8".into(),
            result_id: "058886d7b19e".into(),
            seller_pubkey: "aa".repeat(32),
            commit_oid: "bb".repeat(20),
            repo: "https://bound.invalid/repo.git".into(),
            branch: "maxplayer/bound-branch".into(),
            job_hash: "cc".repeat(32),
            amount_sats: 5,
            accept_event_id: "accept-x".into(),
            accepted_at: 1,
            seller_signature: "dd".repeat(64),
            creq_hash: Some("2ad9b34cbf8c".to_string()),
            accepted_mints: vec!["https://mint.minibits.cash/Bitcoin".into()],
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        let mut explicit = crate::authorize_pay::AuthorizePayRequest {
            job_id: bind.job_id.clone(),
            result_id: bind.result_id.clone(),
            job_class: crate::authorize_pay::JobClass::FromScratch,
            delivery_integrity_hash: bind.commit_oid.clone(),
            job_hash: bind.job_hash.clone(),
            seller_pubkey: bind.seller_pubkey.clone(),
            amount_sats: 5,
            // Caller supplies a DIFFERENT allowlisted locator for the same commit.
            repo: "https://caller.invalid/mirror.git".into(),
            branch: "maxplayer/caller-branch".into(),
            commit_oid: bind.commit_oid.clone(),
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            realized_mint: None,
            contribution: None,
        };
        fill_explicit_request_from_bind(&mut explicit, &bind);
        assert_eq!(explicit.repo, bind.repo, "repo must be sealed from the bind");
        assert_eq!(explicit.branch, bind.branch, "branch must be sealed from the bind");
    }

    // Incident diagnosis: the LIVE failure had the explicit call's job_hash BYTE-EQUAL
    // to the bind (per the failed-run logs). Reproduce that exact shape — ALL explicit fields
    // byte-equal to the bind — and confirm the two constructed requests are identical EVEN without
    // the job_hash fix mattering. If this passes, the incident's divergence was NOT in request
    // construction (it points at the next layer: the bind on disk / re-accept rewrite / config).
    #[test]
    fn byte_equal_explicit_matches_bind_request() {
        let bind = AcceptedBind {
            job_id: "2a195bece5f6".into(),
            claim_id: "0a8bbc5284e8".into(),
            result_id: "058886d7b19e".into(),
            seller_pubkey: "aa".repeat(32),
            commit_oid: "5ce37eeb".repeat(5),
            repo: "https://example.invalid/repo.git".into(),
            branch: "maxplayer/job".into(),
            job_hash: "699c9230".repeat(8),
            amount_sats: 5,
            accept_event_id: "accept-x".into(),
            accepted_at: 1,
            seller_signature: "dd".repeat(64),
            creq_hash: Some("2ad9b34c".repeat(8)),
            accepted_mints: vec!["https://mint.minibits.cash/Bitcoin".into()],
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        let from_bind =
            authorize_request_from_bind(&bind, 5, bind.commit_oid.clone()).expect("bind form");

        // Explicit form with EVERY field byte-equal to the bind (the incident's inputs).
        let mut explicit = crate::authorize_pay::AuthorizePayRequest {
            job_id: bind.job_id.clone(),
            result_id: bind.result_id.clone(),
            job_class: crate::authorize_pay::JobClass::FromScratch,
            delivery_integrity_hash: bind.commit_oid.clone(),
            job_hash: bind.job_hash.clone(), // byte-equal, as in the live failure
            seller_pubkey: bind.seller_pubkey.clone(),
            amount_sats: 5,
            repo: bind.repo.clone(),
            branch: bind.branch.clone(),
            commit_oid: bind.commit_oid.clone(),
            seller_signature: bind.seller_signature.clone(),
            creq_hash: bind.creq_hash.clone(),
            accepted_mints: bind.accepted_mints.clone(),
            realized_mint: None,
            contribution: None,
        };
        fill_explicit_request_from_bind(&mut explicit, &bind);

        assert_eq!(
            explicit, from_bind,
            "with byte-equal inputs the two forms already produce identical requests — the live \
             incident was NOT request construction"
        );
    }

    // Finding V: the explicit-pay form must NOT retain a caller-supplied accepted_mints. The
    // co-signed receipt preimage binds only creq_hash (the accepted SET), not the realized mint, so
    // a caller list is unpinned by the seller cosig; `fill_explicit_request_from_bind` overwrites it
    // from the sealed bind unconditionally. Here the caller passes a mint OUTSIDE the bind's set;
    // after fill the request carries ONLY the bind's set, so planning can never realize at the
    // substituted mint (`plan_payment` realizes only at a mint drawn from that set).
    #[test]
    fn fill_explicit_overwrites_caller_accepted_mints_with_bind() {
        let bound_mint = "https://mint.minibits.cash/Bitcoin".to_string();
        let attacker_mint = "https://evil.example/attacker".to_string();
        let bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 5,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: Some("2ad9b34c".repeat(8)),
            accepted_mints: vec![bound_mint.clone()],
            // The funding-mint SELECTION is sealed in the bind (finding CC). Buyer funds at a mint in
            // the accepted set ⇒ direct payment ⇒ delivery mint equals the funding mint.
            funding_mint: Some(bound_mint.clone()),
            delivery_mint: Some(bound_mint.clone()),
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        let mut explicit = crate::authorize_pay::AuthorizePayRequest {
            job_id: bind.job_id.clone(),
            result_id: bind.result_id.clone(),
            job_class: crate::authorize_pay::JobClass::FromScratch,
            delivery_integrity_hash: bind.commit_oid.clone(),
            job_hash: bind.job_hash.clone(),
            seller_pubkey: bind.seller_pubkey.clone(),
            amount_sats: bind.amount_sats,
            repo: bind.repo.clone(),
            branch: bind.branch.clone(),
            commit_oid: bind.commit_oid.clone(),
            seller_signature: bind.seller_signature.clone(),
            creq_hash: bind.creq_hash.clone(),
            // Caller substitutes a mint OUTSIDE the bound set — for BOTH the accepted set and the
            // realized-mint selection.
            accepted_mints: vec![attacker_mint.clone()],
            realized_mint: Some(attacker_mint.clone()),
            contribution: None,
        };
        fill_explicit_request_from_bind(&mut explicit, &bind);
        assert_eq!(
            explicit.accepted_mints, bind.accepted_mints,
            "caller accepted_mints must be overwritten by the sealed bind"
        );
        assert!(
            !explicit.accepted_mints.contains(&attacker_mint),
            "substituted mint must not survive into the pay request"
        );
        // Finding CC: the caller-supplied realized-mint selection is likewise overwritten by the
        // sealed bind — a caller must not be able to pick the paying mint (which would shift the
        // attempt id).
        assert_eq!(
            explicit.realized_mint,
            bind.funding_mint,
            "caller realized_mint must be overwritten by the sealed bind"
        );
        assert_eq!(explicit.realized_mint.as_deref(), Some(bound_mint.as_str()));
    }

    // Finding CC (end-to-end double-pay regression): the exact retry-stability scenario the
    // mint-freeze closes. The accept-bind seals funding_mint = A (the buyer's then-configured
    // default, A in the accepted set). The buyer THEN flips its configured default to B (also
    // accepted) and RE-RUNS the pay authorization (retry). Because the pay path derives the realized
    // mint from the SEALED bind — not the live config — the retry's PaymentKey/attempt id is
    // byte-identical to the first attempt (the config flip did NOT shift it) and still targets the
    // frozen mint A, so the budget/journal idempotency dedups it instead of minting a SECOND payment
    // identity for one job. Drives the REAL seal path (`authorize_request_from_bind`) and mirrors
    // `authorize_pay`'s exact mint-selection + attempt-id derivation over the resulting request.
    #[tokio::test(flavor = "current_thread")]
    async fn config_default_flip_after_accept_does_not_shift_attempt_id() {
        use crate::authorize_pay::wallet_open_mint_url;
        use crate::crossmint::plan_payment;
        use crate::payment::{
            DeliveryIntegrityHash, JobHash, JobId, PaymentKey, PaymentTerms, ResultId,
        };
        use cashu::{Amount, CurrencyUnit, MintUrl, PublicKey as CashuPublicKey};
        use std::str::FromStr;

        // A and B are BOTH in the accepted set; A is the buyer's default at accept, B the flipped
        // default on retry. `allow_real_mints` is on so two distinct mints both pass the fence.
        let mint_a = "https://mint-a.example";
        let mint_b = "https://mint-b.example";

        // A buyer home whose LIVE config default is B (the buyer flipped it after accept) — the value
        // the pre-fix wallet-open used. The frozen mint A must win over this.
        let root = std::env::temp_dir().join(format!(
            "maxplayer-cc-walletopen-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        home.config.accepted_mints = vec![mint_b.to_string()]; // default_mint() = B
        home.config.allow_real_mints = true;
        assert_eq!(home.config.default_mint(), mint_b, "live config default is B (flipped)");

        let seller_nostr = nostr_sdk::Keys::generate().public_key();
        let seller_p2pk =
            CashuPublicKey::from_str(&format!("02{}", seller_nostr.to_hex())).expect("p2pk");

        let bind = AcceptedBind {
            job_id: "job-cc".into(),
            claim_id: "bb".repeat(32),
            result_id: "res-cc".into(),
            seller_pubkey: seller_nostr.to_hex(),
            commit_oid: "ee".repeat(20),
            repo: "https://example.invalid/repo.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 7,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: String::new(),
            creq_hash: Some("2ad9b34c".repeat(8)),
            accepted_mints: vec![mint_a.to_string(), mint_b.to_string()],
            // Funding sealed at accept from the buyer's then-configured default (A); A is in the
            // accepted set ⇒ direct payment ⇒ delivery equals funding.
            funding_mint: Some(mint_a.to_string()),
            delivery_mint: Some(mint_a.to_string()),
            agent_used: None,
            model_used: None,
            contribution: None,
        };

        // The pay request built through the REAL seal path threads the frozen mint.
        let request =
            authorize_request_from_bind(&bind, bind.amount_sats, bind.commit_oid.clone())
                .expect("request from bind");
        assert_eq!(
            request.realized_mint.as_deref(),
            Some(mint_a),
            "seal must thread the frozen mint into the pay request"
        );

        // Reproduce authorize_pay's mint selection + attempt-id derivation + wallet-open target for a
        // given LIVE config default (the value that changes between the two attempts). Returns the
        // attempt id, the resolved realized mint, and the payment terms (whose `mint` is what the
        // wallet-open seam consumes).
        let attempt_for = |config_default: &str| -> (String, MintUrl, PaymentTerms) {
            let selected = request.realized_mint.as_deref().unwrap_or(config_default);
            let mint = plan_payment(selected, &request.accepted_mints, true)
                .expect("plan payment")
                .realized_mint()
                .clone();
            let terms = PaymentTerms::new(
                mint.clone(),
                Amount::from(request.amount_sats),
                CurrencyUnit::Sat,
                seller_nostr,
                seller_p2pk,
            );
            let key = PaymentKey::new(
                JobId::new(&request.job_id).expect("job id"),
                ResultId::new(&request.result_id).expect("result id"),
                DeliveryIntegrityHash::from_hex(&request.delivery_integrity_hash).expect("oid"),
                JobHash::from_hex(&request.job_hash).expect("job hash"),
                &terms,
                request.creq_hash.clone(),
            );
            (key.attempt_id().as_str().to_owned(), mint, terms)
        };

        let (first_attempt, first_mint, _first_terms) = attempt_for(mint_a); // accept-time default
        let (retry_attempt, retry_mint, retry_terms) = attempt_for(mint_b); // flipped to B, retries

        // (a) attempt id / PaymentKey identity is preserved across the config-default flip.
        assert_eq!(
            first_attempt, retry_attempt,
            "config-default flip after accept must NOT shift the attempt id (no second payment identity)"
        );
        // (b) both attempts target the FROZEN mint A, never the flipped default B.
        assert_eq!(first_mint, MintUrl::from_str(mint_a).unwrap());
        assert_eq!(retry_mint, MintUrl::from_str(mint_a).unwrap());
        assert_ne!(retry_mint, MintUrl::from_str(mint_b).unwrap());

        // (c) THE WALLET-OPEN HALF OF CC. Drive the EXACT production wallet-open expression from
        // authorize_pay's :381 seam — `open_wallet_at_mint_async(home, &wallet_open_mint_url(home,
        // &terms))` — over the retry (home's LIVE default is now B) and observe the REAL opened
        // Wallet's bound mint. It MUST be the frozen mint A, never the flipped default B. Pre-fix
        // (:381 = `open_wallet_async(home)`) the wallet bound to B → budget appended, then the send
        // refuses on mint mismatch and strands the reservation. Reverting `wallet_open_mint_url` to
        // the live default flips this observed mint B↔A (red-on-revert).
        let opened = crate::buyer_fund::open_wallet_at_mint_async(
            &home,
            &wallet_open_mint_url(&home, &retry_terms),
        )
        .await
        .expect("open pay wallet");
        assert_eq!(
            opened.mint_url.to_string(),
            mint_a,
            "pay wallet must open at the FROZEN mint A even though the live config default is B"
        );
        assert_ne!(
            opened.mint_url.to_string(),
            home.config.default_mint(),
            "pay wallet must NOT open at the flipped live config default (B)"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn accept_bind_round_trips_on_disk() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-jobs-bind-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            // Distinct funding/delivery — a cross-mint bind — so the round-trip covers BOTH fields
            // (#495), not just their absence, and pins that the `realized_mint` alias does not clobber
            // the funding write on the way back out.
            funding_mint: Some("https://mint.minibits.cash/Bitcoin".into()),
            delivery_mint: Some("https://mint.cubabitcoin.org".into()),
            // Attribution fields round-trip as written (#261) — Some values here so this test
            // covers them, not just their absence.
            agent_used: Some("claude-agent-acp".into()),
            model_used: Some("claude-opus-5".into()),
            contribution: None,
        };
        write_accepted_bind(&home, &bind).expect("write");
        let loaded = load_accepted_bind(&home, &bind.job_id)
            .expect("load")
            .expect("present");
        assert_eq!(loaded, bind);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding CC + #495 (back-compat): the funding/delivery mint fields are serde-defaulted, so a
    // LEGACY bind JSON serialized before they existed still deserializes — into `None`, the legacy
    // pay-path fallback (live config default). Confirms the AcceptedBind change is not a wire break.
    #[test]
    fn accept_bind_deserializes_legacy_json_without_realized_mint() {
        let legacy = r#"{
            "job_id":"aa","claim_id":"bb","result_id":"cc","seller_pubkey":"dd",
            "commit_oid":"ee","repo":"https://example.invalid/repo.git","branch":"master",
            "job_hash":"ff","amount_sats":5,"accept_event_id":"11","accepted_at":1,
            "seller_signature":"","accepted_mints":["https://mint.example"]
        }"#;
        let bind: AcceptedBind = serde_json::from_str(legacy).expect("legacy bind deserializes");
        assert_eq!(bind.funding_mint, None, "missing field defaults to None (legacy)");
        assert_eq!(bind.delivery_mint, None, "missing field defaults to None (legacy)");
        assert_eq!(bind.accepted_mints, vec!["https://mint.example".to_string()]);

        // #495 rename alias: a bind written BEFORE the rename carries a `realized_mint` key holding the
        // funding selection. The `alias = "realized_mint"` must load it into `funding_mint` (same
        // value, corrected name) — dropping the alias would silently read it as None and regress the
        // pay path to the legacy config-default fallback for every pre-rename bind on disk.
        let pre_rename = r#"{
            "job_id":"aa","claim_id":"bb","result_id":"cc","seller_pubkey":"dd",
            "commit_oid":"ee","repo":"https://example.invalid/repo.git","branch":"master",
            "job_hash":"ff","amount_sats":5,"accept_event_id":"11","accepted_at":1,
            "seller_signature":"","accepted_mints":["https://mint.example"],
            "realized_mint":"https://mint.example"
        }"#;
        let aliased: AcceptedBind = serde_json::from_str(pre_rename).expect("pre-rename bind loads");
        assert_eq!(
            aliased.funding_mint.as_deref(),
            Some("https://mint.example"),
            "the realized_mint alias must load the sealed funding selection"
        );
        assert_eq!(aliased.delivery_mint, None, "pre-rename binds carry no delivery mint");
        // v5 attribution fields (#261): same back-compat contract — a legacy bind written before
        // they existed deserializes to None ("seller never reported"), never an error.
        assert_eq!(bind.agent_used, None, "legacy bind has no attribution");
        assert_eq!(bind.model_used, None, "legacy bind has no attribution");
        // And a bind with None mint fields does not serialize them (skip_serializing_if), so the
        // on-disk shape is unchanged for legacy-equivalent binds. Neither the corrected name nor the
        // old aliased name is emitted.
        let json = serde_json::to_string(&bind).expect("serialize");
        assert!(!json.contains("funding_mint"), "None must not emit the field: {json}");
        assert!(!json.contains("delivery_mint"), "None must not emit the field: {json}");
        assert!(!json.contains("realized_mint"), "None must not emit the aliased field: {json}");
        assert!(!json.contains("agent_used"), "None must not emit the field: {json}");
        assert!(!json.contains("model_used"), "None must not emit the field: {json}");

        // ROLLBACK direction: an OLDER binary reading a NEWER bind survives because AcceptedBind
        // tolerates unknown keys. This pins that `deny_unknown_fields` (house style on config
        // structs) never lands on the bind — adding it would make every rollback choke on binds
        // written by a newer release.
        let newer = r#"{
            "job_id":"aa","claim_id":"bb","result_id":"cc","seller_pubkey":"dd",
            "commit_oid":"ee","repo":"https://example.invalid/repo.git","branch":"master",
            "job_hash":"ff","amount_sats":5,"accept_event_id":"11","accepted_at":1,
            "seller_signature":"","accepted_mints":[],
            "some_future_field":{"nested":true}
        }"#;
        let tolerated: AcceptedBind =
            serde_json::from_str(newer).expect("unknown keys are ignored, never an error");
        assert_eq!(tolerated.job_id, "aa");
    }

    // #495 red-on-revert: on a cross-mint hop the accept-bind must record the DELIVERY (realized)
    // mint — the mint the seller is actually paid at — distinctly from the FUNDING (source) mint the
    // buyer melts. The single historical `realized_mint` field carried the SOURCE, so it named the
    // wrong mint on exactly this case (and matched only by accident on same-mint jobs). `seal_bind_mints`
    // derives BOTH from one plan; accept wires its outputs straight into `funding_mint`/`delivery_mint`.
    // Revert the fix (delivery ← source) and the hop assertion below goes red.
    #[test]
    fn accept_bind_seals_delivery_mint_distinct_from_funding_on_cross_mint() {
        let source = "https://a.example";
        let target = "https://b.example";
        // Buyer funded at `source`; seller accepts only `target` ⇒ no overlap ⇒ a hop.
        let plan = crate::crossmint::plan_payment(source, &[target.to_string()], true)
            .expect("cross-mint plan");
        assert!(plan.is_hop(), "distinct source/target must plan a hop");
        let (funding, delivery) = seal_bind_mints(&plan);
        assert_eq!(funding, source, "funding is the buyer's source mint (what the pay path spends)");
        assert_eq!(delivery, target, "delivery is the mint the seller is realized at (the hop target)");
        assert_ne!(funding, delivery, "cross-mint: the reported delivery mint is NOT the funding mint");

        // Sibling direct-payment case: the same mint on both sides ⇒ delivery equals funding (no hop),
        // which is precisely why the mis-report was invisible on same-mint jobs.
        let direct = crate::crossmint::plan_payment(source, &[source.to_string()], true)
            .expect("direct plan");
        assert!(!direct.is_hop(), "buyer mint in the accepted set is a direct payment");
        let (funding_direct, delivery_direct) = seal_bind_mints(&direct);
        assert_eq!(
            funding_direct, delivery_direct,
            "same-mint: funding equals delivery (the case that hid the defect)"
        );
    }

    // Producer/consumer drift guard (#261): the attribution the buyer reads off a result is the
    // SAME block the seller's exec-metadata stamps. Drives the REAL producer
    // (`seller_exec_metadata` → `git_result_draft`) into the real consumer (`result_attribution`)
    // so a renamed tag on either side goes red here instead of silently reading None forever.
    #[cfg(feature = "wallet")]
    #[test]
    fn result_attribution_reads_the_exec_metadata_the_seller_stamps() {
        let usage = crate::driver::UsageMetadata {
            model: Some("claude-opus-5".into()),
            ..Default::default()
        };
        let exec_metadata = crate::seller_exec::seller_exec_metadata(
            &["claude".into(), "--print".into()],
            None,
            1234,
            Some(&usage),
        );
        let draft = crate::gateway::git_result_draft(
            &"aa".repeat(32),
            &"bb".repeat(32),
            "https://example.invalid/repo.git",
            "main",
            &"e".repeat(40),
            2,
            &"f".repeat(64),
            &"ab".repeat(32),
            "delivery",
            &exec_metadata,
        );
        let (harness, model) = result_attribution(&draft.tags);
        assert_eq!(
            harness.as_deref(),
            Some("claude-agent-acp"),
            "the RESOLVED harness id from the real producer — what ran, not what was asked for"
        );
        assert_eq!(
            model.as_deref(),
            Some("claude-opus-5"),
            "the driver-reported model, absent unless the run surfaced one"
        );

        // Absent-stays-absent: a result with no metadata block yields no attribution — the
        // buyer records honest NULLs, never a fabricated or requested-echo value.
        let bare = crate::gateway::git_result_draft(
            &"aa".repeat(32),
            &"bb".repeat(32),
            "https://example.invalid/repo.git",
            "main",
            &"e".repeat(40),
            2,
            &"f".repeat(64),
            &"ab".repeat(32),
            "delivery",
            &[],
        );
        assert_eq!(result_attribution(&bare.tags), (None, None));
    }

    // Z1 (crash-safe durable bind write): the accept-bind is written via temp-file + sync + atomic
    // rename, so a completed write leaves the full bind at the target and NO leftover temp file, and
    // an overwrite of an existing bind is durable and leaves exactly one file. A truncating write
    // that crashed mid-flush could leave an empty/partial bind → the daemon re-accepts and pays a
    // SECOND time; the atomic path forbids that intermediate state.
    #[test]
    fn accept_bind_write_is_atomic_no_temp_leftover_and_durable_overwrite() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-jobs-bind-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            // Pending marker first (the pre-publish write), then finalize below.
            accept_event_id: String::new(),
            accepted_at: 0,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        write_accepted_bind(&home, &bind).expect("write pending");
        // Finalize (the second write in accept_claim) — overwrites the same target atomically.
        bind.accept_event_id = "11".repeat(32);
        bind.accepted_at = 42;
        write_accepted_bind(&home, &bind).expect("write finalized");

        let loaded = load_accepted_bind(&home, &bind.job_id)
            .expect("load")
            .expect("present");
        assert_eq!(loaded, bind, "finalized bind must be durable and complete");

        // No temp remnant and exactly one bind file (target only) — proves the rename replaced,
        // never left a partial temp beside it.
        let dir = home.root.join(JOBS_DIR);
        let mut json_files = 0usize;
        for entry in std::fs::read_dir(&dir).expect("read jobs dir") {
            let name = entry.expect("entry").file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"), "leftover temp file: {name}");
            if name.ends_with(".json") {
                json_files += 1;
            }
        }
        assert_eq!(json_files, 1, "exactly one bind json expected");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding W: two overlapping accepts for DIFFERENT results of one job must resolve to exactly
    // one durable bind — the loser observes the winner's bind and refuses. Models the accept
    // check→write critical section (acquire_job_lock → load → assert_single_settlement → write)
    // run concurrently from two threads against one shared home. The per-job flock serializes the
    // two, so the second thread reads the first's bind and `assert_single_settlement` refuses; on
    // disk exactly one result stays bound. Without the lock both could observe no bind and both
    // write.
    #[test]
    fn concurrent_accepts_for_different_results_bind_exactly_one() {
        use std::sync::{Arc, Barrier};
        let root = std::env::temp_dir().join(format!(
            "maxplayer-jobs-bind-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = Arc::new(home::bootstrap(&root).expect("home"));
        let job_id = "aa".repeat(32);

        let bind_for = |result_id: &str| AcceptedBind {
            job_id: job_id.clone(),
            claim_id: "bb".repeat(32),
            result_id: result_id.to_owned(),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };

        // The accept check→write critical section, guarded by the per-job lock.
        let attempt = |home: Arc<MaxplayerHome>, job_id: String, result_id: String, bind: AcceptedBind| {
            let _lock = acquire_job_lock(&home, &job_id)?;
            let existing = load_accepted_bind(&home, &job_id)?;
            assert_single_settlement(existing.as_ref(), &job_id, &result_id)?;
            write_accepted_bind(&home, &bind)?;
            Ok::<(), JobLifecycleError>(())
        };

        let barrier = Arc::new(Barrier::new(2));
        let results = ["cc".repeat(32), "dd".repeat(32)];
        let handles: Vec<_> = results
            .iter()
            .map(|result_id| {
                let home = Arc::clone(&home);
                let barrier = Arc::clone(&barrier);
                let job_id = job_id.clone();
                let result_id = result_id.clone();
                let bind = bind_for(&result_id);
                std::thread::spawn(move || {
                    barrier.wait();
                    attempt(home, job_id, result_id, bind)
                })
            })
            .collect();

        let outcomes: Vec<Result<(), JobLifecycleError>> =
            handles.into_iter().map(|h| h.join().expect("thread")).collect();

        let ok = outcomes.iter().filter(|r| r.is_ok()).count();
        let err = outcomes.iter().filter(|r| r.is_err()).count();
        assert_eq!(ok, 1, "exactly one accept must bind");
        assert_eq!(err, 1, "the other accept must refuse");
        let refusal = outcomes
            .iter()
            .find_map(|r| r.as_ref().err())
            .expect("one refusal")
            .to_string();
        assert!(
            refusal.contains("refusing to bind a different result"),
            "loser must refuse via single-settlement guard, got: {refusal}"
        );

        // Exactly one bind on disk, and it is one of the two contested results.
        let bound = load_accepted_bind(&home, &job_id)
            .expect("load")
            .expect("present");
        assert!(results.contains(&bound.result_id));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Finding L: accept writes a PENDING bind before publishing, then finalizes with the accept
    // event id. This models the two-phase durability primitive: a crash after the pending write
    // (but before/after publish) always leaves a local bind on disk, so a public accept can never
    // exist with no local record. Simulates the sequence write-pending → (publish) → finalize.
    #[test]
    fn accept_bind_pending_then_finalize_durability() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-jobs-bind-pending-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let mut bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            // Pending marker: written BEFORE publish (empty id, ts 0).
            accept_event_id: String::new(),
            accepted_at: 0,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        // Phase 1: pending write is durable and reloads with the empty-id marker.
        write_accepted_bind(&home, &bind).expect("write pending");
        let pending = load_accepted_bind(&home, &bind.job_id)
            .expect("load")
            .expect("present after pending write");
        assert!(
            pending.accept_event_id.is_empty(),
            "pending bind must carry the empty accept_event_id marker"
        );

        // Phase 2: finalize with the published id + timestamp overwrites the same record.
        bind.accept_event_id = "11".repeat(32);
        bind.accepted_at = 1;
        write_accepted_bind(&home, &bind).expect("finalize");
        let finalized = load_accepted_bind(&home, &bind.job_id)
            .expect("load")
            .expect("present after finalize");
        assert_eq!(finalized, bind);
        assert!(!finalized.accept_event_id.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authorize_from_bind_requires_buyer_tip_match_hash() {
        let bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        let err = authorize_request_from_bind(&bind, 1, String::new()).expect_err("empty hash");
        assert!(err.to_string().contains("delivery_integrity_hash"));

        // Buyer-supplied hash that disagrees with seller advertised commit_oid → refuse.
        let mismatch =
            authorize_request_from_bind(&bind, 1, "aa".repeat(20)).expect_err("mismatch");
        assert!(
            mismatch.to_string().contains("does not match accepted seller commit_oid"),
            "got: {mismatch}"
        );

        // Matching is allowed only when the buyer independently supplies that oid.
        let req = authorize_request_from_bind(&bind, 1, bind.commit_oid.clone()).expect("ok");
        assert_eq!(req.delivery_integrity_hash, bind.commit_oid);
        assert_eq!(req.seller_pubkey, bind.seller_pubkey);
        assert_eq!(req.commit_oid, bind.commit_oid);
    }

    #[test]
    fn assert_authorize_matches_bind_refuses_seller_mismatch() {
        let bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://github.com/bitcoin/bips.git".into(),
            branch: "master".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        };
        let bad_seller = "00".repeat(32);
        let err = assert_authorize_matches_bind(&bind, &bad_seller, &bind.result_id, &bind.commit_oid)
            .expect_err("mismatch");
        assert!(err.to_string().contains("seller"));
    }

    fn result_view(result_id: &str, seller_pubkey: &str) -> ResultView {
        ResultView {
            result_id: result_id.to_owned(),
            created_at: 100,
            seller_pubkey: seller_pubkey.to_owned(),
            display_name: None,
            job_hash: Some("ff".repeat(32)),
            repo: Some("https://github.com/bitcoin/bips.git".into()),
            branch: Some("master".into()),
            commit_oid: Some("ee".repeat(20)),
            amount_sats: Some(1),
            seller_signature: Some("ab".repeat(64)),
            harness: None,
            model: None,
            contribution: None,
        }
    }

    // CROSS-BIND TOOTH (accept path): an explicit `result_id` authored by a DIFFERENT seller
    // than the accepted claim's seller is REFUSED (the tool must not trust operator input) —
    // the live 21-sat cross-bind fixture shape (claim A + result B). An own-authored result,
    // selected explicitly OR auto, is unchanged.
    #[test]
    fn select_result_refuses_cross_authored_explicit_result_id() {
        let seller_a = "aa".repeat(32);
        let seller_b = "bb".repeat(32);
        let results = vec![
            result_view("result-b", &seller_b),
            result_view("result-a", &seller_a),
        ];

        // Claim seller A, explicit result authored by B → refuse, naming BOTH pubkeys.
        let err = select_result(&results, &seller_a, Some("result-b"))
            .expect_err("cross-authored explicit result_id must refuse");
        let message = err.to_string();
        assert!(
            message.contains(&seller_a) && message.contains(&seller_b),
            "refusal must name both the claim seller and the result author: {message}"
        );
        assert!(
            message.contains("cross-authored"),
            "refusal must be a clear cross-authored refusal: {message}"
        );

        // A's own result, selected explicitly → accepted unchanged.
        let own = select_result(&results, &seller_a, Some("result-a")).expect("own result ok");
        assert_eq!(own.result_id, "result-a");
        assert_eq!(own.seller_pubkey, seller_a);

        // Auto-select (no explicit id) → author-filtered to A, unchanged.
        let auto = select_result(&results, &seller_a, None).expect("auto own result ok");
        assert_eq!(auto.seller_pubkey, seller_a);
    }

    const SELLER_HEX: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn job_view_for(claims: Vec<ClaimView>, results: Vec<ResultView>) -> JobView {
        JobView {
            job_id: "ab".repeat(32),
            offer: None,
            claims,
            results,
            live_claim_id: None,
            accepted: None,
            pending: false,
            read_confirmed: true,
        }
    }

    #[test]
    fn a_claims_capability_reaches_the_award_predicate_from_the_tags_the_seller_emitted() {
        // The seller→buyer join, end to end through both real functions: `claim_draft` writes the
        // tags, `claim_view_from_tags` reads them. The award filter's "no match" and "nothing
        // parsed" are the same false, so this test pins the join itself rather than either side
        // alone.
        let capability = crate::heartbeat::SeatCapability::from_roster(
            &["claude".to_owned()],
            &[crate::heartbeat::RosterModel {
                harness: "claude".to_owned(),
                model: "claude-opus-5".to_owned(),
            }],
        );
        let draft = crate::gateway::claim_draft(
            "offer-id",
            "buyer-pubkey",
            "seller-pubkey",
            "creqA-test",
            &["claude".to_owned()],
            &capability,
        );

        let view = claim_view_from_tags(
            "claim-id".to_owned(),
            7,
            "seller-pubkey".to_owned(),
            "processing".to_owned(),
            &draft.tags,
        );
        assert_eq!(view.capability.harness_families, vec!["claude-code"]);
        assert_eq!(
            view.capability.models,
            vec![crate::heartbeat::HarnessModel {
                family: "claude-code".to_owned(),
                model: "claude-opus-5".to_owned(),
            }],
            "the buyer must recover the model the seller advertised, keyed by family"
        );
        assert!(!view.capability.is_unstated(), "a stated claim must not read as unstated");

        // THE DISCRIMINATOR, in the same test so it cannot be skipped separately: a claim carrying
        // NO capability tags must read as UNSTATED. Without this leg, the assertions above would
        // also pass a parser that fabricated a value, and without the leg above, a parser that read
        // nothing at all would look correct here.
        let bare = crate::gateway::claim_draft(
            "offer-id",
            "buyer-pubkey",
            "seller-pubkey",
            "creqA-test",
            &[],
            &Default::default(),
        );
        let bare_view = claim_view_from_tags(
            "claim-id".to_owned(),
            7,
            "seller-pubkey".to_owned(),
            "processing".to_owned(),
            &bare.tags,
        );
        assert!(
            bare_view.capability.is_unstated(),
            "a claim with no capability tags must read as unstated, not as a default that matches: \
             {:?}",
            bare_view.capability
        );

        // The predicate link of this chain — `filterable_tags` writes → `from_tags` reads → the
        // AWARD PREDICATE decides — lands with the award leg, which owns
        // `claim_meets_capability_request`. What this test proves is the first two links and the join
        // between them: the tags a seller emitted are what a buyer's reader produces, and an absent
        // capability reads as UNSTATED rather than as a default that matches.
        //
        // ⚠ Two passing links say nothing about a third. Do not read this test as covering the award
        // decision; it stops one link short by design.
    }

    // ── #540: collect resolves the DURABLE AWARD, not the exclusive live claim ────────────────
    // A claim with a chosen status + live flag; `award_for` is the buyer's parsed kind-3405 naming
    // the winning claim + its seller. These drive the resolver purely, no relay.
    fn claim_with(claim_id: &str, seller: &str, status: &str, live: bool) -> ClaimView {
        ClaimView {
            claim_id: claim_id.to_owned(),
            created_at: 100,
            seller_pubkey: seller.to_owned(),
            display_name: None,
            status: status.to_owned(),
            live,
            creq: None,
            agents: Vec::new(),
            capability: Default::default(),
        }
    }
    fn award_for(claim_id: &str, seller: &str) -> RelayedAward {
        RelayedAward {
            award_event_id: "aw".repeat(32),
            claim_id: claim_id.to_owned(),
            seller_pubkey: seller.to_owned(),
        }
    }

    // #540 REGRESSION (leg 1): the awarded claim A was DELIVERED, then a newer non-awarded claim B
    // became the single live one. collect must resolve A from the durable award — not the live set.
    // Red-on-revert: the old live-gate resolver filtered `live && delivered` (A not live, B not
    // delivered) → empty → refused, stranding a valid awarded delivery.
    #[test]
    fn select_resolves_the_awarded_claim_even_when_a_newer_claim_is_live() {
        let seller_a = "aa".repeat(32);
        let seller_b = "bb".repeat(32);
        let view = job_view_for(
            vec![
                claim_with("claim-b", &seller_b, "processing", true),
                claim_with("claim-a", &seller_a, CLAIM_STATUS_DELIVERED, false),
            ],
            vec![result_view("result-a", &seller_a)],
        );
        assert_eq!(
            select_deliverable_claim(&view, &award_for("claim-a", &seller_a))
                .expect("the awarded claim resolves despite B being live"),
            "claim-a"
        );
    }

    // leg 5 (B never receives A's award): even if the live non-awarded claim B ALSO delivered, the
    // award pins A. Red-on-revert: the old resolver returned the live+delivered claim → B.
    #[test]
    fn select_never_picks_a_live_non_awarded_claim_over_the_award() {
        let seller_a = "aa".repeat(32);
        let seller_b = "bb".repeat(32);
        let view = job_view_for(
            vec![
                claim_with("claim-b", &seller_b, "processing", true),
                claim_with("claim-a", &seller_a, CLAIM_STATUS_DELIVERED, false),
            ],
            vec![
                result_view("result-b", &seller_b),
                result_view("result-a", &seller_a),
            ],
        );
        assert_eq!(
            select_deliverable_claim(&view, &award_for("claim-a", &seller_a))
                .expect("the award pins A, never the live B"),
            "claim-a"
        );
    }

    // leg 2 (restart-between): resolution depends only on the durable award + per-claim payability,
    // never on WHICH claim is transiently live. A restart re-derives liveness; vary the live flag
    // (the thing a restart changes) and the awarded claim must resolve identically.
    #[test]
    fn select_resolution_is_invariant_under_live_claim_churn() {
        let seller_a = "aa".repeat(32);
        let seller_b = "bb".repeat(32);
        let award = award_for("claim-a", &seller_a);
        let before = job_view_for(
            vec![claim_with("claim-a", &seller_a, CLAIM_STATUS_DELIVERED, true)],
            vec![result_view("result-a", &seller_a)],
        );
        let after = job_view_for(
            vec![
                claim_with("claim-b", &seller_b, "processing", true),
                claim_with("claim-a", &seller_a, CLAIM_STATUS_DELIVERED, false),
            ],
            vec![result_view("result-a", &seller_a)],
        );
        assert_eq!(
            select_deliverable_claim(&before, &award).expect("before churn"),
            select_deliverable_claim(&after, &award).expect("after churn"),
            "the awarded claim must resolve the same regardless of which claim is live"
        );
    }

    // leg 4 (expired-A never pays): an awarded claim past its pay window surfaces EXPIRED; collect
    // must refuse it. Dropping the live gate must NOT re-admit an expired delivery — red-on-revert:
    // removing the payable/status check returns the claim here.
    #[test]
    fn select_refuses_an_expired_award_so_it_never_pays() {
        let seller_a = "aa".repeat(32);
        let view = job_view_for(
            vec![claim_with("claim-a", &seller_a, CLAIM_STATUS_EXPIRED, false)],
            vec![result_view("result-a", &seller_a)],
        );
        let err = select_deliverable_claim(&view, &award_for("claim-a", &seller_a))
            .expect_err("an expired award must refuse");
        assert!(matches!(err, JobLifecycleError::NotFound(_)), "unexpected: {err}");
        assert!(err.to_string().contains("pay window"), "message: {err}");
    }

    // The awarded claim is payable but its seller has published no git delivery yet ⇒ nothing to
    // collect. Fail-closed, named. Red-on-revert: removing the delivered check returns the claim.
    #[test]
    fn select_refuses_when_the_awarded_seller_has_not_delivered() {
        let seller_a = "aa".repeat(32);
        let view = job_view_for(
            vec![claim_with("claim-a", &seller_a, "processing", true)],
            Vec::new(),
        );
        let err = select_deliverable_claim(&view, &award_for("claim-a", &seller_a))
            .expect_err("no delivery must refuse");
        assert!(matches!(err, JobLifecycleError::NotFound(_)), "unexpected: {err}");
        assert!(err.to_string().contains("has not delivered"), "message: {err}");
    }

    // The award names a claim the relay view does not carry (a torn read) ⇒ refuse, never guess.
    #[test]
    fn select_refuses_when_the_awarded_claim_is_absent_from_the_view() {
        let seller_a = "aa".repeat(32);
        let seller_b = "bb".repeat(32);
        let view = job_view_for(
            vec![claim_with("claim-b", &seller_b, "processing", true)],
            vec![result_view("result-a", &seller_a)],
        );
        let err = select_deliverable_claim(&view, &award_for("claim-a", &seller_a))
            .expect_err("an absent awarded claim must refuse");
        assert!(matches!(err, JobLifecycleError::NotFound(_)), "unexpected: {err}");
    }

    fn claim_view(claim_id: &str, created_at: u64, status: &str) -> ClaimView {
        ClaimView {
            claim_id: claim_id.to_owned(),
            created_at,
            seller_pubkey: SELLER_HEX.to_owned(),
            display_name: None,
            status: status.to_owned(),
            live: false,
            creq: None,
            agents: Vec::new(),
            capability: Default::default(),
        }
    }

    /// A minimal delivery result authored by `seller_pubkey` at `created_at` — carries a
    /// `commit_oid` so [`delivery_pay_deadline`] counts it as a delivery.
    fn delivery_result(seller_pubkey: &str, created_at: u64) -> ResultView {
        ResultView {
            result_id: format!("res-{created_at}"),
            created_at,
            seller_pubkey: seller_pubkey.to_owned(),
            display_name: None,
            job_hash: Some("jh".to_owned()),
            repo: Some("relay://repo".to_owned()),
            branch: Some("maxplayer/delivery".to_owned()),
            commit_oid: Some("cc".repeat(20)),
            amount_sats: Some(10),
            seller_signature: Some("sig".to_owned()),
            harness: None,
            model: None,
            contribution: None,
        }
    }

    // A processing claim past its offer deadline with NO delivery surfaces as EXPIRED and is not
    // live. REAL claim/deadline path — a fixed `now` (injected), no relay, no wall-clock.
    #[test]
    fn processing_claim_past_deadline_without_delivery_is_expired_not_live() {
        let deadline = 1_700_000_000u64;
        let mut claims = vec![claim_view("orphan-claim", 100, "processing")];
        // now well past the deadline (still "processing" 25 min later), no result published.
        let live = derive_claim_liveness(&mut claims, &[], Some(deadline), deadline + 1_500);
        assert_eq!(live, None, "an expired claim must never be the live claim");
        assert_eq!(
            claims[0].status, CLAIM_STATUS_EXPIRED,
            "past-deadline processing claim with no delivery must surface as EXPIRED"
        );
        assert!(!claims[0].live, "expired claim must not read live/processing");
    }

    // A claim past the offer deadline whose seller DELIVERED inside the pay window stays payable
    // (DELIVERED, live) — the scheduling clock must not strand completed work.
    #[test]
    fn delivered_claim_past_deadline_within_pay_window_stays_payable() {
        let deadline = 1_700_000_000u64;
        let mut claims = vec![claim_view("delivered-claim", 100, "processing")];
        // Seller delivered right at the deadline; buyer collects a day later — past the offer
        // deadline but comfortably inside the pay window.
        let results = vec![delivery_result(SELLER_HEX, deadline)];
        let now = deadline + 24 * 3_600;
        let live = derive_claim_liveness(&mut claims, &results, Some(deadline), now);
        assert_eq!(
            claims[0].status, CLAIM_STATUS_DELIVERED,
            "a delivered claim past the offer deadline but within the pay window is payable"
        );
        assert_eq!(live.as_deref(), Some("delivered-claim"), "a delivered claim reads live");
        assert!(claims[0].live);
    }

    // Past the offer deadline AND past the delivery pay window ⇒ EXPIRED even with a delivery.
    // The pay window is generous but bounded.
    #[test]
    fn delivered_claim_past_pay_window_is_expired() {
        let deadline = 1_700_000_000u64;
        let mut claims = vec![claim_view("stale-claim", 100, "processing")];
        let results = vec![delivery_result(SELLER_HEX, deadline)];
        let now = deadline + DELIVERY_PAY_WINDOW_SECS + 1;
        let live = derive_claim_liveness(&mut claims, &results, Some(deadline), now);
        assert_eq!(
            claims[0].status, CLAIM_STATUS_EXPIRED,
            "a delivery whose pay window has itself lapsed is no longer payable"
        );
        assert_eq!(live, None);
    }

    // A delivery published by a DIFFERENT seller must not rescue this claim from expiry — the
    // pay window keys on the claim's own seller (guards against a stranger's result reviving a
    // lapsed claim into a pay path).
    #[test]
    fn foreign_seller_delivery_does_not_rescue_expired_claim() {
        let deadline = 1_700_000_000u64;
        let mut claims = vec![claim_view("mine", 100, "processing")];
        let results = vec![delivery_result(&"ee".repeat(32), deadline)];
        let live = derive_claim_liveness(&mut claims, &results, Some(deadline), deadline + 10);
        assert_eq!(
            claims[0].status, CLAIM_STATUS_EXPIRED,
            "a foreign seller's delivery never keeps this claim payable"
        );
        assert_eq!(live, None);
    }

    #[test]
    fn processing_claim_before_deadline_is_live_newest_wins() {
        let deadline = 1_700_000_000u64;
        let mut claims = vec![
            claim_view("newest", 200, "processing"),
            claim_view("older", 100, "processing"),
        ];
        let live = derive_claim_liveness(&mut claims, &[], Some(deadline), deadline - 10);
        assert_eq!(live.as_deref(), Some("newest"), "newest processing claim is live");
        assert!(claims[0].live && !claims[1].live);
        assert_eq!(claims[0].status, "processing", "not expired before the deadline");
    }

    // The SAME fixture flips live→expired purely by advancing the injected `now` — proves
    // expiry is derived from `now`, never stored (and that `now` is load-bearing input).
    #[test]
    fn liveness_flips_with_injected_now_only() {
        let deadline = 1_700_000_000u64;
        let make = || vec![claim_view("c1", 100, "processing")];

        let mut before = make();
        let live_before = derive_claim_liveness(&mut before, &[], Some(deadline), deadline - 1);
        assert_eq!(live_before.as_deref(), Some("c1"));
        assert!(before[0].live && before[0].status == "processing");

        let mut after = make();
        let live_after = derive_claim_liveness(&mut after, &[], Some(deadline), deadline + 1);
        assert_eq!(live_after, None);
        assert!(!after[0].live && after[0].status == CLAIM_STATUS_EXPIRED);
    }

    // No offer deadline known ⇒ expiry cannot be derived; status-based liveness preserved.
    // An `error` claim is never live regardless.
    #[test]
    fn no_deadline_preserves_status_and_error_never_live() {
        let mut claims = vec![
            claim_view("proc", 200, "processing"),
            claim_view("err", 100, "error"),
        ];
        let live = derive_claim_liveness(&mut claims, &[], None, 9_999_999_999);
        assert_eq!(live.as_deref(), Some("proc"));
        assert!(claims[0].live, "processing claim stays live when no deadline is known");
        assert!(!claims[1].live, "error claim is never live");
        assert_eq!(claims[0].status, "processing");
    }

    #[test]
    fn post_job_refuses_missing_seller_without_untargeted() {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-jobs-post-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let err = post_job(
            &home,
            PostJobRequest {
                task: "t".into(),
                output: "text/plain".into(),
                amount_sats: 1,
                seller_pubkey: None,
                untargeted: false,
                deadline_unix: Some(1_800_000_000),
                repo: None,
                branch: None,
                job: JobKind::FromScratch,
                requested_agent: None,
                requested_harness_family: None,
                requested_model: None,
                required_capabilities: Vec::new(),
            },
        )
        .expect_err("seller required");
        assert!(err.to_string().contains("seller_pubkey"));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp_job_home(label: &str) -> (std::path::PathBuf, crate::home::MaxplayerHome) {
        let root = std::env::temp_dir().join(format!(
            "maxplayer-jobs-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        (root, home)
    }

    // Read by `get_job_default_skips_kind_zero_fetch` only, which is `live-mints`-gated below; the
    // gate travels with its one caller so the offline build has no dead struct.
    #[cfg(feature = "live-mints")]
    #[derive(Debug)]
    struct CountMetadataQueries(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    #[cfg(feature = "live-mints")]
    impl nostr_relay_builder::prelude::QueryPolicy for CountMetadataQueries {
        fn admit_query<'a>(
            &'a self,
            query: &'a nostr_sdk::Filter,
            _addr: &'a std::net::SocketAddr,
        ) -> nostr_relay_builder::prelude::BoxedFuture<
            'a,
            nostr_relay_builder::prelude::PolicyResult,
        > {
            Box::pin(async move {
                if query
                    .kinds
                    .as_ref()
                    .is_some_and(|kinds| kinds.contains(&nostr_sdk::Kind::Metadata))
                {
                    self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                nostr_relay_builder::prelude::PolicyResult::Accept
            })
        }
    }

    // The ONE definition of "ready", asserted as a table so the two waiters cannot drift apart.
    // The poll path and the subscription path deliberately differ in MECHANISM; if they ever
    // differed in what they were waiting FOR, the same job would read complete to the daemon and
    // pending to the CLI. Extracting the predicate is what makes that unwritable — this pins it.
    #[test]
    fn readiness_is_one_shared_definition() {
        let mut view = JobView {
            job_id: "job".into(),
            offer: None,
            claims: Vec::new(),
            results: Vec::new(),
            live_claim_id: None,
            accepted: None,
            pending: false,
            read_confirmed: true,
        };
        assert!(!view_is_ready(&view, WaitFor::Claim));
        assert!(!view_is_ready(&view, WaitFor::Result));

        view.live_claim_id = Some("claim".into());
        assert!(view_is_ready(&view, WaitFor::Claim));
        assert!(
            !view_is_ready(&view, WaitFor::Result),
            "a live claim is not a delivery — waiting for a result must not be satisfied by one"
        );
    }

    /// A signed event carrying nothing but an `e` tag. The wake path inspects only that tag, so
    /// this is the whole of what a forwarded event contributes: the signal, never the truth.
    async fn wake_event(signer: &nostr_sdk::Keys, job_id: &str) -> std::sync::Arc<nostr_sdk::Event> {
        use nostr_sdk::prelude::{EventBuilder, Tag};
        let event = EventBuilder::new(nostr_sdk::Kind::Custom(JOB_RESULT_KIND), "")
            .tag(Tag::parse(["e", job_id]).expect("e tag"))
            .sign(signer)
            .await
            .expect("sign wake");
        std::sync::Arc::new(event)
    }

    // TOOTH — the wake primitive returns for every reason the caller must re-check for, and for
    // nothing else.
    //
    // `Lagged` is the reviewer trap: it means this receiver fell behind and events were DROPPED for
    // it — i.e. something happened. Returning re-checks; treating it as a failure would turn a busy
    // relay into a spurious timeout, which then looks like a relay fault rather than a client bug.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wake_returns_on_a_match_on_lag_and_on_expiry_but_not_on_an_unrelated_event() {
        let seller = nostr_sdk::Keys::generate();
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);

        // A matching event wakes it.
        tx.send(wake_event(&seller, "job-a").await).expect("send");
        tokio::time::timeout(
            Duration::from_secs(2),
            await_job_event(&mut rx, "job-a", Duration::from_secs(30)),
        )
        .await
        .expect("a matching event must wake the wait");

        // An event for a DIFFERENT job does not: waking on it would send the caller into a pointless
        // re-fetch on every other job's traffic.
        tx.send(wake_event(&seller, "job-b").await).expect("send");
        assert!(
            tokio::time::timeout(
                Duration::from_millis(400),
                await_job_event(&mut rx, "job-a", Duration::from_secs(30)),
            )
            .await
            .is_err(),
            "an unrelated job's event must not wake this wait"
        );

        // Lag wakes it — we cannot know what we missed, so the only safe response is to re-check.
        for index in 0..8 {
            tx.send(wake_event(&seller, &format!("flood-{index}")).await)
                .expect("send");
        }
        tokio::time::timeout(
            Duration::from_secs(2),
            await_job_event(&mut rx, "job-a", Duration::from_secs(30)),
        )
        .await
        .expect("Lagged must wake the wait — it is a re-check signal, not a failure");

        // And the window expiring returns rather than hanging.
        tokio::time::timeout(
            Duration::from_secs(2),
            await_job_event(&mut rx, "job-a", Duration::from_millis(200)),
        )
        .await
        .expect("the window must bound the wait");
    }

    // TOOTH — the wait resolves ON EVENT ARRIVAL, not on the safety re-check.
    //
    // This is the whole point of the piece: `get_job(wait_for=…)` used to sleep 400ms and then
    // rebuild a Client, reconnect, and run three sequential fetches, every iteration. Now an event
    // wakes it and it re-reads once.
    //
    // The TIMING assertion is what makes this a tooth rather than a demonstration. The result is
    // genuinely on the relay, so a wait with no event wake still succeeds — three seconds later, at
    // SAFETY_RECHECK. Asserting only "it returned ready" would pass with the subscription removed
    // entirely. Asserting it returned FASTER than the backstop is what proves the event did the work.
    //
    // The result is published by a SEPARATE identity (a seller), which is both the faithful shape
    // and the safe one: a single client can never observe its own published events.
    //
    // BITE: drop the `tx.send(...)` wake below and this goes red on elapsed, not on correctness.
    //
    // NETWORK (#720): the relay here is local, but `post_job_async` resolves a fee floor at the
    // home's mint BEFORE it publishes, and `temp_job_home` bootstraps the shipped default
    // (mint.minibits.cash). With no network that preflight refuses `mint_unreachable` and the
    // `.expect("post job")` below panics — the test never reaches its own subject. Not silenced:
    // `live-mints` is ON in the money-path CI job, which has a network. See the feature's comment
    // in Cargo.toml.
    #[cfg(feature = "live-mints")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wait_resolves_on_arrival_rather_than_on_the_safety_recheck() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let (root, mut home) = temp_job_home("wait-on-arrival");
        home.config.relay_url = relay.url().await.to_string();

        let posted = post_job_async(
            &home,
            PostJobRequest {
                task: "wake on arrival".into(),
                output: "text/plain".into(),
                amount_sats: 2,
                seller_pubkey: Some(nostr_sdk::Keys::generate().public_key().to_hex()),
                untargeted: false,
                deadline_unix: Some(now_unix() + 3_600),
                repo: None,
                branch: None,
                job: JobKind::FromScratch,
                requested_agent: None,
                requested_harness_family: None,
                requested_model: None,
                required_capabilities: Vec::new(),
            },
        )
        .await
        .expect("post job");
        let job_id = posted.job_id.clone();
        let buyer_pubkey = buyer_keys(&home).expect("keys").public_key().to_hex();

        let (tx, rx) = tokio::sync::broadcast::channel(8);
        let waiting_home = home.clone();
        let waiting_job = job_id.clone();
        let started = tokio::time::Instant::now();
        let waiter = tokio::spawn(async move {
            get_job_awaiting_events_async(
                &waiting_home,
                GetJobRequest {
                    job_id: waiting_job,
                    wait_for: Some(WaitFor::Result),
                    timeout_secs: Some(30),
                    include_display_names: false,
                },
                rx,
            )
            .await
        });

        // Let the catch-up fetch finish and the waiter park on the fan-out, so what we measure is
        // the wake and not a race with the first read.
        tokio::time::sleep(Duration::from_millis(400)).await;

        let seller = nostr_sdk::Keys::generate();
        let draft = crate::gateway::result_draft(
            &job_id,
            &buyer_pubkey,
            "text/plain",
            2,
            "job-hash",
            "seller-signature",
            "delivered",
            None,
            &[],
        );
        publish_draft_async(&home, &seller, &draft)
            .await
            .expect("publish the seller result");
        tx.send(wake_event(&seller, &job_id).await).expect("wake");

        let view = waiter.await.expect("join").expect("wait");
        let elapsed = started.elapsed();
        assert!(!view.pending, "the wait must report the job ready");
        assert!(!view.results.is_empty(), "the delivered result must be in the view");
        assert!(
            elapsed < SAFETY_RECHECK,
            "the wait must resolve on the EVENT, not on the {SAFETY_RECHECK:?} backstop — took \
             {elapsed:?}; if this fails on timing alone the subscription is not doing the work"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // NETWORK (#720): same shape as the wait test above — the relay is local, but the
    // `post_job_async` fee floor reaches the home's shipped default mint before publishing, so this
    // cannot run under `net: denied`. ON in the money-path CI job; see `live-mints` in Cargo.toml.
    #[cfg(feature = "live-mints")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_job_default_skips_kind_zero_fetch() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let metadata_queries =
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let relay = LocalRelay::new(
            RelayBuilder::default()
                .query_policy(CountMetadataQueries(std::sync::Arc::clone(&metadata_queries))),
        );
        relay.run().await.expect("relay run");

        let (root, mut home) = temp_job_home("display-name-opt-in");
        home.config.relay_url = relay.url().await.to_string();
        let posted = post_job_async(
            &home,
            PostJobRequest {
                task: "test display-name opt-in".into(),
                output: "text/plain".into(),
                amount_sats: 2,
                seller_pubkey: Some(nostr_sdk::Keys::generate().public_key().to_hex()),
                untargeted: false,
                deadline_unix: Some(now_unix() + 3_600),
                repo: None,
                branch: None,
                job: JobKind::FromScratch,
                requested_agent: None,
                requested_harness_family: None,
                requested_model: None,
                required_capabilities: Vec::new(),
            },
        )
        .await
        .expect("post job");

        let default_view = get_job_async(
            &home,
            GetJobRequest {
                job_id: posted.job_id.clone(),
                wait_for: None,
                timeout_secs: None,
                include_display_names: false,
            },
        )
        .await
        .expect("default get_job");
        assert!(default_view.offer.is_some(), "fixture offer must be fetched");
        assert_eq!(
            metadata_queries.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "default get_job must not issue a kind-0 profile query"
        );

        get_job_async(
            &home,
            GetJobRequest {
                job_id: posted.job_id,
                wait_for: None,
                timeout_secs: None,
                include_display_names: true,
            },
        )
        .await
        .expect("opt-in get_job");
        assert!(
            metadata_queries.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "include_display_names=true must issue a kind-0 profile query"
        );

        let _ = std::fs::remove_dir_all(root);
        relay.shutdown();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_job_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-post");
        let err = post_job(
            &home,
            PostJobRequest {
                task: "t".into(),
                output: "text/plain".into(),
                amount_sats: 1,
                seller_pubkey: Some("aa".repeat(32)),
                untargeted: false,
                deadline_unix: Some(1_800_000_000),
                repo: None,
                branch: None,
                job: JobKind::FromScratch,
                requested_agent: None,
                requested_harness_family: None,
                requested_model: None,
                required_capabilities: Vec::new(),
            },
        )
        .expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_job_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-get");
        let err = get_job(
            &home,
            GetJobRequest {
                job_id: "aa".repeat(32),
                wait_for: None,
                timeout_secs: None,
                include_display_names: false,
            },
        )
        .expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accept_claim_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-accept");
        let err = accept_claim(
            &home,
            AcceptClaimRequest {
                job_id: "aa".repeat(32),
                claim_id: "bb".repeat(32),
                result_id: None,
            },
        )
        .expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publish_draft_sync_refuses_inside_runtime() {
        let (root, home) = temp_job_home("nested-publish-draft");
        let keys = nostr_sdk::Keys::generate();
        let draft = EventDraft::new(JOB_OFFER_KIND, Vec::new(), "nested-guard");
        let err = publish_draft(&home, &keys, &draft).expect_err("must refuse nested block_on");
        assert!(
            err.to_string().contains("nested block_on refused"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("publish_draft"),
            "op name missing: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Accept-path: contribution resolution (echo-equality + fail-closed) ─────
    fn offer_view_contribution(owner: &str, url: &str, base_branch: &str, base_oid: &str) -> OfferView {
        OfferView {
            event_id: "of".repeat(16),
            created_at: 1,
            author_pubkey: "aa".repeat(32),
            author_display_name: None,
            task: "t".into(),
            output: "o".into(),
            amount_sats: 1,
            deadline_unix: 10,
            seller_pubkey: None,
            seller_display_name: None,
            targeted: false,
            repo: None,
            branch: None,
            job_class: Some(crate::contribution::JOB_CLASS_CONTRIBUTION.to_owned()),
            contribution: Some(ContributionOfferView {
                target_owner_pubkey: owner.to_owned(),
                target_clone_url: url.to_owned(),
                base_branch: base_branch.to_owned(),
                base_oid: base_oid.to_owned(),
                accepts: vec!["fork".into()],
            }),
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: Vec::new(),
        }
    }

    fn result_view_contribution(owner: &str, url: &str, base_branch: &str, base_oid: &str, sig: &str) -> ResultView {
        let mut r = result_view(&"cc".repeat(32), &"dd".repeat(32));
        r.contribution = Some(ContributionResultView {
            target_owner_pubkey: owner.to_owned(),
            target_clone_url: url.to_owned(),
            base_branch: base_branch.to_owned(),
            base_oid: base_oid.to_owned(),
            tuple_signature: sig.to_owned(),
        });
        r
    }

    #[test]
    fn accept_contribution_records_offer_authority_and_store_ref() {
        let owner = "aa".repeat(32);
        let url = "https://relay.maxplayer.test/git/owner/repo.git";
        let base_oid = "77".repeat(20);
        let offer = offer_view_contribution(&owner, url, "main", &base_oid);
        let result = result_view_contribution(&owner, url, "main", &base_oid, "sigbytes");
        let bind = resolve_accepted_contribution(&offer, &result, &"ee".repeat(20))
            .expect("resolve ok")
            .expect("is contribution");
        // Authority = the OFFER, not the echo.
        assert_eq!(bind.target_owner_pubkey, owner);
        assert_eq!(bind.base_oid, base_oid);
        assert_eq!(bind.tuple_signature, "sigbytes");
        assert_eq!(
            bind.store_ref,
            crate::delivery_git::PayPathDeliveryVerifier::store_ref_for(&"ee".repeat(20))
        );
    }

    #[test]
    fn accept_contribution_refuses_echo_target_mismatch() {
        let url = "https://relay.maxplayer.test/git/owner/repo.git";
        let base_oid = "77".repeat(20);
        let offer = offer_view_contribution(&"aa".repeat(32), url, "main", &base_oid);
        // Result echoes a DIFFERENT target owner — equality-check refuses.
        let result = result_view_contribution(&"bb".repeat(32), url, "main", &base_oid, "s");
        let err = resolve_accepted_contribution(&offer, &result, &"ee".repeat(20))
            .expect_err("echo target mismatch must refuse");
        assert!(err.to_string().contains("echo mismatch"), "got {err}");
    }

    #[test]
    fn accept_contribution_refuses_echo_base_mismatch() {
        let owner = "aa".repeat(32);
        let url = "https://relay.maxplayer.test/git/owner/repo.git";
        let offer = offer_view_contribution(&owner, url, "main", &"77".repeat(20));
        // Result echoes a DIFFERENT base_oid.
        let result = result_view_contribution(&owner, url, "main", &"88".repeat(20), "s");
        let err = resolve_accepted_contribution(&offer, &result, &"ee".repeat(20))
            .expect_err("echo base mismatch must refuse");
        assert!(err.to_string().contains("echo mismatch"), "got {err}");
    }

    #[test]
    fn accept_malformed_contribution_offer_fails_closed_not_from_scratch() {
        // job_class=contribution but pins failed to parse (contribution=None) ⇒ REFUSE.
        let mut offer = offer_view_contribution(&"aa".repeat(32), "https://x/r.git", "main", &"77".repeat(20));
        offer.contribution = None; // simulate a malformed contribution offer
        let result = result_view(&"cc".repeat(32), &"dd".repeat(32));
        let err = resolve_accepted_contribution(&offer, &result, &"ee".repeat(20))
            .expect_err("malformed contribution offer must refuse (fail-closed)");
        assert!(err.to_string().contains("malformed"), "got {err}");
    }

    #[test]
    fn accept_contribution_requires_a_contribution_result() {
        let owner = "aa".repeat(32);
        let url = "https://relay.maxplayer.test/git/owner/repo.git";
        let offer = offer_view_contribution(&owner, url, "main", &"77".repeat(20));
        // A from-scratch result (no contribution echo) against a contribution offer ⇒ refuse.
        let result = result_view(&"cc".repeat(32), &"dd".repeat(32));
        assert!(resolve_accepted_contribution(&offer, &result, &"ee".repeat(20)).is_err());
    }

    #[test]
    fn from_scratch_offer_resolves_to_no_contribution() {
        let mut offer = offer_view_contribution(&"aa".repeat(32), "https://x/r.git", "main", &"77".repeat(20));
        offer.job_class = None;
        offer.contribution = None;
        let result = result_view(&"cc".repeat(32), &"dd".repeat(32));
        assert_eq!(
            resolve_accepted_contribution(&offer, &result, &"ee".repeat(20)).expect("ok"),
            None
        );
    }

    #[test]
    fn authorize_request_from_bind_threads_contribution() {
        let mut bind = AcceptedBind {
            job_id: "aa".repeat(32),
            claim_id: "bb".repeat(32),
            result_id: "cc".repeat(32),
            seller_pubkey: "dd".repeat(32),
            commit_oid: "ee".repeat(20),
            repo: "https://relay.maxplayer.test/git/seller/fork.git".into(),
            branch: "maxplayer/contribution/x".into(),
            job_hash: "ff".repeat(32),
            amount_sats: 1,
            accept_event_id: "11".repeat(32),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: Some(AcceptedContribution {
                target_owner_pubkey: "aa".repeat(32),
                target_clone_url: "https://relay.maxplayer.test/git/owner/repo.git".into(),
                base_branch: "main".into(),
                base_oid: "77".repeat(20),
                tuple_signature: "cafe".into(),
                store_ref: "refs/maxplayer/deliveries/eeee".into(),
            }),
        };
        let req = authorize_request_from_bind(&bind, 1, bind.commit_oid.clone()).expect("ok");
        let c = req.contribution.expect("threaded");
        assert_eq!(c.target_owner_pubkey, "aa".repeat(32));
        assert_eq!(c.base_oid, "77".repeat(20));
        assert_eq!(c.tuple_signature, "cafe");
        // From-scratch bind ⇒ None threaded.
        bind.contribution = None;
        let req2 = authorize_request_from_bind(&bind, 1, bind.commit_oid.clone()).expect("ok");
        assert!(req2.contribution.is_none());
    }

    #[test]
    fn contribution_offer_view_parses_pins_and_malformed_is_none() {
        // A well-formed contribution offer's tags parse into the view.
        let offer = crate::contribution::ContributionOffer {
            target: crate::contribution::TargetRepoPin::new(
                "aa".repeat(32),
                "https://relay.maxplayer.test/git/owner/repo.git",
            )
            .unwrap(),
            base: crate::contribution::ContributionBase::new("main", "77".repeat(20)).unwrap(),
            accepts: vec!["fork".into()],
        };
        let tags = crate::contribution::contribution_offer_tags(&offer);
        let view = contribution_offer_view(&tags).expect("parsed");
        assert_eq!(view.target_owner_pubkey, "aa".repeat(32));
        assert_eq!(view.base_oid, "77".repeat(20));
        // A contribution offer missing the base tag ⇒ view None (surfaced as job_class-present +
        // contribution-None, which accept refuses).
        let malformed = vec![crate::gateway::TagSpec::new([
            crate::contribution::TAG_JOB_CLASS,
            crate::contribution::JOB_CLASS_CONTRIBUTION,
        ])];
        assert!(contribution_offer_view(&malformed).is_none());
    }

    // ── Buyer POST-path: contribution offer spec (validation + tag emission) ─────
    fn contribution_spec(
        owner: &str,
        url: &str,
        branch: &str,
        oid: &str,
        accepts: Option<Vec<String>>,
    ) -> ContributionSpec {
        ContributionSpec {
            target_repo_owner: owner.into(),
            target_repo_url: url.into(),
            base_branch: branch.into(),
            base_oid: oid.into(),
            accepts,
        }
    }

    fn contribution_post_request(
        owner: &str,
        url: &str,
        branch: &str,
        oid: &str,
        accepts: Option<Vec<String>>,
    ) -> PostJobRequest {
        PostJobRequest {
            task: "t".into(),
            output: "text/plain".into(),
            amount_sats: 1,
            seller_pubkey: None,
            untargeted: true,
            deadline_unix: Some(10),
            repo: None,
            branch: None,
            job: JobKind::Contribution(contribution_spec(owner, url, branch, oid, accepts)),
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: Vec::new(),
        }
    }

    #[test]
    fn post_job_contribution_round_trip_offer_tags_bind_to_offer_values() {
        // The load-bearing round-trip: post_job contribution params -> BUILT event tags ->
        // parse_contribution_offer yields exactly {owner,url,branch,oid} -> emitted tags ARE the
        // canonical constructor output (no drift) -> the accept-path binds to the OFFER's values.
        let owner = "aa".repeat(32);
        let url = "https://relay.maxplayer.test/git/owner/repo.git";
        let base_oid = "77".repeat(20);
        let request = contribution_post_request(&owner, url, "main", &base_oid, None);

        let spec = match &request.job {
            JobKind::Contribution(spec) => spec,
            JobKind::FromScratch => panic!("built a contribution request"),
        };
        let contribution =
            contribution_offer_from_spec(spec).expect("valid contribution params");
        let draft =
            build_offer_draft(&request, 10, Some(&contribution)).expect("draft built");

        // (a) canonical parse of the BUILT tags yields exactly the pinned values.
        let parsed = crate::contribution::parse_contribution_offer(&draft.tags)
            .expect("parse ok")
            .expect("is a contribution");
        assert_eq!(parsed.target.owner_pubkey(), owner);
        assert_eq!(parsed.target.clone_url(), url);
        assert_eq!(parsed.base.branch(), "main");
        assert_eq!(parsed.base.oid(), base_oid);
        assert!(parsed.accepts_fork());

        // (b) emitted tags ARE the canonical constructor output (no drift).
        let expected_tags = crate::contribution::contribution_offer_tags(&contribution);
        assert!(
            draft.tags.ends_with(&expected_tags),
            "emitted contribution tags must equal the canonical constructor output"
        );

        // (c) the accept-path binds to the OFFER's values, threaded from the EMITTED tags.
        let mut offer_view = offer_view_contribution(&owner, url, "main", &base_oid);
        offer_view.contribution = contribution_offer_view(&draft.tags);
        let result = result_view_contribution(&owner, url, "main", &base_oid, "sigbytes");
        let bind = resolve_accepted_contribution(&offer_view, &result, &"ee".repeat(20))
            .expect("resolve ok")
            .expect("is a contribution");
        assert_eq!(bind.target_owner_pubkey, owner);
        assert_eq!(bind.target_clone_url, url);
        assert_eq!(bind.base_branch, "main");
        assert_eq!(bind.base_oid, base_oid);
    }

    /// A from-scratch post request carrying a capability request, for the gate tests below.
    fn post_request_requesting(
        agent: Option<&str>,
        family: Option<&str>,
        model: Option<&str>,
        capabilities: &[&str],
    ) -> PostJobRequest {
        PostJobRequest {
            task: "t".into(),
            output: "text/plain".into(),
            amount_sats: 3,
            seller_pubkey: Some("bb".repeat(32)),
            untargeted: false,
            deadline_unix: Some(10),
            repo: None,
            branch: None,
            job: JobKind::FromScratch,
            requested_agent: agent.map(str::to_owned),
            requested_harness_family: family.map(str::to_owned),
            requested_model: model.map(str::to_owned),
            required_capabilities: capabilities.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    // #897 — the post path REFUSES a request no seat could ever satisfy, before any event is built.
    //
    // Posting commits: it arms the auto-award and puts a signed offer on the relay with its deadline
    // running. So an unsatisfiable request is not merely useless, it converts a caller's typo into a
    // committed offer and a guaranteed park. Post time is the cheapest moment it can surface.
    //
    // Asserted on the ERROR, and separately on the draft NOT being built: a gate that returned the
    // error after emitting the event would satisfy an error-only assertion.
    #[test]
    fn post_job_refuses_a_request_no_seat_could_satisfy() {
        // Out-of-vocabulary family: no seat can advertise it, because families reach the wire only
        // through `harness_family_for_preset`.
        let error = build_offer_draft(&post_request_requesting(None, Some("gpt-cli"), None, &[]), 10, None)
            .expect_err("an unknown harness family must be refused");
        assert!(
            error.to_string().contains("gpt-cli") && error.to_string().contains("not a known family"),
            "the error must name the value AND the vocabulary, so a caller can fix it without \
             reading our source: {error}"
        );
        assert!(
            error.to_string().contains("codex"),
            "the error must list the known families — naming the defect without the alternatives \
             makes the caller guess: {error}"
        );

        // Out-of-vocabulary capability token.
        let error =
            build_offer_draft(&post_request_requesting(None, None, None, &["kubernetes"]), 10, None)
            .expect_err("an unknown capability token must be refused");
        assert!(
            error.to_string().contains("kubernetes") && error.to_string().contains("node"),
            "the error must name the bad token and the known ones: {error}"
        );

        // A model with no PRESET — refused by the SATISFIABILITY gate, which asks the award predicate
        // rather than restating the rule. A family does not rescue it: dispatch never reads one, so
        // the second row here is the one that would silently execute on the wrong harness.
        for (agent, family) in [(None, None), (None, Some("claude-code"))] {
            let error =
                build_offer_draft(&post_request_requesting(agent, family, Some("opus"), &[]), 10, None)
                    .expect_err("a model with no harness preset must be refused");
            assert!(
                error.to_string().contains("no seat could ever satisfy"),
                "family={family:?}: unsatisfiable by construction, and the error should say so \
                 rather than reading as a vocabulary complaint: {error}"
            );
        }
        let error =
            build_offer_draft(&post_request_requesting(None, None, Some("opus"), &[]), 10, None)
                .expect_err("a model with no harness preset must be refused");
        assert!(
            error.to_string().contains("no seat could ever satisfy"),
            "a model-only request is unsatisfiable by construction and the error should say so \
             rather than reading as a vocabulary complaint: {error}"
        );
        assert!(
            error.to_string().contains("opus"),
            "the error must name the model the caller asked for: {error}"
        );

        // A preset and a family naming DIFFERENT harnesses. Dispatch honours the preset, so this
        // offer would ask for codex and run Claude — and because a multi-harness seat advertises
        // both families, no claim-level check can see it. Refused before the offer is signed.
        let error = build_offer_draft(
            &post_request_requesting(Some("claude"), Some("codex"), None, &[]),
            10,
            None,
        )
        .expect_err("a family contradicting the preset must be refused");
        assert!(
            error.to_string().contains("claude") && error.to_string().contains("codex"),
            "the error must name BOTH sides of the contradiction so the caller knows which to \
             change: {error}"
        );

        // CONTROLS: every satisfiable shape must still post. Without these the assertions above are
        // equally explained by a gate that refuses everything with a capability request on it.
        for (agent, family, model, capabilities) in [
            (None, None, None, Vec::new()),
            (None, Some("codex"), None, Vec::new()),
            (Some("codex"), None, None, Vec::new()),
            (Some("codex"), Some("codex"), Some("gpt-5.6-sol[low]"), Vec::new()),
            // The family DERIVED from the preset rather than stated — the shape a caller reaches
            // for first, and the one a stricter rule would have broken.
            (Some("codex"), None, Some("gpt-5.6-sol[low]"), Vec::new()),
            (None, None, None, vec!["rust"]),
            (None, Some("claude-code"), None, vec!["rust", "node"]),
        ] {
            let request = post_request_requesting(agent, family, model, &capabilities);
            assert!(
                build_offer_draft(&request, 10, None).is_ok(),
                "control: {agent:?}/{family:?}/{model:?}/{capabilities:?} is satisfiable and must post"
            );
        }
    }

    // The request reaches the EMITTED TAGS, and an absent request emits nothing (#897).
    //
    // The gateway tests cover the draft→tags→parse round trip. This covers the seam ABOVE it: that
    // `post_job`'s own request object is what feeds that round trip, rather than the fields being
    // carried on the type and dropped on the way to the event.
    #[test]
    fn post_job_emits_the_capability_request_it_was_given() {
        let draft = build_offer_draft(
            &post_request_requesting(
                Some("codex"),
                Some("codex"),
                Some("gpt-5.6-sol[low]"),
                &["rust"],
            ),
            10,
            None,
        )
        .expect("draft");

        let param = |name: &str| {
            draft
                .tags
                .iter()
                .find(|tag| {
                    tag.first() == Some("param") && tag.0.get(1).map(String::as_str) == Some(name)
                })
                .map(|tag| tag.0[2..].to_vec())
        };
        assert_eq!(param("harness_family"), Some(vec!["codex".to_owned()]));
        assert_eq!(param("harness_model"), Some(vec!["gpt-5.6-sol[low]".to_owned()]));
        assert_eq!(param("capability"), Some(vec!["rust".to_owned()]));

        // And a post with no request is byte-identical to one built before any of this existed —
        // the property that makes filtering opt-in on the wire, not just in the predicate.
        let plain = build_offer_draft(&post_request_requesting(None, None, None, &[]), 10, None)
            .expect("draft");
        assert_eq!(
            plain,
            OfferDraft::new("t", "text/plain", 3, 10, "bb".repeat(32)).to_event_draft(),
            "a post with no capability request must emit the pre-#897 offer exactly"
        );
    }

    #[test]
    fn post_job_from_scratch_emits_byte_identical_tags() {
        // No contribution params ⇒ Ok(None) ⇒ built tags are byte-identical to the bare offer.
        let request = PostJobRequest {
            task: "t".into(),
            output: "text/plain".into(),
            amount_sats: 3,
            seller_pubkey: Some("bb".repeat(32)),
            untargeted: false,
            deadline_unix: Some(10),
            repo: None,
            branch: None,
            job: JobKind::FromScratch,
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: Vec::new(),
        };
        let contribution: Option<crate::contribution::ContributionOffer> = match &request.job {
            JobKind::FromScratch => None,
            JobKind::Contribution(spec) => Some(contribution_offer_from_spec(spec).expect("ok")),
        };
        assert!(contribution.is_none(), "from-scratch ⇒ no contribution offer");
        let draft = build_offer_draft(
            &request,
            10,
            contribution.as_ref(),
        )
        .expect("draft");
        let expected = OfferDraft::new(
            "t",
            "text/plain",
            3,
            10,
            "bb".repeat(32),
        )
        .to_event_draft();
        assert_eq!(draft, expected, "from-scratch draft must be byte-identical");
        assert!(!crate::contribution::is_contribution_tags(&draft.tags));
        // The budget guard fires ONLY over-cap and does NOT touch tag emission — a normal
        // within-cap post (amount 3, well within the default cap) passes the guard, so emitted
        // tags for a normal post are unchanged (byte-identical, asserted above).
        assert!(
            assert_amount_within_budget_cap(3, crate::home::DEFAULT_PER_JOB_BUDGET_SATS).is_ok(),
            "a within-cap post must pass the budget guard"
        );
    }

    // Partial-param (all-or-nothing) refusal moved to the flat-args → `JobKind` mapping in the MCP
    // tool layer: a partial contribution set is now unrepresentable in `PostJobRequest`, so the core
    // no longer has a partial case to refuse (the type enforces it at compile time).

    #[test]
    fn post_job_contribution_bad_fields_refuse() {
        let owner = "aa".repeat(32);
        let url = "https://relay.maxplayer.test/git/owner/repo.git";
        let oid = "77".repeat(20);
        // bad owner (not 64-hex)
        assert!(
            contribution_offer_from_spec(&contribution_spec("nothex", url, "main", &oid, None))
                .is_err()
        );
        // bad oid (not 40 lowercase hex)
        assert!(
            contribution_offer_from_spec(&contribution_spec(&owner, url, "main", "xyz", None))
                .is_err()
        );
        // bad base branch (leading dash)
        assert!(
            contribution_offer_from_spec(&contribution_spec(&owner, url, "-x", &oid, None)).is_err()
        );
        // bad url (forbidden scheme via the transport allowlist)
        assert!(contribution_offer_from_spec(&contribution_spec(
            &owner,
            "file:///tmp/repo.git",
            "main",
            &oid,
            None
        ))
        .is_err());
        // accepts present but without "fork" (fork is the only supported delivery) ⇒ refuse.
        assert!(contribution_offer_from_spec(&contribution_spec(
            &owner,
            url,
            "main",
            &oid,
            Some(vec!["patch".into()])
        ))
        .is_err());
    }

    #[test]
    fn post_job_contribution_requires_exactly_40_lowercase_hex_base_oid() {
        let owner = "aa".repeat(32);
        let url = "https://relay.maxplayer.test/git/owner/repo.git";

        let malformed = contribution_spec(&owner, url, "main", &"a".repeat(64), None);
        let error = contribution_offer_from_spec(&malformed)
            .expect_err("post validation must refuse a 64-hex base_oid");
        assert!(
            matches!(&error, JobLifecycleError::Input(message) if message.contains(
                "base_oid must be exactly 40 lowercase hex chars"
            )),
            "post refusal must identify the malformed base_oid: {error}"
        );

        let valid = contribution_spec(&owner, url, "main", &"a".repeat(40), None);
        let offer = contribution_offer_from_spec(&valid)
            .expect("post validation must accept a 40-lowercase-hex base_oid");
        assert_eq!(offer.base.oid(), "a".repeat(40));

        let padded = contribution_spec(
            &owner,
            url,
            "main",
            &format!(" {} ", "a".repeat(40)),
            None,
        );
        assert!(
            contribution_offer_from_spec(&padded).is_err(),
            "post validation must not normalize a non-exact base_oid"
        );
    }

    #[test]
    fn post_job_contribution_refuses_ext_url_at_post() {
        // ext:: clone URL refused at POST time — a buyer must not publish an unverifiable offer.
        let owner = "aa".repeat(32);
        let oid = "77".repeat(20);
        let err = contribution_offer_from_spec(&contribution_spec(
            &owner,
            "ext::sh -c evil",
            "main",
            &oid,
            None,
        ))
        .expect_err("ext refused at post");
        assert!(err.to_string().contains("refused"), "{err}");
    }

    // ── Post-time per-job budget-cap validation ───────────────────────────────────
    #[test]
    fn budget_cap_guard_over_cap_refuses_at_and_under_cap_pass() {
        // over-cap ⇒ refuse, naming the config key + BOTH numbers + the restart remedy.
        let err = assert_amount_within_budget_cap(40, 21).expect_err("over-cap refused");
        let msg = err.to_string();
        assert!(msg.contains("per_job_budget_sats"), "names the config key: {msg}");
        assert!(msg.contains("40"), "names the amount: {msg}");
        assert!(msg.contains("21"), "names the cap: {msg}");
        assert!(msg.contains("RESTART"), "names the remedy: {msg}");
        // at-cap ⇒ passes (mirrors the budget gate's `amount > cap` refuse condition).
        assert!(assert_amount_within_budget_cap(21, 21).is_ok(), "at-cap must pass");
        // under-cap ⇒ passes, unchanged.
        assert!(assert_amount_within_budget_cap(20, 21).is_ok(), "under-cap must pass");
        // The shipped default per-job cap (30_000, #378) binds too: one over refuses.
        assert!(
            assert_amount_within_budget_cap(
                crate::home::DEFAULT_PER_JOB_BUDGET_SATS + 1,
                crate::home::DEFAULT_PER_JOB_BUDGET_SATS,
            )
            .is_err(),
            "one over the shipped default per-job cap must refuse"
        );
    }

    #[test]
    fn post_job_deadline_past_refused_names_field_and_values() {
        let err = resolve_post_deadline(Some(1_700_000_000), 1_700_000_001)
            .expect_err("past deadline must refuse");
        let msg = err.to_string();
        assert!(msg.contains("deadline_unix"), "names the field: {msg}");
        assert!(msg.contains("given=1700000000"), "shows given value: {msg}");
        assert!(msg.contains("current=1700000001"), "shows current value: {msg}");
    }

    #[test]
    fn post_job_deadline_zero_refused() {
        let err = resolve_post_deadline(Some(0), 1_700_000_001)
            .expect_err("zero deadline must refuse");
        let msg = err.to_string();
        assert!(msg.contains("deadline_unix"), "{msg}");
        assert!(msg.contains("given=0"), "{msg}");
        assert!(msg.contains("current=1700000001"), "{msg}");
    }

    #[test]
    fn post_job_deadline_omitted_defaults_to_one_hour_from_now() {
        assert_eq!(
            resolve_post_deadline(None, 1_700_000_001).expect("omitted deadline defaults"),
            1_700_003_601
        );
    }

    #[test]
    fn post_job_deadline_future_accepted() {
        assert_eq!(
            resolve_post_deadline(Some(1_700_000_002), 1_700_000_001)
                .expect("future deadline accepted"),
            1_700_000_002
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_job_over_budget_cap_refused_before_wallet_or_publish() {
        // An over-cap post refuses AT POST (before the wallet opens / anything publishes), so this
        // runs fully offline. Field case: amount 40 with per_job_budget_sats = 21.
        let (root, mut home) = temp_job_home("over-cap");
        home.config.per_job_budget_sats = 21;
        let err = post_job_async(
            &home,
            PostJobRequest {
                task: "t".into(),
                output: "text/plain".into(),
                amount_sats: 40,
                seller_pubkey: Some("aa".repeat(32)),
                untargeted: false,
                deadline_unix: Some(1_800_000_000),
                repo: None,
                branch: None,
                job: JobKind::FromScratch,
                requested_agent: None,
                requested_harness_family: None,
                requested_model: None,
                required_capabilities: Vec::new(),
            },
        )
        .await
        .expect_err("over-cap post must refuse");
        let msg = err.to_string();
        assert!(msg.contains("per_job_budget_sats"), "{msg}");
        assert!(msg.contains("40") && msg.contains("21"), "{msg}");
        assert!(msg.contains("RESTART"), "{msg}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Sign an award-kind event carrying exactly `tags`. Built tag-by-tag rather than through
    /// [`gateway::award_draft`] so a test can express shapes that helper cannot produce — a
    /// reversed `p` order, a missing tag, a second seller — which is the whole point of testing a
    /// parser that has to survive events it did not write.
    async fn award_event_with(
        signer: &nostr_sdk::Keys,
        tags: Vec<Vec<&str>>,
    ) -> nostr_sdk::Event {
        use nostr_sdk::prelude::{EventBuilder, Tag};
        let mut builder = EventBuilder::new(nostr_sdk::Kind::Custom(JOB_AWARD_KIND), "accepted");
        for tag in tags {
            builder = builder.tag(Tag::parse(tag).expect("tag"));
        }
        builder.sign(signer).await.expect("sign award")
    }

    // ★ THE DISCRIMINATING TEST for the seller field. An award carries two `p` tags — this buyer
    // and the seller — and `award_draft` happens to write buyer-then-seller. Reading position would
    // pass against every event we generate and silently record the BUYER as the seller the day that
    // order changes or another client writes the award.
    //
    // So both orders are asserted, and the reversed one is the leg that fails under positional
    // logic. A money row naming the wrong seller is not a cosmetic error: it is who the delivery
    // watcher pays.
    #[tokio::test(flavor = "current_thread")]
    async fn the_seller_is_the_p_tag_that_is_not_us_whatever_order_it_appears_in() {
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let seller_hex = nostr_sdk::Keys::generate().public_key().to_hex();
        let offer = "a".repeat(64);
        let claim = "c".repeat(64);

        for (label, p_tags) in [
            ("buyer first (as award_draft writes it)", vec![&buyer_hex, &seller_hex]),
            ("seller first (positional logic fails here)", vec![&seller_hex, &buyer_hex]),
        ] {
            let event = award_event_with(
                &buyer,
                vec![
                    vec!["e", &offer, "", "root"],
                    vec!["e", &claim],
                    vec!["p", p_tags[0]],
                    vec!["p", p_tags[1]],
                ],
            )
            .await;

            let parsed = parse_relayed_award(&event, &buyer_hex, &offer)
                .unwrap_or_else(|error| panic!("{label}: expected a parse, got: {error}"));
            assert_eq!(parsed.seller_pubkey, seller_hex, "{label}: wrong seller");
            assert_eq!(parsed.claim_id, claim, "{label}: wrong claim");
            assert_eq!(parsed.award_event_id, event.id.to_hex(), "{label}: wrong award id");
        }
    }

    // RED LEGS. Every one of these fields lands in a money row, so each missing or ambiguous
    // property must REFUSE rather than resolve to something plausible. Table-driven because the
    // interesting claim is that the set is complete, not that any single case works.
    #[tokio::test(flavor = "current_thread")]
    async fn an_award_missing_any_field_the_ledger_needs_refuses_to_parse() {
        let buyer = nostr_sdk::Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let seller_hex = nostr_sdk::Keys::generate().public_key().to_hex();
        let other_hex = nostr_sdk::Keys::generate().public_key().to_hex();
        let offer = "a".repeat(64);
        let elsewhere = "b".repeat(64);
        let claim = "c".repeat(64);

        let cases: Vec<(&str, Vec<Vec<&str>>, &str)> = vec![
            (
                "no claim `e` — nothing says WHICH claim won",
                vec![vec!["e", &offer, "", "root"], vec!["p", &buyer_hex], vec!["p", &seller_hex]],
                "claim",
            ),
            (
                "no root `e` — nothing says which offer it roots on",
                vec![vec!["e", &claim], vec!["p", &buyer_hex], vec!["p", &seller_hex]],
                "claim",
            ),
            (
                "roots on a DIFFERENT offer than the job we read it for",
                vec![
                    vec!["e", &elsewhere, "", "root"],
                    vec!["e", &claim],
                    vec!["p", &buyer_hex],
                    vec!["p", &seller_hex],
                ],
                "roots on offer",
            ),
            (
                "only our own `p` — no seller to pay",
                vec![vec!["e", &offer, "", "root"], vec!["e", &claim], vec!["p", &buyer_hex]],
                "other than this buyer",
            ),
            (
                "two non-buyer `p`s — the seller is ambiguous, so picking one would invent it",
                vec![
                    vec!["e", &offer, "", "root"],
                    vec!["e", &claim],
                    vec!["p", &buyer_hex],
                    vec!["p", &seller_hex],
                    vec!["p", &other_hex],
                ],
                "more than one non-buyer",
            ),
        ];

        for (label, tags, expected_fragment) in cases {
            let event = award_event_with(&buyer, tags).await;
            let error = parse_relayed_award(&event, &buyer_hex, &offer)
                .expect_err(&format!("{label}: must refuse, not resolve"));
            assert!(
                error.contains(expected_fragment),
                "{label}: refusal should say why (wanted {expected_fragment:?}), got: {error}"
            );
        }
    }

    // ⚠ Two AWARDs for one job are not a fault on their own: `award_presence_async` reports what
    // the relay holds for a ledger that may be missing a row, so refusing on count alone would
    // refuse to repair precisely when a repair is what is needed. Multiplicity is ambiguous only
    // when the events DISAGREE about what to write.
    #[test]
    fn agreeing_awards_are_not_ambiguous_and_repair_from_the_earliest() {
        let award = |id: &str, claim: &str, seller: &str| RelayedAward {
            award_event_id: id.to_owned(),
            claim_id: claim.to_owned(),
            seller_pubkey: seller.to_owned(),
        };
        let seller = "5".repeat(64);
        let claim = "c".repeat(64);

        // A single award repairs, obviously.
        let one = reduce_parsed_awards(vec![award("aaa", &claim, &seller)]);
        assert!(matches!(&one, AwardPresence::Repairable(a) if a.award_event_id == "aaa"));

        // Two selections that agree: same claim, same seller. Nothing to choose between, so it
        // repairs — and from the EARLIEST. The caller passes them oldest-first; a later duplicate
        // must not win just by arriving last.
        let two = reduce_parsed_awards(vec![
            award("the-earliest", &claim, &seller),
            award("the-later", &claim, &seller),
        ]);
        match two {
            AwardPresence::Repairable(a) => assert_eq!(
                a.award_event_id, "the-earliest",
                "a repair takes the earliest agreeing award, never the last one seen"
            ),
            other => panic!("agreeing awards must repair, got: {other:?}"),
        }
    }

    // RED LEG for the agreement rule. Two 3405s that disagree about what to write are genuinely
    // ambiguous, and picking one would launder a guess into a money row. Both fields are checked
    // because either alone would pass a set that disagrees only on the other.
    #[test]
    fn disagreeing_awards_refuse_rather_than_pick_one() {
        let award = |id: &str, claim: &str, seller: &str| RelayedAward {
            award_event_id: id.to_owned(),
            claim_id: claim.to_owned(),
            seller_pubkey: seller.to_owned(),
        };
        let seller = "5".repeat(64);
        let other_seller = "6".repeat(64);
        let claim = "c".repeat(64);
        let other_claim = "d".repeat(64);

        for (label, set) in [
            (
                "same seller, different claim",
                vec![award("a", &claim, &seller), award("b", &other_claim, &seller)],
            ),
            (
                "same claim, different seller — who gets paid",
                vec![award("a", &claim, &seller), award("b", &claim, &other_seller)],
            ),
        ] {
            match reduce_parsed_awards(set) {
                AwardPresence::Unrepairable { detail, .. } => assert!(
                    detail.contains("disagree"),
                    "{label}: refusal should say they disagree, got: {detail}"
                ),
                other => panic!("{label}: must refuse, got: {other:?}"),
            }
        }
    }

    // The OK:false classifier is the one place a relay's words become a money decision (#322), so
    // pin every NIP-01 prefix to its verdict. `duplicate:` is the load-bearing surprise: on a
    // same-bytes retry it is what SUCCESS looks like, and misreading it as a refusal would release
    // funds for an award that is public. The Unresolved set is equally load-bearing in the other
    // direction: session/transmission verdicts (`rate-limited:`, `auth-required:`), the NIP-01
    // transient catch-all (`error:`), and words we don't understand must NEVER release funds.
    #[test]
    fn ok_false_classification_pins_every_prefix_to_its_verdict() {
        assert_eq!(
            classify_ok_false("duplicate: already have this event"),
            SendOutcome::Acked,
            "duplicate means the relay HOLDS the event — a confirmation, not a refusal"
        );
        for held in [
            "rate-limited: slow down",
            "auth-required: we only accept events from registered users",
            "error: could not connect to the database",
            "a message with no machine-readable prefix at all",
        ] {
            assert_eq!(
                classify_ok_false(held),
                SendOutcome::Unresolved { detail: held.to_owned() },
                "{held:?} judges the transmission/session, not the event — must hold, not refuse"
            );
        }
        for refusal in [
            "blocked: no spam",
            "invalid: bad sig",
            "pow: difficulty 28 required",
            "restricted: members only",
            "unsupported: kind",
        ] {
            assert_eq!(
                classify_ok_false(refusal),
                SendOutcome::Refused { detail: refusal.to_owned() },
                "a deliberate, understood OK:false stores nothing — {refusal:?} must refuse"
            );
        }
    }

    // The two pre-network guards in `send_signed_award_async` run BEFORE any client exists, so
    // they are pure in-process assertions — and load-bearing: transmitting bytes that are not
    // the pinned event would make confirm/record/probe chase an event that is not public, and
    // the pay-window terminalizer would then repudiate a possibly-public award. Both damage
    // classes report Unresolved (local corruption is never "never landed") and transmit nothing
    // (the relay URL below is a closed port that would error loudly if dialled — but the guards
    // return first).
    #[tokio::test(flavor = "current_thread")]
    async fn send_refuses_to_transmit_bytes_that_are_not_the_pinned_event() {
        use nostr_sdk::prelude::{EventBuilder, JsonUtil, Keys, Kind};
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(3405), "")
            .sign_with_keys(&keys)
            .expect("sign");
        let json = event.as_json();

        // Bytes carrying a DIFFERENT id than the attempt is keyed on: refused locally.
        let outcome =
            send_signed_award_async(&keys, "ws://127.0.0.1:1", &"f".repeat(64), &json).await;
        assert!(
            matches!(&outcome, SendOutcome::Unresolved { detail } if detail.contains("keyed on")),
            "wrong-id bytes must not transmit: {outcome:?}"
        );

        // Tampered content under the stored id: the id field still matches the attempt, so the
        // id check passes and `verify()` is what catches it (the id no longer hashes the body).
        let tampered = json.replace("\"content\":\"\"", "\"content\":\"x\"");
        assert_ne!(tampered, json, "the tamper must actually change the bytes");
        let outcome =
            send_signed_award_async(&keys, "ws://127.0.0.1:1", &event.id.to_hex(), &tampered)
                .await;
        assert!(
            matches!(&outcome, SendOutcome::Unresolved { detail } if detail.contains("verification")),
            "tampered bytes must not transmit: {outcome:?}"
        );
    }

    // Only the relay's own `OK:false` may produce Refused. Every OTHER failure of a send —
    // timeout, transport, not-connected — says nothing about whether the event landed, and
    // mapping any of them to Refused would re-open #322 (release + re-select on a lost OK).
    #[test]
    fn only_an_explicit_ok_false_can_refuse() {
        use nostr_sdk::pool::relay::Error as RelayError;
        assert!(matches!(
            classify_send_error(RelayError::Timeout),
            SendOutcome::Unresolved { .. }
        ));
        assert!(matches!(
            classify_send_error(RelayError::NotConnected),
            SendOutcome::Unresolved { .. }
        ));
        assert!(matches!(
            classify_send_error(RelayError::RelayMessage("blocked: policy".to_owned())),
            SendOutcome::Refused { .. }
        ));
        assert!(matches!(
            classify_send_error(RelayError::RelayMessage("duplicate: seen".to_owned())),
            SendOutcome::Acked
        ));
    }

    // §1 RED-PROVE — the positive control `award_presence_async` (né `has_award_async`) never had.
    //
    // The probe is the sole input to "Invariant A: never award twice", and every observation of it
    // has been `false`. A guard whose only observed outcome is the negative one is not evidence of
    // anything: `fetch_events` returns Ok(empty) on timeout, so `Ok(false)` conflates "the relay says
    // there is no award" with "nothing arrived in time" — and the filter asks for our OWN authored
    // events, which is the one case nostr-sdk is known to treat specially.
    //
    // So: point it at a job that PROVABLY has an award and demand `true`. TRUE means the probe works
    // and §1 is a three-state fix. Empty means Invariant A has been decorative since it was written.
    //
    // Ignored because it needs a live relay and a real home. Run explicitly:
    //   REDPROVE_HOME=<home> REDPROVE_JOB=<job_id> REDPROVE_AWARD=<award_event_id> \
    //     cargo test -p maxplayer-core red_prove_has_award -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a live relay and a real home; run explicitly with --ignored"]
    async fn red_prove_has_award_async_returns_true_for_a_known_award() {
        use nostr_sdk::prelude::{Client, EventId, Filter};

        let root = std::env::var("REDPROVE_HOME").expect("REDPROVE_HOME");
        let job_id = std::env::var("REDPROVE_JOB").expect("REDPROVE_JOB");
        let award_id = std::env::var("REDPROVE_AWARD").expect("REDPROVE_AWARD");
        let home = home::bootstrap(&root).expect("bootstrap home");
        let keys = buyer_keys(&home).expect("buyer keys");
        eprintln!(
            "REDPROVE relay={} author={}",
            home.config.relay_url,
            keys.public_key().to_hex()
        );

        // POSITIVE CONTROL FIRST, built the same way the probe builds its client: fetch the known
        // award BY ID. If this comes back empty the connection (or NIP-42 auth) is the problem, and
        // an empty probe result below would say nothing at all about presence.
        let client = Client::new(keys.clone());
        client
            .add_relay(&home.config.relay_url)
            .await
            .expect("add relay");
        client.connect().await;
        let control = client
            .fetch_events(
                Filter::new().id(EventId::from_hex(&award_id).expect("award id hex")),
                Duration::from_secs(10),
            )
            .await
            .expect("control fetch by id");
        eprintln!("CONTROL fetch-award-by-id -> {} event(s)", control.len());

        let probe = award_presence_async(&home, &keys, &job_id, Duration::from_secs(10)).await;
        eprintln!("PROBE award_presence_async -> {probe:?}");

        assert!(
            !control.is_empty(),
            "POSITIVE CONTROL FAILED: could not read a known event by id, so the probe's result is \
             uninterpretable — fix the connection/auth before drawing any conclusion"
        );
        let found = match probe.expect("probe should not error once the control passes") {
            PresenceRead::Present(found) => found,
            other => panic!(
                "award_presence_async returned {other:?} for a job with a KNOWN award while the \
                 control passed — the guard cannot detect the thing it exists to detect"
            ),
        };
        // Identity, not just presence: a probe that returns SOME award for the job would satisfy a
        // bare `is_some()` while pointing at the wrong event, and the refusal message quotes this id
        // to an operator.
        assert_eq!(
            found.award_event_id(),
            award_id,
            "the probe found an award for this job but not the KNOWN one"
        );
        // And the parse must survive a REAL event, not just the synthetic ones the unit tests build.
        // An award this buyer itself published is the easiest case there is; if it cannot be parsed
        // into a complete record, repair is dead on arrival in the field however green the unit
        // tests are.
        assert!(
            matches!(found, AwardPresence::Repairable(_)),
            "a real award published by this buyer must parse into a complete record, got: {found:?}"
        );
    }
}
