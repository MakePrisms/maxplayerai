//! Seller daemon state machine.
//!
//! Loop:
//! 1. Subscribe **offers + gift-wraps from START** (early pay buffered).
//! 2. On targeted offer passing the rate-gate → claim feedback → journal claim (single-flight).
//! 3. Run agent (`--features acp` fail-closed) → git push (allowlist+scrub) → result.
//! 4. **Reconcile** buffered/already-received 1059 wraps against the new result.
//! 5. Bind job_id(+result_id) → `terms_for_offer` → `CdkSellerReceive::receive`
//!    (`Amount == offer.amount`) → journal receipt (`amount_received == offer.amount`).
//!
//! Never logs NIP-17 plaintext / tokens / key material.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(feature = "acp")]
use sha2::{Digest, Sha256};

use crate::buyer_fund::{self, FundError};
use crate::contribution::ContributionOffer;
use crate::driver::UsageMetadata;
use crate::episode::{Episode, EpisodeKind, EpisodeLog, EpisodeOutcome, UsageRecord};
use crate::gateway::{
    self, claim_draft, error_draft, git_result_draft, parse_award, parse_offer, EventDraft,
    OfferParseError, ParsedAward, ParsedOffer, TagSpec, JOB_AWARD_KIND, JOB_OFFER_KIND,
};
use crate::home::{self, HomeError, MobeeHome, DEFAULT_MINT_URL};
use crate::job_lifecycle::{event_to_draft, job_hash_for_offer};
use crate::payment_send::ReceivedPayment;
use crate::payment_wallet::{CdkSellerReceive, PaymentPolicy, PaymentWalletError};
use crate::seller::{
    cashu_secret_from_nostr_hex, job_deadline_unix, plan_orphaned_claims, rate_gate_allows,
    require_seller_config, sign_receipt_hash, unwrap_own_payment_gift_wrap, ClaimLiveness,
    OrphanClaim, SellerError, SellerJournal,
};
use crate::seller_git::{self, SellerGitError};

/// In-flight single-flight lock (one job in the PROCESSING phase per process).
/// Held from claim through delivery (result-kind), then released — a delivered-but-unpaid
/// job awaiting payment does NOT hold this lock.
static FLIGHT: AtomicBool = AtomicBool::new(false);

/// Upper bound on delivered-but-unpaid jobs tracked concurrently (bounded memory).
/// Reaching it back-pressures new claims with a logged skip reason (never a silent drop).
const AWAITING_PAYMENT_CAP: usize = 16;

/// Upper bound on claims awaiting a buyer award concurrently (bounded memory). A claim is cheap —
/// no compute runs until the award arrives — so a seller may hold several at once; reaching this
/// cap back-pressures new claims with a logged skip reason (never a silent drop).
const AWAITING_AWARD_CAP: usize = 16;

/// Cadence (seconds) of the periodic sweep that releases claims whose offer deadline passed with no
/// award. A pure per-job check (`offer.deadline_unix <= now`) rides the select loop on this tick, so
/// a claim never waits for an award past the offer it was made against. A FIXED constant, not a user
/// config knob; the env override below is a test seam only (mirrors the wrap-backfill cadence).
const AWARD_SWEEP_INTERVAL_SECS: u64 = 30;

/// Test-only env seam overriding [`AWARD_SWEEP_INTERVAL_SECS`] (NOT a user config knob; mirrors
/// [`WRAP_BACKFILL_INTERVAL_ENV`]). A `0` or unparseable value is ignored. No production path sets it.
const AWARD_SWEEP_INTERVAL_ENV: &str = "MOBEE_AWARD_SWEEP_INTERVAL_SECS";

/// Bound on agent attempts per job. A transient agent error is retried up to
/// this many times, but only while the job deadline still has room — the retry loop never
/// outlives the deadline (see [`run_agent_with_retry`]).
const MAX_AGENT_ATTEMPTS: u32 = 3;

/// Relay-read timeout for the backfill pre-claim money-safety check ([`SellerDaemon::
/// backfill_offer_blocked`]). Matches the job-lifecycle fetch budget: fetches terminate on the
/// relay's EOSE, so this is an upper bound, not a fixed wait. On a slow/unreachable relay the
/// check fails CLOSED (skip), so a small budget is safe.
const BACKFILL_CHECK_TIMEOUT_SECS: u64 = 5;

/// Cadence (seconds) of the periodic seller wrap backfill. A running daemon re-runs the
/// SAME stored-wrap backfill the boot path uses every this-many seconds, so an AGED relay
/// subscription that has silently stopped delivering kind-1059 payment gift-wraps still recovers
/// WITHOUT a restart. Field-observed: fresh subscriptions deliver a wrap within ~1 min, but a
/// subscription ~10+ min old was seen to go deaf and never deliver again — a payment then sat
/// unredeemed until the daemon was manually restarted (which re-ran the boot backfill). This is a
/// FIXED constant, NOT a user config knob (charter: no new config); the env override below exists
/// only as a test seam (mirrors the heartbeat cadence).
const WRAP_BACKFILL_INTERVAL_SECS: u64 = 300;

/// Slack subtracted from the oldest-unsettled-delivery timestamp when it bounds the wrap-backfill
/// `since` cursor. The buyer's payment wrap is created AFTER the seller's delivery, but clock skew
/// between the two parties could place the wrap's `created_at` slightly before the delivery ts;
/// this margin (1h) keeps such a wrap inside the fetch window.
const WRAP_BACKFILL_MARGIN_SECS: u64 = 3600;

/// Test-only env seam overriding [`WRAP_BACKFILL_INTERVAL_SECS`] (NOT a user config knob; mirrors
/// `MOBEE_HEARTBEAT_INTERVAL_SECS`). A `0` or unparseable value is ignored. No production path
/// sets it — the periodic cadence is the fixed constant in production.
const WRAP_BACKFILL_INTERVAL_ENV: &str = "MOBEE_WRAP_BACKFILL_INTERVAL_SECS";

#[derive(Debug)]
pub enum DaemonError {
    Seller(SellerError),
    Home(HomeError),
    Fund(FundError),
    Wallet(PaymentWalletError),
    Git(SellerGitError),
    Relay(String),
    Agent(String),
    Policy(String),
    Config(String),
    AcpRequired,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seller(error) => write!(f, "{error}"),
            Self::Home(error) => write!(f, "{error}"),
            Self::Fund(error) => write!(f, "{error}"),
            Self::Wallet(error) => write!(f, "{error}"),
            Self::Git(error) => write!(f, "{error}"),
            Self::Relay(message) => write!(f, "seller relay error: {message}"),
            Self::Agent(message) => write!(f, "seller agent error: {message}"),
            Self::Policy(message) => write!(f, "seller policy: {message}"),
            Self::Config(message) => write!(f, "seller config: {message}"),
            Self::AcpRequired => write!(
                f,
                "seller agent-run requires rebuilding with the acp feature: \
                 cargo run -p mobee --features acp -- sell run"
            ),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Whether a pay-path error is an EXPECTED idempotent re-see of an already-redeemed kind-1059
/// (the payment landed on an earlier delivery — a relay re-delivery of the gift-wrap, or a
/// restart). Two idempotent surfaces reach the pay-path log: the journal's pay-once guard
/// (`SellerError::Journal` "already receipted") and the mint reporting the proofs already spent
/// (cdk `TokenAlreadySpent`, surfaced as a `PaymentWalletError::Wallet` string). Both mean the
/// sats are already ours, so the re-see is logged at info, not error.
///
/// LOGGING classification ONLY — redeem, matching, reconcile and control flow are unchanged;
/// the error is still returned and handled identically, only the log line's severity differs.
fn is_idempotent_already_redeemed(error: &DaemonError) -> bool {
    // Journal pay-once guard: typed variant + our own stable message.
    if let DaemonError::Seller(SellerError::Journal(message)) = error {
        if message.to_ascii_lowercase().contains("already receipted") {
            return true;
        }
    }
    // Mint-level already-spent. cdk surfaces `TokenAlreadySpent` ("Token Already Spent") as a
    // string in `PaymentWalletError::Wallet` — there is no typed variant, so this matches on the
    // message substring.
    // TODO: match a typed cdk already-spent error instead of substring.
    let message = error.to_string().to_ascii_lowercase();
    message.contains("already spent") || message.contains("already redeemed")
}

/// Decision for a seller-receive result on the redeem path (finding S).
enum RedeemDecision {
    /// Receive succeeded — finalize a receipt for this redeemed amount.
    Finalize(u64),
    /// Idempotent re-see: the token is already spent AND a COMPLETED receipt exists for this job, so
    /// we already collected AND receipted it. No-op — never double-collect / re-receipt / re-announce.
    IdempotentNoOp,
    /// Fail closed — do NOT finalize; refuse and buffer for manual reconcile.
    Refuse(DaemonError),
}

/// Classify a seller-receive result on the redeem path (finding S). The load-bearing rule: NEVER
/// infer "our swap already landed" from a pending-receive breadcrumb. The breadcrumb is appended
/// before EVERY swap, so it proves only intent, never a prior collection — inferring collection from
/// it let a malicious buyer replay an already-redeemed seller-locked token against a NEW same-value
/// job and forge a receipt for zero new funds (theft-of-service).
///
/// The ONLY positive proof of OUR OWN prior collection is a COMPLETED `Receipt` for this job
/// (`has_receipt == true`), so on an "already spent" error:
/// - `has_receipt == Ok(true)`  → idempotent no-op (the legit backfill/restart re-see);
/// - `has_receipt == Ok(false)` → refuse + buffer (replay/theft, or a genuine interrupted redeem —
///   both indistinguishable, so fail closed and leave it for manual reconcile);
/// - `has_receipt == Err(_)`    → refuse (fail CLOSED: an unreadable receipt journal must NEVER be
///   read as "no receipt ⇒ safe to proceed" — same posture as the wrap-entry gate, item G).
///
/// Any non-already-spent receive error also refuses. `has_receipt` is a closure so the journal is
/// read only on the already-spent branch, and so the decision is unit-testable without a mint.
fn classify_redeem_outcome(
    receive_result: Result<u64, DaemonError>,
    has_receipt: impl FnOnce() -> Result<bool, SellerError>,
) -> RedeemDecision {
    match receive_result {
        Ok(amount) => RedeemDecision::Finalize(amount),
        Err(error) if !is_idempotent_already_redeemed(&error) => RedeemDecision::Refuse(error),
        Err(error) => match has_receipt() {
            Ok(true) => RedeemDecision::IdempotentNoOp,
            Ok(false) => RedeemDecision::Refuse(error),
            Err(read_err) => RedeemDecision::Refuse(DaemonError::from(read_err)),
        },
    }
}

/// Redeem-time accepted-mint set for a job (Fix Q — terms-drift). A RESTORED/reconstructed job
/// (non-empty `job_accepted_mints`) settles against the mints it ORIGINALLY advertised, so a
/// config change across restart can neither strand an old payment (mint dropped) nor let a
/// newly-added mint settle an old claim. A live job delivered this process (empty) settles against
/// current config, which authored its creq. Pure so the selection is unit-testable without a wallet.
fn redeem_accepted_mints<'a>(
    job_accepted_mints: &'a [String],
    config_accepted_mints: &'a [String],
) -> &'a [String] {
    if job_accepted_mints.is_empty() {
        config_accepted_mints
    } else {
        job_accepted_mints
    }
}

/// (g) Collect-leg observability: the single, key-material-free line logged the moment a
/// kind-1059 payment is redeemed (proofs swapped at the mint), so the collect leg is
/// diagnosable in the daemon's stderr. NEVER includes the token or any secret.
fn collect_ok_log_line(
    job_id: &str,
    result_id: &str,
    amount_received: u64,
    expected: u64,
    mint: &str,
) -> String {
    format!(
        "seller collect ok: job_id={job_id} result_id={result_id} \
         amount_received={amount_received} expected={expected} mint={mint}"
    )
}

impl From<SellerError> for DaemonError {
    fn from(value: SellerError) -> Self {
        Self::Seller(value)
    }
}
impl From<HomeError> for DaemonError {
    fn from(value: HomeError) -> Self {
        Self::Home(value)
    }
}
impl From<FundError> for DaemonError {
    fn from(value: FundError) -> Self {
        Self::Fund(value)
    }
}
impl From<PaymentWalletError> for DaemonError {
    fn from(value: PaymentWalletError) -> Self {
        Self::Wallet(value)
    }
}
impl From<SellerGitError> for DaemonError {
    fn from(value: SellerGitError) -> Self {
        Self::Git(value)
    }
}

/// Active claimed job (single-flight slot).
#[derive(Debug, Clone)]
pub struct ActiveJob {
    pub job_id: String,
    pub buyer_pubkey: String,
    pub offer: ParsedOffer,
    pub claim_id: String,
    pub result_id: Option<String>,
    pub deadline_unix: u64,
    pub workdir: PathBuf,
    /// The parsed contribution offer (target pin + base + accepts) when this is a
    /// contribution job; `None` ⇒ from-scratch (empty-base delivery).
    pub contribution: Option<ContributionOffer>,
    /// Delivery facts captured at successful result-kind publish, carried through
    /// `mark_delivered` so the terminal episode (paid at receipt, or unpaid at eviction) is a
    /// single complete append. `None` until the job delivers. Diagnostic only — never money state.
    pub delivery: Option<DeliveryRecord>,
    /// The mints this job ORIGINALLY advertised (creq `m` list), populated ONLY for a job
    /// restored/reconstructed after restart (Fix Q). The redeem guard uses these instead of
    /// current config so a config change cannot strand an old payment or let a newly-added mint
    /// settle an old claim. EMPTY for a job delivered in this process — current config authored
    /// its creq and IS the original terms.
    pub accepted_mints: Vec<String>,
}

/// The delivery-time facts an [`Episode`] needs, captured once at result-kind publish.
/// Stashed on the [`ActiveJob`] so the (possibly later) paid/unpaid terminal writes one complete
/// episode without re-deriving anything on the money path.
#[derive(Debug, Clone)]
pub struct DeliveryRecord {
    pub result_id: String,
    pub commit_oid: String,
    pub git_remote: String,
    pub branch: String,
    pub delivery_kind: String,
    pub harness: String,
    pub wall_time_ms: u64,
    pub usage: Option<UsageMetadata>,
    pub transcript_ref: String,
    pub deliver_ts: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorReasonCode {
    AgentSpawnFailed,
    AgentRunFailed,
    AgentTimeout,
    GitForkFailed,
    GitPushFailed,
    ContributionUnsupported,
    ContributionMalformed,
    ClaimReleased,
    Internal,
}

impl ErrorReasonCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::AgentSpawnFailed => "agent_spawn_failed",
            Self::AgentRunFailed => "agent_run_failed",
            Self::AgentTimeout => "agent_timeout",
            Self::GitForkFailed => "git_fork_failed",
            Self::GitPushFailed => "git_push_failed",
            Self::ContributionUnsupported => "contribution_unsupported",
            Self::ContributionMalformed => "contribution_malformed",
            Self::ClaimReleased => "claim_released",
            Self::Internal => "internal",
        }
    }
}

/// What [`SellerDaemon::classify_offer`] decided to do with an offer event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferDisposition {
    /// Admitted — claim the offer (single-flight reservation happens next).
    Claim(ClaimIntent),
    /// Not claimed — carries a named, loggable reason (never a silent drop).
    Skip(OfferSkip),
}

/// What [`SellerDaemon::on_award_event`] did with a kind-award AWARD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwardOutcome {
    /// The award named this seller's claim — the job is now in the `active` slot, ready to execute.
    Execute { job_id: String },
    /// The award named a DIFFERENT claim — this seller's claim was released without executing.
    Released { job_id: String, reason: String },
    /// No outstanding claim of ours matched (late, unrelated, or author not the offer buyer).
    Ignored,
}

/// The pure award-match decision over the parked claims — no I/O, so the security-critical
/// selection is unit-testable. An award binds to a parked claim ONLY when it roots on that claim's
/// offer AND its author is that offer's buyer (a third party cannot drive execute or release).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AwardMatch {
    /// The matched claim (by index) is the one the award names — execute it.
    Execute(usize),
    /// The award names a different claim than the matched one — release ours.
    Release(usize),
    /// No parked claim matches this award — ignore it.
    Ignored,
}

fn match_award(awaiting_award: &[ActiveJob], award: &ParsedAward, award_author: &str) -> AwardMatch {
    match awaiting_award
        .iter()
        .position(|job| job.job_id == award.offer_id && job.buyer_pubkey == award_author)
    {
        Some(index) if awaiting_award[index].claim_id == award.claim_id => AwardMatch::Execute(index),
        Some(index) => AwardMatch::Release(index),
        None => AwardMatch::Ignored,
    }
}

/// Everything needed to publish a claim + journal it, resolved without relay I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimIntent {
    pub job_id: String,
    pub buyer_pubkey: String,
    pub offer: ParsedOffer,
    pub deadline_unix: u64,
    /// Parsed contribution offer, threaded into the active job.
    pub contribution: Option<ContributionOffer>,
}

/// Enumerated reasons an offer is not claimed. Every variant maps to a logged reason —
/// there is no silent-drop path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferSkip {
    /// Event is not a offer-kind offer.
    NotAnOffer { kind: u16 },
    /// Offer tags did not parse.
    Unparseable,
    /// Offer carried a protocol version this seller does not speak (`OfferParseError::UnsupportedVersion`).
    /// Distinct from [`Self::Unparseable`]: a cross-version offer is well-formed under another
    /// version, not malformed tags. Surfaces as a stable `refusal_reason_code` so operators and
    /// buyers can tell version skew from genuine parse failure.
    UnsupportedVersion { version: String },
    /// Offer's own deadline (`param deadline`, absolute unix) has already passed at `now`.
    /// Money-safety: a lapsed offer is REFUSED so a backfilled (stored) offer can never be
    /// resurrected with a fresh `now + timeout` deadline (the pre-backfill hazard in
    /// `job_deadline_unix`). A pure function of the offer event + `now`, so it holds for a
    /// live offer too — an offer whose deadline already passed is dead regardless of delivery.
    DeadlineExpired { deadline_unix: u64, now: u64 },
    /// Rate-gate refused (not targeted to us / below rate / untargeted without opt-in).
    RateGate { reason: String },
    /// Journal already has a claim/receipt/release for this job (dedup).
    AlreadyProcessed,
    /// A job is already in the PROCESSING phase (single-flight). NOT triggered by
    /// delivered-but-unpaid jobs — those await payment without holding the slot.
    ProcessingBusy { job_id: String },
    /// Too many delivered-but-unpaid jobs pending payment (bounded-memory back-pressure).
    AwaitingPaymentFull { capacity: usize },
    /// Too many claims awaiting a buyer award (bounded-memory back-pressure).
    AwaitingAwardFull { capacity: usize },
    /// A `job-class=contribution` offer arrived but this seller has contribution support
    /// disabled (`contribution_enabled=false`). Emits a feedback-kind error (interop courtesy) instead
    /// of running it as from-scratch.
    ContributionUnsupported,
    /// A `job-class=contribution` offer whose target-repo/base pins are malformed —
    /// refused (fail-closed; never run as from-scratch). Emits a feedback-kind error.
    ContributionMalformed { reason: String },
}

impl OfferSkip {
    /// Machine-mappable variant name (`refusal_reason_code`). Stable enumerated set.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAnOffer { .. } => "NotAnOffer",
            Self::Unparseable => "Unparseable",
            Self::UnsupportedVersion { .. } => "UnsupportedVersion",
            Self::DeadlineExpired { .. } => "DeadlineExpired",
            Self::RateGate { .. } => "RateGate",
            Self::AlreadyProcessed => "AlreadyProcessed",
            Self::ProcessingBusy { .. } => "ProcessingBusy",
            Self::AwaitingPaymentFull { .. } => "AwaitingPaymentFull",
            Self::AwaitingAwardFull { .. } => "AwaitingAwardFull",
            Self::ContributionUnsupported => "ContributionUnsupported",
            Self::ContributionMalformed { .. } => "ContributionMalformed",
        }
    }

    /// Human-readable skip reason for logging (never empty).
    pub fn reason(&self) -> String {
        match self {
            Self::NotAnOffer { kind } => format!("not a kind-{JOB_OFFER_KIND} offer (kind {kind})"),
            Self::Unparseable => "offer tags did not parse".to_string(),
            Self::UnsupportedVersion { version } => {
                format!("unsupported mobee protocol version {version:?}")
            }
            Self::DeadlineExpired { deadline_unix, now } => format!(
                "offer deadline {deadline_unix} already passed at now={now} (expired; refused — a lapsed offer is never claimed or resurrected)"
            ),
            Self::RateGate { reason } => format!("rate-gate: {reason}"),
            Self::AlreadyProcessed => "already claimed/receipted/released (journal dedup)".to_string(),
            Self::ProcessingBusy { job_id } => {
                format!("single-flight busy: job {job_id} is in the processing phase")
            }
            Self::AwaitingPaymentFull { capacity } => {
                format!("awaiting-payment backlog full (cap {capacity}); back-pressuring new claims")
            }
            Self::AwaitingAwardFull { capacity } => {
                format!("awaiting-award backlog full (cap {capacity}); back-pressuring new claims")
            }
            Self::ContributionUnsupported => {
                "contribution offer refused: seller contribution support is disabled (feedback-kind interop courtesy)".to_string()
            }
            Self::ContributionMalformed { reason } => {
                format!("contribution offer refused (malformed pins, fail-closed): {reason}")
            }
        }
    }
}

/// Reason string journaled + surfaced when a claim is released during restart-reconcile.
fn reconcile_reason(liveness: ClaimLiveness) -> &'static str {
    match liveness {
        ClaimLiveness::Expired => "claim expired before daemon restart (deadline passed, unpaid)",
        ClaimLiveness::Live => {
            "daemon restarted mid-execution; live claim released (does not resume in-memory job state)"
        }
    }
}

/// Buffered early payment (received before/while result published).
struct BufferedPay {
    event_id: String,
    received: ReceivedPayment,
}

/// Seller daemon runtime state.
pub struct SellerDaemon {
    home: MobeeHome,
    keys: nostr_sdk::Keys,
    seller_pubkey: String,
    journal: SellerJournal,
    /// Early / unmatched 1059 payments awaiting reconcile (ids only logged).
    pay_buffer: VecDeque<BufferedPay>,
    /// Claims published and awaiting the buyer's kind-award AWARD. These hold NO single-flight and
    /// run NO compute: the awarded seller moves its job to `active` and executes, every other
    /// claimant releases quietly. Bounded by [`AWAITING_AWARD_CAP`].
    awaiting_award: Vec<ActiveJob>,
    /// The PROCESSING-phase job (holds single-flight). `None` when idle, only awaiting award, or
    /// only awaiting pay.
    active: Option<ActiveJob>,
    /// DELIVERED-but-unpaid jobs (result-kind published, payment not yet redeemed). These do
    /// NOT hold single-flight, so new offers can still be claimed while payment is pending.
    awaiting_payment: Vec<ActiveJob>,
}

