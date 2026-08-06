use std::fmt;

use serde::{Deserialize, Serialize};

use crate::delivery::{CommitOid, DeliveryError, GitDelivery};

pub const MAXPLAYER_TAG: &str = "maxplayer";
// maxplayer protocol version. maxplayer events occupy a dedicated kind block, so a parser only ever
// matches maxplayer's own events.
pub const PROTOCOL_VERSION: &str = "1";

// All kind NUMBERS live in `crate::kinds` (the one registry); re-exported here so the historical
// `gateway::JOB_*_KIND` paths keep resolving.
pub use crate::kinds::{
    JOB_ACCEPT_KIND, JOB_AWARD_KIND, JOB_CLAIM_KIND, JOB_FEEDBACK_KIND, JOB_OFFER_KIND,
    JOB_RECEIPT_KIND, JOB_RESULT_KIND,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSpec(pub Vec<String>);

impl TagSpec {
    pub fn new<const N: usize>(values: [&str; N]) -> Self {
        Self(values.into_iter().map(str::to_owned).collect())
    }

    pub fn first(&self) -> Option<&str> {
        self.0.first().map(String::as_str)
    }

    pub fn value(&self) -> Option<&str> {
        self.0.get(1).map(String::as_str)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDraft {
    pub kind: u16,
    pub tags: Vec<TagSpec>,
    pub content: String,
}

impl EventDraft {
    pub fn new(kind: u16, tags: Vec<TagSpec>, content: impl Into<String>) -> Self {
        Self {
            kind,
            tags,
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferDraft {
    pub task: String,
    pub output: String,
    pub amount_sats: u64,
    pub deadline_unix: u64,
    pub seller_pubkey: Option<String>,
    /// The harness this job asks for, as `["param", "agent", …]`. `None` (or `"any"`) ⇒ no
    /// preference: any seller may claim and run it on whichever harness it prefers.
    pub requested_agent: Option<String>,
}

impl OfferDraft {
    pub fn new(
        task: impl Into<String>,
        output: impl Into<String>,
        amount_sats: u64,
        deadline_unix: u64,
        seller_pubkey: impl Into<String>,
    ) -> Self {
        Self {
            task: task.into(),
            output: output.into(),
            amount_sats,
            deadline_unix,
            seller_pubkey: Some(seller_pubkey.into()),
            requested_agent: None,
        }
    }

    pub fn untargeted(
        task: impl Into<String>,
        output: impl Into<String>,
        amount_sats: u64,
        deadline_unix: u64,
    ) -> Self {
        Self {
            task: task.into(),
            output: output.into(),
            amount_sats,
            deadline_unix,
            seller_pubkey: None,
            requested_agent: None,
        }
    }

    /// Request a specific harness for this job. A canonicalised-away value (`any`, blank) records
    /// no request, so "no preference" has exactly one representation on the wire.
    pub fn requesting_agent(mut self, requested_agent: Option<&str>) -> Self {
        self.requested_agent = crate::seller_agents::normalize_request(requested_agent);
        self
    }

    pub fn to_event_draft(&self) -> EventDraft {
        // The offer does not name a mint — the seller authors the accepted mint(s) in its claim
        // `creq`, so there is no `["mint", …]` tag here.
        let mut tags = vec![
            TagSpec::new(["i", &self.task]),
            TagSpec::new(["output", &self.output]),
            TagSpec::new(["amount", &self.amount_sats.to_string(), "sat"]),
            TagSpec::new(["param", "deadline", &self.deadline_unix.to_string()]),
        ];
        if let Some(requested_agent) = &self.requested_agent {
            tags.push(TagSpec::new([
                "param",
                crate::seller_agents::AGENT_PARAM,
                requested_agent,
            ]));
        }
        if let Some(seller_pubkey) = &self.seller_pubkey {
            tags.push(TagSpec::new(["p", seller_pubkey]));
        }
        tags.push(maxplayer_tag());
        tags.push(version_tag());

        EventDraft::new(JOB_OFFER_KIND, tags, "")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParsedOffer {
    pub task: String,
    pub output: String,
    pub amount: u64,
    pub unit: String,
    pub deadline_unix: u64,
    pub seller_pubkey: Option<String>,
    /// The harness this job requested, canonicalised. `None` ⇒ no preference (the parameter was
    /// absent, blank, or the explicit `any`).
    pub requested_agent: Option<String>,
}

impl ParsedOffer {
    pub fn is_targeted(&self) -> bool {
        self.seller_pubkey.is_some()
    }

    pub fn seller_matches(&self, seller_pubkey: &str) -> bool {
        match self.seller_pubkey.as_deref() {
            Some(target) => target == seller_pubkey,
            None => true,
        }
    }

    pub fn assert_seller_matches(&self, seller_pubkey: &str) -> Result<(), TargetingError> {
        match self.seller_pubkey.as_deref() {
            Some(target) if target != seller_pubkey => Err(TargetingError {
                expected: target.to_owned(),
                actual: seller_pubkey.to_owned(),
            }),
            _ => Ok(()),
        }
    }
}

pub fn is_targeted(offer: &ParsedOffer) -> bool {
    offer.is_targeted()
}

pub fn assert_seller_matches(
    offer: &ParsedOffer,
    seller_pubkey: &str,
) -> Result<(), TargetingError> {
    offer.assert_seller_matches(seller_pubkey)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetingError {
    pub expected: String,
    pub actual: String,
}

impl fmt::Display for TargetingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "offer targets seller {}, not {}",
            self.expected, self.actual
        )
    }
}

impl std::error::Error for TargetingError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfferParseError {
    WrongKind(u16),
    MissingTag(&'static str),
    InvalidAmount(String),
    InvalidDeadline(String),
    UnsupportedUnit(String),
    UnsupportedVersion(String),
    MissingMaxplayerTag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitResultParseError {
    WrongKind(u16),
    MissingTag(&'static str),
    /// Namespace guard: a result event without the `["t","maxplayer"]` tag.
    MissingMaxplayerTag,
    UnsupportedDelivery(String),
    InvalidDelivery(DeliveryError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundGitDeliveryError {
    WrongOfferKind(u16),
    MissingOfferTag(&'static str),
    UnsupportedOfferDelivery(String),
    Result(GitResultParseError),
    TargetMismatch,
}

impl fmt::Display for BoundGitDeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongOfferKind(kind) => write!(f, "expected kind {JOB_OFFER_KIND}, got {kind}"),
            Self::MissingOfferTag(tag) => write!(f, "missing required git offer tag {tag}"),
            Self::UnsupportedOfferDelivery(delivery) => {
                write!(f, "unsupported offer delivery {delivery:?}")
            }
            Self::Result(error) => error.fmt(f),
            Self::TargetMismatch => {
                f.write_str("git result repository or branch does not match the offer")
            }
        }
    }
}

impl std::error::Error for BoundGitDeliveryError {}

impl fmt::Display for GitResultParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongKind(kind) => write!(f, "expected kind {JOB_RESULT_KIND}, got {kind}"),
            Self::MissingTag(tag) => write!(f, "missing required git result tag {tag}"),
            Self::MissingMaxplayerTag => write!(f, "missing t=maxplayer tag"),
            Self::UnsupportedDelivery(delivery) => {
                write!(f, "unsupported result delivery {delivery:?}")
            }
            Self::InvalidDelivery(error) => write!(f, "invalid git result delivery: {error}"),
        }
    }
}

impl std::error::Error for GitResultParseError {}

impl fmt::Display for OfferParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongKind(kind) => write!(f, "expected kind {JOB_OFFER_KIND}, got {kind}"),
            Self::MissingTag(tag) => write!(f, "missing required tag {tag}"),
            Self::InvalidAmount(value) => write!(f, "invalid amount tag value {value:?}"),
            Self::InvalidDeadline(value) => write!(f, "invalid deadline tag value {value:?}"),
            Self::UnsupportedUnit(unit) => write!(f, "unsupported amount unit {unit:?}"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported maxplayer version {version:?}"),
            Self::MissingMaxplayerTag => write!(f, "missing t=maxplayer tag"),
        }
    }
}

impl std::error::Error for OfferParseError {}

pub fn parse_offer(event: &EventDraft) -> Result<ParsedOffer, OfferParseError> {
    if event.kind != JOB_OFFER_KIND {
        return Err(OfferParseError::WrongKind(event.kind));
    }
    if !has_tag_value(&event.tags, "t", MAXPLAYER_TAG) {
        return Err(OfferParseError::MissingMaxplayerTag);
    }
    let version = first_tag_value(&event.tags, "v").ok_or(OfferParseError::MissingTag("v"))?;
    if version != PROTOCOL_VERSION {
        return Err(OfferParseError::UnsupportedVersion(version.to_owned()));
    }

    let amount_tag =
        first_tag(&event.tags, "amount").ok_or(OfferParseError::MissingTag("amount"))?;
    let amount_value = amount_tag
        .0
        .get(1)
        .ok_or(OfferParseError::MissingTag("amount"))?;
    let unit = amount_tag
        .0
        .get(2)
        .ok_or(OfferParseError::MissingTag("amount unit"))?;
    if unit != "sat" {
        return Err(OfferParseError::UnsupportedUnit(unit.clone()));
    }
    let amount = amount_value
        .parse()
        .map_err(|_| OfferParseError::InvalidAmount(amount_value.clone()))?;

    let deadline = event
        .tags
        .iter()
        .find(|tag| {
            tag.0.first().map(String::as_str) == Some("param")
                && tag.0.get(1).map(String::as_str) == Some("deadline")
        })
        .and_then(|tag| tag.0.get(2))
        .ok_or(OfferParseError::MissingTag("param deadline"))?;
    let deadline_unix = deadline
        .parse()
        .map_err(|_| OfferParseError::InvalidDeadline(deadline.clone()))?;

    Ok(ParsedOffer {
        task: first_tag_value(&event.tags, "i")
            .ok_or(OfferParseError::MissingTag("i"))?
            .to_owned(),
        output: first_tag_value(&event.tags, "output")
            .ok_or(OfferParseError::MissingTag("output"))?
            .to_owned(),
        amount,
        unit: unit.clone(),
        deadline_unix,
        seller_pubkey: first_tag_value(&event.tags, "p").map(str::to_owned),
        requested_agent: crate::seller_agents::normalize_request(param_value(
            &event.tags,
            crate::seller_agents::AGENT_PARAM,
        )),
    })
}

/// Read a `["param", <name>, <value>]` parameter off an event's tags.
fn param_value<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a str> {
    tags.iter()
        .find(|tag| {
            tag.0.first().map(String::as_str) == Some("param")
                && tag.0.get(1).map(String::as_str) == Some(name)
        })
        .and_then(|tag| tag.0.get(2))
        .map(String::as_str)
}

/// Parses the buyer-visible git delivery fields carried by a result event.
pub fn parse_git_result_delivery(event: &EventDraft) -> Result<GitDelivery, GitResultParseError> {
    if event.kind != JOB_RESULT_KIND {
        return Err(GitResultParseError::WrongKind(event.kind));
    }
    // Namespace guard: reject a foreign event squatting the result kind before reading any
    // delivery field.
    if !has_tag_value(&event.tags, "t", MAXPLAYER_TAG) {
        return Err(GitResultParseError::MissingMaxplayerTag);
    }
    let delivery = first_tag_value(&event.tags, "delivery")
        .ok_or(GitResultParseError::MissingTag("delivery"))?;
    if delivery != "git" {
        return Err(GitResultParseError::UnsupportedDelivery(
            delivery.to_owned(),
        ));
    }
    let repo =
        first_tag_value(&event.tags, "repo").ok_or(GitResultParseError::MissingTag("repo"))?;
    let branch =
        first_tag_value(&event.tags, "branch").ok_or(GitResultParseError::MissingTag("branch"))?;
    let commit =
        first_tag_value(&event.tags, "commit").ok_or(GitResultParseError::MissingTag("commit"))?;
    let commit_oid = CommitOid::parse(commit).map_err(GitResultParseError::InvalidDelivery)?;
    GitDelivery::new(repo, branch, commit_oid).map_err(GitResultParseError::InvalidDelivery)
}

/// Parses a result only when it targets the repository and branch named by the offer.
pub fn parse_bound_git_delivery(
    offer: &EventDraft,
    result: &EventDraft,
) -> Result<GitDelivery, BoundGitDeliveryError> {
    if offer.kind != JOB_OFFER_KIND {
        return Err(BoundGitDeliveryError::WrongOfferKind(offer.kind));
    }
    let delivery = first_tag_value(&offer.tags, "delivery")
        .ok_or(BoundGitDeliveryError::MissingOfferTag("delivery"))?;
    if delivery != "git" {
        return Err(BoundGitDeliveryError::UnsupportedOfferDelivery(
            delivery.to_owned(),
        ));
    }
    let offer_repo = first_tag_value(&offer.tags, "repo")
        .ok_or(BoundGitDeliveryError::MissingOfferTag("repo"))?;
    let offer_branch = first_tag_value(&offer.tags, "branch")
        .ok_or(BoundGitDeliveryError::MissingOfferTag("branch"))?;
    let delivery = parse_git_result_delivery(result).map_err(BoundGitDeliveryError::Result)?;
    if delivery.repo() != offer_repo || delivery.branch() != offer_branch {
        return Err(BoundGitDeliveryError::TargetMismatch);
    }
    Ok(delivery)
}

/// Kind-claim CLAIM draft (`status=processing`). The claim carries the seller-authored
/// NUT-18 payment request as a `["creq", "creqA…"]` tag — the claim *is*
/// the invoice. Build `creq` with [`creq::build_seller_creq`]; buyers read it back with
/// [`creq::parse_creq`].
///
/// The offer `e` tag is marked `root`, so an observer holding only public tags can join the claim
/// to its offer without guessing at `e`-tag position.
///
/// `agents` advertises the harnesses this seller can run (preference order) as
/// `["mobee_agent", …]`, so the buyer's award filter can hold the claim to the harness its job
/// asked for. Empty ⇒ the tag is omitted rather than sent empty.
pub fn claim_draft(
    offer_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
    creq: &str,
    agents: &[String],
) -> EventDraft {
    let mut tags = vec![
        TagSpec::new(["e", offer_id, "", "root"]),
        TagSpec::new(["p", buyer_pubkey]),
        TagSpec::new(["p", seller_pubkey]),
        TagSpec::new(["creq", creq]),
    ];
    if let Some(tag) = crate::heartbeat::agent_tag(agents) {
        tags.push(tag);
    }
    status_draft(JOB_CLAIM_KIND, "processing", tags)
}

/// Kind-award AWARD draft (`status=accepted`). Buyer-authored selection of a claim — e-tags the
/// offer (root) + the winning claim, p-tags the buyer and the awarded seller. The seller runs its
/// agent only once this award names its own claim, so a job drawing many claims burns compute on
/// one seller. Its own buyer-authored kind — a selection must not ride the seller's feedback kind.
pub fn award_draft(
    offer_id: &str,
    claim_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
) -> EventDraft {
    status_draft(
        JOB_AWARD_KIND,
        "accepted",
        vec![
            TagSpec::new(["e", offer_id, "", "root"]),
            TagSpec::new(["e", claim_id]),
            TagSpec::new(["p", buyer_pubkey]),
            TagSpec::new(["p", seller_pubkey]),
        ],
    )
}

/// Kind-accept ACCEPT draft (`status=accepted`). Buyer-authored pay-bind against one verified
/// result — same tag shape as [`award_draft`], on its own kind.
///
/// The kind is the whole point. Selection and pay-authorisation are different statements about a
/// job, and while they shared `JOB_AWARD_KIND` the only way to tell them apart was to count events
/// for that job — which is not a discriminator, because two events of one kind is also what a
/// re-publish looks like. A seller could not distinguish claim-won from pay-authorised, and any
/// award-presence read had to reconcile a multiplicity it could not interpret.
pub fn accept_draft(
    offer_id: &str,
    claim_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
) -> EventDraft {
    status_draft(
        JOB_ACCEPT_KIND,
        "accepted",
        vec![
            TagSpec::new(["e", offer_id, "", "root"]),
            TagSpec::new(["e", claim_id]),
            TagSpec::new(["p", buyer_pubkey]),
            TagSpec::new(["p", seller_pubkey]),
        ],
    )
}

/// The two ids a buyer-authored selection or pay-bind carries: the offer it roots on and the claim
/// it names. Both are read from the event's `e` tags — the `root`-marked `e` is the offer, the other
/// `e` the claim. A seller matches `claim_id` against its own published claim to decide
/// execute-versus-release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAward {
    pub offer_id: String,
    pub claim_id: String,
}

/// Parse a kind-award AWARD event into the offer + winning-claim ids it selects, or `None` when the
/// event is not an award or lacks the two `e` tags. Pure over the draft so the seller's match logic
/// is unit-testable.
pub fn parse_award(event: &EventDraft) -> Option<ParsedAward> {
    if event.kind != JOB_AWARD_KIND {
        return None;
    }
    parse_offer_and_claim_tags(event)
}

/// Parse a kind-accept ACCEPT event into the offer + claim ids its pay-bind names, or `None` when
/// the event is not an accept or lacks the two `e` tags.
///
/// Deliberately a separate entry point rather than a widened [`parse_award`]: a caller that means
/// "is this a selection?" and a caller that means "is this a pay-bind?" must not be able to satisfy
/// each other by accident, which is the failure the shared kind produced.
pub fn parse_accept(event: &EventDraft) -> Option<ParsedAward> {
    if event.kind != JOB_ACCEPT_KIND {
        return None;
    }
    parse_offer_and_claim_tags(event)
}

/// The offer + claim `e`-tag shape shared by AWARD and ACCEPT. Written once: the two events carry
/// identical tags and differ only by kind, so duplicating this would be one fact in two places.
/// Each public parser gates on its own kind before calling in.
fn parse_offer_and_claim_tags(event: &EventDraft) -> Option<ParsedAward> {
    let e_tags: Vec<&TagSpec> = event
        .tags
        .iter()
        .filter(|tag| tag.first() == Some("e"))
        .collect();
    let is_root = |tag: &TagSpec| tag.0.get(3).map(String::as_str) == Some("root");
    let offer_id = e_tags
        .iter()
        .find(|tag| is_root(tag))
        .and_then(|tag| tag.value())?;
    let claim_id = e_tags
        .iter()
        .find(|tag| !is_root(tag))
        .and_then(|tag| tag.value())?;
    Some(ParsedAward {
        offer_id: offer_id.to_owned(),
        claim_id: claim_id.to_owned(),
    })
}

/// The settled offer id a co-signed kind-3400 receipt names, or `None` when the event is not a
/// receipt or carries no `root`-marked `e` tag. A receipt roots its offer exactly as every other
/// lifecycle stage does (`["e", offer_id, "", "root"]`, see [`receipt_draft`]); the other `e` is the
/// result, not a claim, so only the root id is returned. Pure over the draft so the seller's
/// terminal-eligibility gate is unit-testable, and gated on the kind so a caller that means "which
/// offer did this receipt settle?" can never be satisfied by a non-receipt event.
pub fn settled_offer_id(event: &EventDraft) -> Option<String> {
    if event.kind != JOB_RECEIPT_KIND {
        return None;
    }
    event
        .tags
        .iter()
        .filter(|tag| tag.first() == Some("e"))
        .find(|tag| tag.0.get(3).map(String::as_str) == Some("root"))
        .and_then(|tag| tag.value())
        .map(str::to_owned)
}

/// Optional git delivery tags on a result-kind result (`delivery=git` + repo/branch/commit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitResultTags<'a> {
    pub repo: &'a str,
    pub branch: &'a str,
    pub commit_sha: &'a str,
}

/// Kind-result draft. Pass `Some(git)` to attach delivery/repo/branch/commit tags;
/// `exec_metadata` appends the seller-claimed usage block (may be empty).
pub fn result_draft(
    offer_id: &str,
    buyer_pubkey: &str,
    output: &str,
    amount_sats: u64,
    job_hash: &str,
    seller_signature: &str,
    content: impl Into<String>,
    git: Option<GitResultTags<'_>>,
    exec_metadata: &[TagSpec],
) -> EventDraft {
    let mut tags = vec![
        TagSpec::new(["e", offer_id, "", "root"]),
        TagSpec::new(["p", buyer_pubkey]),
    ];
    if let Some(git) = git {
        tags.push(TagSpec::new(["delivery", "git"]));
        tags.push(TagSpec::new(["output", output]));
        tags.push(TagSpec::new(["commit", git.commit_sha]));
        tags.push(TagSpec::new(["repo", git.repo]));
        tags.push(TagSpec::new(["branch", git.branch]));
    } else {
        tags.push(TagSpec::new(["output", output]));
    }
    tags.push(TagSpec::new(["amount", &amount_sats.to_string(), "sat"]));
    tags.push(TagSpec::new(["job-hash", job_hash]));
    tags.push(TagSpec::new(["sig", "seller", seller_signature]));
    // exec-metadata (seller-claimed, unsigned — sig/seller does NOT cover it).
    tags.extend(exec_metadata.iter().cloned());
    tags.push(maxplayer_tag());
    tags.push(version_tag());
    EventDraft::new(JOB_RESULT_KIND, tags, content)
}

/// Thin wrapper: result-kind git delivery via [`result_draft`] + [`GitResultTags`].
/// `exec_metadata` is the optional seller-claimed usage block (may be empty).
pub fn git_result_draft(
    offer_id: &str,
    buyer_pubkey: &str,
    repo: &str,
    branch: &str,
    commit_sha: &str,
    amount_sats: u64,
    job_hash: &str,
    seller_signature: &str,
    content: impl Into<String>,
    exec_metadata: &[TagSpec],
) -> EventDraft {
    result_draft(
        offer_id,
        buyer_pubkey,
        "text/plain",
        amount_sats,
        job_hash,
        seller_signature,
        content,
        Some(GitResultTags {
            repo,
            branch,
            commit_sha,
        }),
        exec_metadata,
    )
}

/// The protocol-v1 §10 feedback reason-code vocabulary. A `FEEDBACK` carries the code as an
/// authoritative `["reason_code", <code>]` tag; `content` stays human-readable and is explanatory
/// only. A reader MUST treat the tag as authoritative for the class and MUST NOT parse `content` to
/// determine it; an unrecognised code falls back to the coarse class named by `status` (the code is a
/// newer peer, not a broken one), so the vocabulary is extensible.
///
/// The set is deliberately COMPLETE, not just the code that prompted its introduction (`no_sentinel`):
/// per §10, a vocabulary added only at the site that happened to prompt it reproduces the original
/// class-ambiguity defect with a tag sitting on top of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasonCode {
    /// Offer amount is below the seller's rate floor — a price decline, not a work error.
    BelowRate,
    /// Offer speaks a protocol major this seat does not — a version reject, distinct from malformed.
    UnsupportedVersion,
    /// The trade's mint set does not intersect the seat's accepted mints.
    MintIncompatible,
    /// The seat is at capacity and declines to take the work.
    AtCapacity,
    /// The work execution failed (the agent could not produce the deliverable).
    ExecutionFailed,
    /// Execution succeeded but the delivery (snapshot/push/publish) failed.
    DeliveryFailed,
    /// The delivery carried no execution sentinel — a refusal that DOES count against the seller (§19).
    NoSentinel,
}

impl ReasonCode {
    /// The stable `reason_code` tag value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BelowRate => "below_rate",
            Self::UnsupportedVersion => "unsupported_version",
            Self::MintIncompatible => "mint_incompatible",
            Self::AtCapacity => "at_capacity",
            Self::ExecutionFailed => "execution_failed",
            Self::DeliveryFailed => "delivery_failed",
            Self::NoSentinel => "no_sentinel",
        }
    }
}