impl SellerDaemon {
    pub fn open(home: MobeeHome) -> Result<Self, DaemonError> {
        require_seller_config(&home)?;
        if home.config.accepted_mints.is_empty() {
            return Err(DaemonError::Config(
                "seller accepted_mints must be non-empty".to_owned(),
            ));
        }
        // Real-mint fence: with `allow_real_mints=false` (default), only the
        // testnut/dev allow-list is admissible; set `allow_real_mints=true` to admit any
        // well-formed https mint (the deliberate real-money switch).
        for mint in &home.config.accepted_mints {
            if !crate::home::mint_allowed(mint, home.config.allow_real_mints) {
                return Err(DaemonError::Config(format!(
                    "seller mint fail-closed: accepted_mints entry {mint} is not an allow-listed \
                     testnut/dev mint ({DEFAULT_MINT_URL}); set allow_real_mints=true to admit a \
                     real https mint"
                )));
            }
        }
        let secret = home::read_secret_key_hex(&home)?;
        let keys = nostr_sdk::Keys::parse(&secret)
            .map_err(|error| DaemonError::Home(HomeError::Key(format!("parse: {error}"))))?;
        let seller_pubkey = keys.public_key().to_hex();
        let journal = SellerJournal::open(&home)?;
        Ok(Self {
            home,
            keys,
            seller_pubkey,
            journal,
            pay_buffer: VecDeque::new(),
            awaiting_award: Vec::new(),
            active: None,
            awaiting_payment: Vec::new(),
        })
    }

    pub fn seller_pubkey(&self) -> &str {
        &self.seller_pubkey
    }

    pub fn home(&self) -> &MobeeHome {
        &self.home
    }

    /// Build the seller heartbeat for the current daemon state, or `None` when there is no
    /// `[seller]` config (no advertised rate to publish). `accepting` is `n` while the
    /// single-flight slot is held (the seller is busy on a job); `queue_depth` is that in-flight
    /// count. Reads state only — never publishes.
    fn heartbeat_draft(&self) -> Option<crate::heartbeat::HeartbeatDraft> {
        let cfg = require_seller_config(&self.home).ok()?;
        Some(crate::heartbeat::heartbeat_for_state(
            self.active.is_some(),
            cfg.rate_sats,
        ))
    }