/// Kind-feedback FEEDBACK draft carrying the §10 `reason_code` tag — the authoritative class
/// discriminator a reader keys on. The `status` tag stays `error` (as every emitting site here
/// always has): the coarse status is a fallback for readers that do not know a code, and the buyer's
/// claim-list view keys on it, so re-classing it (a `below_rate`/`no_sentinel` refusal is `refusal`
/// per §10's table) is a deliberate view change left as a follow-up, not smuggled in here.
///
/// The offer `e` tag is marked `root`, so a failure is attributable to its job from public tags
/// alone — a refusal that cannot be joined to an offer is invisible in a seller's reliability
/// record, which is the half of reputation that only failures carry.
///
/// `content` carries the human-readable reason (a display-only mirror of the code); empty preserves the
/// historical empty-content callers.
pub fn error_draft(
    offer_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
    reason_code: ReasonCode,
    content: impl Into<String>,
) -> EventDraft {
    let mut draft = status_draft(
        JOB_FEEDBACK_KIND,
        "error",
        vec![
            TagSpec::new(["e", offer_id, "", "root"]),
            TagSpec::new(["p", buyer_pubkey]),
            TagSpec::new(["p", seller_pubkey]),
            TagSpec::new(["reason_code", reason_code.as_str()]),
        ],
    );
    draft.content = content.into();
    draft
}

/// Delivery binding echoed into a kind-3400 receipt. Both fields are in the
/// co-signed preimage, so the settled receipt attests which git object was paid for and
/// its kind (commit vs tree) is not forgeable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiptDelivery<'a> {
    /// Full lowercase git oid of the delivered object.
    pub integrity_hash: &'a str,
    /// `fork` | `patch`.
    pub kind: &'a str,
}

/// SHA-256 hex of a seller-authored NUT-18 payment request string.
///
/// The bind is over the FULL `creq` tag-value string (the `creqA…` base64url-CBOR string) as
/// UTF-8 bytes — never a re-decoded/re-encoded form — so buyer and seller hash byte-identical
/// input. Both the attempt id ([`crate::payment::PaymentKey`]) and the co-signed receipt preimage
/// ([`crate::receipt::ReceiptPreimage`]) bind this hash, and the receipt event carries it as a
/// `["creq-hash", <hex>]` tag.
pub fn creq_hash_hex(creq: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(creq.as_bytes());
    hex::encode(hasher.finalize())
}

/// Buyer-authored kind-3400 receipt draft. Fixed tag order + a pinned `created_at` at the
/// event-build site give a deterministic event id (idempotent republish). `delivery` adds
/// the delivery binding tags; `exec_metadata` appends the buyer's filtered echo (may be empty —
/// seller-claimed, NOT covered by the co-signatures). `creq_hash` is the seller-authored
/// request hash bound into the co-signed preimage; `None` for a claim that carries no `creq`.
pub fn receipt_draft(
    offer_id: &str,
    result_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
    mint: &str,
    amount_sats: u64,
    job_hash: &str,
    seller_signature: &str,
    buyer_signature: &str,
    creq_hash: Option<&str>,
    delivery: Option<ReceiptDelivery<'_>>,
    exec_metadata: &[TagSpec],
) -> EventDraft {
    let mut tags = vec![
        TagSpec::new(["job-hash", job_hash]),
        TagSpec::new(["amount", &amount_sats.to_string(), "sat"]),
        TagSpec::new(["e", offer_id, "", "root"]),
        TagSpec::new(["e", result_id, "", "reply"]),
        TagSpec::new(["p", buyer_pubkey]),
        TagSpec::new(["p", seller_pubkey]),
        TagSpec::new(["mint", mint]),
        TagSpec::new(["sig", "seller", seller_signature]),
        TagSpec::new(["sig", "buyer", buyer_signature]),
    ];
    // Emit the seller-authored request hash alongside the mint/job-hash tags when the trade
    // bound one. A trade with no creq omits the tag entirely.
    if let Some(creq_hash) = creq_hash {
        tags.push(TagSpec::new(["creq-hash", creq_hash]));
    }
    if let Some(delivery) = delivery {
        tags.push(TagSpec::new([
            "delivery_integrity_hash",
            delivery.integrity_hash,
        ]));
        tags.push(TagSpec::new(["delivery_kind", delivery.kind]));
    }
    tags.extend(exec_metadata.iter().cloned());
    tags.push(maxplayer_tag());
    tags.push(version_tag());
    EventDraft::new(JOB_RECEIPT_KIND, tags, "")
}