    /// Try to take the single-flight slot.
    pub fn try_begin_flight() -> bool {
        FLIGHT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn end_flight() {
        FLIGHT.store(false, Ordering::SeqCst);
    }

    pub fn in_flight() -> bool {
        FLIGHT.load(Ordering::SeqCst)
    }

    /// Handle one offer-kind offer event: publish + journal a claim and park it awaiting the
    /// buyer's award. Returns `Ok(Some(job_id))` when claimed, `Ok(None)` when skipped.
    ///
    /// Claiming runs NO compute and takes NO single-flight slot — the claim only says "I would do
    /// this". Execution waits for a kind-award AWARD naming this claim ([`Self::on_award_event`]),
    /// so a job drawing many claims never burns compute on more than the one seller the buyer picks.
    /// Skips are NEVER silent — every non-claim path is logged with its reason ([`OfferSkip::reason`]).
    pub async fn on_offer_event(
        &mut self,
        event: &nostr_sdk::Event,
    ) -> Result<Option<String>, DaemonError> {
        let now = now_unix();
        let intent = match self.classify_offer(event, now)? {
            OfferDisposition::Skip(skip) => {
                eprintln!("seller skip offer {}: {}", event.id.to_hex(), skip.reason());
                // Durable refusal capture (previously eprintln-only). Best-effort —
                // an episode write never affects the skip decision or the money path.
                self.record_refused_episode(event, &skip);
                // Announce the refusal (with its machine-readable reason code). Skip the same two
                // non-freshly-classified cases the episode capture skips — a non-offer and a dedup
                // re-see are noise, not a seller decision. `amount` rides along when the offer
                // parsed (unparseable ⇒ None).
                if !matches!(skip, OfferSkip::NotAnOffer { .. } | OfferSkip::AlreadyProcessed) {
                    let amount = parse_offer(&event_to_draft(event)).ok().map(|o| o.amount);
                    self.announce(crate::announce::AnnounceEvent::refused(
                        now_unix(),
                        &self.seller_pubkey,
                        &event.id.to_hex(),
                        skip.code(),
                        &skip.reason(),
                        amount,
                    ));
                }
                // Bundled buyer-visibility: a TARGETED-to-self under-rate refusal also emits a
                // feedback-kind `status=error` so the buyer learns why. Open-pool under-rate stays
                // log-only (spam guard). No-op for every other skip reason. Content carries the
                // same machine-readable rate-gate reason already logged here (buyers distinguish
                // rate-refusal from a crash).
                if let OfferSkip::RateGate { reason } = &skip {
                    self.publish_under_rate_error_if_targeted(event, reason).await;
                }
                // Interop courtesy: a seller that cannot/​will not handle a contribution
                // offer emits a feedback-kind `status=error` so the buyer does not wait on a delivery
                // that will never come. NOT a security control — buyer refusal is the boundary.
                if matches!(
                    skip,
                    OfferSkip::ContributionUnsupported | OfferSkip::ContributionMalformed { .. }
                ) {
                    let content = contribution_refusal_error_content(&skip);
                    let draft = error_draft(
                        &event.id.to_hex(),
                        &event.pubkey.to_hex(),
                        &self.seller_pubkey,
                        content,
                    );
                    let _ = publish_draft(&self.home, &self.keys, &draft).await;
                }
                return Ok(None);
            }
            OfferDisposition::Claim(intent) => intent,
        };

        // The seller authors the claim's payment terms as a NUT-18 payment
        // request (`creq…`) — accepted mints from its OWN config (not the offer), amount/unit
        // copied from the offer, single-use, addressed to the seller's key. The claim is the
        // invoice; the buyer satisfies this `creq`.
        let creq = match gateway::creq::build_seller_creq(
            &intent.job_id,
            intent.offer.amount,
            &intent.offer.unit,
            &self.home.config.accepted_mints,
            &self.seller_pubkey,
        ) {
            Ok(creq) => creq,
            Err(error) => {
                return Err(DaemonError::Seller(SellerError::Io(error.to_string())));
            }
        };
        let claim = claim_draft(
            &intent.job_id,
            &intent.buyer_pubkey,
            &self.seller_pubkey,
            &creq,
        );
        // Sign the claim FIRST (its deterministic id == the published id), so we can durably
        // journal the CLAIMED transition BEFORE putting a live payable creq on the relay. This
        // ordering closes the re-claim gap: publishing first and journaling second (the old order)
        // left a live creq with no local record on a crash/journal-failure in between, so a
        // restart would re-claim the same offer. Now a journal failure aborts with NO live creq;
        // a crash after publish still has the claim recorded (restart-reconcile can release it).
        let claim_event = match sign_draft(&self.keys, &claim) {
            Ok(event) => event,
            Err(error) => {
                return Err(error);
            }
        };
        let claim_id = claim_event.id.to_hex();
        // Journal the CLAIMED transition WITH the deadline/claim_id/buyer so a restart can
        // reconcile this claim without the relay (restart-reconcile) — BEFORE publish.
        if let Err(error) = self.journal.append_claim(
            &intent.job_id,
            &claim_id,
            &intent.buyer_pubkey,
            intent.deadline_unix,
        ) {
            return Err(error.into());
        }
        // Publish only AFTER the durable claim record exists.
        if let Err(error) = send_signed_event(&self.home, &self.keys, &claim_event).await {
            return Err(error);
        }

        let workdir = job_workdir(&self.home, &intent.job_id);
        if let Err(error) = std::fs::create_dir_all(&workdir) {
            return Err(DaemonError::Seller(SellerError::Io(error.to_string())));
        }

        // CLAIMED visibility: emit the claim signal on BOTH surfaces so a claim is observable
        // (not merely journaled) ahead of the award — a stderr line (for the log-tailing
        // sidecar) and the structured announce event.
        eprintln!(
            "seller claimed offer job_id={} claim_id={claim_id} buyer={} amount={} deadline={} (awaiting award)",
            intent.job_id, intent.buyer_pubkey, intent.offer.amount, intent.deadline_unix
        );
        self.announce(crate::announce::AnnounceEvent::claimed(
            now_unix(),
            &self.seller_pubkey,
            &intent.job_id,
            &intent.buyer_pubkey,
            intent.offer.amount,
            intent.deadline_unix,
        ));

        let job_id = intent.job_id.clone();
        // Park the claim awaiting the buyer's award — no single-flight, no compute yet.
        self.awaiting_award.push(ActiveJob {
            job_id: intent.job_id,
            buyer_pubkey: intent.buyer_pubkey,
            offer: intent.offer,
            claim_id,
            result_id: None,
            deadline_unix: intent.deadline_unix,
            workdir,
            contribution: intent.contribution,
            delivery: None,
            // Live job: current config authored this creq and is the original terms; the redeem
            // guard reads config directly (see the redeem path). Populated only on restore.
            accepted_mints: Vec::new(),
        });
        Ok(Some(job_id))
    }

    /// Handle one kind-award AWARD event: the buyer's selection among a job's claims.
    ///
    /// Matches the award against the seller's outstanding claims by the award author (which MUST be
    /// the offer buyer) and the offer it roots on. When the award names THIS seller's claim, the job
    /// moves into the single-flight `active` slot and [`AwardOutcome::Execute`] tells the loop to run
    /// it. When it names a DIFFERENT claim, the seller's claim is released without running (the buyer
    /// picked someone else). An award for no outstanding claim of ours — a late, unrelated, or
    /// spoofed one (author ≠ buyer) — is ignored. Never runs relay I/O beyond the release feedback.
    pub async fn on_award_event(&mut self, event: &nostr_sdk::Event) -> AwardOutcome {
        let Some(award) = parse_award(&event_to_draft(event)) else {
            return AwardOutcome::Ignored;
        };
        let award_author = event.pubkey.to_hex();
        match match_award(&self.awaiting_award, &award, &award_author) {
            AwardMatch::Ignored => {
                eprintln!(
                    "seller award ignored event={} offer={} claim={}: no matching outstanding claim (late, unrelated, or author not the offer buyer)",
                    event.id.to_hex(),
                    award.offer_id,
                    award.claim_id
                );
                AwardOutcome::Ignored
            }
            AwardMatch::Execute(index) => {
                // We won. Take the single-flight slot and promote to the executing slot; the loop
                // runs the existing execute path next. `try_begin_flight` succeeds here because the
                // loop is single-tasked (an in-progress execution blocks it, so nothing else holds
                // the slot).
                if !Self::try_begin_flight() {
                    eprintln!(
                        "seller award job_id={}: single-flight busy; leaving claim parked for the next award",
                        award.offer_id
                    );
                    return AwardOutcome::Ignored;
                }
                let job = self.awaiting_award.remove(index);
                let job_id = job.job_id.clone();
                eprintln!(
                    "seller awarded job_id={} claim_id={} by buyer={award_author}; executing",
                    job.job_id, job.claim_id
                );
                self.active = Some(job);
                AwardOutcome::Execute { job_id }
            }
            AwardMatch::Release(index) => {
                // The buyer awarded a different seller — release our claim without ever executing.
                let job = self.awaiting_award.remove(index);
                let reason = format!("buyer awarded a different claim ({})", award.claim_id);
                self.release_claim(&job, &reason).await;
                AwardOutcome::Released {
                    job_id: job.job_id,
                    reason,
                }
            }
        }
    }

    /// Release every parked claim whose offer deadline has passed with no award (pure per-job check
    /// against injected `now`). The seller ran no compute — the award never came in time — so each is
    /// released quietly. Rides the periodic award-sweep tick; returns the `(job_id, reason)` of each.
    pub async fn release_expired_awaiting_award(&mut self, now: u64) -> Vec<(String, String)> {
        let mut kept = Vec::with_capacity(self.awaiting_award.len());
        let mut expired = Vec::new();
        for job in std::mem::take(&mut self.awaiting_award) {
            if job.offer.deadline_unix <= now {
                expired.push(job);
            } else {
                kept.push(job);
            }
        }
        self.awaiting_award = kept;
        let mut released = Vec::with_capacity(expired.len());
        for job in &expired {
            let reason = "offer deadline passed with no award".to_owned();
            self.release_claim(job, &reason).await;
            released.push((job.job_id.clone(), reason));
        }
        released
    }

    /// Release a parked claim: journal a terminal RELEASE (so it never reads live or re-claims),
    /// announce it, and best-effort publish a feedback-kind error so the buyer sees the claim is
    /// gone. Reuses the same release/orphan machinery as restart-reconcile. Never takes or frees the
    /// single-flight slot — a parked claim never held it.
    async fn release_claim(&mut self, job: &ActiveJob, reason: &str) {
        if let Err(error) = self.journal.append_release(&job.job_id, reason) {
            eprintln!(
                "seller WARN: failed to journal release for job_id={} (continuing): {error}",
                job.job_id
            );
        }
        eprintln!(
            "seller released claim job_id={} claim_id={}: {reason}",
            job.job_id, job.claim_id
        );
        self.announce(crate::announce::AnnounceEvent::released(
            now_unix(),
            &self.seller_pubkey,
            &job.job_id,
            reason,
        ));
        let content = seller_error_content(ErrorReasonCode::ClaimReleased, reason);
        let draft = error_draft(
            &job.job_id,
            &job.buyer_pubkey,
            &self.seller_pubkey,
            content,
        );
        let _ = publish_draft(&self.home, &self.keys, &draft).await;
    }

    /// Decide, WITHOUT any relay I/O, whether an offer event should be claimed.
    ///
    /// `now` is injected so the deadline is a pure function of inputs. Single-flight is
    /// enforced ONLY for the PROCESSING slot (`self.active`): a delivered-but-unpaid job
    /// in `awaiting_payment` does not block (silent-drop fix).
    fn classify_offer(
        &self,
        event: &nostr_sdk::Event,
        now: u64,
    ) -> Result<OfferDisposition, DaemonError> {
        if event.kind.as_u16() != JOB_OFFER_KIND {
            return Ok(OfferDisposition::Skip(OfferSkip::NotAnOffer {
                kind: event.kind.as_u16(),
            }));
        }
        let draft = event_to_draft(event);
        let offer = match parse_offer(&draft) {
            Ok(offer) => offer,
            // Cross-version / unsupported protocol version is a distinct refusal, not a
            // generic parse failure — buyers and operators must be able to tell version skew
            // from genuinely malformed offer tags.
            Err(OfferParseError::UnsupportedVersion(version)) => {
                return Ok(OfferDisposition::Skip(OfferSkip::UnsupportedVersion {
                    version,
                }));
            }
            Err(_) => return Ok(OfferDisposition::Skip(OfferSkip::Unparseable)),
        };
        // The offer no longer names a mint that the seller must match — the
        // seller's `accepted_mints` are asserted later against the *paid* token (redeem guard),
        // so there is nothing to gate here. (`offer.mint_url` stays present and readable until
        // Jobs D/E re-point the remaining reads.)
        // Offer-freshness gate (money-safety, backfill guard #a) — a deliberate ALWAYS-ON
        // refusal on EVERY offer path (live, open-pool backfill, AND the targeted filter's
        // full-history backfill; classify-level, so no filter shape can bypass it). The offer
        // carries its own absolute deadline (`param deadline`, always present — `parse_offer`
        // requires it). If it has already passed we REFUSE here, BEFORE `job_deadline_unix` —
        // which would otherwise hand a stale offer a fresh `now + timeout` deadline and
        // resurrect a lapsed job (the pre-existing hazard: stored targeted offers were
        // re-deliverable on every restart and got fresh deadlines). `offer.deadline_unix > now`
        // is exactly the still-usable branch of `job_deadline_unix`, so this gate is its safe
        // complement. Pure over (offer, now).
        if offer.deadline_unix <= now {
            return Ok(OfferDisposition::Skip(OfferSkip::DeadlineExpired {
                deadline_unix: offer.deadline_unix,
                now,
            }));
        }
        let seller_cfg = require_seller_config(&self.home)?;
        // Parse the contribution class. A malformed contribution offer is REFUSED
        // (fail-closed, never run from-scratch); a contribution offer to a seller with support
        // disabled is refused as an interop courtesy. Both emit a feedback-kind in `on_offer_event`.
        let contribution = match crate::contribution::parse_contribution_offer(&draft.tags) {
            Ok(value) => value,
            Err(error) => {
                return Ok(OfferDisposition::Skip(OfferSkip::ContributionMalformed {
                    reason: error.to_string(),
                }));
            }
        };
        if contribution.is_some() && !seller_cfg.contribution_enabled {
            return Ok(OfferDisposition::Skip(OfferSkip::ContributionUnsupported));
        }
        if let Err(error) = rate_gate_allows(
            &offer,
            &self.seller_pubkey,
            seller_cfg.rate_sats,
            seller_cfg.claim_open_pool,
        ) {
            // Prefer the raw policy message (e.g. "offer amount 3 sat below seller rate_sats 5")
            // over Display's "seller policy refused: …" prefix — that string is logged and
            // plumbed into the feedback-kind content for targeted under-rate refusals.
            let reason = match error {
                SellerError::Policy(message) => message,
                other => other.to_string(),
            };
            return Ok(OfferDisposition::Skip(OfferSkip::RateGate { reason }));
        }
        let job_id = event.id.to_hex();
        if self.journal.has_claim(&job_id)? {
            return Ok(OfferDisposition::Skip(OfferSkip::AlreadyProcessed));
        }
        // Single-flight is for PROCESSING only. Delivered-but-unpaid jobs live in
        // `awaiting_payment` and MUST NOT block a new claim.
        if let Some(active) = &self.active {
            return Ok(OfferDisposition::Skip(OfferSkip::ProcessingBusy {
                job_id: active.job_id.clone(),
            }));
        }
        if self.awaiting_payment.len() >= AWAITING_PAYMENT_CAP {
            return Ok(OfferDisposition::Skip(OfferSkip::AwaitingPaymentFull {
                capacity: AWAITING_PAYMENT_CAP,
            }));
        }
        if self.awaiting_award.len() >= AWAITING_AWARD_CAP {
            return Ok(OfferDisposition::Skip(OfferSkip::AwaitingAwardFull {
                capacity: AWAITING_AWARD_CAP,
            }));
        }
        let deadline_unix = job_deadline_unix(&offer, seller_cfg, now);
        Ok(OfferDisposition::Claim(ClaimIntent {
            job_id,
            buyer_pubkey: event.pubkey.to_hex(),
            offer,
            deadline_unix,
            contribution,
        }))
    }

    /// Backfill pre-claim money-safety check (guards #c/#d) for a BACKFILLED offer — one posted
    /// before the daemon came online. Reads the job's current relay state (offer + feedback-kind
    /// claims + result-kind results, with claim liveness derived against `now`) and returns
    /// `Some(reason)` if the offer must be SKIPPED, `None` to proceed to the normal claim path.
    ///
    /// This is the guard that stops a stored offer from stomping a trade that already moved on
    /// while the daemon was offline:
    ///  * **#c** any result-kind result exists (from ANY seller) ⇒ already delivered/settled;
    ///  * **#d** a LIVE claim (feedback-kind `processing`, not past the offer deadline) is held by
    ///    ANOTHER seller ⇒ an in-flight trade — do not stomp it.
    ///
    /// FAIL-CLOSED: if the relay read errors (cannot determine current state) the offer is
    /// SKIPPED with a logged reason rather than optimistically claimed. Never runs on the
    /// live path (see the caller's `created_at < daemon_start_unix` gate), so it adds no relay
    /// round-trip to offers posted while the daemon runs.
    async fn backfill_offer_blocked(&self, event: &nostr_sdk::Event) -> Option<String> {
        let job_id = event.id.to_hex();
        let now = now_unix();
        let view = match crate::job_lifecycle::fetch_job_view_async(
            &self.home,
            &self.keys,
            &job_id,
            Duration::from_secs(BACKFILL_CHECK_TIMEOUT_SECS),
            now,
        )
        .await
        {
            Ok(view) => view,
            Err(error) => {
                return Some(format!(
                    "backfill relay pre-claim check failed (fail-closed skip): {error}"
                ));
            }
        };
        // #c — already delivered/settled by ANY seller (a result-kind result exists).
        if let Some(result) = view.results.first() {
            return Some(format!(
                "already delivered: {} result-kind result(s) on relay (newest {} by {}); not re-claiming a settled job",
                view.results.len(),
                result.result_id,
                result.seller_pubkey
            ));
        }
        // #d — a LIVE claim held by ANOTHER seller (don't stomp an in-flight trade). Expired
        // claims are already excluded from `live_claim_id` (liveness derived vs the offer
        // deadline); our OWN prior claim is caught by the durable journal in `classify_offer`.
        if let Some(live_id) = &view.live_claim_id {
            if let Some(claim) = view.claims.iter().find(|c| &c.claim_id == live_id) {
                if claim.seller_pubkey != self.seller_pubkey {
                    return Some(format!(
                        "live claim {live_id} held by another seller {} (offer in-flight); not stomping",
                        claim.seller_pubkey
                    ));
                }
            }
        }
        None
    }

    /// Bundled buyer-visibility (step 5): when a **targeted-to-self** offer is refused only for
    /// being below the seller's rate floor, publish a feedback-kind `status=error` so the buyer
    /// learns WHY (in addition to the local skip log). OPEN-POOL / untargeted under-rate refusals
    /// stay LOG-ONLY (a fleet of rate-N sellers each feedback-ing every cheap open offer would spam
    /// the relay). `reason` is the machine-readable rate-gate string (same as the skip log) and
    /// is written into the event content. Publish failure is logged, never fatal. Called only on
    /// a `RateGate` skip.
    async fn publish_under_rate_error_if_targeted(&self, event: &nostr_sdk::Event, reason: &str) {
        let draft = event_to_draft(event);
        let Ok(offer) = parse_offer(&draft) else {
            return;
        };
        let Ok(seller_cfg) = require_seller_config(&self.home) else {
            return;
        };
        let targeted_to_self = offer.seller_pubkey.as_deref() == Some(self.seller_pubkey.as_str());
        if !(targeted_to_self && offer.amount < seller_cfg.rate_sats) {
            return; // open-pool / wrong-target / not-under-rate ⇒ log-only (handled by caller).
        }
        let error = under_rate_error_draft(
            &event.id.to_hex(),
            &event.pubkey.to_hex(),
            &self.seller_pubkey,
            reason,
        );
        match publish_draft(&self.home, &self.keys, &error).await {
            Ok(id) => eprintln!(
                "seller under-rate refusal surfaced: feedback-kind error={id} offer={} (amount {} < rate_sats {}) reason={reason}",
                event.id.to_hex(),
                offer.amount,
                seller_cfg.rate_sats
            ),
            Err(error) => eprintln!(
                "seller WARN: under-rate refusal feedback-kind publish failed offer={}: {error}",
                event.id.to_hex()
            ),
        }
    }

    /// DELIVERED transition: move the PROCESSING job to `awaiting_payment` and free the
    /// single-flight slot so new offers can be claimed while payment is pending.
    /// The payment binding is preserved (job_id + result_id) for [`try_apply_payment`].
    fn mark_delivered(&mut self) {
        if let Some(job) = self.active.take() {
            // `result_id` was set by `execute_active_job` on the successful publish path.
            // Durably journal the delivered-but-unpaid transition so a restart rebuilds this
            // job into awaiting_payment and a stored/buffered wrap can bind + redeem. Best-effort
            // but LOUD — a failed write risks re-stranding the payment across a restart.
            if let Some(result_id) = job.result_id.as_deref() {
                if let Err(error) = self.journal.append_delivery(
                    &job.job_id,
                    result_id,
                    job.offer.amount,
                    &job.offer.unit,
                    &job.buyer_pubkey,
                    // Journal the ORIGINAL advertised mints (Fix Q). Config is fixed for a process
                    // lifetime, so config.accepted_mints here equals the creq `m` list authored at
                    // claim; persisting it lets a restart redeem against the original terms.
                    &self.home.config.accepted_mints,
                ) {
                    eprintln!(
                        "seller WARN: failed to journal delivery for job_id={} (payment recovery on restart may be degraded): {error}",
                        job.job_id
                    );
                }
            }
            self.awaiting_payment.push(job);
            while self.awaiting_payment.len() > AWAITING_PAYMENT_CAP {
                let dropped = self.awaiting_payment.remove(0);
                eprintln!(
                    "seller drop awaiting-payment job_id={} (backlog cap {AWAITING_PAYMENT_CAP})",
                    dropped.job_id
                );
                // Delivered-but-UNPAID terminal (backpressure eviction). Best-effort.
                self.record_delivered_unpaid_episode(&dropped);
            }
        }
        Self::end_flight();
    }

    /// Z3 fail-closed gate: may this daemon claim NEW work? The seller journal is the durable record
    /// of prior active/delivered state; if it is unreadable/corrupt at startup, reconciliation
    /// recovers nothing, so claiming new work would take on fresh liability while blind to prior
    /// in-flight jobs. A clean read ⇒ claiming allowed; a read error ⇒ receive-only / NO-CLAIM.
    pub fn claim_allowed_after_journal_check(&self) -> bool {
        self.journal.entries().is_ok()
    }

    /// Pure restart-reconcile plan: orphaned in-flight claims (journaled, no receipt, no
    /// release) classified Expired/Live by the injected `now`. No relay, no wall-clock.
    pub fn reconcile_plan(&self, now: u64) -> Result<Vec<OrphanClaim>, DaemonError> {
        Ok(plan_orphaned_claims(&self.journal.entries()?, now))
    }

    /// Durable restart-reconcile (NO relay): journal a terminal RELEASE for every orphaned
    /// in-flight claim so it can never read live again and is never re-claimed. Idempotent.
    /// Returns the plan that was acted on.
    pub fn reconcile_journal(&mut self, now: u64) -> Result<Vec<OrphanClaim>, DaemonError> {
        let plan = self.reconcile_plan(now)?;
        for orphan in &plan {
            self.journal
                .append_release(&orphan.job_id, reconcile_reason(orphan.liveness))?;
        }
        Ok(plan)
    }

    /// Rebuild delivered-but-unpaid jobs (journaled `Delivery`, no `Receipt`/`Release`) into
    /// `awaiting_payment` at boot, so a backfilled or buffered payment wrap can bind and redeem.
    /// Without this, a job the offer-scan classifies "already delivered" is never re-added and its
    /// wrap buffers forever. Idempotent: skips jobs already present; respects the backlog cap. The
    /// rebuilt offer carries only the money-critical fields the redeem path reads (amount/unit);
    /// its `seller_pubkey` is pinned to THIS seller (not a `None` wildcard) so the redeem path's
    /// `assert_seller_matches` binds to us exactly rather than matching any seller.
    pub fn restore_delivered_unpaid(&mut self) -> Result<usize, DaemonError> {
        let unpaid = self.journal.deliveries_awaiting_receipt()?;
        let mut restored = 0usize;
        for d in unpaid {
            if self
                .awaiting_payment
                .iter()
                .any(|job| job.job_id == d.job_id)
            {
                continue;
            }
            if self.awaiting_payment.len() >= AWAITING_PAYMENT_CAP {
                break;
            }
            let unit = if d.unit.is_empty() {
                "sat".to_string()
            } else {
                d.unit
            };
            self.awaiting_payment.push(ActiveJob {
                job_id: d.job_id.clone(),
                buyer_pubkey: d.buyer_pubkey,
                offer: ParsedOffer {
                    task: String::new(),
                    output: String::new(),
                    amount: d.amount,
                    unit,
                    deadline_unix: 0,
                    // Pin to this seller (we authored the delivery) — not a None wildcard.
                    seller_pubkey: Some(self.seller_pubkey.clone()),
                },
                claim_id: String::new(),
                result_id: Some(d.result_id),
                deadline_unix: 0,
                workdir: job_workdir(&self.home, &d.job_id),
                contribution: None,
                delivery: None,
                // Redeem against the ORIGINAL advertised mints journaled at delivery (Fix Q), not
                // current config. Empty (a pre-field delivery line) falls back to config downstream.
                accepted_mints: d.accepted_mints,
            });
            restored += 1;
        }
        Ok(restored)
    }

    /// Reconstruct a delivered-job bind for a buffered/backfilled payment wrap from ON-RELAY
    /// ground truth, for the recovery case [`restore_delivered_unpaid`] cannot cover: a seat that
    /// delivered under an older build (or whose journal is lost/incomplete) has NO local `Delivery`
    /// entry, so its wrap references a job that never enters `awaiting_payment` and buffers forever.
    ///
    /// Pure over the fetched [`JobView`] + the received payment (the async caller does the relay
    /// I/O and the `awaiting_payment` push), so every fail-closed branch is unit-testable without a
    /// relay. Returns `Ok(ActiveJob)` only when EVERY money-critical fact verifies against events
    /// this seller itself authored; otherwise `Err(reason)` and the caller leaves the wrap buffered.
    ///
    /// Guards (all fail-closed — a partial reconstruction never redeems):
    ///  * the result (kind [`JOB_RESULT_KIND`]) for `J` MUST be authored by self (a foreign author
    ///    is refused — money only ever binds to this seller's own delivery);
    ///  * the claim (kind 3402) for `J` MUST be authored by self AND carry a parseable `creq`;
    ///  * that `creq` MUST bind the payload: same job id, matching unit, the payload's realized
    ///    mint listed in the `creq`, and the `creq` amount equal to the on-relay offer amount;
    ///  * the payload's realized mint MUST be in the seller's `accepted_mints` (the SAME allow-list
    ///    the redeem guard enforces — checked here so a foreign mint leaves the wrap buffered
    ///    rather than erroring the ingest);
    ///  * the on-relay offer MUST be present (its amount/unit are the redeem terms).
    ///
    /// The rebuilt job pins `seller_pubkey` to THIS seller (we authored the result+claim it binds
    /// to) rather than a `None` wildcard, so `assert_seller_matches` binds to us exactly; it carries
    /// the money-critical amount/unit from the offer, and the EXISTING redeem path re-checks every
    /// guard (`assert_redeem_mint`, terms, amount) before any funds move.
    fn reconstruct_delivered_bind(
        &self,
        view: &crate::job_lifecycle::JobView,
        received: &ReceivedPayment,
    ) -> Result<ActiveJob, String> {
        let job_id = received.payload.job_id();
        // Own result (kind 3403, author == self). A result authored by ANOTHER seller is a foreign
        // delivery — never bind money to it.
        let own_result = view
            .results
            .iter()
            .find(|result| result.seller_pubkey == self.seller_pubkey)
            .ok_or_else(|| {
                format!(
                    "no self-authored result on relay for job_id={job_id} (results={}); \
                     refusing reconstruction",
                    view.results.len()
                )
            })?;
        // Own claim (kind 3402, author == self) carrying the seller-authored NUT-18 `creq`.
        let own_claim = view
            .claims
            .iter()
            .find(|claim| claim.seller_pubkey == self.seller_pubkey)
            .ok_or_else(|| {
                format!("no self-authored claim on relay for job_id={job_id}; refusing reconstruction")
            })?;
        let creq = own_claim.creq.as_deref().ok_or_else(|| {
            format!(
                "self claim for job_id={job_id} carries no creq (cannot verify payment terms); \
                 refusing reconstruction"
            )
        })?;
        // Bind the reconstruction to the seller-authored request FIELD BY FIELD: the checks below
        // verify the fetched claim's creq against the payload (payment id == job id, realized mint
        // listed, unit match) and against the on-relay offer (amount). The seller side stores no
        // creq hash to compare a recomputed one against (the accept-bind's `creq_hash` is a buyer
        // artifact), so the parsed fields ARE the binding.
        let request = gateway::creq::parse_creq(creq).map_err(|error| {
            format!("self claim creq for job_id={job_id} unparseable: {error}; refusing reconstruction")
        })?;
        if request.payment_id.as_deref() != Some(job_id) {
            return Err(format!(
                "creq mismatch: creq payment id {:?} != job_id={job_id}; refusing reconstruction",
                request.payment_id
            ));
        }
        let payload_mint = &received.payload.payload.mint;
        if !request.mints.contains(payload_mint) {
            return Err(format!(
                "creq mismatch: payload mint {payload_mint} not authored in self claim creq for \
                 job_id={job_id}; refusing reconstruction"
            ));
        }
        if request.unit.as_ref() != Some(&received.payload.payload.unit) {
            return Err(format!(
                "creq mismatch: payload unit {:?} != creq unit {:?} for job_id={job_id}; \
                 refusing reconstruction",
                received.payload.payload.unit,
                request.unit
            ));
        }
        // On-relay offer supplies the redeem terms (amount/unit) the seller-authored creq copied.
        let offer = view.offer.as_ref().ok_or_else(|| {
            format!("no offer on relay for job_id={job_id}; refusing reconstruction")
        })?;
        if request.amount != Some(cashu::Amount::from(offer.amount_sats)) {
            return Err(format!(
                "creq mismatch: creq amount {:?} != offer amount {} for job_id={job_id}; \
                 refusing reconstruction",
                request.amount, offer.amount_sats
            ));
        }
        // The payload's realized mint MUST be one this seller advertised — the SAME allow-list the
        // redeem guard (`assert_redeem_mint`) enforces, applied here so an unlisted mint leaves the
        // wrap buffered instead of erroring the ingest. Never relaxes the allow-list.
        let accepted = self
            .home
            .config
            .accepted_mints
            .iter()
            .map(|entry| mint_url(entry))
            .collect::<Result<std::collections::HashSet<_>, _>>()
            .map_err(|error| format!("accepted_mints parse for job_id={job_id}: {error}"))?;
        if !accepted.contains(payload_mint) {
            return Err(format!(
                "payload mint {payload_mint} not in seller accepted_mints for job_id={job_id}; \
                 refusing reconstruction"
            ));
        }
        Ok(ActiveJob {
            job_id: job_id.to_owned(),
            buyer_pubkey: offer.author_pubkey.clone(),
            offer: ParsedOffer {
                task: String::new(),
                output: String::new(),
                amount: offer.amount_sats,
                unit: received.payload.payload.unit.to_string(),
                deadline_unix: 0,
                // Pin to this seller (we authored the bound result+claim) — not a None wildcard.
                seller_pubkey: Some(self.seller_pubkey.clone()),
            },
            claim_id: own_claim.claim_id.clone(),
            result_id: Some(own_result.result_id.clone()),
            deadline_unix: 0,
            workdir: job_workdir(&self.home, job_id),
            contribution: None,
            delivery: None,
            // Redeem against the mints the ON-RELAY creq originally advertised (Fix Q), read from
            // the seller's own claim above — the original terms, not current config.
            accepted_mints: request.mints.iter().map(|mint| mint.to_string()).collect(),
        })
    }

    /// Fetch on-relay ground truth for a buffered wrap whose delivered-job bind is missing from the
    /// local journal, reconstruct the bind fail-closed ([`reconstruct_delivered_bind`]), and
    /// restore it into `awaiting_payment` so the EXISTING redeem path runs. Returns `true` iff a
    /// bind was restored. Any relay-fetch or verify failure logs a clear reason and returns `false`
    /// (the caller leaves the wrap buffered). Never redeems on a partial reconstruction.
    async fn reconstruct_bind_from_relay(&mut self, received: &ReceivedPayment) -> bool {
        let job_id = received.payload.job_id().to_owned();
        if self.awaiting_payment.len() >= AWAITING_PAYMENT_CAP {
            eprintln!(
                "seller wrap reconstruct job_id={job_id}: awaiting_payment full (cap \
                 {AWAITING_PAYMENT_CAP}); leaving buffered"
            );
            return false;
        }
        let view = match crate::job_lifecycle::fetch_job_view_async(
            &self.home,
            &self.keys,
            &job_id,
            Duration::from_secs(BACKFILL_CHECK_TIMEOUT_SECS),
            now_unix(),
        )
        .await
        {
            Ok(view) => view,
            Err(error) => {
                eprintln!(
                    "seller wrap reconstruct job_id={job_id}: relay fetch failed (fail-closed, \
                     leaving buffered): {error}"
                );
                return false;
            }
        };
        match self.reconstruct_delivered_bind(&view, received) {
            Ok(job) => {
                eprintln!(
                    "seller wrap reconstruct job_id={job_id}: bind reconstructed from on-relay \
                     result+claim (result_id={} claim_id={}); restoring into awaiting_payment",
                    job.result_id.as_deref().unwrap_or_default(),
                    job.claim_id
                );
                self.awaiting_payment.push(job);
                true
            }
            Err(reason) => {
                eprintln!("seller wrap reconstruct job_id={job_id}: {reason}");
                false
            }
        }
    }

    /// Full startup reconcile: durable journal release (above) + best-effort feedback-kind
    /// error to surface the dead claim to the buyer. Publish failure is logged, not fatal —
    /// the journal release is the durable guarantee; the buyer view also derives expiry.
    pub async fn reconcile_on_startup(
        &mut self,
        now: u64,
    ) -> Result<Vec<OrphanClaim>, DaemonError> {
        let plan = self.reconcile_journal(now)?;
        for orphan in &plan {
            let reason = reconcile_reason(orphan.liveness);
            // RECONCILE-RELEASED announce: an orphaned in-flight claim released on startup.
            self.announce(crate::announce::AnnounceEvent::reconcile_released(
                now_unix(),
                &self.seller_pubkey,
                &orphan.job_id,
                &format!("{:?}", orphan.liveness),
                reason,
            ));
            let content = reconcile_error_content(orphan.liveness);
            let draft = error_draft(
                &orphan.job_id,
                &orphan.buyer_pubkey,
                &self.seller_pubkey,
                content,
            );
            match publish_draft(&self.home, &self.keys, &draft).await {
                Ok(id) => eprintln!(
                    "seller reconcile: released orphaned claim job_id={} liveness={:?} kind7000={id} reason={reason}",
                    orphan.job_id, orphan.liveness
                ),
                Err(error) => eprintln!(
                    "seller reconcile: released orphaned claim job_id={} liveness={:?} (feedback-kind publish deferred: {error}) reason={reason}",
                    orphan.job_id, orphan.liveness
                ),
            }
        }
        Ok(plan)
    }

    /// Buffer or attempt apply of one kind-1059 gift wrap (ONE decode site).
    /// Prefer [`ingest_gift_wrap`] which handles buffer-vs-apply correctly.
    pub async fn on_gift_wrap_event(
        &mut self,
        event: &nostr_sdk::Event,
    ) -> Result<Option<ReceiptOutcome>, DaemonError> {
        ingest_gift_wrap(self, event).await
    }

    /// After publishing result: reconcile buffered wraps so early pay still lands.
    pub async fn reconcile_payments(&mut self) -> Result<Option<ReceiptOutcome>, DaemonError> {
        reconcile_after_result(self).await
    }

    fn buffer_payment(&mut self, event_id: String, received: ReceivedPayment) {
        if self.pay_buffer.iter().any(|entry| entry.event_id == event_id) {
            return;
        }
        if self.pay_buffer.len() >= 32 {
            self.pay_buffer.pop_front();
        }
        self.pay_buffer.push_back(BufferedPay { event_id, received });
    }

    async fn try_apply_payment(
        &mut self,
        received: ReceivedPayment,
    ) -> Result<Option<ReceiptOutcome>, DaemonError> {
        // Bind to the delivered-but-unpaid job this payment declares (by job id — the NUT-18
        // payload's `id`). `result_id` is resolved LOCALLY from the bound job: NUT-18 carries no
        // result id (only `id` == the job id), and a job id is unique in `awaiting_payment`.
        // Never scans `active` — the processing slot is not a payment target.
        let payload_job = received.payload.job_id().to_owned();
        let Some(idx) = self
            .awaiting_payment
            .iter()
            .position(|job| job.job_id.as_str() == payload_job)
        else {
            // No delivered job binds this payment yet — caller should buffer.
            return Ok(None);
        };
        let job = self.awaiting_payment[idx].clone();
        let local_job = job.job_id.clone();
        let local_result = job
            .result_id
            .clone()
            .expect("awaiting-payment job always carries a result_id");
        let expected_amount = job.offer.amount;
        let offer = job.offer.clone();
        // The redeem terms + all money-path records (redeem log / journal receipt /
        // announce) key off the REALIZED mint the buyer actually paid at (the NUT-18 payload's
        // `mint`), not the seller default. The redeem guard below refuses unless that mint is one
        // the seller advertised in its creq.
        let payload_mint = received.payload.payload.mint.clone();
        let mint = payload_mint.to_string();
        let token = received.payload.to_token();

        // Bind BEFORE journal — wrong-job refuse (no misattribution). Matched by
        // construction above; kept as a defensive guard.
        if payload_job != local_job {
            return Err(DaemonError::Policy(format!(
                "payment bind refused: payload job ({payload_job}) != local ({local_job})"
            )));
        }

        let buyer = received.buyer_pubkey.to_hex();
        // Third-party settlement guard: the authenticated NIP-17 seal sender MUST be the bound
        // offer buyer (the pubkey folded into the seller-signed receipt preimage). Without this a
        // THIRD party could pay-once and close someone else's job. Fail closed on mismatch; the
        // receipt below binds the offer buyer (== `buyer` after this check).
        if buyer != job.buyer_pubkey {
            return Err(DaemonError::Policy(format!(
                "payment sender refused: seal sender {buyer} is not the bound offer buyer \
                 {} for job {local_job}",
                job.buyer_pubkey
            )));
        }
        // Redeem guard: the paid token's mint must be one the seller advertised
        // (`∈ accepted_mints`) AND equal the payload's declared mint. Fix Q — a RESTORED job
        // settles against the mints it ORIGINALLY advertised (journaled at delivery / read off the
        // on-relay creq), NOT current config, so a config change across restart cannot strand an
        // old payment or let a newly-added mint settle an old claim. A live job (empty
        // `job.accepted_mints`) uses config, which authored its creq this run.
        let redeem_mints =
            redeem_accepted_mints(&job.accepted_mints, &self.home.config.accepted_mints);
        let accepted_mints = redeem_mints
            .iter()
            .map(|entry| mint_url(entry))
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        let policy = PaymentPolicy::new(accepted_mints.iter().cloned());
        let terms = policy.terms_for_offer(payload_mint.clone(), &offer, &self.seller_pubkey)?;
        // Amount in terms == offer.amount (NOT rate_sats).
        // Real-mint fence (issue #49): the restored/live redeem path must also fail closed on a
        // non-allowlisted real mint, exactly as send/melt do. The redeem guard above only checks the
        // mint is one this seller ADVERTISED; this additionally enforces the `allow_real_mints`
        // policy so a real mint can never settle unless the operator opted in.
        if !home::mint_allowed(&mint, self.home.config.allow_real_mints) {
            return Err(DaemonError::Policy(format!(
                "redeem refused: mint {mint} not allowed (allow_real_mints={})",
                self.home.config.allow_real_mints
            )));
        }
        let secret = home::read_secret_key_hex(&self.home)?;
        let cashu_key = cashu_secret_from_nostr_hex(&secret)?;
        // Must await — seller loop already owns a tokio runtime; blocking open nests block_on → panic.
        // Multi-mint: open the shared store at the buyer's REALIZED (payload) mint, not the seller
        // default — the buyer paid seller-locked ecash at `mint`, and `require_wallet_matches`
        // refuses unless the wallet is bound to that same mint. The realized mint already passed the
        // accepted-set redeem guard AND the `allow_real_mints` fence above.
        let wallet = buyer_fund::open_wallet_at_mint_async(&self.home, &mint).await?;
        let adapter = CdkSellerReceive::new(&wallet, cashu_key);

        // Durable intent-to-receive breadcrumb BEFORE the mint swap. If append_receipt later fails
        // (the swap DID land the money), a restart re-sees this wrap and — because the mint reports
        // the token already spent AND this pending-receive record exists — finalizes the receipt
        // instead of leaving the job unpaid forever. token_hash is SHA-256 of the token string; no
        // proof/secret material is journaled.
        let token_hash = {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(token.to_string().as_bytes());
            hex::encode(hasher.finalize())
        };
        self.journal.append_pending_receive(
            &local_job,
            &local_result,
            &token_hash,
            &buyer,
            &mint,
            expected_amount,
        )?;

        let receive_result = adapter
            .receive(&token, &terms, &accepted_mints, &payload_mint)
            .await
            .map(|amount| amount.to_u64())
            .map_err(DaemonError::from);
        // Positive proof of OUR prior collection = a COMPLETED receipt for this job, read fail-closed.
        // A pending-receive breadcrumb is NEVER proof (finding S — see `classify_redeem_outcome`).
        let amount_received =
            match classify_redeem_outcome(receive_result, || self.journal.has_receipt(&local_job)) {
                RedeemDecision::Finalize(amount) => amount,
                RedeemDecision::IdempotentNoOp => {
                    // Already collected AND receipted (legit backfill/restart re-see). Drop the stale
                    // binding; never double-collect, re-receipt, or re-announce.
                    eprintln!(
                        "seller receive idempotent no-op: token already-spent AND a completed receipt \
                         exists (job_id={local_job} result_id={local_result}); nothing to finalize"
                    );
                    self.awaiting_payment.remove(idx);
                    return Ok(None);
                }
                RedeemDecision::Refuse(error) => {
                    if is_idempotent_already_redeemed(&error) {
                        eprintln!(
                            "seller receive REFUSED (already-spent but NO completed receipt — no proof \
                             of a prior collection by us): job_id={local_job} result_id={local_result} \
                             mint={mint} expected={expected_amount}; NOT finalizing — buffered for \
                             manual reconcile"
                        );
                    }
                    return Err(error);
                }
            };
        // (g) collect-leg observability: sats redeemed at the mint (no key/token material).
        eprintln!(
            "{}",
            collect_ok_log_line(
                &local_job,
                &local_result,
                amount_received,
                expected_amount,
                &mint
            )
        );

        self.journal.append_receipt(
            &local_job,
            &local_result,
            &payload_job,
            // NUT-18 carries no result id; the bound job's result id is authoritative.
            &local_result,
            amount_received,
            expected_amount,
            &mint,
            &buyer,
            true,
        )?;

        // Capture the delivered-PAID terminal episode. Best-effort — written AFTER the
        // authoritative receipt above and can never fail or alter it. Seeds the retro.
        self.record_paid_episode(&job, amount_received, expected_amount);
        // COLLECTED announce: sats redeemed at the mint + receipt journaled. Emitted AFTER the
        // authoritative receipt; a sink failure can never affect the money that already landed.
        self.announce(crate::announce::AnnounceEvent::collected(
            now_unix(),
            &self.seller_pubkey,
            &local_job,
            &local_result,
            amount_received,
            expected_amount,
            &mint,
        ));

        let outcome = ReceiptOutcome {
            job_id: local_job,
            result_id: local_result,
            amount_received,
        };
        // PAID: drop the delivered binding. Single-flight was already freed at delivery.
        self.awaiting_payment.remove(idx);
        Ok(Some(outcome))
    }

    /// Run agent → push → publish result. On fail/timeout publish feedback error and clear flight.
    pub async fn execute_active_job(&mut self) -> Result<String, DaemonError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| DaemonError::Policy("no active job".into()))?
            .clone();
        if now_unix() > active.deadline_unix {
            self.fail_active(ErrorReasonCode::AgentTimeout, "job deadline exceeded")
                .await?;
            return Err(DaemonError::Policy("job deadline exceeded".into()));
        }

        let seller_cfg = require_seller_config(&self.home)?.clone();
        // Empty-base: stamp delivery identity into a fresh git workdir (no
        // harness commit). Deliver only if every commit is agent-authored + non-empty tree.
        // Do NOT capture before-OID on empty / require advancement — dogfood is agent-from-empty.
        let identity = seller_git::DeliveryAgentIdentity::for_seller(&self.seller_pubkey);
        // A contribution job forks the PINNED target at base_oid onto a per-job unique
        // branch carrying the FULL job_id; a from-scratch job uses the empty-base workdir.
        let branch = match &active.contribution {
            Some(_) => crate::contribution::ForkRef::unique_branch(&active.job_id),
            None => format!("mobee/{}", &active.job_id[..8.min(active.job_id.len())]),
        };
        let init_result = match &active.contribution {
            Some(contribution) => {
                // Fork base fetch needs NIP-98 auth for relay-git reads — same seller key the
                // push path reads below. Kept local to this arm so the anonymous
                // empty-base path is untouched; fetch itself gates auth to relay-git targets.
                let fork_auth = seller_git::PushAuth {
                    secret_key_hex: home::read_secret_key_hex(&self.home)?,
                };
                seller_git::init_contribution_workdir(
                    &active.workdir,
                    &identity,
                    contribution.target.clone_url(),
                    contribution.base.branch(),
                    contribution.base.oid(),
                    &branch,
                    Some(&fork_auth),
                )
            }
            None => {
                seller_git::init_empty_delivery_workdir(&active.workdir, &identity)
            }
        };
        if let Err(error) = init_result {
            let code = if active.contribution.is_some() {
                ErrorReasonCode::GitForkFailed
            } else {
                ErrorReasonCode::Internal
            };
            self.fail_active(code, &error.to_string()).await?;
            return Err(error.into());
        }
        let run_started = std::time::Instant::now();
        // The daemon OWNS delivery — append explicit, secret-free instructions so
        // the agent commits its deliverable to git (the daemon pushes it) instead of guessing.
        // Read-on-start: inline the MEMORY.md index when memory is enabled (byte-identical
        // prompt when disabled).
        let memory_section = self.read_on_start_section();
        let prompt = compose_agent_prompt(
            &active.offer.task,
            &seller_cfg.git_remote,
            memory_section.as_deref(),
        );
        // Retry a transient agent error while the deadline still has room. The
        // feedback-kind error (fail_active, below) is published only after the attempt budget or
        // the deadline is spent — a transient failure never burns the claim early.
        let run_result = run_agent_with_retry(
            active.deadline_unix,
            MAX_AGENT_ATTEMPTS,
            now_unix,
            |_attempt| {
                // Each attempt runs under the job's *remaining* deadline, not a
                // hardcoded 300s.
                let job_timeout = unified_job_timeout(active.deadline_unix, now_unix());
                run_agent_job(
                    &seller_cfg.agent_command,
                    &prompt,
                    &active.workdir,
                    &identity,
                    job_timeout,
                )
            },
        )
        .await;
        // Wall-time is always measurable; token/model/cost ride out on `usage` only when the
        // ACP driver actually surfaced them (absent-stays-absent → `None`).
        let wall_time_ms = run_started.elapsed().as_millis() as u64;
        let usage = match run_result {
            Ok(usage) => usage,
            Err(error) => {
                self.fail_active(agent_error_reason_code(&error), &error.to_string())
                    .await?;
                return Err(error);
            }
        };
        // Delivery. The agent edits files; it never commits (any commits it makes are scratch and
        // ignored). Snapshot the final workdir tree into ONE commit under the delivery identity,
        // parented on the pinned base (or a root commit for a from-scratch job). An empty tree —
        // or one identical to the base — is refused with a precise reason. The offer does not yet
        // carry a completion-check command over the wire (adding an offer field is the open
        // question flagged in the PR), so the snapshot's tree-diff gate is the sole completion
        // signal; [`run_completion_check`] is the seam that gates delivery once the field exists.
        let base_oid = active.contribution.as_ref().map(|c| c.base.oid());
        let message = delivery_message(&active.offer.task);
        if let Err(error) =
            seller_git::snapshot_delivery(&active.workdir, &identity, base_oid, &branch, &message)
        {
            self.fail_active(ErrorReasonCode::AgentRunFailed, &error.to_string())
                .await?;
            return Err(error.into());
        }

        // NIP-98: key from 0600 file → git child env only (never argv / never logged).
        let push_secret = home::read_secret_key_hex(&self.home)?;
        let push_auth = seller_git::PushAuth {
            secret_key_hex: push_secret,
        };
        let commit = match seller_git::push_branch_with_auth(
            &active.workdir,
            &seller_cfg.git_remote,
            &branch,
            Some(&push_auth),
        ) {
            Ok(oid) => oid,
            Err(error) => {
                // Display path must not echo the secret (SellerGitError is scrubbed).
                self.fail_active(ErrorReasonCode::GitPushFailed, &error.to_string())
                    .await?;
                return Err(error.into());
            }
        };
        drop(push_auth);

        let job_hash = job_hash_for_offer(&active.job_id, &active.offer.task, active.offer.amount);
        // The seller signs the RECEIPT PREIMAGE (binds the trade + the
        // delivered git object, D4) — not the bare job-hash. The buyer reconstructs this
        // exact preimage and co-signs it. `exec_metadata_commitment` is the empty marker:
        // exec-metadata is NOT covered by the co-signature (seller-claimed).
        // Derive the delivery discriminator from the SAME typed `Delivery` the buyer's pay path
        // uses (rather than a `DeliveryKind::Fork` hardcode) so both sides agree by construction ("fork").
        let delivery_kind = seller_delivery_kind(&seller_cfg.git_remote, &branch, &commit)?;
        // Bind the seller-authored `creq` into the receipt so BOTH co-signatures
        // commit to the payment terms the seller published. The creq is reconstructed from the
        // SAME inputs used at claim time (`build_seller_creq` is pure over job id / amount / unit /
        // accepted_mints / seller key), so its hash equals the one the buyer read off the claim
        // and threaded through its pay path — the co-signatures agree by construction. The specific
        // realized mint is deliberately NOT in the preimage (the seller signs at delivery, before
        // the buyer chooses which accepted mint to pay from); the accepted-mint SET is bound via
        // this creq_hash, so buyer/seller cosigs agree for ANY accepted mint.
        let authored_creq = gateway::creq::build_seller_creq(
            &active.job_id,
            active.offer.amount,
            &active.offer.unit,
            &self.home.config.accepted_mints,
            &self.seller_pubkey,
        )
        .map_err(|error| DaemonError::Seller(SellerError::Io(error.to_string())))?;
        let preimage = crate::receipt::ReceiptPreimage {
            job_hash: job_hash.clone(),
            offer_id: active.job_id.clone(),
            amount: active.offer.amount,
            unit: "sat".to_owned(),
            buyer_pubkey: active.buyer_pubkey.clone(),
            seller_pubkey: self.seller_pubkey.clone(),
            delivery_integrity_hash: commit.clone(),
            delivery_kind: delivery_kind.as_str().to_owned(),
            exec_metadata_commitment: crate::receipt::EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
            creq_hash: Some(gateway::creq_hash_hex(&authored_creq)),
        };
        let seller_sig = sign_receipt_hash(&self.keys, &preimage.digest_hex())?;
        // Harness-generic PUBLIC seller-claimed usage block (opportunistic;
        // absent fields stay absent). `usage` carries the ACP-native token/model/cost the driver
        // surfaced this run — `None` when the harness exposed nothing.
        let exec_metadata = seller_exec_metadata(
            &seller_cfg.agent_command,
            seller_cfg.agent.as_deref(),
            wall_time_ms,
            usage.as_ref(),
        );
        let mut draft = git_result_draft(
            &active.job_id,
            &active.buyer_pubkey,
            &seller_cfg.git_remote,
            &branch,
            &commit,
            active.offer.amount,
            &job_hash,
            &seller_sig,
            format!("delivery commit {commit}"),
            &exec_metadata,
        );
        // On a contribution, echo the pinned target + base + accepts and append the
        // seller's schnorr signature over the authorship tuple {job_id, seller_pubkey, target_repo,
        // base_oid, fork_ref, commit_oid}. The fork_ref is (this seller's git_remote, the
        // per-job unique branch); the fork tip is `commit`. The buyer verifies this sig at the
        // pre-pay seam and equality-checks the echo against its signed offer.
        if let Some(contribution) = &active.contribution {
            match crate::contribution::ForkRef::new(&seller_cfg.git_remote, &branch) {
                Ok(fork) => {
                    let tuple = crate::contribution::AuthorshipTuple {
                        job_id: active.job_id.clone(),
                        seller_pubkey: self.seller_pubkey.clone(),
                        target: contribution.target.clone(),
                        base_oid: contribution.base.oid().to_owned(),
                        fork,
                        commit_oid: commit.clone(),
                    };
                    let tuple_sig = crate::contribution::sign_authorship_tuple(&self.keys, &tuple);
                    draft
                        .tags
                        .extend(crate::contribution::contribution_result_tags(contribution, &tuple_sig));
                }
                Err(error) => {
                    self.fail_active(ErrorReasonCode::Internal, &error.to_string())
                        .await?;
                    return Err(DaemonError::Seller(SellerError::Io(error.to_string())));
                }
            }
        }
        let result_id = match publish_draft(&self.home, &self.keys, &draft).await {
            Ok(id) => id,
            Err(error) => {
                self.fail_active(ErrorReasonCode::Internal, &error.to_string())
                    .await?;
                return Err(error);
            }
        };
        if let Some(slot) = self.active.as_mut() {
            slot.result_id = Some(result_id.clone());
            // Stash the delivery facts so the terminal episode (paid at receipt, or
            // unpaid at eviction) is one complete append. `transcript_ref` is a POINTER to the
            // already-durable per-job transcript (`run_agent_job` wrote it); never a copy.
            let (harness, _) =
                harness_and_transport(&seller_cfg.agent_command, seller_cfg.agent.as_deref());
            slot.delivery = Some(DeliveryRecord {
                result_id: result_id.clone(),
                commit_oid: commit.clone(),
                git_remote: seller_cfg.git_remote.clone(),
                branch: branch.clone(),
                delivery_kind: delivery_kind.as_str().to_owned(),
                harness,
                wall_time_ms,
                usage: usage.clone(),
                transcript_ref: format!("seller-jobs/{}/seller-run.jsonl", active.job_id),
                deliver_ts: now_unix(),
            });
        }
        // DELIVERED announce: the result-kind result is published and pushed. Diagnostic only.
        self.announce(crate::announce::AnnounceEvent::delivered(
            now_unix(),
            &self.seller_pubkey,
            &active.job_id,
            &result_id,
            &commit,
            active.offer.amount,
        ));
        Ok(result_id)
    }

    async fn fail_active(
        &mut self,
        code: ErrorReasonCode,
        human_reason: &str,
    ) -> Result<(), DaemonError> {
        if let Some(active) = self.active.take() {
            let reason = seller_error_content(code, human_reason);
            // Capture the ERRORED terminal, threading `reason` (previously discarded)
            // into `error_reason`. Best-effort; the feedback-kind below is the buyer-facing surface.
            self.record_errored_episode(&active, &reason);
            // JOB-FAILED announce (with the machine reason). Diagnostic; the feedback-kind stays the
            // buyer-facing surface.
            self.announce(crate::announce::AnnounceEvent::job_failed(
                now_unix(),
                &self.seller_pubkey,
                &active.job_id,
                &reason,
            ));
            let draft = error_draft(
                &active.job_id,
                &active.buyer_pubkey,
                &self.seller_pubkey,
                reason,
            );
            let _ = publish_draft(&self.home, &self.keys, &draft).await;
        }
        Self::end_flight();
        Ok(())
    }

    /// Read-on-start: the rendered `MEMORY.md` section to inline into the job prompt, or
    /// `None` when memory is disabled or there is no non-empty index. Ensures the memory dir on
    /// first use (seeding operator-notes.md + a non-empty index). Best-effort: any error degrades
    /// to `None` (no memory), never blocks the job.
    fn read_on_start_section(&self) -> Option<String> {
        let cfg = &self.home.config.seller_memory;
        if !cfg.memory_enabled {
            return None;
        }
        let dir = crate::seller_memory::memory_dir(&self.home.root);
        if let Err(error) = crate::seller_memory::ensure_memory_dir(&dir) {
            eprintln!("seller memory: ensure dir failed (skipping read-on-start): {error}");
            return None;
        }
        match crate::seller_memory::read_on_start_section(
            &dir,
            cfg.read_on_start_template_path.as_deref(),
        ) {
            Ok(section) => section,
            Err(error) => {
                eprintln!("seller memory: read-on-start failed (skipping): {error}");
                None
            }
        }
    }

    /// Retro: resolve the plan for a delivered-PAID job, or `None` to skip. `None` when
    /// retro is disabled, the memory dir can't be prepared, or no `delivered_paid` episode exists
    /// for `job_id` (retro fires on delivered-paid ONLY — refusals/errors never reach here).
    /// Driver-free so it is testable without the `acp` feature.
    fn retro_context(&self, job_id: &str) -> Option<RetroPlan> {
        let cfg = &self.home.config.seller_memory;
        if !cfg.retro_enabled {
            return None;
        }
        let memory_dir = crate::seller_memory::memory_dir(&self.home.root);
        if let Err(error) = crate::seller_memory::ensure_memory_dir(&memory_dir) {
            eprintln!("seller retro: ensure memory dir failed (skip): {error}");
            return None;
        }
        let episode = match EpisodeLog::open(&self.home.root).last_delivered_paid(job_id) {
            Ok(Some(episode)) => episode,
            Ok(None) => return None,
            Err(error) => {
                eprintln!("seller retro: episode read failed (skip): {error}");
                return None;
            }
        };
        // Seed the retro with the episode + the ABSOLUTE transcript path (fresh turn
        // seeded with episode + transcript_ref; the transcript is a pointer, never copied).
        let episode_json = serde_json::to_string_pretty(&episode).unwrap_or_default();
        let transcript_abs = episode
            .transcript_ref
            .as_ref()
            .map(|rel| self.home.root.join(rel).display().to_string())
            .unwrap_or_else(|| "(no transcript recorded for this job)".to_owned());
        let prompt = crate::seller_memory::retro_prompt(
            &memory_dir,
            &episode_json,
            &transcript_abs,
            cfg.retro_prompt_path.as_deref(),
        );
        let seller_cfg = require_seller_config(&self.home).ok()?;
        let log_path = job_workdir(&self.home, job_id).join("seller-retro.jsonl");
        Some(RetroPlan {
            memory_dir,
            prompt,
            log_path,
            agent_command: seller_cfg.agent_command.clone(),
        })
    }

    /// Retro trigger: after a delivered-PAID receipt, run ONE best-effort agent turn to
    /// update memory. **Fully detached** — it MUST NOT run in the seller event loop: a retro is a
    /// whole agent turn (up to the job timeout, or a hang) and the loop is single-tasked, so an
    /// inline retro would stop the daemon from collecting kind-1059 payments (the money path) and
    /// from claiming offers until the retro finished (regression: "wraps parked, never collected").
    /// So it runs on its OWN OS thread with its OWN runtime and this call returns immediately;
    /// the money path never waits on retro. No-op without the `acp` feature.
    #[cfg(feature = "acp")]
    fn maybe_run_retro(&self, job_id: &str) {
        let Some(plan) = self.retro_context(job_id) else {
            return;
        };
        if plan.agent_command.is_empty() {
            return;
        }
        // AcpDriver is !Send, so it is BUILT inside the thread; `plan` is owned/Send.
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("seller retro: runtime build failed (skip): {error}");
                    return;
                }
            };
            if let Some(parent) = plan.log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let timeout = Duration::from_secs(crate::seller::DEFAULT_JOB_TIMEOUT_SECS);
            let mut driver = crate::driver::AcpDriver::new(
                crate::driver::AgentCommand::new(
                    plan.agent_command[0].clone(),
                    plan.agent_command[1..].to_vec(),
                ),
                crate::driver::PermissionOutcome::Allow,
                timeout,
            );
            if let Err(error) = runtime.block_on(run_retro_turn(
                &mut driver,
                &plan.memory_dir,
                &plan.prompt,
                &plan.log_path,
            )) {
                eprintln!("seller retro: best-effort turn failed (money path unaffected): {error}");
            }
        });
    }

    #[cfg(not(feature = "acp"))]
    fn maybe_run_retro(&self, _job_id: &str) {}

    /// Best-effort episode append — diagnostic, NEVER fails a trade. Logs on error, swallows it.
    /// Episodes carry ids/amounts/mint/task-text/self-reported-usage only; no token/key material.
    fn write_episode(&self, episode: &Episode) {
        let log = EpisodeLog::open(&self.home.root);
        if let Err(error) = log.append(episode) {
            eprintln!(
                "seller episode capture failed (non-fatal) job_id={}: {error}",
                episode.job_id
            );
        }
        // Source of truth (the append above) is attempted FIRST; only then does the brain-telemetry
        // stream fan the same episode out to the pluggable sink/mirror. Best-effort and off the hot
        // path — a sink/mirror failure never blocks the seller loop or affects the append above.
        self.emit_telemetry(episode);
    }

    /// Best-effort brain/episode telemetry — fan one captured episode out to the `[telemetry]` sink
    /// command and/or mirror file so an operator can watch the seller's reasoning/economics live.
    /// NEVER blocks the event loop or the episode append: [`crate::telemetry::emit`] does the mirror
    /// write inline (fast, durable append) and dispatches the sink on its OWN detached thread,
    /// returning immediately; a missing/slow/hung/failing sink is logged once and swallowed. No-op
    /// when the channel is disabled or unpointed. Telemetry carries only the episode (ids/amounts/
    /// task-text/self-reported-usage) + the public seller pubkey/job id — never a token/key/secret.
    fn emit_telemetry(&self, episode: &Episode) {
        let cfg = &self.home.config.telemetry;
        if !cfg.enabled {
            return;
        }
        crate::telemetry::emit(cfg, &crate::telemetry::TelemetryEvent::episode(now_unix(), episode));
    }

    /// Best-effort lifecycle announce — dispatch one JSON event to the configured
    /// `[seller_announce]` sink command. NEVER blocks the event loop: [`crate::announce::dispatch`]
    /// runs the whole spawn+write+bounded-wait on its OWN detached thread and returns immediately;
    /// a missing/slow/hung/failing sink is logged once and swallowed. No-op when no sink is
    /// configured (feature OFF ⇒ zero behavior change). Announce events carry only ids/amounts/
    /// reasons already public on the relay or in the seller log — never a token/key/plaintext.
    fn announce(&self, event: crate::announce::AnnounceEvent) {
        let cfg = &self.home.config.seller_announce;
        if !cfg.is_enabled() {
            return;
        }
        crate::announce::dispatch(
            &cfg.command,
            Duration::from_millis(cfg.timeout_ms),
            &event,
        );
    }

    /// Refused terminal. Best-effort re-parse of the offer for its facts (classify does
    /// not hand them back). No episode for a non-offer, a dedup re-see, or an unparseable event —
    /// those are not freshly-classified jobs.
    fn record_refused_episode(&self, event: &nostr_sdk::Event, skip: &OfferSkip) {
        if matches!(skip, OfferSkip::NotAnOffer { .. } | OfferSkip::AlreadyProcessed) {
            return;
        }
        let draft = event_to_draft(event);
        let Ok(offer) = parse_offer(&draft) else {
            return; // Unparseable ⇒ no offer facts to record.
        };
        let rate = require_seller_config(&self.home)
            .map(|cfg| cfg.rate_sats)
            .unwrap_or(0);
        let contribution = crate::contribution::parse_contribution_offer(&draft.tags)
            .ok()
            .flatten();
        let mut episode = Episode::new(
            EpisodeKind::Refused,
            EpisodeOutcome::Refused,
            now_unix(),
            &self.seller_pubkey,
            event.id.to_hex(),
        );
        fill_offer_facts(
            &mut episode,
            &offer,
            &event.pubkey.to_hex(),
            rate,
            self.home.config.default_mint(),
            contribution.as_ref(),
        );
        episode.refusal_reason_code = Some(skip.code().to_owned());
        episode.refusal_reason = Some(skip.reason());
        self.write_episode(&episode);
    }

    /// Errored terminal (claimed job that failed before or during delivery).
    fn record_errored_episode(&self, active: &ActiveJob, reason: &str) {
        let rate = require_seller_config(&self.home)
            .map(|cfg| cfg.rate_sats)
            .unwrap_or(0);
        let mut episode = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::Errored,
            now_unix(),
            &self.seller_pubkey,
            &active.job_id,
        );
        fill_offer_facts(
            &mut episode,
            &active.offer,
            &active.buyer_pubkey,
            rate,
            self.home.config.default_mint(),
            active.contribution.as_ref(),
        );
        episode.claim_id = Some(active.claim_id.clone());
        episode.resolved_deadline_unix = Some(active.deadline_unix);
        episode.error_reason = Some(reason.to_owned());
        if let Some(delivery) = &active.delivery {
            fill_delivery_facts(&mut episode, delivery);
        }
        self.write_episode(&episode);
    }

    /// Delivered-PAID terminal. Complete episode: offer + claim + delivery + payment.
    fn record_paid_episode(&self, job: &ActiveJob, amount_received: u64, expected_amount: u64) {
        let rate = require_seller_config(&self.home)
            .map(|cfg| cfg.rate_sats)
            .unwrap_or(0);
        let mut episode = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredPaid,
            now_unix(),
            &self.seller_pubkey,
            &job.job_id,
        );
        fill_offer_facts(
            &mut episode,
            &job.offer,
            &job.buyer_pubkey,
            rate,
            self.home.config.default_mint(),
            job.contribution.as_ref(),
        );
        episode.claim_id = Some(job.claim_id.clone());
        episode.resolved_deadline_unix = Some(job.deadline_unix);
        if let Some(delivery) = &job.delivery {
            fill_delivery_facts(&mut episode, delivery);
        }
        episode.amount_received = Some(amount_received);
        episode.expected_amount = Some(expected_amount);
        episode.swap_ok = Some(true);
        episode.collect_ts = Some(now_unix());
        self.write_episode(&episode);
    }

    /// Delivered-but-UNPAID terminal (awaiting-payment backpressure eviction).
    fn record_delivered_unpaid_episode(&self, job: &ActiveJob) {
        let rate = require_seller_config(&self.home)
            .map(|cfg| cfg.rate_sats)
            .unwrap_or(0);
        let mut episode = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredUnpaid,
            now_unix(),
            &self.seller_pubkey,
            &job.job_id,
        );
        fill_offer_facts(
            &mut episode,
            &job.offer,
            &job.buyer_pubkey,
            rate,
            self.home.config.default_mint(),
            job.contribution.as_ref(),
        );
        episode.claim_id = Some(job.claim_id.clone());
        episode.resolved_deadline_unix = Some(job.deadline_unix);
        if let Some(delivery) = &job.delivery {
            fill_delivery_facts(&mut episode, delivery);
        }
        episode.expected_amount = Some(job.offer.amount);
        self.write_episode(&episode);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptOutcome {
    pub job_id: String,
    pub result_id: String,
    pub amount_received: u64,
}