/// Build a `status`-tagged draft of the given kind (claim `claim`, award `award`, feedback `feedback`).
/// Claim, award, and feedback are distinct kinds; the `status` tag is retained so status-based
/// view logic can read a single field across them.
fn status_draft(kind: u16, status: &str, mut tags: Vec<TagSpec>) -> EventDraft {
    tags.insert(0, TagSpec::new(["status", status]));
    tags.push(maxplayer_tag());
    tags.push(version_tag());
    EventDraft::new(kind, tags, "")
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

fn maxplayer_tag() -> TagSpec {
    TagSpec::new(["t", MAXPLAYER_TAG])
}

fn version_tag() -> TagSpec {
    TagSpec::new(["v", PROTOCOL_VERSION])
}

#[cfg(feature = "gateway")]
pub mod nostr {
    use nostr_sdk::prelude::{EventBuilder, Kind, Tag};

    use super::{EventDraft, TagSpec};

    pub fn event_builder(
        draft: &EventDraft,
    ) -> Result<EventBuilder, nostr_sdk::prelude::tag::Error> {
        let mut builder = EventBuilder::new(Kind::Custom(draft.kind), draft.content.clone());
        builder.allow_self_tagging = true;
        for tag in &draft.tags {
            builder = builder.tag(to_tag(tag)?);
        }
        Ok(builder)
    }

    fn to_tag(tag: &TagSpec) -> Result<Tag, nostr_sdk::prelude::tag::Error> {
        Tag::parse(tag.0.clone())
    }
}

/// The seller-authored NUT-18 payment request (`creq…`).
///
/// The party getting paid authors the payment terms: at claim time the seller builds a
/// NUT-18 [`PaymentRequest`] (amount `a`, unit `u`, accepted mints `m`, a nostr transport
/// to its own key, single-use `s`, no `nut10` locking condition) using the cashu crate's
/// shipped `nut18` types, and attaches its `creqA…` `Display` as the claim's `["creq", …]`
/// tag (see [`claim_draft`]). Buyers read it back with [`parse_creq`]. The encoding is never
/// hand-rolled — CBOR/base64 and the `creqA` prefix come from cashu's `PaymentRequest`.
#[cfg(feature = "wallet")]
pub mod creq {
    use std::fmt;
    use std::str::FromStr;

    use cashu::nuts::nut18::{PaymentRequest, PaymentRequestBuilder, Transport, TransportType};
    use cashu::{CurrencyUnit, MintUrl};
    use nostr_sdk::prelude::{Nip19Profile, ToBech32};
    use nostr_sdk::PublicKey;

    /// Failure building or parsing a claim `creq`.
    #[derive(Debug)]
    pub enum CreqError {
        /// An `accepted_mints` entry is not a well-formed mint URL.
        Mint(String),
        /// The seller pubkey is not valid hex / not a valid key.
        SellerKey(String),
        /// Encoding the seller nprofile failed.
        Nprofile(String),
        /// Building the NUT-18 transport failed (missing required field).
        Transport(&'static str),
        /// The `creq` string did not parse as a NUT-18 payment request.
        Parse(String),
    }

    impl fmt::Display for CreqError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Mint(m) => write!(f, "creq: invalid accepted mint url: {m}"),
                Self::SellerKey(e) => write!(f, "creq: invalid seller pubkey: {e}"),
                Self::Nprofile(e) => write!(f, "creq: nprofile encode failed: {e}"),
                Self::Transport(e) => write!(f, "creq: transport build failed: {e}"),
                Self::Parse(e) => write!(f, "creq: parse failed: {e}"),
            }
        }
    }

    impl std::error::Error for CreqError {}

    /// Build the seller-authored NUT-18 payment request for a claim and return its `creqA…`
    /// encoding for the claim's `["creq", …]` tag.
    ///
    /// - `payment_id` → NUT-18 `i` (the job/attempt id).
    /// - `amount`/`unit` → `a`/`u`, copied from the offer.
    /// - `accepted_mints` → `m`, the seller's own accepted-mint list (order preserved; the
    ///   first entry is the seller's advertised default).
    /// - `seller_pubkey_hex` → one nostr [`Transport`] whose target is the seller's `nprofile`
    ///   with a `[["n","17"]]` NIP-17 tag.
    ///
    /// `s = true` (single-use: one claim, one payment) and no `nut10` locking condition is set
    /// (payment is not coupled to a delivery/attestation condition).
    pub fn build_seller_creq(
        payment_id: &str,
        amount: u64,
        unit: &str,
        accepted_mints: &[String],
        seller_pubkey_hex: &str,
    ) -> Result<String, CreqError> {
        // CurrencyUnit::from_str is infallible (unknown units fall back to Custom), so an
        // offer unit always maps to a NUT-18 unit.
        let unit = CurrencyUnit::from_str(unit).unwrap_or(CurrencyUnit::Custom(unit.to_owned()));
        let mints = accepted_mints
            .iter()
            .map(|m| MintUrl::from_str(m).map_err(|e| CreqError::Mint(format!("{m}: {e}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let seller_key =
            PublicKey::from_hex(seller_pubkey_hex).map_err(|e| CreqError::SellerKey(e.to_string()))?;
        // Empty relay list: the transport addresses the seller's key; relay hints are optional.
        let nprofile = Nip19Profile::new(seller_key, [])
            .to_bech32()
            .map_err(|e| CreqError::Nprofile(e.to_string()))?;
        let transport = Transport::builder()
            .transport_type(TransportType::Nostr)
            .target(nprofile)
            .add_tag(vec!["n".to_string(), "17".to_string()])
            .build()
            .map_err(CreqError::Transport)?;
        let request = PaymentRequestBuilder::default()
            .payment_id(payment_id)
            .amount(amount)
            .unit(unit)
            .single_use(true)
            .mints(mints)
            .add_transport(transport)
            .build();
        Ok(request.to_string())
    }

    /// Parse a claim's `creq` tag value back into a NUT-18 [`PaymentRequest`]. Accepts the
    /// `creqA…` (CBOR) form emitted by [`build_seller_creq`]; `PaymentRequest::from_str` also
    /// accepts the NUT-26 `creqB…` bech32 form.
    pub fn parse_creq(tag_value: &str) -> Result<PaymentRequest, CreqError> {
        PaymentRequest::from_str(tag_value).map_err(|e| CreqError::Parse(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    // TOOTH — an offer's harness request rides the existing `param` grammar and round-trips, and
    // "no preference" has exactly ONE representation on the wire: no tag. `any` and blank
    // canonicalise to that same absence, so a buyer stating indifference and one omitting it post
    // byte-identical offers.
    #[test]
    fn offer_carries_a_requested_agent_or_nothing_at_all() {
        let plain = OfferDraft::untargeted("t", "text/plain", 5, 1_800_000_001);
        let asking = plain.clone().requesting_agent(Some("Codex"));
        let draft = asking.to_event_draft();
        let param = draft
            .tags
            .iter()
            .find(|tag| tag.first() == Some("param") && tag.0.get(1).map(String::as_str) == Some("agent"))
            .expect("offer carries the agent param");
        assert_eq!(param.0, vec!["param", "agent", "codex"], "canonicalised on the way out");
        assert_eq!(
            parse_offer(&draft).expect("parse").requested_agent.as_deref(),
            Some("codex")
        );

        for indifferent in [None, Some("any"), Some("  "), Some("ANY")] {
            let draft = plain.clone().requesting_agent(indifferent).to_event_draft();
            assert_eq!(
                draft,
                plain.to_event_draft(),
                "{indifferent:?} must post the same offer as no request at all"
            );
            assert_eq!(parse_offer(&draft).expect("parse").requested_agent, None);
        }
    }

    // TOOTH — a claim advertises the harnesses its seller can run, in order; a seller that states
    // none emits a byte-identical pre-registry claim rather than an empty tag.
    #[test]
    fn claim_advertises_its_harnesses_in_order() {
        let advertised = claim_draft(
            "job-1",
            "buyer",
            "seller",
            "creqAtest",
            &["claude".to_owned(), "codex".to_owned()],
        );
        let tag = advertised
            .tags
            .iter()
            .find(|tag| tag.first() == Some("mobee_agent"))
            .expect("claim advertises its harnesses");
        assert_eq!(tag.0, vec!["mobee_agent", "claude", "codex"]);
        assert_eq!(
            crate::heartbeat::agents_from_tags(&advertised.tags),
            vec!["claude", "codex"]
        );

        let silent = claim_draft("job-1", "buyer", "seller", "creqAtest", &[]);
        assert!(silent.tags.iter().all(|tag| tag.first() != Some("mobee_agent")));
        assert!(crate::heartbeat::agents_from_tags(&silent.tags).is_empty());
    }

    use super::*;

    const BUYER: &str = "buyer";
    const SELLER: &str = "seller";
    const OTHER_SELLER: &str = "other-seller";
    const TESTNUT_MINT_URL: &str = "https://testnut.cashu.space";

    #[test]
    fn offer_draft_uses_locked_job_microstandard_tags() {
        let draft = OfferDraft::new(
            "write hello.txt",
            "text/plain",
            7,
            1_800_000_000,
            SELLER,
        )
        .to_event_draft();

        assert_eq!(draft.kind, JOB_OFFER_KIND);
        assert_eq!(draft.content, "");
        assert_eq!(
            draft.tags,
            vec![
                TagSpec::new(["i", "write hello.txt"]),
                TagSpec::new(["output", "text/plain"]),
                TagSpec::new(["amount", "7", "sat"]),
                TagSpec::new(["param", "deadline", "1800000000"]),
                TagSpec::new(["p", SELLER]),
                TagSpec::new(["t", MAXPLAYER_TAG]),
                TagSpec::new(["v", PROTOCOL_VERSION]),
            ]
        );
    }

    #[test]
    fn untargeted_offer_draft_omits_seller_tag() {
        let draft = OfferDraft::untargeted(
            "write hello.txt",
            "text/plain",
            7,
            1_800_000_000,
        )
        .to_event_draft();

        assert_eq!(draft.kind, JOB_OFFER_KIND);
        assert!(!has_tag_value(&draft.tags, "p", SELLER));
        assert_eq!(
            parse_offer(&draft).expect("parse offer").seller_pubkey,
            None
        );
    }

    #[test]
    fn parse_offer_round_trips_locked_tags() {
        let draft = OfferDraft::new(
            "summarize",
            "application/json",
            3,
            1_800_000_001,
            SELLER,
        )
        .to_event_draft();

        assert_eq!(
            parse_offer(&draft).expect("parse offer"),
            ParsedOffer {
                task: "summarize".into(),
                output: "application/json".into(),
                amount: 3,
                unit: "sat".into(),
                deadline_unix: 1_800_000_001,
                seller_pubkey: Some(SELLER.into()),
                requested_agent: None,
            }
        );
    }

    // Wire-cutover red-leg (rename PR B): a v1 offer round-trips, and the pre-flip wire is rejected
    // BOTH ways — t=mobee as out-of-namespace, v=0 as an unsupported version. This is the partition
    // the flag day accepts: rc.2 seats still speaking t=mobee / v=0 are invisible to a v1 parser.
    #[test]
    fn legacy_mobee_v0_offer_is_rejected_under_v1() {
        let ok = OfferDraft::new("summarize", "application/json", 3, 1_800_000_001, SELLER)
            .to_event_draft();
        assert!(parse_offer(&ok).is_ok(), "a v1 offer (t=maxplayer, v=1) must round-trip");

        let mut legacy_tag = ok.clone();
        for tag in legacy_tag.tags.iter_mut() {
            if tag.first() == Some("t") {
                tag.0 = vec!["t".to_owned(), "mobee".to_owned()];
            }
        }
        assert!(
            matches!(parse_offer(&legacy_tag), Err(OfferParseError::MissingMaxplayerTag)),
            "a legacy t=mobee offer must be rejected as outside the maxplayer namespace"
        );

        let mut legacy_ver = ok.clone();
        for tag in legacy_ver.tags.iter_mut() {
            if tag.first() == Some("v") {
                tag.0 = vec!["v".to_owned(), "0".to_owned()];
            }
        }
        assert!(
            matches!(parse_offer(&legacy_ver), Err(OfferParseError::UnsupportedVersion(v)) if v == "0"),
            "a legacy v=0 offer must be rejected as an unsupported version"
        );
    }

    #[test]
    fn targeting_helpers_fail_closed_for_targeted_offers() {
        let targeted = parse_offer(
            &OfferDraft::new("task", "text/plain", 1, 2, SELLER).to_event_draft(),
        )
        .expect("targeted offer");
        let untargeted = parse_offer(
            &OfferDraft::untargeted("task", "text/plain", 1, 2).to_event_draft(),
        )
        .expect("untargeted offer");

        assert!(is_targeted(&targeted));
        assert!(!is_targeted(&untargeted));
        assert!(targeted.seller_matches(SELLER));
        assert!(!targeted.seller_matches(OTHER_SELLER));
        assert!(untargeted.seller_matches(OTHER_SELLER));
        assert_seller_matches(&targeted, SELLER).expect("matching seller");
        assert_seller_matches(&untargeted, OTHER_SELLER).expect("untargeted seller");
        assert_eq!(
            assert_seller_matches(&targeted, OTHER_SELLER),
            Err(TargetingError {
                expected: SELLER.into(),
                actual: OTHER_SELLER.into(),
            })
        );
    }

    #[test]
    fn claim_and_award_use_split_maxplayer_kinds() {
        // The claim (processing) is its own claim kind, and the buyer-authored award
        // is the award kind — each distinct from the seller's feedback kind.
        assert_eq!(
            claim_draft("offer", BUYER, SELLER, "creqAtest", &[]),
            EventDraft::new(
                JOB_CLAIM_KIND,
                vec![
                    TagSpec::new(["status", "processing"]),
                    TagSpec::new(["e", "offer", "", "root"]),
                    TagSpec::new(["p", BUYER]),
                    TagSpec::new(["p", SELLER]),
                    TagSpec::new(["creq", "creqAtest"]),
                    TagSpec::new(["t", MAXPLAYER_TAG]),
                    TagSpec::new(["v", PROTOCOL_VERSION]),
                ],
                ""
            )
        );

        assert_eq!(
            award_draft("offer", "claim", BUYER, SELLER),
            EventDraft::new(
                JOB_AWARD_KIND,
                vec![
                    TagSpec::new(["status", "accepted"]),
                    TagSpec::new(["e", "offer", "", "root"]),
                    TagSpec::new(["e", "claim"]),
                    TagSpec::new(["p", BUYER]),
                    TagSpec::new(["p", SELLER]),
                    TagSpec::new(["t", MAXPLAYER_TAG]),
                    TagSpec::new(["v", PROTOCOL_VERSION]),
                ],
                ""
            )
        );

        // The awarded seller reads back the offer + winning-claim ids from that same award.
        assert_eq!(
            parse_award(&award_draft("offer", "claim", BUYER, SELLER)),
            Some(ParsedAward {
                offer_id: "offer".into(),
                claim_id: "claim".into(),
            })
        );
        // A non-award event yields no selection.
        assert_eq!(
            parse_award(&claim_draft("offer", BUYER, SELLER, "creqAtest", &[])),
            None
        );
    }

    #[test]
    fn every_lifecycle_draft_roots_its_offer_e_tag() {
        // Every lifecycle stage after the offer carries exactly one `root`-marked `e` tag naming the
        // offer, so an observer holding nothing but public tags can join any stage to its job. The
        // stage this exists for is FEEDBACK: a refusal that cannot be joined to an offer is missing
        // from the seller's reliability record, and award-without-delivery is the signal that record
        // is for.
        //
        // Written over the set rather than once per builder so the shared property is asserted in
        // one place. ⚠ It does NOT catch a builder added later — nothing here enumerates the
        // builders; the crate exposes the kinds as seven separate constants and no list to check a
        // new one against. A new lifecycle builder needs a row added by hand.
        const OFFER: &str = "offer";
        let lifecycle = [
            ("claim", claim_draft(OFFER, BUYER, SELLER, "creqAtest", &[])),
            ("award", award_draft(OFFER, "claim", BUYER, SELLER)),
            ("accept", accept_draft(OFFER, "claim", BUYER, SELLER)),
            (
                "result",
                result_draft(
                    OFFER,
                    BUYER,
                    "text/plain",
                    7,
                    "hash",
                    "seller-sig",
                    "done",
                    None,
                    &[],
                ),
            ),
            ("feedback", error_draft(OFFER, BUYER, SELLER, ReasonCode::ExecutionFailed, "refused")),
            (
                "receipt",
                receipt_draft(
                    OFFER,
                    "result",
                    BUYER,
                    SELLER,
                    TESTNUT_MINT_URL,
                    7,
                    "hash",
                    "seller-sig",
                    "buyer-sig",
                    None,
                    None,
                    &[],
                ),
            ),
        ];

        for (stage, draft) in &lifecycle {
            let rooted: Vec<&TagSpec> = draft
                .tags
                .iter()
                .filter(|tag| {
                    tag.first() == Some("e") && tag.0.get(3).map(String::as_str) == Some("root")
                })
                .collect();
            // Exactly one, not at-least-one: a second root marker would make the job root ambiguous
            // to a reader that takes the first match, which is the failure the marker removes.
            assert_eq!(
                rooted.len(),
                1,
                "{stage}: expected exactly one root-marked e tag, found {}",
                rooted.len()
            );
            assert_eq!(
                rooted[0].value(),
                Some(OFFER),
                "{stage}: the root marker must name the offer, not another event in the chain"
            );
        }

        // The stages covered are the trade block minus the offer itself. Asserted against the kind
        // constants so a renumbering cannot leave a stage silently uncovered.
        let covered: Vec<u16> = lifecycle.iter().map(|(_, draft)| draft.kind).collect();
        assert_eq!(
            covered,
            vec![
                JOB_CLAIM_KIND,
                JOB_AWARD_KIND,
                JOB_ACCEPT_KIND,
                JOB_RESULT_KIND,
                JOB_FEEDBACK_KIND,
                JOB_RECEIPT_KIND,
            ]
        );
        assert!(
            !covered.contains(&JOB_OFFER_KIND),
            "the offer is the root; it does not tag one"
        );
    }

    #[test]
    fn result_and_receipt_keep_market_tags_outside_driver() {
        let result = result_draft(
            "offer",
            BUYER,
            "text/plain",
            7,
            "hash",
            "seller-sig",
            "done",
            None,
            &[],
        );
        assert_eq!(result.kind, JOB_RESULT_KIND);
        assert_eq!(result.content, "done");
        assert!(has_tag_value(&result.tags, "job-hash", "hash"));
        assert!(has_tag_value_at(&result.tags, "sig", 1, "seller"));
        assert!(has_tag_value_at(&result.tags, "sig", 2, "seller-sig"));

        let receipt = receipt_draft(
            "offer",
            "result",
            BUYER,
            SELLER,
            TESTNUT_MINT_URL,
            7,
            "hash",
            "seller-sig",
            "buyer-sig",
            None,
            None,
            &[],
        );
        assert_eq!(receipt.kind, JOB_RECEIPT_KIND);
        assert!(has_tag_value(&receipt.tags, "mint", TESTNUT_MINT_URL));
        // No creq bound ⇒ no creq-hash tag.
        assert!(first_tag(&receipt.tags, "creq-hash").is_none());
        assert!(has_tag_value_at(&receipt.tags, "e", 1, "result"));
        assert!(has_tag_value_at(&receipt.tags, "e", 3, "reply"));
        assert_eq!(
            receipt
                .tags
                .iter()
                .filter(|tag| tag.first() == Some("sig"))
                .count(),
            2
        );
        assert!(has_tag_value_at(&receipt.tags, "sig", 1, "seller"));
        assert!(has_tag_value_at(&receipt.tags, "sig", 1, "buyer"));
        // No delivery binding requested ⇒ the binding tags are absent from the receipt.
        assert!(first_tag(&receipt.tags, "delivery_integrity_hash").is_none());
    }

    #[test]
    fn receipt_draft_binds_delivery_and_echoes_exec_metadata() {
        let exec = vec![
            TagSpec::new(["harness", "claude-agent-acp"]),
            TagSpec::new(["metadata_trust", "seller-claimed"]),
            TagSpec::new(["wall_time", "1234", "ms"]),
        ];
        let receipt = receipt_draft(
            "offer",
            "result",
            BUYER,
            SELLER,
            TESTNUT_MINT_URL,
            7,
            "hash",
            "seller-sig",
            "buyer-sig",
            Some(&"cc".repeat(32)),
            Some(ReceiptDelivery {
                integrity_hash: &"a".repeat(40),
                kind: "fork",
            }),
            &exec,
        );
        // A bound creq surfaces as a `creq-hash` tag on the receipt event.
        assert!(has_tag_value(&receipt.tags, "creq-hash", &"cc".repeat(32)));
        // Delivery binding present and typed.
        assert!(has_tag_value(
            &receipt.tags,
            "delivery_integrity_hash",
            &"a".repeat(40)
        ));
        assert!(has_tag_value(&receipt.tags, "delivery_kind", "fork"));
        // Filtered echo carried through, with its required provenance marker.
        assert!(has_tag_value(&receipt.tags, "harness", "claude-agent-acp"));
        assert!(has_tag_value(&receipt.tags, "metadata_trust", "seller-claimed"));
        // t/v markers stay last.
        assert_eq!(receipt.tags[receipt.tags.len() - 2], maxplayer_tag());
        assert_eq!(receipt.tags[receipt.tags.len() - 1], version_tag());
    }

    #[test]
    fn settled_offer_id_reads_the_root_offer_of_a_receipt_only() {
        // A co-signed receipt names its settled offer as the `root`-marked `e` tag.
        let receipt = receipt_draft(
            "the-offer", "result", BUYER, SELLER, TESTNUT_MINT_URL, 7, "hash", "seller-sig",
            "buyer-sig", None, None, &[],
        );
        assert_eq!(settled_offer_id(&receipt).as_deref(), Some("the-offer"));
        // The kind gate is load-bearing: a result carries an identical root `e` for the SAME offer,
        // but it is not a settlement signal, so "which offer did this receipt settle?" must return
        // None for it — never conflate a delivery with a settlement.
        let result = git_result_draft(
            "the-offer", BUYER, "https://example.invalid/repo.git", "maxplayer/job",
            &"a".repeat(40), 7, "hash", "seller-sig", "commit", &[],
        );
        assert_eq!(settled_offer_id(&result), None);
    }

    #[test]
    fn result_draft_carries_seller_claimed_exec_metadata_after_sig() {
        let exec = vec![
            TagSpec::new(["harness", "codex-acp-ng"]),
            TagSpec::new(["metadata_trust", "seller-claimed"]),
            TagSpec::new(["tokens", "3172", "total"]),
        ];
        let result = git_result_draft(
            "offer",
            BUYER,
            "https://example.invalid/repo.git",
            "maxplayer/job",
            &"a".repeat(40),
            7,
            "hash",
            "seller-sig",
            "commit",
            &exec,
        );
        assert!(has_tag_value(&result.tags, "harness", "codex-acp-ng"));
        assert!(has_tag_value(&result.tags, "metadata_trust", "seller-claimed"));
        // exec-metadata sits after the seller signature, before the protocol markers.
        let sig_at = result
            .tags
            .iter()
            .position(|tag| tag.first() == Some("sig"))
            .unwrap();
        let harness_at = result
            .tags
            .iter()
            .position(|tag| tag.first() == Some("harness"))
            .unwrap();
        assert!(harness_at > sig_at);
    }

    #[test]
    fn git_result_parses_repo_branch_and_full_commit_oid() {
        let result = EventDraft::new(
            JOB_RESULT_KIND,
            vec![
                TagSpec::new(["delivery", "git"]),
                TagSpec::new(["repo", "https://example.invalid/repo.git"]),
                TagSpec::new(["branch", "maxplayer/job"]),
                TagSpec::new(["commit", &"a".repeat(40)]),
                TagSpec::new(["t", MAXPLAYER_TAG]),
            ],
            "",
        );

        let delivery = parse_git_result_delivery(&result).expect("parse git delivery");
        assert_eq!(delivery.repo(), "https://example.invalid/repo.git");
        assert_eq!(delivery.branch(), "maxplayer/job");
        assert_eq!(delivery.commit_oid().as_str(), "a".repeat(40));
    }

    #[test]
    fn git_result_refuses_an_abbreviated_commit_oid() {
        let result = EventDraft::new(
            JOB_RESULT_KIND,
            vec![
                TagSpec::new(["delivery", "git"]),
                TagSpec::new(["repo", "repo"]),
                TagSpec::new(["branch", "work"]),
                TagSpec::new(["commit", "abc123"]),
                TagSpec::new(["t", MAXPLAYER_TAG]),
            ],
            "",
        );

        assert_eq!(
            parse_git_result_delivery(&result),
            Err(GitResultParseError::InvalidDelivery(
                DeliveryError::InvalidCommitOid
            ))
        );
    }

    #[test]
    fn git_result_cannot_redirect_away_from_the_offered_repo_or_branch() {
        let offer = EventDraft::new(
            JOB_OFFER_KIND,
            vec![
                TagSpec::new(["delivery", "git"]),
                TagSpec::new(["repo", "https://example.invalid/offered.git"]),
                TagSpec::new(["branch", "maxplayer/job"]),
            ],
            "",
        );
        let redirected = EventDraft::new(
            JOB_RESULT_KIND,
            vec![
                TagSpec::new(["delivery", "git"]),
                TagSpec::new(["repo", "https://attacker.invalid/other.git"]),
                TagSpec::new(["branch", "maxplayer/job"]),
                TagSpec::new(["commit", &"a".repeat(40)]),
                TagSpec::new(["t", MAXPLAYER_TAG]),
            ],
            "",
        );

        assert_eq!(
            parse_bound_git_delivery(&offer, &redirected),
            Err(BoundGitDeliveryError::TargetMismatch)
        );
    }

    fn has_tag_value_at(tags: &[TagSpec], name: &str, index: usize, value: &str) -> bool {
        tags.iter().any(|tag| {
            tag.0.first().map(String::as_str) == Some(name)
                && tag.0.get(index).map(String::as_str) == Some(value)
        })
    }
}

/// Seller-authored `creq` in the claim. Gated on `wallet` because the
/// `creq` builder uses cashu's `nut18` types (only linked under that feature).
#[cfg(all(test, feature = "wallet"))]
mod creq_tests {
    use std::str::FromStr;

    use cashu::nuts::nut18::TransportType;
    use cashu::{Amount, CurrencyUnit, MintUrl};

    use super::creq::{build_seller_creq, parse_creq};
    use super::{claim_draft, TagSpec};

    const MINT_A: &str = "https://testnut.cashudevkit.org";
    const MINT_B: &str = "https://mint.example.com";

    fn seller_hex() -> String {
        nostr_sdk::Keys::generate().public_key().to_hex()
    }

    /// The claim carries a `creq` tag whose value starts with "creqA".
    #[test]
    fn claim_carries_creq() {
        let seller = seller_hex();
        let creq =
            build_seller_creq("job-1", 21, "sat", &[MINT_A.to_string()], &seller).expect("build creq");
        assert!(creq.starts_with("creqA"), "creq must start with creqA: {creq}");

        let draft = claim_draft("job-1", "buyer-pubkey", &seller, &creq, &[]);
        let creq_tag = draft
            .tags
            .iter()
            .find(|tag| tag.first() == Some("creq"))
            .expect("claim carries a creq tag");
        assert_eq!(creq_tag.value(), Some(creq.as_str()));
        assert!(creq_tag.value().unwrap().starts_with("creqA"));
    }

    /// Round-trip: `PaymentRequest::from_str(tag)` yields a=offer.amount, u=offer.unit,
    /// m=accepted_mints (order preserved), one nostr transport to the seller, single-use, no nut10.
    #[test]
    fn creq_roundtrip() {
        let seller = seller_hex();
        let mints = vec![MINT_A.to_string(), MINT_B.to_string()];
        let creq = build_seller_creq("attempt-9", 21, "sat", &mints, &seller).expect("build creq");

        let request = parse_creq(&creq).expect("parse creq");
        assert_eq!(request.payment_id.as_deref(), Some("attempt-9"));
        assert_eq!(request.amount, Some(Amount::from(21)));
        assert_eq!(request.unit, Some(CurrencyUnit::Sat));
        assert_eq!(
            request.mints,
            vec![
                MintUrl::from_str(MINT_A).unwrap(),
                MintUrl::from_str(MINT_B).unwrap(),
            ]
        );
        assert_eq!(request.single_use, Some(true));
        assert!(request.nut10.is_none(), "no nut10 locking condition");

        assert_eq!(request.transports.len(), 1, "exactly one transport");
        let transport = &request.transports[0];
        assert_eq!(transport._type, TransportType::Nostr);
        assert!(
            transport.target.starts_with("nprofile1"),
            "transport target is the seller nprofile: {}",
            transport.target
        );
        assert_eq!(
            transport.tags,
            vec![vec!["n".to_string(), "17".to_string()]],
            "NIP-17 transport tag",
        );
    }

    /// The `creq` tag is a stable round-trip through the claim draft: the exact string the
    /// seller authored is what a buyer parses off the claim.
    #[test]
    fn claim_creq_tag_parses_back() {
        let seller = seller_hex();
        let creq =
            build_seller_creq("job-7", 5, "sat", &[MINT_A.to_string()], &seller).expect("build creq");
        let draft = claim_draft("job-7", "buyer", &seller, &creq, &[]);
        let tag: &TagSpec = draft
            .tags
            .iter()
            .find(|tag| tag.first() == Some("creq"))
            .expect("creq tag");
        let request = parse_creq(tag.value().unwrap()).expect("parse creq off claim");
        assert_eq!(request.amount, Some(Amount::from(5)));
        assert_eq!(request.mints, vec![MintUrl::from_str(MINT_A).unwrap()]);
    }
}