fn mint_url(raw: &str) -> Result<cashu::MintUrl, DaemonError> {
    use std::str::FromStr;
    cashu::MintUrl::from_str(raw)
        .map_err(|error| DaemonError::Policy(format!("invalid mint url: {error}")))
}

/// Opportunistic driver usage → serde-friendly episode mirror (absent-stays-absent;
/// a field the harness did not surface stays `None`, never zero-filled).
fn usage_record(usage: Option<&UsageMetadata>) -> UsageRecord {
    let Some(usage) = usage else {
        return UsageRecord::default();
    };
    UsageRecord {
        model: usage.model.clone(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost_amount: usage.cost.as_ref().map(|cost| cost.amount.clone()),
        cost_basis: usage.cost.as_ref().map(|cost| cost.basis.clone()),
        // Transport is a property of the harness/adapter (see `harness_and_transport`), not of a
        // per-run usage capture, so the episode's usage mirror does not carry it.
        transport: None,
    }
}

/// Fill the offer-facts group shared by every episode kind. Pure over its inputs.
fn fill_offer_facts(
    episode: &mut Episode,
    offer: &ParsedOffer,
    buyer_pubkey: &str,
    rate_sats: u64,
    // `episode.mint` is sourced from the seller's default accepted mint rather
    // than `offer.mint_url` (consumer re-pointed off the offer). Job E replaces this with the
    // realized mint from the buyer's payment payload.
    mint: &str,
    contribution: Option<&ContributionOffer>,
) {
    episode.offer_task = offer.task.clone();
    episode.output_type = offer.output.clone();
    episode.amount = offer.amount;
    episode.unit = offer.unit.clone();
    episode.mint = mint.to_owned();
    episode.deadline_unix = offer.deadline_unix;
    episode.offer_target = offer.seller_pubkey.clone();
    episode.buyer_pubkey = buyer_pubkey.to_owned();
    episode.configured_rate_sats = rate_sats;
    match contribution {
        Some(contribution) => {
            episode.job_class = "contribution".to_owned();
            episode.contribution_target = Some(contribution.target.clone_url().to_owned());
            episode.contribution_base_oid = Some(contribution.base.oid().to_owned());
        }
        None => episode.job_class = "from_scratch".to_owned(),
    }
}

/// Fill the delivery-facts group from a captured [`DeliveryRecord`].
fn fill_delivery_facts(episode: &mut Episode, delivery: &DeliveryRecord) {
    episode.result_id = Some(delivery.result_id.clone());
    episode.commit_oid = Some(delivery.commit_oid.clone());
    episode.fork_git_remote = Some(delivery.git_remote.clone());
    episode.fork_branch = Some(delivery.branch.clone());
    episode.delivery_kind = Some(delivery.delivery_kind.clone());
    episode.usage = usage_record(delivery.usage.as_ref());
    episode.wall_time_ms = Some(delivery.wall_time_ms);
    episode.harness = Some(delivery.harness.clone());
    episode.transcript_ref = Some(delivery.transcript_ref.clone());
    episode.deliver_ts = Some(delivery.deliver_ts);
}

/// Run the job-carried completion check in `workdir`, if the offer specifies one. `None` ⇒ no
/// check (the snapshot's tree-diff gate stands alone). A non-zero exit refuses the delivery with a
/// precise reason so the seat does not deliver work that fails its own acceptance command.
///
/// The check runs via the login shell (`sh -c`) in the job workdir — the same jailed directory the
/// agent worked in. This is the completion seam; the wire field that carries the command is the
/// open question flagged in the PR. Until that field exists the delivery path has no live call
/// site (an offer-less check is always `None` ⇒ no-op), so the seam is kept under its tests only.
#[cfg_attr(not(test), allow(dead_code))]
fn run_completion_check(workdir: &Path, check: Option<&str>) -> Result<(), DaemonError> {
    let Some(command) = check.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .status()
        .map_err(|error| {
            DaemonError::Policy(format!("delivery refused: completion check failed to run: {error}"))
        })?;
    if !status.success() {
        return Err(DaemonError::Policy(format!(
            "delivery refused: completion check `{command}` exited {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "by signal".to_owned()),
        )));
    }
    Ok(())
}

/// A concise, single-line delivery-commit message derived from the offer task: the first non-empty
/// line, whitespace-collapsed and length-capped. Falls back to a fixed label for an empty task.
fn delivery_message(task: &str) -> String {
    let summary: String = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if summary.is_empty() {
        return "mobee delivery".to_owned();
    }
    let capped: String = summary.chars().take(72).collect();
    format!("mobee delivery: {capped}")
}

/// Delivery discriminator for the seller's commit/fork delivery, derived from the SAME typed
/// [`GitDelivery`](crate::delivery::GitDelivery) the buyer's pay path uses — NOT a hardcoded
/// label — so buyer and seller derive it from one abstraction (`"fork"`). Fails closed if the
/// just-pushed fields somehow do not type (impossible on the success path — a git push returns
/// a canonical oid); never silently relabels or emits a bogus kind.
fn seller_delivery_kind(
    git_remote: &str,
    branch: &str,
    commit_oid: &str,
) -> Result<crate::receipt::DeliveryKind, DaemonError> {
    let delivery = crate::delivery::GitDelivery::new(
        git_remote.to_owned(),
        branch.to_owned(),
        crate::delivery::CommitOid::parse(commit_oid.to_owned())
            .map_err(|error| DaemonError::Policy(format!("delivery oid: {error}")))?,
    )
    .map_err(|error| DaemonError::Policy(format!("delivery typing: {error}")))?;
    Ok(delivery.delivery_kind())
}

/// Build the seller-claimed PUBLIC usage block for a result-kind result.
///
/// This block is PUBLIC and harness-generic. It is **opportunistic**:
/// emit only fields the seller can source. `harness` is resolved from the configured preset
/// label (else the agent command), `wall_time` is measured, and
/// `metadata_trust=seller-claimed` is required whenever any field is present (anchor rule).
///
/// `usage_transport` is the harness/adapter's declared capture axis (`acp-native` for the
/// codex adapter, `side-channel` otherwise), resolved from the configured harness identity.
///
/// Token / model / cost tags are appended **only where the driver surfaced them**
/// (absent-stays-absent, never zero-filled — a fabricated `0` is worse than a rendered dash).
/// `total` = `input + output + reasoning` (locked rule); cache siblings are evidence and are
/// NEVER summed into `total`. When `usage` is `None` the block is exactly the four base
/// tags — no-capture trades stay honestly dashed.
fn seller_exec_metadata(
    agent_command: &[String],
    agent_preset: Option<&str>,
    wall_time_ms: u64,
    usage: Option<&UsageMetadata>,
) -> Vec<TagSpec> {
    let (harness, transport) = harness_and_transport(agent_command, agent_preset);
    let wall = wall_time_ms.to_string();

    let mut tags = vec![
        TagSpec::new(["harness", harness.as_str()]),
        TagSpec::new(["usage_transport", transport]),
        TagSpec::new(["metadata_trust", "seller-claimed"]),
        TagSpec::new(["wall_time", wall.as_str(), "ms"]),
    ];

    if let Some(u) = usage {
        if let Some(model) = &u.model {
            tags.push(TagSpec::new(["model", model.as_str()]));
        }
        // Own the string renders so the borrows outlive each `TagSpec::new` call.
        let total = u.total_tokens().map(|n| n.to_string());
        let input = u.input_tokens.map(|n| n.to_string());
        let output = u.output_tokens.map(|n| n.to_string());
        let reasoning = u.reasoning_tokens.map(|n| n.to_string());
        let cache_read = u.cache_read_tokens.map(|n| n.to_string());
        let cache_write = u.cache_write_tokens.map(|n| n.to_string());
        if let Some(v) = &total {
            tags.push(TagSpec::new(["tokens", v.as_str(), "total"]));
        }
        if let Some(v) = &input {
            tags.push(TagSpec::new(["tokens", v.as_str(), "input"]));
        }
        if let Some(v) = &output {
            tags.push(TagSpec::new(["tokens", v.as_str(), "output"]));
        }
        if let Some(v) = &reasoning {
            tags.push(TagSpec::new(["tokens", v.as_str(), "reasoning"]));
        }
        if let Some(v) = &cache_read {
            tags.push(TagSpec::new(["tokens", v.as_str(), "cache_read"]));
        }
        if let Some(v) = &cache_write {
            tags.push(TagSpec::new(["tokens", v.as_str(), "cache_write"]));
        }
        if let Some(cost) = &u.cost {
            tags.push(TagSpec::new([
                "cost",
                cost.amount.as_str(),
                "usd",
                cost.basis.as_str(),
            ]));
        }
    }

    tags
}

/// Best-effort harness id + usage transport.
///
/// The configured **preset label** (`claude`|`cursor`|`codex`, [`SellerConfig::agent`]) is the
/// authoritative harness/adapter identity and is preferred over argv inspection: presets launch
/// the ACP adapter via `npx <adapter-package>` (argv0 = `npx`), so an argv0-naive id emitted
/// `npx` — which a downstream harness-family classifier maps to `harness_family="other"`, hiding
/// real claude/codex/cursor jobs. When no preset label is present (raw
/// `--agent-argv` power-user hatch) fall back to scanning the FULL adapter argv (not just argv0):
/// the adapter package name (e.g. `@agentclientprotocol/claude-agent-acp`) still carries the
/// family. Unknown ⇒ the command basename + the conservative `side-channel`.
fn harness_and_transport(
    agent_command: &[String],
    agent_preset: Option<&str>,
) -> (String, &'static str) {
    // Preset label is authoritative — resolve from the adapter identity, never argv0.
    // A non-built-in label is a config-defined `[agents]` preset: the preset name IS the
    // harness identity (conservative `side-channel` transport — nothing is known about it).
    if let Some(preset) = agent_preset {
        match preset.trim().to_ascii_lowercase().as_str() {
            "claude" => return ("claude-agent-acp".to_owned(), "side-channel"),
            "codex" => return ("codex-acp-ng".to_owned(), "acp-native"),
            "cursor" => return ("cursor-agent".to_owned(), "side-channel"),
            "" => {}
            _ => return (preset.trim().to_owned(), "side-channel"),
        }
    }
    // Hatch fallback: scan the FULL argv (adapter identity), not just argv0.
    let joined = agent_command.join(" ").to_ascii_lowercase();
    if joined.contains("codex") {
        ("codex-acp-ng".to_owned(), "acp-native")
    } else if joined.contains("cursor") {
        ("cursor-agent".to_owned(), "side-channel")
    } else if joined.contains("claude") {
        ("claude-agent-acp".to_owned(), "side-channel")
    } else {
        let program = agent_command.first().map(String::as_str).unwrap_or("");
        let basename = program.rsplit('/').next().unwrap_or(program);
        let harness = if basename.is_empty() {
            "unknown".to_owned()
        } else {
            basename.to_owned()
        };
        (harness, "side-channel")
    }
}

fn job_workdir(home: &MobeeHome, job_id: &str) -> PathBuf {
    home.root.join("seller-jobs").join(job_id)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The ONE coherent job timeout. The ACP driver's idle/response timeout is
/// derived from the job's own deadline (`--job-timeout-secs` → offer deadline → default, via
/// [`job_deadline_unix`]) so a job has a single predictable deadline. Before this the driver
/// used a hardcoded 300s idle-timeout that silently conflicted with `--job-timeout-secs`
/// (a live codex seller hung ~300s on an ACP request while the job deadline said otherwise).
/// Saturating: a non-positive remaining window yields `Duration::ZERO`, which fails the run
/// cleanly at the deadline rather than hanging.
fn unified_job_timeout(deadline_unix: u64, now_unix: u64) -> Duration {
    Duration::from_secs(deadline_unix.saturating_sub(now_unix))
}

/// Run the agent with bounded retries that stay WITHIN the job deadline.
///
/// A transient agent error is retried until either the attempt budget (`max_attempts`) is
/// spent OR the deadline (`deadline_unix`, checked against injected `now`) passes. The error
/// is surfaced to the caller — which then publishes the feedback-kind error exactly once — ONLY
/// after one of those limits is reached. This stops a transient failure from immediately
/// burning the claim while the deadline still has room. `run` is invoked with the 1-based
/// attempt number and awaited to completion before
/// any retry, so attempts never overlap.
async fn run_agent_with_retry<F, Fut>(
    deadline_unix: u64,
    max_attempts: u32,
    now: impl Fn() -> u64,
    mut run: F,
) -> Result<Option<UsageMetadata>, DaemonError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<Option<UsageMetadata>, DaemonError>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match run(attempt).await {
            Ok(usage) => return Ok(usage),
            // Retry only while BOTH an attempt and the deadline remain; otherwise surface the
            // error so the caller publishes feedback-kind exactly once (past deadline / exhausted).
            Err(_) if attempt < max_attempts && now() < deadline_unix => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Daemon-owned delivery. The daemon appends explicit, secret-free delivery
/// instructions to the agent's task prompt so the agent delivers by committing its work to
/// the git repository in its working directory — rather than guessing a delivery channel.
/// The daemon performs the authenticated push of the committed branch to the bound remote
/// (NIP-98; the agent is never handed a key), so this text carries NO secret — it is public
/// prompt text built only from the task and the (public) remote URL.
fn compose_agent_prompt(task: &str, git_remote: &str, memory_section: Option<&str>) -> String {
    let base = format!(
        "{task}\n\n\
         ---\n\
         DELIVERY (required). Deliver your work by committing it with git in your current \
         working directory:\n\
         - Make one or more non-empty commits authored by you. Do not leave the deliverable \
         uncommitted and do not only print it to the console.\n\
         - You do NOT need to push and you are NOT handed any credentials: the daemon pushes \
         your committed branch to the bound git remote ({git_remote}) on your behalf.\n\
         Anything not committed to git will not be delivered."
    );
    // Read-on-start: when memory is enabled the rendered index section is appended.
    // When `None` (memory_enabled=false, or no non-empty index) the output is byte-IDENTICAL to
    // the memory-disabled prompt (golden invariant).
    match memory_section {
        Some(section) => format!("{base}\n\n{section}"),
        None => base,
    }
}

/// Kind-feedback `status=error` draft for a targeted under-rate refusal. Content carries the
/// machine-readable rate-gate reason (same string the skip log already has) so buyers can
/// distinguish rate-refusal from a crash / empty-content failure.
fn under_rate_error_draft(
    offer_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
    reason: &str,
) -> EventDraft {
    error_draft(offer_id, buyer_pubkey, seller_pubkey, reason)
}

fn seller_error_content(code: ErrorReasonCode, human: &str) -> String {
    let sanitized = truncate_human_reason(&strip_absolute_paths(human), 300);
    let human = if sanitized.trim().is_empty() {
        "seller aborted the job without a detailed reason"
    } else {
        sanitized.trim()
    };
    format!("{}: {human}", code.as_str())
}

fn strip_absolute_paths(input: &str) -> String {
    input
        .split_whitespace()
        .map(strip_absolute_path_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_absolute_path_token(token: &str) -> String {
    let mut first_path = None;
    for (idx, ch) in token.char_indices() {
        if ch == '/' {
            first_path = Some(idx);
            break;
        }
    }
    let Some(start) = first_path else {
        return token.to_owned();
    };
    if start > 0 && token[..start].ends_with(':') && token[start..].starts_with("//") {
        return token.to_owned();
    }
    if start > 0 {
        let prefix = &token[..start];
        if prefix
            .chars()
            .last()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return token.to_owned();
        }
    }

    let suffix_start = token[start..]
        .find(|ch: char| matches!(ch, ',' | ';' | ':' | ')' | ']' | '}'))
        .map(|offset| start + offset)
        .unwrap_or(token.len());
    format!("{}<path>{}", &token[..start], &token[suffix_start..])
}

fn truncate_human_reason(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in input.chars().take(max_chars) {
        out.push(ch);
    }
    out
}

fn agent_error_reason_code(error: &DaemonError) -> ErrorReasonCode {
    match error {
        DaemonError::AcpRequired => ErrorReasonCode::AgentSpawnFailed,
        DaemonError::Agent(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("failed to spawn") || lower.contains("no such file") {
                ErrorReasonCode::AgentSpawnFailed
            } else if lower.contains("timed out")
                || lower.contains("timeout")
                || lower.contains("deadline")
            {
                ErrorReasonCode::AgentTimeout
            } else {
                ErrorReasonCode::AgentRunFailed
            }
        }
        _ => ErrorReasonCode::AgentRunFailed,
    }
}

fn contribution_refusal_error_content(skip: &OfferSkip) -> String {
    match skip {
        OfferSkip::ContributionUnsupported => {
            seller_error_content(ErrorReasonCode::ContributionUnsupported, &skip.reason())
        }
        OfferSkip::ContributionMalformed { .. } => {
            seller_error_content(ErrorReasonCode::ContributionMalformed, &skip.reason())
        }
        _ => seller_error_content(ErrorReasonCode::Internal, &skip.reason()),
    }
}

fn reconcile_error_content(liveness: ClaimLiveness) -> String {
    seller_error_content(ErrorReasonCode::ClaimReleased, reconcile_reason(liveness))
}

/// Build + sign an [`EventDraft`] into a signed nostr event WITHOUT publishing it. The signed
/// event carries its deterministic id (`event.id`), so a caller can durably journal that id
/// BEFORE the network publish (the write-before-publish ordering the claim path needs).
fn sign_draft(
    keys: &nostr_sdk::Keys,
    draft: &EventDraft,
) -> Result<nostr_sdk::Event, DaemonError> {
    let builder = gateway::nostr::event_builder(draft)
        .map_err(|error| DaemonError::Relay(format!("event builder: {error}")))?;
    builder
        .sign_with_keys(keys)
        .map_err(|error| DaemonError::Relay(format!("sign: {error}")))
}

/// Publish an already-signed event and return the accepted event id (== `event.id`).
async fn send_signed_event(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    event: &nostr_sdk::Event,
) -> Result<String, DaemonError> {
    use nostr_sdk::prelude::Client;

    let client = Client::new(keys.clone());
    client
        .add_relay(&home.config.relay_url)
        .await
        .map_err(|error| DaemonError::Relay(format!("add relay: {error}")))?;
    client.connect().await;
    let output = client
        .send_event_to([&home.config.relay_url], event)
        .await;
    client.disconnect().await;
    let output = output.map_err(|error| DaemonError::Relay(format!("send: {error}")))?;
    if output.success.is_empty() {
        return Err(DaemonError::Relay("no relay accepted event".into()));
    }
    Ok(output.val.to_hex())
}

async fn publish_draft(
    home: &MobeeHome,
    keys: &nostr_sdk::Keys,
    draft: &EventDraft,
) -> Result<String, DaemonError> {
    let event = sign_draft(keys, draft)?;
    send_signed_event(home, keys, &event).await
}

/// Publish one addressable kind-30340 heartbeat off the daemon loop's tick. This is best-effort
/// liveness/discovery signal: it MUST never crash or wedge the loop, so a publish failure or a
/// hung relay is bounded by a timeout and log-and-continue (the loop keeps serving offers and
/// collecting payments regardless). No-op when there is no `[seller]` config.
async fn publish_heartbeat(daemon: &SellerDaemon) {
    let Some(draft) = daemon.heartbeat_draft() else {
        return;
    };
    let event = draft.to_event_draft();
    let publish = publish_draft(&daemon.home, &daemon.keys, &event);
    match tokio::time::timeout(std::time::Duration::from_secs(10), publish).await {
        Ok(Ok(id)) => eprintln!(
            "seller heartbeat published id={id} accepting={} queue_depth={} rate_sats={}",
            draft.accepting, draft.queue_depth, draft.rate_sats
        ),
        Ok(Err(error)) => {
            eprintln!("seller heartbeat publish failed (continuing): {error}")
        }
        Err(_) => eprintln!("seller heartbeat publish timed out (continuing)"),
    }
}

#[cfg(feature = "acp")]
async fn run_agent_job(
    agent_command: &[String],
    prompt: &str,
    workdir: &Path,
    identity: &seller_git::DeliveryAgentIdentity,
    timeout: Duration,
) -> Result<Option<UsageMetadata>, DaemonError> {
    use crate::driver::{AcpDriver, AgentCommand, ContentBlock, PromptTurn, SessionConfig};
    use crate::engine::{RunParams, run_job};
    use crate::event::JobId;
    use crate::log::EventLog;

    if agent_command.is_empty() {
        return Err(DaemonError::Config("agent_command empty".into()));
    }
    // The ACP idle/response timeout IS the unified job timeout — never a
    // hardcoded 300s that could override or conflict with `--job-timeout-secs`.
    let mut driver = AcpDriver::new(
        AgentCommand::new(agent_command[0].clone(), agent_command[1..].to_vec()),
        crate::driver::PermissionOutcome::Allow,
        timeout,
    );
    let log_path = workdir.join("seller-run.jsonl");
    let mut log = EventLog::open(&log_path)
        .map_err(|error| DaemonError::Agent(error.to_string()))?;
    let params = RunParams {
        session_config: SessionConfig {
            cwd: workdir.to_path_buf(),
            mcp_servers: Vec::new(),
            env: identity.git_env(),
        },
        prompt: PromptTurn {
            input: vec![ContentBlock::Text {
                text: prompt.to_owned(),
            }],
        },
    };
    let outcome = run_job(
        &mut driver,
        &mut log,
        &JobId(format!("seller-{}", short_hash(prompt))),
        params,
        &mut |_| {},
    )
    .await
    .map_err(|error| DaemonError::Agent(error.to_string()))?;
    match outcome.terminal {
        crate::event::JobExecutionStatus::Completed => Ok(outcome.usage),
        other => Err(DaemonError::Agent(format!("agent terminal {other:?}"))),
    }
}

#[cfg(not(feature = "acp"))]
async fn run_agent_job(
    _agent_command: &[String],
    _prompt: &str,
    _workdir: &Path,
    _identity: &seller_git::DeliveryAgentIdentity,
    _timeout: Duration,
) -> Result<Option<UsageMetadata>, DaemonError> {
    Err(DaemonError::AcpRequired)
}

#[cfg(feature = "acp")]
fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..8])
}

/// Everything a retro turn needs, resolved WITHOUT a driver so it is testable and works
/// under `--no-default-features` (no `acp`). `None` from [`SellerDaemon::retro_context`] means "do
/// not run a retro" (disabled, or no paid episode to distill).
#[derive(Debug, Clone)]
struct RetroPlan {
    memory_dir: PathBuf,
    prompt: String,
    log_path: PathBuf,
    agent_command: Vec<String>,
}

/// Retro write-back: run ONE best-effort agent turn whose session cwd is the memory dir
/// (so the agent can read/write `MEMORY.md` and topic files by relative path), seeded with the
/// retro prompt. Merge-not-clobber is enforced at RUNTIME here, not by prompt prose: operator-owned
/// files are snapshotted before the turn and byte-restored after — regardless of what the agent
/// did. Generic over [`Driver`] so it is exercised with the mock driver in tests and an
/// `AcpDriver` in production.
///
/// The money path NEVER waits on this: the caller invokes it only after the receipt is journaled,
/// and swallows any error it returns.
async fn run_retro_turn<D: crate::driver::Driver>(
    driver: &mut D,
    memory_dir: &Path,
    prompt: &str,
    log_path: &Path,
) -> Result<(), DaemonError> {
    use crate::driver::{ContentBlock, PromptTurn, SessionConfig};
    use crate::engine::{run_job, RunParams};
    use crate::event::{JobExecutionStatus, JobId};
    use crate::log::EventLog;

    // Merge-not-clobber (runtime): capture operator files BEFORE the turn.
    let snapshot = crate::seller_memory::snapshot_operator_files(memory_dir)
        .map_err(|error| DaemonError::Agent(format!("retro snapshot: {error}")))?;
    let mut log =
        EventLog::open(log_path).map_err(|error| DaemonError::Agent(error.to_string()))?;
    let params = RunParams {
        session_config: SessionConfig {
            // cwd = memory dir ⇒ the retro turn's writes land in `memory/` by relative path.
            cwd: memory_dir.to_path_buf(),
            mcp_servers: Vec::new(),
            env: Vec::new(),
        },
        prompt: PromptTurn {
            input: vec![ContentBlock::Text {
                text: prompt.to_owned(),
            }],
        },
    };
    let outcome = run_job(driver, &mut log, &JobId("seller-retro".to_owned()), params, &mut |_| {})
        .await;
    // Restore operator files whatever the turn did (success, failure, or clobber attempt).
    let restore = crate::seller_memory::restore_snapshot(&snapshot);
    let outcome = outcome.map_err(|error| DaemonError::Agent(error.to_string()))?;
    restore.map_err(|error| DaemonError::Agent(format!("retro restore: {error}")))?;
    match outcome.terminal {
        JobExecutionStatus::Completed => Ok(()),
        other => Err(DaemonError::Agent(format!("retro terminal {other:?}"))),
    }
}

/// Handle one gift-wrap: unwrap (one site), then apply or buffer.
pub async fn ingest_gift_wrap(
    daemon: &mut SellerDaemon,
    event: &nostr_sdk::Event,
) -> Result<Option<ReceiptOutcome>, DaemonError> {
    let event_id = event.id.to_hex();
    // Log EVERY wrap the seller sees — silence must mean "no wraps", never "lost money".
    // The applied case logs a receipt at the caller; the not-ours / buffered cases log here.
    eprintln!("seller wrap seen event={event_id}");
    let Some(received) = unwrap_own_payment_gift_wrap(&daemon.keys, event).await? else {
        eprintln!("seller wrap event={event_id}: not a decodable own-payment wrap (skipped)");
        return Ok(None);
    };
    match try_apply_or_buffer(daemon, event_id, received).await? {
        ApplyResult::Applied(outcome) => Ok(Some(outcome)),
        ApplyResult::Buffered => Ok(None),
    }
}

enum ApplyResult {
    Applied(ReceiptOutcome),
    Buffered,
}

async fn try_apply_or_buffer(
    daemon: &mut SellerDaemon,
    event_id: String,
    received: ReceivedPayment,
) -> Result<ApplyResult, DaemonError> {
    let payload_job = received.payload.job_id().to_owned();
    // An already-receipted job's wrap is a re-see of consumed money (idempotent) — skip it,
    // do NOT buffer it forever. The journal pay-once guard is the source of truth.
    match daemon.journal.has_receipt(&payload_job) {
        Ok(true) => {
            eprintln!(
                "seller wrap event={event_id}: job {payload_job} already receipted, skipping (not buffered)"
            );
            return Ok(ApplyResult::Buffered);
        }
        Ok(false) => {}
        Err(error) => {
            // Fail closed: a journal read error must NOT read as "no receipt yet" (which would fall
            // through and redeem a job that may already be paid). Buffer the wrap and refuse to
            // receive until the journal reads clean again. Non-secret diagnostic only.
            eprintln!(
                "seller wrap event={event_id}: has_receipt journal read failed for job {payload_job} \
                 (buffering, fail-closed): {error}"
            );
            daemon.buffer_payment(event_id, received);
            return Ok(ApplyResult::Buffered);
        }
    }
    // Does a delivered-but-unpaid job bind this payment? Bind by job id (the NUT-18 payload's
    // `id`) — result id is resolved locally in `try_apply_payment` (NUT-18 carries no result id).
    let binds = daemon
        .awaiting_payment
        .iter()
        .any(|job| job.job_id.as_str() == payload_job);
    if !binds {
        // No delivered job matches yet. Before buffering, try to reconstruct the delivered-job
        // bind from ON-RELAY ground truth: a seat that delivered under an older build (or whose
        // journal is lost/incomplete) has no local `Delivery` entry for `restore_delivered_unpaid`
        // to rebuild, so the wrap would otherwise buffer forever (the exact recovery case the
        // backfill exists for). Reconstruction is fail-closed (see `reconstruct_delivered_bind`) —
        // it binds only a job whose result AND claim this seller itself authored, and hands the
        // job to the SAME redeem path with its full guards. A miss (early pay for a
        // still-processing job, an unverifiable wrap) leaves the wrap buffered as before;
        // misattribution is impossible — `try_apply_payment` only receives against an exact
        // job+result match.
        if !daemon.reconstruct_bind_from_relay(&received).await {
            eprintln!(
                "seller wrap event={event_id} buffered: no delivered job binds job_id={} yet",
                received.payload.job_id()
            );
            daemon.buffer_payment(event_id, received);
            return Ok(ApplyResult::Buffered);
        }
        // Bind restored from the relay — fall through to the normal apply path (full redeem guards).
    }
    match daemon.try_apply_payment(received).await? {
        Some(outcome) => Ok(ApplyResult::Applied(outcome)),
        None => {
            eprintln!("seller wrap event={event_id}: bound job not applied (re-buffered)");
            Ok(ApplyResult::Buffered)
        }
    }
}

/// Reconcile buffered payments after result publish (early-pay). Applies every buffered
/// wrap that now binds a delivered job; leaves the rest buffered. Returns the last receipt.
pub async fn reconcile_after_result(
    daemon: &mut SellerDaemon,
) -> Result<Option<ReceiptOutcome>, DaemonError> {
    // Drain into a snapshot; unmatched wraps are re-buffered into the (now empty) pay_buffer.
    let mut pending = std::mem::take(&mut daemon.pay_buffer);
    let mut done = None;
    while let Some(BufferedPay { event_id, received }) = pending.pop_front() {
        match try_apply_or_buffer(daemon, event_id, received).await? {
            ApplyResult::Applied(outcome) => done = Some(outcome),
            ApplyResult::Buffered => {}
        }
    }
    Ok(done)
}

/// Blocking wrapper for [`run_forever`] (current-thread tokio runtime).
pub fn run_forever_blocking(daemon: SellerDaemon) -> Result<(), DaemonError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| DaemonError::Config(format!("tokio runtime: {error}")))?;
    runtime.block_on(run_forever(daemon))
}

/// Env override for the boot push-preflight (see [`boot_preflight_enabled`]).
const BOOT_PUSH_PREFLIGHT_ENV: &str = "MOBEE_SELLER_BOOT_PUSH_PREFLIGHT";

/// Boot push-preflight gate: probe iff the `[seller_preflight]` knob is on AND the env override does
/// not disable it. An env value of `0/false/no/off` disables (the tests-off switch); the env can
/// never force the probe on when the config knob is off. Pure over (config, env) → unit-testable.
fn boot_preflight_enabled(config: &crate::home::MobeeConfig, env_override: Option<&str>) -> bool {
    if let Some(value) = env_override {
        let v = value.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "0" | "false" | "no" | "off") {
            return false;
        }
    }
    config.seller_preflight.boot_push_preflight
}

/// Format the boot-preflight outcome into ONE log line. NEVER returns an error — a failed probe is
/// advisory: the daemon names the git-version cause + fix and keeps running (reads/claims still work;
/// per-job deliveries fail-close per job as today). Pure over the probe result, so the probe seam is
/// mockable in tests without git or a relay.
fn boot_preflight_outcome(result: Result<(), crate::seller_git::SellerGitError>) -> String {
    match result {
        Ok(()) => "seller boot push preflight OK (write-auth path reachable)".to_owned(),
        Err(error) => format!(
            "seller WARN: boot push preflight FAILED ({error}). Most likely cause: git < 2.54 \
             silently drops the Authorization credential on the git-receive-pack POST (reads work, \
             pushes 401) — run `git version` and upgrade to 2.54+ (or run `mobee doctor`). \
             Continuing to run: reads and claims still work; deliveries will fail-close per job \
             until this is fixed."
        ),
    }
}

/// Run the boot push-preflight through a mockable probe seam. Returns the log line to emit, or
/// `None` when the preflight is disabled. Never blocks boot and never errors.
fn run_boot_push_preflight<P>(enabled: bool, probe: P) -> Option<String>
where
    P: FnOnce() -> Result<(), crate::seller_git::SellerGitError>,
{
    if !enabled {
        return None;
    }
    Some(boot_preflight_outcome(probe()))
}

/// Wire the real probe seam from a live daemon and emit the outcome to stderr. Only probes when a
/// relay-git canonical delivery repo is configured — public/BYO https deliveries push to the
/// seller's own remote, outside the relay NIP-98 write-auth surface this guards. Best-effort: a
/// missing seller key or config just skips the probe. Never blocks boot.
fn run_boot_push_preflight_for_daemon(daemon: &SellerDaemon) {
    let enabled = boot_preflight_enabled(
        &daemon.home.config,
        std::env::var(BOOT_PUSH_PREFLIGHT_ENV).ok().as_deref(),
    );
    let Some(seller) = daemon.home.config.seller.clone() else {
        return;
    };
    if !crate::delivery_transport::is_relay_git_locator(&seller.git_remote) {
        return;
    }
    let auth = crate::home::read_secret_key_hex(&daemon.home)
        .ok()
        .map(|secret_key_hex| crate::seller_git::PushAuth { secret_key_hex });
    let outcome = run_boot_push_preflight(enabled, || {
        crate::seller_git::preflight_push_probe(&seller.git_remote, auth.as_ref())
    });
    if let Some(line) = outcome {
        eprintln!("{line}");
    }
}

/// Outcome of [`wait_for_nip42_auth`].
pub(crate) enum AuthWait {
    /// The relay issued a NIP-42 challenge and `automatic_authentication` completed it.
    Authenticated,
    /// The relay issued NO challenge within the window. NOT a failure (see the fn doc).
    NoChallenge,
}

/// Drain `notifications` until the relay's NIP-42 AUTH completes, the relay actively rejects
/// auth (fatal), or the window elapses with no challenge (`NoChallenge`, non-fatal).
///
/// Shared by the seller receive path and the buyer receipt-publish path — mobee-relay requires
/// NIP-42 AUTH for the p-gated kind-1059 subscribe AND for all writes, and the handshake shape is
/// identical either way. Callers map the outcome to their own gate: the seller degrades on
/// `NoChallenge`; the buyer fails closed on anything but `Authenticated`.
///
/// Caller must subscribe `relay.notifications()` **before** `connect` so the
/// `Authenticated` event cannot be missed.
///
/// mobee-relay p-gates kind-1059: unauthenticated `REQ kinds:[1059] #p:self` is
/// `CLOSED` with `restricted:` (not `auth-required:`). nostr-sdk 0.44 treats
/// `restricted:` as `Remove` — the sub is dropped, so the post-auth
/// `resubscribe()` never restores it. Auth **before** the 1059 subscribe is
/// therefore load-bearing for seller receive, and mobee-relay challenges on connect so
/// `Authenticated` arrives in milliseconds.
///
/// A window with NO challenge is reported as `NoChallenge` rather than a fatal error: a relay
/// that challenges only lazily (on the first `REQ`/`EVENT`, e.g. the in-process test relay) will
/// challenge when the daemon subscribes below, and `automatic_authentication` completes auth
/// then. The caller logs the degrade loudly. An ACTIVE rejection (`AuthenticationFailed`) or a
/// relay shutdown stays fatal (fail-closed), unchanged.
pub(crate) async fn wait_for_nip42_auth(
    notifications: &mut tokio::sync::broadcast::Receiver<nostr_sdk::pool::RelayNotification>,
    timeout: std::time::Duration,
) -> Result<AuthWait, DaemonError> {
    use nostr_sdk::pool::RelayNotification;

    let within_window = tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayNotification::Authenticated) => return Ok(AuthWait::Authenticated),
                Ok(RelayNotification::AuthenticationFailed) => {
                    return Err(DaemonError::Relay(
                        "NIP-42 authentication failed (required for kind-1059 p-gated receive)"
                            .into(),
                    ));
                }
                Ok(RelayNotification::Shutdown) => {
                    return Err(DaemonError::Relay(
                        "relay shutdown before NIP-42 authentication".into(),
                    ));
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(DaemonError::Relay(
                        "relay notification channel closed before NIP-42 authentication".into(),
                    ));
                }
            }
        }
    })
    .await;

    // Elapsed with no challenge → NoChallenge (non-fatal). Within the window → the loop's result
    // (Authenticated, or a fatal active failure).
    within_window.unwrap_or(Ok(AuthWait::NoChallenge))
}

/// Cap on the number of STORED offers the UNTARGETED (open-pool) offer filter requests on
/// (re)subscribe when a backfill window is set — a flood guard on the INITIAL query only.
/// nostr 0.44 `Filter::limit` is "maximum number of events returned in the initial query"
/// (`nostr-0.44.4/src/filter.rs`): it bounds the stored-events burst, does NOT affect live
/// streaming, and is NOT part of `match_event`. So even a very large `offer_backfill_secs` can
/// pull at most this many stored open-pool offers (the rest arrive live). Distinct from
/// `limit(0)` (the live-only sentinel: request ZERO stored events).
const OFFER_BACKFILL_LIMIT: usize = 500;

/// Build the seller's offer-kind offer subscription filter(s).
///
/// Always includes the TARGETED filter (`#p` == seller pubkey) in its ORIGINAL shape — no
/// `since`, no `limit` — at ALL `backfill_secs` values: stored targeted offers are addressed to
/// this seller and have always backfilled in full on (re)subscribe (p-pinned + low-volume ⇒ no
/// firehose risk; bounding it would be a pure regression). Staleness
/// protection on this path is the classify-level deadline-expiry refusal, not a filter bound.
///
/// When `claim_open_pool` is set, ALSO returns an UNtargeted filter (no pubkey pin): open-pool
/// offers carry no `p` tag, so a pubkey-pinned filter alone never delivers them and
/// `--claim-open-pool` is DOA. A targeted offer matches BOTH filters (deduped by event id at
/// the call site); the downstream money-safety gates (`classify_offer` rate/expiry + the
/// backfill pre-claim check) still decide whether any backfilled offer is actually claimed.
///
/// The backfill window applies to the UNTARGETED filter ONLY — the field gap it fixes is an
/// open-pool offer posted before the daemon came online, invisible by design
/// under the live-only bound:
///  * `backfill_secs == 0` → **live-only** (byte-identical pre-backfill shape):
///    `since(subscribe_now)` + `limit(0)` (request ZERO stored events). Only open-pool offers
///    posted WHILE the daemon runs are delivered — no full-history offer firehose on startup.
///  * `backfill_secs > 0` → `since(subscribe_now - backfill_secs).limit(OFFER_BACKFILL_LIMIT)`:
///    stored open-pool offers within the window backfill (bounded burst); `limit(0)` is DROPPED
///    (it would suppress every stored event and defeat the window).
fn offer_subscription_filters(
    seller_pubkey: nostr_sdk::PublicKey,
    claim_open_pool: bool,
    subscribe_now: nostr_sdk::Timestamp,
    backfill_secs: u64,
) -> Vec<nostr_sdk::Filter> {
    use nostr_sdk::prelude::{Filter, Kind, Timestamp};

    // TARGETED (`#p == self`): ORIGINAL shape, untouched by the window knob. The `#t=mobee`
    // namespace guard is required so a foreign event squatting the offer kind is never even
    // delivered.
    let mut filters = vec![Filter::new()
        .kind(Kind::Custom(JOB_OFFER_KIND))
        .hashtag(gateway::MOBEE_TAG)
        .pubkey(seller_pubkey)];

    if claim_open_pool {
        // UNtargeted (open-pool): the backfill window applies HERE only. `since` anchor:
        // `now - backfill_secs` (saturating); backfill_secs == 0 ⇒ `since(now)`.
        let since = Timestamp::from(subscribe_now.as_secs().saturating_sub(backfill_secs));
        let untargeted = Filter::new()
            .kind(Kind::Custom(JOB_OFFER_KIND))
            .hashtag(gateway::MOBEE_TAG)
            .since(since);
        let untargeted = if backfill_secs > 0 {
            // Window requested: bounded stored-offer burst (drop the live-only `limit(0)`).
            untargeted.limit(OFFER_BACKFILL_LIMIT)
        } else {
            // Live-only: `limit(0)` requests ZERO stored events (byte-identical pre-backfill).
            untargeted.limit(0)
        };
        filters.push(untargeted);
    }
    filters
}

/// The seller's LIVE offer subscription(s), grouped as they are registered on the relay.
///
/// Each element is ONE long-lived subscription — a single NIP-01 `REQ` whose filters the relay
/// OR-matches. The offer filters are grouped into ONE subscription: the pinned (`#p` ==
/// self) filter AND — under `claim_open_pool` — the un-pinned open-pool filter ride the SAME
/// `REQ`. This grouping is load-bearing: registering the un-pinned filter as a SEPARATE second
/// subscription delivers stored events (backfill) but no LIVE offers — a running open-pool seller
/// would never react to a fresh untargeted offer, claiming it only after a restart re-fetched it
/// from stored events. Callers MUST subscribe each group as one `REQ`
/// (one `pool().subscribe(filters, ..)` call), never one subscription per filter.
fn offer_subscriptions(
    seller_pubkey: nostr_sdk::PublicKey,
    claim_open_pool: bool,
    subscribe_now: nostr_sdk::Timestamp,
    backfill_secs: u64,
) -> Vec<Vec<nostr_sdk::Filter>> {
    vec![offer_subscription_filters(
        seller_pubkey,
        claim_open_pool,
        subscribe_now,
        backfill_secs,
    )]
}

/// The seller's kind-1059 payment (gift-wrap) filter: p-tagged to the seller, **NO `t=mobee`
/// hashtag**. NIP-59 gift-wraps are opaque and CANNOT carry a namespace tag, so a hashtag filter
/// here would match zero wraps and silently strand real payments. This is the tag-free
/// invariant the regression test pins; it is used for BOTH the live subscription and the boot
/// backfill.
fn wrap_subscription_filter(seller_pubkey: nostr_sdk::PublicKey) -> nostr_sdk::Filter {
    nostr_sdk::Filter::new()
        .kind(nostr_sdk::Kind::GiftWrap)
        .pubkey(seller_pubkey)
}

/// The seller's kind-award AWARD filter: p-tagged to the seller and namespace-guarded with
/// `t=mobee` (awards carry the mobee hashtag), so the seller learns when a buyer selects a claim.
/// LIVE only — the pre-work award is meaningful only while the claim is still parked; a restart
/// releases parked claims via reconcile, so there is no award backfill to serve.
fn award_subscription_filter(seller_pubkey: nostr_sdk::PublicKey) -> nostr_sdk::Filter {
    nostr_sdk::Filter::new()
        .kind(nostr_sdk::prelude::Kind::Custom(JOB_AWARD_KIND))
        .hashtag(gateway::MOBEE_TAG)
        .pubkey(seller_pubkey)
}

/// The seller's OWN heartbeat subscription: kind-30340 authored by this seller, `d=mobee-seller`.
///
/// The relay-stall watchdog (#142) subscribes to this on the LIVE loop client so every heartbeat
/// the daemon publishes round-trips back through the SAME subscription that ingests awards and
/// payments. The round-trip is the in-process liveness probe of the RECEIVE path — the exact thing
/// the silent stall breaks — so its absence past the stall threshold means the subscription died
/// and the daemon must reconnect. Keyed by author + kind + `d` (never by event id): an addressable
/// event is superseded in place, but each republish still arrives as a fresh delivery on the sub.
fn self_heartbeat_subscription_filter(seller_pubkey: nostr_sdk::PublicKey) -> nostr_sdk::Filter {
    nostr_sdk::Filter::new()
        .kind(nostr_sdk::prelude::Kind::Custom(
            crate::heartbeat::SELLER_HEARTBEAT_KIND,
        ))
        .author(seller_pubkey)
        .identifier(crate::heartbeat::SELLER_HEARTBEAT_D)
}

/// Relay-stall watchdog threshold in seconds: `interval_secs * missed_intervals`, each clamped to
/// at least 1 so the product is always positive (the watchdog can never trip on the first tick).
/// Pure so the arithmetic is unit-testable without a live daemon.
fn stall_threshold_secs(interval_secs: u64, missed_intervals: u32) -> u64 {
    interval_secs
        .max(1)
        .saturating_mul(u64::from(missed_intervals.max(1)))
}

/// Whether the live subscription is presumed dead: no own heartbeat has round-tripped for at least
/// `threshold_secs`. Pure over an elapsed-seconds reading so the staleness decision is unit-testable
/// with a fake clock (the caller supplies `Instant::elapsed().as_secs()` in production).
fn subscription_stalled(elapsed_secs: u64, threshold_secs: u64) -> bool {
    elapsed_secs >= threshold_secs
}

/// Overlap margin (seconds) subtracted from the last-known-good heartbeat timestamp when computing
/// the `since` cursor for the post-stall resubscribe. The window backfills events published during
/// the stall; the idempotent handlers (in-loop offer dedup + durable journal `has_claim`, wrap
/// pay-once guard, award match against still-parked claims) absorb the overlap re-delivery.
const STALL_OVERLAP_MARGIN_SECS: u64 = 60;

/// Tear down the silently-dead relay connection and rebuild it: reconnect, re-run the NIP-42
/// handshake, and resubscribe ALL filters with `since = overlap` so awards/offers/payment-wraps
/// published during the stall are re-delivered and ingested. Returns the fresh offer subscription
/// id(s) so the caller can keep tracking the Loud-Closed fallback. On any reconnect/auth/subscribe
/// error it returns `Err` WITHOUT clearing the existing subscriptions, so the daemon stays
/// recoverable (the caller retries on the next heartbeat tick, and production auto-reconnect keeps
/// the old subs live if the relay returns on its own).
#[allow(clippy::too_many_arguments)]
async fn reconnect_and_resubscribe(
    client: &nostr_sdk::Client,
    relay: &nostr_sdk::prelude::Relay,
    seller_pubkey: nostr_sdk::PublicKey,
    claim_enabled: bool,
    claim_open_pool: bool,
    heartbeat_enabled: bool,
    overlap_since: nostr_sdk::Timestamp,
    auth_wait: Duration,
) -> Result<Vec<nostr_sdk::prelude::SubscriptionId>, DaemonError> {
    use nostr_sdk::prelude::{Filter, Kind, SubscribeOptions};

    // Fresh receiver BEFORE the reconnect so the `Authenticated` notification can't be missed.
    let mut relay_notifications = relay.notifications();
    // Drop the dead connection and dial again. `connect()` spawns a fresh connection task
    // regardless of the relay's auto-reconnect option, so this drives recovery even when the
    // silent half-open socket never tripped the SDK's own reconnection.
    client.disconnect().await;
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(20)).await;
    // Re-run NIP-42: the kind-1059 resubscribe below is p-gated on it (same constraint as boot).
    let _ = wait_for_nip42_auth(&mut relay_notifications, auth_wait).await?;
    // Replace the stale pool subscriptions wholesale — clear them only AFTER a successful
    // reconnect+auth so a failed recovery never leaves the daemon reconnected-but-deaf.
    client.unsubscribe_all().await;

    let mut offer_sub_ids = Vec::new();
    if claim_enabled {
        // Offers ride ONE grouped REQ (targeted + open-pool), each with `since=overlap` so an offer
        // posted during the stall backfills. On a RECONNECT (unlike boot) the targeted filter also
        // carries the window: we only need the stall gap, and the backfill pre-claim money-safety
        // check + journal `has_claim` still gate whether any backfilled offer is actually claimed.
        let mut offer_filters = vec![Filter::new()
            .kind(Kind::Custom(JOB_OFFER_KIND))
            .hashtag(gateway::MOBEE_TAG)
            .pubkey(seller_pubkey)
            .since(overlap_since)];
        if claim_open_pool {
            offer_filters.push(
                Filter::new()
                    .kind(Kind::Custom(JOB_OFFER_KIND))
                    .hashtag(gateway::MOBEE_TAG)
                    .since(overlap_since)
                    .limit(OFFER_BACKFILL_LIMIT),
            );
        }
        let output = client
            .pool()
            .subscribe(offer_filters, SubscribeOptions::default())
            .await
            .map_err(|error| DaemonError::Relay(format!("resubscribe offers: {error}")))?;
        offer_sub_ids.push(output.val);
        // Awards WITH `since=overlap`: the missed-award leg of #142 (no 3405 ingested for 60+ min).
        // An award re-delivered for a claim we already executed/released no longer matches an
        // outstanding parked claim, so `on_award_event` ignores it — the overlap is safe.
        client
            .subscribe(
                award_subscription_filter(seller_pubkey).since(overlap_since),
                None,
            )
            .await
            .map_err(|error| DaemonError::Relay(format!("resubscribe awards: {error}")))?;
    }
    // Payment wraps WITH `since=overlap` so a kind-1059 that landed during the stall re-delivers and
    // redeems through the idempotent `ingest_gift_wrap` path (pay-once journal guard absorbs re-see).
    client
        .subscribe(
            wrap_subscription_filter(seller_pubkey).since(overlap_since),
            None,
        )
        .await
        .map_err(|error| DaemonError::Relay(format!("resubscribe 1059: {error}")))?;
    // Own-heartbeat watchdog subscription, so round-trip detection resumes on the fresh sub.
    if heartbeat_enabled {
        client
            .subscribe(self_heartbeat_subscription_filter(seller_pubkey), None)
            .await
            .map_err(|error| {
                DaemonError::Relay(format!("resubscribe self-heartbeat: {error}"))
            })?;
    }
    Ok(offer_sub_ids)
}

/// Cap on the number of most-recently-seen offer ids retained for in-loop dedup.
///
/// The dedup set is DEFENSE-IN-DEPTH only (see [`offer_event_should_process`]): it collapses an
/// offer that matched >1 filter, or a reconnect re-delivery, into one `on_offer_event` call. It
/// is NOT the money-idempotency guard — that is the DURABLE journal `has_claim` check in
/// `classify_offer`, which skips any already-claimed offer regardless of this set (and which the
/// daemon relies on wholesale after any restart, when this set starts empty). So forgetting the
/// OLDEST ids is safe: it cannot enable a second claim, only a re-check against the journal.
/// Without a bound the set would grow by one `EventId` per offer for the seller's whole lifetime
/// (a slow leak); 10k recent ids is ample to dedup filter-overlap and reconnect re-delivery.
const SEEN_OFFERS_CAP: usize = 10_000;

/// Insertion-ordered bounded set of seen offer ids.
///
/// A `VecDeque` holds ids in insertion order (oldest at the front) for O(1) eviction; the
/// `HashSet` gives O(1) membership. On an insert that grows the set past [`SEEN_OFFERS_CAP`] the
/// oldest id is evicted from BOTH. A recently-seen id is still reported "already seen"; only ids
/// older than the last `SEEN_OFFERS_CAP` distinct ids are forgotten. The two structures stay in
/// lockstep: an id is pushed to `order` exactly when it is newly added to `set`, and eviction
/// pops the front of `order` and removes that same id from `set`, so `order` never holds a
/// duplicate and `order.len() == set.len()` always.
#[derive(Default)]
struct BoundedSeen {
    order: VecDeque<nostr_sdk::EventId>,
    set: std::collections::HashSet<nostr_sdk::EventId>,
}

impl BoundedSeen {
    /// Insert `id`, returning `true` iff it was NEW — matching the `HashSet::insert` bool
    /// semantic [`offer_event_should_process`] relies on. A currently-retained id returns
    /// `false` (already seen). When the insert grows the set past [`SEEN_OFFERS_CAP`], the
    /// oldest retained id is evicted from both structures.
    fn insert(&mut self, id: nostr_sdk::EventId) -> bool {
        if !self.set.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > SEEN_OFFERS_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.set.len()
    }
}

/// Whether a relay event in the seller loop should be handed to `on_offer_event`.
///
/// True iff the event is a offer-kind offer not seen before. Routing is by KIND ONLY: a
/// non-p-tagged (open-pool) offer routes exactly like a targeted one, so the notification path
/// never drops untargeted offers. The event-id dedup makes a targeted offer that matched more
/// than one offer filter (or a reconnect re-delivery) reach `on_offer_event` at most once. The
/// set is bounded ([`BoundedSeen`]); forgetting an OLD id is money-safe (durable `has_claim`).
fn offer_event_should_process(
    event_kind: u16,
    event_id: nostr_sdk::EventId,
    seen_offers: &mut BoundedSeen,
) -> bool {
    event_kind == JOB_OFFER_KIND && seen_offers.insert(event_id)
}

/// Optional hooks for the seller loop, kept crate-internal so the public entrypoint stays a
/// one-arg [`run_forever`]. Production uses [`RunHooks::default`]; the integration test supplies
/// a readiness sender and a short auth-wait to drive the loop deterministically.
#[derive(Default)]
pub(crate) struct RunHooks {
    /// Fires once, right after the daemon is online (subscribed + past the NIP-42 auth wait) —
    /// i.e. the point at which it will react to LIVE offers. The test owns the receiver and uses
    /// it to assert readiness instead of scraping stderr.
    pub ready: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// Override the NIP-42 auth wait. `None` = production default (20s). A challenge-on-connect
    /// relay authenticates in milliseconds regardless of this value.
    pub auth_wait: Option<std::time::Duration>,
    /// Fires after each PERIODIC wrap-backfill run with the number of stored kind-1059(s) the
    /// fetch returned. The integration test uses it to prove the periodic timer fires and re-runs
    /// the stored-wrap backfill (test seam, not a stderr scrape). `None` in production.
    pub wrap_backfill_done: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
    /// Fires with the job_id when a kind-award AWARD for THIS seller's claim promotes the job to
    /// execution. The award tests assert the AWARDED seller runs (vs a released/ignored one)
    /// without scraping stderr. `None` in production.
    pub awarded: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Fires with `(job_id, reason)` when a parked claim is released without executing — the buyer
    /// awarded another claim, or the offer deadline passed with no award. `None` in production.
    pub released: Option<tokio::sync::mpsc::UnboundedSender<(String, String)>>,
    /// Override the relay's auto-reconnect option (`None` = production default, `true`). The
    /// relay-stall integration test sets this `false` so a killed connection is NOT silently
    /// re-established by the SDK — reproducing the field's dead-subscription-with-no-SDK-recovery
    /// so the in-process watchdog is the ONLY path back to a live subscription.
    pub relay_reconnect: Option<bool>,
    /// Fires once each time the relay-stall watchdog completes a reconnect + resubscribe. The
    /// integration test awaits it to prove recovery fired without scraping stderr. `None` in
    /// production.
    pub stall_recovered: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

/// Long-running seller loop: NIP-42 AUTH, then subscribe offers+1059 from START.
pub async fn run_forever(daemon: SellerDaemon) -> Result<(), DaemonError> {
    run_forever_hooked(daemon, RunHooks::default()).await
}

/// What the event loop does when `notifications.recv()` returns an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecvControl {
    /// Stay alive and keep processing — a transient lag must NEVER permanently deafen the seller.
    Continue,
    /// The channel is closed (pool shut down) — end the loop.
    Stop,
}

/// Classify a broadcast `recv()` error. A `Lagged` (the loop fell behind while blocked in a long
/// agent turn) is RECOVERABLE: tokio drops the overflowed messages but keeps delivering new ones,
/// so the seller continues rather than going silently deaf to all further offers AND kind-1059
/// payments (the money-path regression: wraps parked, never collected). Only `Closed` stops it.
fn classify_recv_error(error: &tokio::sync::broadcast::error::RecvError) -> RecvControl {
    match error {
        tokio::sync::broadcast::error::RecvError::Lagged(_) => RecvControl::Continue,
        tokio::sync::broadcast::error::RecvError::Closed => RecvControl::Stop,
    }
}

/// Effective periodic wrap-backfill cadence (seconds): the [`WRAP_BACKFILL_INTERVAL_ENV`] test
/// seam wins over the [`WRAP_BACKFILL_INTERVAL_SECS`] constant; a `0` or unparseable value is
/// ignored (falls back to the constant). This is the extracted cadence DECISION function so the
/// timing is unit-testable without a live relay (no production config path sets the env).
fn resolve_wrap_backfill_interval_secs() -> u64 {
    match std::env::var(WRAP_BACKFILL_INTERVAL_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => secs,
            _ => WRAP_BACKFILL_INTERVAL_SECS,
        },
        Err(_) => WRAP_BACKFILL_INTERVAL_SECS,
    }
}

/// Effective award-sweep cadence (seconds): the [`AWARD_SWEEP_INTERVAL_ENV`] test seam wins over
/// the [`AWARD_SWEEP_INTERVAL_SECS`] constant; a `0` or unparseable value falls back to the constant.
/// Extracted so the timing is unit-testable (no production config path sets the env).
fn resolve_award_sweep_interval_secs() -> u64 {
    match std::env::var(AWARD_SWEEP_INTERVAL_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => secs,
            _ => AWARD_SWEEP_INTERVAL_SECS,
        },
        Err(_) => AWARD_SWEEP_INTERVAL_SECS,
    }
}

/// Compute the wrap-backfill `since` cursor from the two journal reads, or ABORT (finding Y).
///
/// A journal read ERROR (e.g. a corrupt journal line surfacing as `SellerError::Journal`) must NOT
/// default to since=0 — that silently turns a corrupt journal into a full-history backfill. Return
/// `Err` so the caller aborts this cycle and retries next tick. A missing/empty journal is folded to
/// `Ok(None)` inside the journal reads (legitimate first boot) and yields `Ok(0)` here. Clamps the
/// last-receipt anchor to the oldest unsettled delivery (minus the skew margin) so a receipt for a
/// newer job never advances the cursor past an older unsettled job's uncollected wrap. Pure so the
/// abort-on-error path is unit-testable without a live journal.
fn resolve_backfill_since(
    last_receipt: Result<Option<u64>, SellerError>,
    oldest_unsettled: Result<Option<u64>, SellerError>,
) -> Result<u64, SellerError> {
    let last_receipt = last_receipt?.unwrap_or(0);
    Ok(match oldest_unsettled? {
        Some(oldest) => last_receipt.min(oldest.saturating_sub(WRAP_BACKFILL_MARGIN_SECS)),
        None => last_receipt,
    })
}

/// Fetch stored kind-1059 payment gift-wraps p-tagged to this seller since the last journaled
/// receipt and run EACH through the SAME idempotent redeem path as the live subscription
/// ([`ingest_gift_wrap`] → `try_apply_or_buffer`: already-receipted wraps skip, already-redeemed
/// refuse at the mint, unverifiable buffer — all existing money guards, unchanged).
///
/// Shared by the BOOT backfill (recovery on restart) and the PERIODIC backfill (recovery
/// for a RUNNING daemon whose aged relay subscription silently stopped delivering wraps). The
/// `since` cursor and filter are identical on both paths; a fresh short-lived REQ (`fetch_events`)
/// is what recovers the aged-subscription case. Timeout-bounded + log-and-continue: a slow or
/// failing relay can NEVER wedge the caller. `source` is a log marker ("" boot, " (periodic)"
/// periodic); `authed_note` is an optional diagnostic suffix on the fetch-attempt line. Returns
/// the number of stored wraps the fetch returned (0 on error/timeout) so callers can surface it.
async fn run_wrap_backfill(
    daemon: &mut SellerDaemon,
    client: &nostr_sdk::Client,
    wrap_filter: nostr_sdk::Filter,
    source: &str,
    authed_note: &str,
) -> usize {
    // Cursor = the last receipt ts, BUT never later than the oldest delivered-but-unpaid job
    // (minus a skew margin). The global `last_receipt_ts` alone is wrong: a receipt for a NEWER
    // job would advance the cursor past an OLDER unsettled job, skipping its still-uncollected
    // payment wrap forever. Clamping to the oldest unsettled delivery keeps that wrap in range;
    // the per-job idempotency guards (`has_receipt` skip, mint already-redeemed refuse) make the
    // wider window safe to re-scan.
    // Finding Y: a journal ERROR computing the cursor must ABORT this cycle, never default to
    // since=0 — a corrupt journal silently becoming a full-history backfill. `resolve_backfill_since`
    // folds a missing/empty journal to 0 (legitimate first boot, surfaced as `Ok(None)` inside the
    // journal reads) but propagates a real read error so we bail and retry next cycle. `has_receipt`
    // still prevents an unsafe redeem, but fail-closed is correct here.
    let backfill_since = match resolve_backfill_since(
        daemon.journal.last_receipt_ts(),
        daemon.journal.oldest_unsettled_delivery_ts(),
    ) {
        Ok(since) => since,
        Err(error) => {
            eprintln!(
                "seller wrap backfill{source}: ABORT — journal cursor read failed \
                 (retrying next cycle, NOT defaulting to since=0): {error}"
            );
            return 0;
        }
    };
    // Log the ATTEMPT unconditionally, BEFORE the fetch — silence must never read as "no wraps".
    eprintln!(
        "seller wrap backfill{source}: fetching stored kind-1059(s) since ts={backfill_since}{authed_note}"
    );
    // Hard-cap the fetch so an auth-gated relay that never EOSEs cannot wedge the caller.
    match tokio::time::timeout(
        Duration::from_secs(15),
        client.fetch_events(
            wrap_filter.since(nostr_sdk::Timestamp::from(backfill_since)),
            Duration::from_secs(12),
        ),
    )
    .await
    {
        Ok(Ok(events)) => {
            let count = events.len();
            eprintln!(
                "seller wrap backfill{source}: {count} stored kind-1059(s) returned since ts={backfill_since}"
            );
            for event in events {
                match ingest_gift_wrap(daemon, &event).await {
                    Ok(Some(receipt)) => eprintln!(
                        "seller receipt (backfill{source}) job_id={} result_id={} amount_received={}",
                        receipt.job_id, receipt.result_id, receipt.amount_received
                    ),
                    Ok(None) => {}
                    Err(error) if is_idempotent_already_redeemed(&error) => eprintln!(
                        "seller pay: kind-1059 already redeemed (backfill{source} idempotent re-see): {error}"
                    ),
                    Err(error) => eprintln!("seller pay path (backfill{source}): {error}"),
                }
            }
            count
        }
        Ok(Err(error)) => {
            eprintln!(
                "seller WARN: wrap backfill{source} fetch failed (continuing; live 1059 subscription active): {error}"
            );
            0
        }
        Err(_) => {
            eprintln!(
                "seller WARN: wrap backfill{source} fetch timed out after 15s (continuing; live 1059 subscription active)"
            );
            0
        }
    }
}

/// [`run_forever`] with test/observability hooks (see [`RunHooks`]).
pub(crate) async fn run_forever_hooked(
    mut daemon: SellerDaemon,
    hooks: RunHooks,
) -> Result<(), DaemonError> {
    use std::time::Duration;
    use nostr_sdk::prelude::{
        Client, Kind, RelayMessage, RelayPoolNotification, RelayUrl, SubscribeOptions,
        SubscriptionId, Timestamp,
    };

    let client = Client::new(daemon.keys.clone());
    // Default is true; set explicitly — seller receive depends on it.
    client.automatic_authentication(true);

    let relay_url_str = daemon.home.config.relay_url.clone();
    // Add via the pool so the relay-stall watchdog test can disable SDK auto-reconnect; production
    // keeps the default (true) so a genuine TCP drop is still re-established by the SDK, with the
    // watchdog as the backstop for the SILENT half-open stall the SDK cannot see (#142).
    let relay_reconnect = hooks.relay_reconnect.unwrap_or(true);
    client
        .pool()
        .add_relay(
            &relay_url_str,
            nostr_sdk::prelude::RelayOptions::default().reconnect(relay_reconnect),
        )
        .await
        .map_err(|error| DaemonError::Relay(format!("add relay: {error}")))?;

    let relay_url = RelayUrl::parse(&relay_url_str)
        .map_err(|error| DaemonError::Relay(format!("parse relay url: {error}")))?;
    let relay = client
        .relays()
        .await
        .get(&relay_url)
        .cloned()
        .ok_or_else(|| DaemonError::Relay("relay missing after add_relay".into()))?;

    // MUST subscribe before connect — Authenticated is not re-emitted.
    let mut relay_notifications = relay.notifications();
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(20)).await;
    let auth_wait = hooks.auth_wait.unwrap_or(Duration::from_secs(20));
    let nip42_label = match wait_for_nip42_auth(&mut relay_notifications, auth_wait).await? {
        AuthWait::Authenticated => "authenticated",
        AuthWait::NoChallenge => {
            eprintln!(
                "seller WARN: relay issued no NIP-42 AUTH challenge within {auth_wait:?}; \
                 proceeding (automatic_authentication stays ON, so a relay that challenges on a \
                 REQ is still authenticated). If this relay p-gates kind-1059, seller receive may \
                 be degraded until auth completes."
            );
            "no-challenge"
        }
    };

    // Fail-closed journal gate: the seller journal is the durable record of prior active/delivered
    // state. If it is unreadable/corrupt at startup, reconciliation below recovers NOTHING, so
    // claiming new work would take on fresh liability while blind to prior in-flight jobs (their
    // incoming payments would buffer or depend on relay reconstruction). Least-disruptive correct
    // option: enter a receive-only / NO-CLAIM degraded mode — keep the wrap subscription + backfill
    // so recoverable pending payments can still settle, but do NOT subscribe to offers (no new
    // claims) until an operator repairs the journal and restarts. A single explicit read here is the
    // clean seam: the pay-once guard (`journal.has_receipt`) reads the same journal, so redeem is
    // already fail-closed on corruption; this ensures the claim path is too.
    let claim_enabled = daemon.claim_allowed_after_journal_check();
    if !claim_enabled {
        eprintln!(
            "seller FAIL-CLOSED: journal unreadable at startup; entering RECEIVE-ONLY degraded \
             mode — will NOT claim new offers until the journal reads clean. Repair the journal \
             and restart to resume claiming."
        );
    }

    // Restart-reconcile: release any orphaned in-flight claims from a prior run BEFORE
    // serving new offers, so a claim left live by a crash never reads "processing" forever.
    // Durable via journal; feedback-kind surface is best-effort.
    match daemon.reconcile_on_startup(now_unix()).await {
        Ok(plan) if !plan.is_empty() => {
            eprintln!(
                "seller reconcile: released {} orphaned claim(s) on startup",
                plan.len()
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("seller reconcile failed on startup (continuing): {error}"),
    }

    // Rebuild delivered-but-unpaid jobs into awaiting_payment BEFORE the wrap subscribe +
    // backfill, so a stored/buffered payment wrap can bind and redeem on this boot (the missing leg
    // between "wrap seen" and "collect ok"). Non-fatal on error.
    match daemon.restore_delivered_unpaid() {
        Ok(n) if n > 0 => {
            eprintln!("seller reconcile: restored {n} delivered-but-unpaid job(s) into awaiting_payment")
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("seller WARN: failed to restore delivered-but-unpaid jobs (continuing): {error}")
        }
    }

    // Offer subscription: the TARGETED filter (p-tag == seller) AND — under open-pool — the
    // BOUNDED un-pinned filter ride ONE long-lived subscription (a single REQ, OR-matched per
    // NIP-01) via `offer_subscriptions` + `pool().subscribe`. Registered as a SEPARATE second
    // subscription, the un-pinned filter would deliver stored events (backfill) but never LIVE
    // offers, so a running open-pool seller would ignore fresh untargeted offers. Group
    // them into ONE REQ (`Client::subscribe` takes a single filter — one REQ per filter is the
    // bug). The event-id dedup in the loop still processes each offer exactly once. Sub id(s) are
    // captured so the Loud-Closed fallback can detect a relay CLOSE of the offer subscription.
    let seller_pubkey = daemon.keys.public_key();
    // Create the notifications receiver BEFORE any REQ (offer subscribe, wrap subscribe, wrap
    // backfill). A tokio broadcast only delivers to receivers that already exist, so a stored event
    // returned by a REQ before this receiver is created would be dropped. This latent race widened
    // once the boot backfill added a network round-trip between the offer subscribe and the
    // loop — capture the receiver up front so backfilled offers/wraps are never missed.
    let mut notifications = client.notifications();
    let (claim_open_pool, offer_backfill_secs) = require_seller_config(&daemon.home)
        .map(|cfg| (cfg.claim_open_pool, cfg.offer_backfill_secs))
        .unwrap_or((false, 0));
    // Single subscribe anchor, shared by the UNTARGETED filter's window (`since(now - window)`;
    // the targeted filter carries no time bound) and the backfill discriminator below
    // (`created_at < daemon_start_unix` ⇒ posted before we came online ⇒ subject to the
    // pre-claim money-safety check, on BOTH paths — targeted-history backfill included).
    // Captured once so the filter bound and the discriminator agree.
    let subscribe_now = Timestamp::now();
    let daemon_start_unix = subscribe_now.as_secs();
    let mut offer_sub_ids: Vec<SubscriptionId> = Vec::new();
    // Heartbeat + relay-stall watchdog config, resolved BEFORE the subscription section so the
    // self-heartbeat watchdog subscription can be registered alongside the offer/award/wrap subs.
    // Env overrides config for tests. The watchdog rides the heartbeat mechanism (own heartbeats
    // round-tripping on the live sub), so it is inert when heartbeats are disabled.
    let heartbeat_enabled =
        crate::heartbeat::resolve_enabled(&daemon.home.config.seller_heartbeat);
    let heartbeat_interval_secs =
        crate::heartbeat::resolve_interval_secs(&daemon.home.config.seller_heartbeat);
    let stall_missed_intervals =
        crate::heartbeat::resolve_stall_missed_intervals(&daemon.home.config.seller_heartbeat);
    let stall_threshold = stall_threshold_secs(heartbeat_interval_secs, stall_missed_intervals);
    // Receive-only degraded mode (unreadable journal): skip the offer subscription entirely so the
    // daemon claims NO new work, while the wrap subscription + backfill below still run to settle
    // recoverable pending payments.
    if claim_enabled {
        for filters in offer_subscriptions(
            seller_pubkey,
            claim_open_pool,
            subscribe_now,
            offer_backfill_secs,
        ) {
            let output = client
                .pool()
                .subscribe(filters, SubscribeOptions::default())
                .await
                .map_err(|error| DaemonError::Relay(format!("subscribe offers: {error}")))?;
            offer_sub_ids.push(output.val);
        }
        // Award subscription rides alongside offers (same claim-enabled gate): a seller that
        // claims must hear the buyer's award to know when to execute. Skipped in receive-only mode
        // (no claims ⇒ no awards to await).
        client
            .subscribe(award_subscription_filter(seller_pubkey), None)
            .await
            .map_err(|error| DaemonError::Relay(format!("subscribe awards: {error}")))?;
    } else {
        eprintln!(
            "seller receive-only: offer subscription SKIPPED (journal unreadable) — no new claims \
             this run"
        );
    }
    let wrap_filter = wrap_subscription_filter(seller_pubkey);
    client
        .subscribe(wrap_filter.clone(), None)
        .await
        .map_err(|error| DaemonError::Relay(format!("subscribe 1059: {error}")))?;

    // Relay-stall watchdog subscription (#142): subscribe to our OWN heartbeat so each published
    // heartbeat round-trips back on this live subscription. If none round-trips within the stall
    // threshold the receive path is presumed dead and the heartbeat-tick watchdog below reconnects.
    // Only meaningful when heartbeats are enabled (the round-trip source); otherwise inert.
    if heartbeat_enabled {
        client
            .subscribe(self_heartbeat_subscription_filter(seller_pubkey), None)
            .await
            .map_err(|error| {
                DaemonError::Relay(format!("subscribe self-heartbeat (watchdog): {error}"))
            })?;
    }

    // Status line: never echo secrets.
    eprintln!(
        "seller daemon online pubkey={} relay={} mint={} nip42={nip42_label}",
        daemon.seller_pubkey(),
        daemon.home.config.relay_url,
        daemon.home.config.default_mint()
    );
    // ONLINE announce: subscribed + past the NIP-42 auth wait ⇒ reacting to live offers.
    daemon.announce(crate::announce::AnnounceEvent::online(
        now_unix(),
        daemon.seller_pubkey(),
        &daemon.home.config.relay_url,
        daemon.home.config.default_mint(),
        nip42_label,
    ));
    // Readiness hook: online + subscribed ⇒ ready to react to LIVE offers.
    if let Some(ready) = &hooks.ready {
        let _ = ready.send(());
    }

    // Boot backfill (recovery) — runs AFTER online/readiness so the daemon reports up promptly
    // and the backfill can never hide behind a hang. kind-1059 is auth-gated on mobee-relay (dark
    // kind): a REQ sent before NIP-42 completes is CLOSED `restricted:` and dropped, so the stored
    // wrap is never served (the live-acceptance failure). Confirm auth FIRST, then fetch stored
    // wraps p-tagged to us since the last journaled receipt and run each through the SAME redeem
    // path — idempotent via the journal pay-once guard, so it can never double-spend. A live offer
    // posted during this window is buffered in `notifications` and drained when the loop starts.
    let backfill_authed = nip42_label == "authenticated"
        || matches!(
            // If the connect-time challenge already authenticated us this returns immediately; if
            // the relay defers to the REQ, the subscribes above triggered the challenge and this
            // catches the completion. Short + non-fatal so it never wedges boot.
            wait_for_nip42_auth(&mut relay_notifications, Duration::from_secs(3)).await,
            Ok(AuthWait::Authenticated)
        );
    // Boot recovery: run the shared stored-wrap backfill once (source marker "", auth diagnostic
    // suffix). The periodic timer in the loop re-runs this SAME helper on a cadence so a running
    // daemon whose subscription later ages out still recovers without a restart.
    let _ = run_wrap_backfill(
        &mut daemon,
        &client,
        wrap_filter,
        "",
        &format!(" (nip42_authed={backfill_authed})"),
    )
    .await;

    // Boot push preflight: surface WRITE-auth/git-version breakage now, not at job-delivery time.
    // Advisory only — logs-and-continues; never refuses boot (see run_boot_push_preflight).
    run_boot_push_preflight_for_daemon(&daemon);

    // NIP-34 delivery-repo announce (kind-30617): advertise the seller's canonical relay-git
    // delivery remote so buyers/clients can discover it before any push. Parameterized-replaceable
    // (idempotent across launches) and best-effort — logs and continues; a refused/unreachable
    // relay never blocks boot. Only relay-git remotes carry a derivable NIP-34 repo id (BYO https
    // sellers push to their own remote, outside this discovery surface), so gate on that.
    if let Some(seller_cfg) = daemon.home.config.seller.clone() {
        if crate::delivery_transport::is_relay_git_locator(&seller_cfg.git_remote) {
            match crate::profile::announce_seller_delivery_repo_async(
                &daemon.home,
                &seller_cfg.git_remote,
            )
            .await
            {
                Ok(event_id) => {
                    eprintln!("seller NIP-34 delivery-repo announce ok event={event_id}")
                }
                Err(error) => eprintln!(
                    "seller WARN: NIP-34 delivery-repo announce failed (continuing): {error}"
                ),
            }
        }
    }

    // Both offer filters ride ONE subscription, so the relay delivers each offer once even when
    // a targeted offer matches both filters. Keep an event-id dedup as defense-in-depth (e.g.
    // reconnect re-delivery) so each offer id reaches `on_offer_event` at most once. Bounded to
    // the most-recent `SEEN_OFFERS_CAP` ids so it can't leak over the seller's lifetime; an
    // evicted-then-re-delivered claimed offer is still caught by the durable journal `has_claim`.
    let mut seen_offers = BoundedSeen::default();
    // Loud-Closed fallback fires at most once (targeted-only is not itself broad-filtered).
    let mut offer_fallback_done = false;
    // `notifications` was created up front (before the REQs) so no backfilled event is dropped.

    // Heartbeat cadence (resolved above). `interval()`'s first `tick()` completes immediately, so
    // an enabled seller advertises liveness right after going online, then every `interval_secs`.
    let mut heartbeat_interval =
        tokio::time::interval(Duration::from_secs(heartbeat_interval_secs.max(1)));
    if heartbeat_enabled {
        eprintln!(
            "seller heartbeat enabled: kind-30340 d=mobee-seller every {heartbeat_interval_secs}s"
        );
    }
    // Relay-stall watchdog state (#142): the wall-clock + monotonic instant of the last own
    // heartbeat that round-tripped on the live subscription. Seeded to "now" at online so a healthy
    // seller never trips before its first heartbeat round-trips; the monotonic `Instant` drives the
    // staleness measure (robust to wall-clock jumps) and the unix stamp drives the resubscribe
    // `since` cursor. The watchdog check rides the heartbeat tick (below) so it needs no extra timer.
    let mut last_self_heartbeat_seen = tokio::time::Instant::now();
    let mut last_self_heartbeat_seen_unix = now_unix();
    if heartbeat_enabled {
        eprintln!(
            "seller relay-stall watchdog enabled: reconnect if no own heartbeat round-trips within \
             {stall_threshold}s ({stall_missed_intervals} missed intervals)"
        );
    }

    // Periodic wrap backfill: re-run the boot stored-wrap backfill every N seconds so a
    // running daemon whose relay subscription has aged out (silently stopped delivering kind-1059
    // wraps) recovers WITHOUT a restart. `interval_at` (not `interval`) starts one period out, so
    // the first periodic run does NOT double the boot backfill we just ran. It rides the SAME
    // select loop as the heartbeat (never a blocking side-thread) and each run is timeout-bounded
    // + log-and-continue, so it can never wedge or crash offer/payment handling.
    let wrap_backfill_interval_secs = resolve_wrap_backfill_interval_secs();
    let mut wrap_backfill_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(wrap_backfill_interval_secs),
        Duration::from_secs(wrap_backfill_interval_secs),
    );
    eprintln!(
        "seller wrap backfill (periodic) enabled: re-fetch stored kind-1059(s) every {wrap_backfill_interval_secs}s"
    );

    // Award sweep: release parked claims whose offer deadline passed with no award. Rides the same
    // select loop (never a blocking side-thread); `interval_at` starts one period out.
    let award_sweep_interval_secs = resolve_award_sweep_interval_secs();
    let mut award_sweep_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(award_sweep_interval_secs),
        Duration::from_secs(award_sweep_interval_secs),
    );
    eprintln!(
        "seller award sweep enabled: release claims past their offer deadline with no award every {award_sweep_interval_secs}s"
    );

    loop {
        // Resilient recv: a broadcast LAG (the loop fell behind while blocked in a long agent
        // turn) must NOT permanently deafen the daemon. tokio's broadcast drops the overflowed
        // messages and keeps delivering NEW ones, so we LOG and CONTINUE; only a genuinely closed
        // channel ends the loop. Before this a `Lagged` ended `while let Ok(..)` — the seller went
        // silently deaf to all further offers AND kind-1059 payments (wraps parked, never
        // collected) until restart. Missed stored events re-backfill on resubscribe/restart.
        let notification = tokio::select! {
            // The heartbeat tick rides the SAME loop (never a blocking side-thread): publishing is
            // timeout-bounded + log-and-continue, so it can NEVER wedge or crash offer/payment
            // handling. Disabled ⇒ the branch is inert and select only waits on the relay stream.
            _ = heartbeat_interval.tick(), if heartbeat_enabled => {
                // Relay-stall watchdog (#142): the daemon subscribes to its OWN heartbeat, so each
                // published heartbeat round-trips back on this live subscription. If none has for at
                // least the stall threshold the subscription died silently — reconnect + resubscribe
                // with a `since` overlap so awards/offers/wraps published during the stall ingest.
                // Checked BEFORE publishing (evaluating whether prior heartbeats round-tripped) so it
                // rides the existing timer with no extra select branch.
                let stall_elapsed = last_self_heartbeat_seen.elapsed().as_secs();
                if subscription_stalled(stall_elapsed, stall_threshold) {
                    let overlap_since = Timestamp::from(
                        last_self_heartbeat_seen_unix.saturating_sub(STALL_OVERLAP_MARGIN_SECS),
                    );
                    eprintln!(
                        "seller RELAY-STALL detected: no own heartbeat round-tripped in {stall_elapsed}s \
                         (threshold {stall_threshold}s); subscription presumed dead — reconnecting + \
                         resubscribing with since={} overlap",
                        overlap_since.as_secs()
                    );
                    match reconnect_and_resubscribe(
                        &client,
                        &relay,
                        seller_pubkey,
                        claim_enabled,
                        claim_open_pool,
                        heartbeat_enabled,
                        overlap_since,
                        auth_wait,
                    )
                    .await
                    {
                        Ok(new_ids) => {
                            offer_sub_ids = new_ids;
                            // Fresh subscription ⇒ the Loud-Closed fallback may fire again.
                            offer_fallback_done = false;
                            // Grace: reset the watchdog clock so it does not immediately re-fire
                            // before the first post-reconnect heartbeat round-trips.
                            last_self_heartbeat_seen = tokio::time::Instant::now();
                            last_self_heartbeat_seen_unix = now_unix();
                            eprintln!(
                                "seller RELAY-STALL recovered: reconnected + resubscribed \
                                 (offers+awards+1059+heartbeat, since={} overlap)",
                                overlap_since.as_secs()
                            );
                            if let Some(tx) = &hooks.stall_recovered {
                                let _ = tx.send(());
                            }
                        }
                        Err(error) => {
                            // Leave the watchdog clock untouched so the next heartbeat tick retries.
                            eprintln!(
                                "seller RELAY-STALL recovery FAILED (will retry next heartbeat tick): {error}"
                            );
                        }
                    }
                }
                publish_heartbeat(&daemon).await;
                continue;
            }
            // Periodic wrap backfill tick. Same idempotent redeem path as boot; a fresh REQ
            // recovers an aged, silently-deaf subscription. Timeout-bounded inside the helper.
            _ = wrap_backfill_interval.tick() => {
                let count = run_wrap_backfill(
                    &mut daemon,
                    &client,
                    wrap_subscription_filter(seller_pubkey),
                    " (periodic)",
                    "",
                )
                .await;
                if let Some(tx) = &hooks.wrap_backfill_done {
                    let _ = tx.send(count);
                }
                continue;
            }
            // Award sweep tick: release parked claims whose offer deadline passed with no award.
            _ = award_sweep_interval.tick() => {
                for (job_id, reason) in daemon.release_expired_awaiting_award(now_unix()).await {
                    if let Some(tx) = &hooks.released {
                        let _ = tx.send((job_id, reason));
                    }
                }
                continue;
            }
            recv = notifications.recv() => match recv {
                Ok(notification) => notification,
                Err(error) => match classify_recv_error(&error) {
                    RecvControl::Continue => {
                        eprintln!(
                            "seller WARN: notification stream {error}; continuing (NOT going deaf). \
                             Missed stored offers/payments re-backfill on resubscribe."
                        );
                        continue;
                    }
                    RecvControl::Stop => break,
                },
            },
        };
        match notification {
            RelayPoolNotification::Event { event, .. } => {
                // Own-heartbeat round-trip (relay-stall watchdog, #142): our own kind-30340 event
                // coming back on this subscription proves the RECEIVE path is alive. Refresh the
                // liveness clocks and drop it — heartbeats are diagnostic and never route to offer,
                // award, or payment handling.
                if event.kind.as_u16() == crate::heartbeat::SELLER_HEARTBEAT_KIND
                    && event.pubkey == seller_pubkey
                {
                    last_self_heartbeat_seen = tokio::time::Instant::now();
                    last_self_heartbeat_seen_unix = now_unix();
                    continue;
                }
                if event.kind == Kind::GiftWrap {
                    match ingest_gift_wrap(&mut daemon, &event).await {
                        Ok(Some(receipt)) => {
                            eprintln!(
                                "seller receipt job_id={} result_id={} amount_received={}",
                                receipt.job_id, receipt.result_id, receipt.amount_received
                            );
                            // Delivered-PAID ⇒ best-effort retro. Detached (own thread);
                            // returns immediately so wrap collection is never blocked.
                            daemon.maybe_run_retro(&receipt.job_id);
                        }
                        Ok(None) => {}
                        // Idempotent re-see (info, not error): the sats already landed.
                        Err(error) if is_idempotent_already_redeemed(&error) => eprintln!(
                            "seller pay: kind-1059 already redeemed (idempotent re-see, no action): {error}"
                        ),
                        Err(error) => eprintln!("seller pay path: {error}"),
                    }
                    continue;
                }
                // A kind-award AWARD (buyer selection) decides whether a parked claim executes or
                // releases. Awards are p-tagged to us + mobee-hashtagged, so they never reach the
                // offer routing below. Execution lives HERE now — a claim runs ONLY once awarded.
                if event.kind.as_u16() == JOB_AWARD_KIND {
                    match daemon.on_award_event(&event).await {
                        AwardOutcome::Execute { job_id } => {
                            if let Some(tx) = &hooks.awarded {
                                let _ = tx.send(job_id);
                            }
                            match daemon.execute_active_job().await {
                                Ok(result_id) => {
                                    eprintln!("seller published result_id={result_id}");
                                    // DELIVERED: free the single-flight slot so new offers can
                                    // be claimed while this job awaits payment.
                                    daemon.mark_delivered();
                                    match reconcile_after_result(&mut daemon).await {
                                        Ok(Some(receipt)) => {
                                            eprintln!(
                                                "seller receipt (reconcile) job_id={} amount_received={}",
                                                receipt.job_id, receipt.amount_received
                                            );
                                            // Delivered-PAID ⇒ detached best-effort retro.
                                            daemon.maybe_run_retro(&receipt.job_id);
                                        }
                                        Ok(None) => {}
                                        // Idempotent re-see (info, not error): sats already landed.
                                        Err(error) if is_idempotent_already_redeemed(&error) => eprintln!(
                                            "seller reconcile: kind-1059 already redeemed (idempotent re-see, no action): {error}"
                                        ),
                                        Err(error) => eprintln!("seller reconcile: {error}"),
                                    }
                                }
                                Err(error) => eprintln!("seller job failed: {error}"),
                            }
                        }
                        AwardOutcome::Released { job_id, reason } => {
                            if let Some(tx) = &hooks.released {
                                let _ = tx.send((job_id, reason));
                            }
                        }
                        AwardOutcome::Ignored => {}
                    }
                    continue;
                }
                // An offer (offer-kind) routes to `on_offer_event` REGARDLESS of p-tag — open-pool
                // offers carry none, so a p-tag gate here would silently drop them. Deduped by
                // event id so each offer is processed once (see `offer_event_should_process`).
                if offer_event_should_process(event.kind.as_u16(), event.id, &mut seen_offers) {
                    // BACKFILL money-safety pre-claim check (guards #c/#d), for offers posted
                    // BEFORE we came online (`created_at < daemon_start_unix`). A live offer
                    // (posted while we run) keeps the byte-identical fast path — no relay query.
                    // This gates on backfilled-vs-live, NOT on window size, so it holds for ANY
                    // `offer_backfill_secs`. Fail-closed: an inconclusive relay read SKIPS. Every
                    // outcome is logged with a reason (never a silent drop).
                    if event.created_at.as_secs() < daemon_start_unix {
                        if let Some(reason) = daemon.backfill_offer_blocked(&event).await {
                            eprintln!("seller skip offer {}: {reason}", event.id.to_hex());
                            continue;
                        }
                    }
                    // Publish + park the claim; execution waits for the buyer's award above.
                    match daemon.on_offer_event(&event).await {
                        Ok(Some(job_id)) => {
                            eprintln!("seller awaiting award job_id={job_id}")
                        }
                        Ok(None) => {}
                        Err(error) => eprintln!("seller offer path: {error}"),
                    }
                }
            }
            // Loud-Closed fallback: the relay CLOSED a subscription. If it's the
            // grouped OFFER subscription (e.g. the relay restricts the broad open-pool filter),
            // the seller would go SILENTLY deaf to offers. LOG IT LOUDLY and degrade to the
            // TARGETED-only offer filter (`#p==self`) — which a relay that only objects to the
            // broad filter still accepts — so the seller stays targeted-alive with a visible log
            // instead of dying quietly. nostr-sdk removes a CLOSED (error/restricted/blocked) sub
            // and does not re-subscribe it, so this re-subscribe is the recovery path.
            RelayPoolNotification::Message {
                message: RelayMessage::Closed { subscription_id, message },
                ..
            } => {
                let is_offer_sub = offer_sub_ids.iter().any(|id| id == subscription_id.as_ref());
                if is_offer_sub && !offer_fallback_done {
                    offer_fallback_done = true;
                    eprintln!(
                        "seller WARN: relay CLOSED the offer subscription (sub_id={}): \"{}\". \
                         Falling back to the TARGETED-only offer filter (#p==self); open-pool \
                         (untargeted) offers will NOT be received until the relay accepts the \
                         grouped subscription again.",
                        subscription_id.as_ref(),
                        message
                    );
                    // Re-subscribe targeted-only. The targeted filter carries no time bound
                    // (original full-backfill shape at all window values), so stored targeted
                    // offers — including any posted while the grouped subscription was down —
                    // are re-delivered; the window/anchor args are passed for signature
                    // uniformity and unused on this path (`claim_open_pool = false`).
                    match client
                        .pool()
                        .subscribe(
                            offer_subscription_filters(
                                seller_pubkey,
                                false,
                                Timestamp::now(),
                                offer_backfill_secs,
                            ),
                            SubscribeOptions::default(),
                        )
                        .await
                    {
                        Ok(output) => offer_sub_ids.push(output.val),
                        Err(error) => eprintln!(
                            "seller ERROR: targeted-only fallback subscribe failed: {error}"
                        ),
                    }
                }
            }
            RelayPoolNotification::Shutdown => break,
            _ => {}
        }
    }
    Ok(())
}

/// Serialises tests that touch the process-global single-flight lock [`FLIGHT`]: the local-relay
/// daemon integration tests (which claim offers, taking `FLIGHT`) and `single_flight_mutex`.
/// Without this they race on `FLIGHT` when cargo runs tests concurrently. Poison-tolerant at the
/// lock sites (a panicking test must not wedge the others).
#[cfg(test)]
static FLIGHT_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests;

#[cfg(test)]
mod local_relay_it;
