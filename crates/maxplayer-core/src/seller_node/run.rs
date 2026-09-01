//! The seller node's live run loop — the durable node's relay surface.
//!
//! Boot opens the durable [`SellerNode`] (exclusive home lock + store + wallet/signer actors),
//! reconciles durable state, then connects ONE authenticated relay client (NIP-42) that both
//! ingests marketplace events and — via the shared [`RelayPublisher`] — drains the outbox. The loop
//! routes each event to the store: offers to consider, awards that bind a claim, gift-wraps that
//! settle a delivery.
//!
//! SCAFFOLD STATUS: boot + the drain/dispatch skeleton are wired. The offer→claim, award→execute,
//! and gift-wrap→pay arms + the #150 relay-stall watchdog + #162 recovery-retry are ported on top of
//! this in the following cutover steps (marked `PORT` below); `maxplayer seller` is NOT yet pointed here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Every operator-facing line in this file goes through these: `opline!` stamps and prints,
// `opline_verbose!` does the same only under MAXPLAYER_VERBOSE. Neither is `eprintln!` — an
// unstamped line here would be the one line an operator cannot place in time (#489).
use crate::{opline, opline_verbose};

use nostr_sdk::prelude::{
    Client, EventId, Filter, Keys, Kind, Output, RelayOptions, RelayPoolNotification, RelayUrl,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::contribution::{
    contribution_serve_gate, parse_contribution_offer, ContributionOffer, ContributionServeGate,
};
use crate::env_provision::{
    self, EffectOutput, EnvBackend, EnvEffects, EnvPosture, EnvProvisionError, HostEnvRunner,
    ProvisionRefusal,
};
use crate::gateway::{
    self, claim_draft, error_draft, git_result_draft, parse_award, parse_offer, OfferParseError,
    ParsedOffer, ReasonCode,
};
use crate::home::{self, MaxplayerHome};
use crate::job_lifecycle::{event_to_draft, job_hash_for_offer};
use crate::kinds::{
    JOB_ACCEPT_KIND, JOB_AWARD_KIND, JOB_OFFER_KIND, JOB_RECEIPT_KIND, JOB_REJECT_KIND,
    JOB_RESULT_KIND,
};
use crate::receipt::{ReceiptPreimage, EXEC_METADATA_COMMITMENT_EMPTY};
use crate::relay_auth::{self, AuthWait};
use crate::seller::rate_gate_allows;
use crate::seller_agents::AgentRegistry;
use crate::seller_exec::{
    compose_agent_prompt, delivery_message, job_workdir, run_agent_job, run_agent_with_retry,
    seller_delivery_kind, seller_exec_metadata, unified_job_timeout, AgentRunTimeout, ExecError,
    SandboxPolicy,
};
use crate::seller_roster::{ExecutionFailure, Fault, LiveRoster, MissingCapability, Unavailable};
use crate::seller_git::{self, DeliveryAgentIdentity};

use super::outbox::drain_once;
use super::publisher::RelayPublisher;
use super::shutdown;
use super::{now_unix, NodeError, SellerNode};

/// How long (seconds) the outbox publisher keeps retrying a claim event before it expires. Matches
/// the legacy claim TTL: a claim outlives a slow relay but never lingers indefinitely.
const CLAIM_PUBLISH_WINDOW_SECS: i64 = 3600;
/// Upper bound on parked claims awaiting an award (bounded memory / back-pressure), mirroring the
/// legacy AWAITING_AWARD_CAP: a claim is cheap (no compute until the award), so several may be held.
const AWAITING_AWARD_CAP: i64 = 32;
/// Bounded agent-run attempts within the job deadline before the claim is failed (mirrors the legacy
/// MAX_AGENT_ATTEMPTS): a transient agent error is retried while the deadline still has room.
const MAX_AGENT_ATTEMPTS: u32 = 3;
/// How long (seconds) the outbox publisher keeps retrying a result event before it expires. Longer
/// than the claim window — the delivery is the earned artifact and must survive a slow/absent buyer.
const RESULT_PUBLISH_WINDOW_SECS: i64 = 86_400;
/// Buyer-facing reason on any post-award execution failure: generic (never leaks internal paths or
/// error detail — the operator log carries the specifics) but enough that the buyer learns the job
/// failed instead of waiting on a delivery that will never come.
const EXEC_FAILURE_FEEDBACK: &str = "seller could not complete the job (execution failed before delivery)";
/// Buyer-facing reason when the requested harness is not available on this seat, so the job was
/// never dispatched — a display-only mirror of the `capability_missing` reason_code (§10; the tag
/// governs). Worded as NEVER STARTED, not as failed: the buyer's useful next move is another seat,
/// where a retry here would only reproduce the same refusal.
const CAPABILITY_MISSING_FEEDBACK: &str =
    "seller could not start the job: the requested harness is not available on this seat";
/// The reason code the dispatch-refusal arm of `execute_job` emits — a job that reached execution
/// with no serving harness for the harness its buyer asked for (#821).
///
/// ⛔ **Named as a `pub(crate)` const rather than written inline so the BUYER side can assert on the
/// very value this emitter uses.** That arm is POST-AWARD, so whatever code it emits must be a member
/// of `buyer::is_releasable_failure_feedback`: a code outside that set leaves the buyer's reservation
/// held until the deadline reconcile instead of freeing it on the feedback. Nothing related the emit
/// site to that predicate before, so the label could move out of the releasable set with every test
/// green — see `the_undispatchable_arms_reason_code_is_releasable`.
pub(crate) const UNDISPATCHABLE_REASON_CODE: ReasonCode = ReasonCode::CapabilityMissing;
/// Buyer-facing reason when execution succeeded but the delivery (snapshot/push/publish) failed —
/// a display-only mirror of the `delivery_failed` reason_code (§10; the tag governs).
const DELIVERY_FAILURE_FEEDBACK: &str =
    "seller executed the job but delivery failed before it reached you";
/// Buyer-facing reason when the node observed no genuine execution (the quota-dead case §19 catches),
/// so it refused to deliver without a sentinel — a display-only mirror of the `no_sentinel` reason_code.
const NO_SENTINEL_FEEDBACK: &str =
    "seller refused delivery: no execution was observed, so no execution sentinel was produced";

// TODO(multi-slot): this default is a placeholder pending real award-latency measurement — how long
// a live buyer actually takes between our claim and its award. It is deliberately far below
// CLAIM_PUBLISH_WINDOW_SECS (3600), which stays long for relay resilience: the claim keeps
// retrying on the wire for an hour, but a slot it reserved is reclaimed much sooner if no award
// arrives, so a loaded node does not strand capacity on claims buyers never act on. Operators
// override with `[seller] claim_award_timeout_secs`.
/// Default seconds a parked, unawarded claim may hold its reserved execution slot before the lapse
/// sweep reclaims it. See [`SlotGate`].
const DEFAULT_CLAIM_AWARD_TIMEOUT_SECS: u64 = 300;

/// Homogeneous execution-slot admission for the seller node: at most `capacity` awarded jobs run
/// concurrently, and every slot is identical (it runs whatever harness the job asked for — there is
/// no per-slot typing).
///
/// A permit is RESERVED when the node claims an offer ([`SellerNodeRunner::on_offer`]) and released
/// when the job reaches a terminal outcome (delivery or failure — the permit is moved into the
/// execution task and dropped when it returns, so every early-return, error, and panic path releases
/// by construction), when the buyer awards another seller ([`SellerNodeRunner::on_award`]), or when a
/// parked claim lapses unawarded (the sweep). Reserve-at-claim is what makes a fully loaded node
/// invisible to the market: with no free permit it does not claim, so it never appears.
///
/// The gate is consulted only on the single event loop, which never runs concurrently with itself,
/// so the "is a slot free?" decision is race-free without any additional locking discipline — the
/// `Mutex` here exists only so the gate can be shared behind an `Arc` (execution runs off the loop),
/// never for contention. Reserved-but-not-yet-executing permits are parked keyed by job id; the
/// award moves a permit out into the execution task, and a release/lapse drops it.
struct SlotGate {
    permits: Arc<Semaphore>,
    parked: Mutex<HashMap<String, ParkedSlot>>,
    lapse_after: Duration,
    /// Configured ceiling, clamped as `new` clamps it. Held so a report can carry its denominator: a
    /// count of resumed jobs means nothing without the capacity they are being bounded to.
    capacity: usize,
}

/// A reserved slot held for a claim that is awaiting its award. `reserved_at` bounds how long the
/// claim may sit unawarded before the lapse sweep reclaims the slot.
struct ParkedSlot {
    permit: OwnedSemaphorePermit,
    reserved_at: Instant,
}

/// Outcome of [`SlotGate::try_reserve`]. The caller needs to tell a fresh reservation apart from a
/// re-seen offer whose slot is already parked: only a fresh reservation is released if the claim
/// then turns out to be a dedup no-op or fails to journal (releasing an already-parked job's slot
/// would strand a live claim).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reserve {
    /// A slot was newly reserved for this job id (a permit was taken).
    Reserved,
    /// This job id already held a reserved slot (a re-seen offer) — no new permit taken.
    AlreadyParked,
    /// Every slot is busy — the node is fully loaded and must not claim.
    Full,
}

impl SlotGate {
    fn new(capacity: usize, lapse_after: Duration) -> Self {
        Self {
            // `capacity.max(1)`: zero slots would mean a node that can never claim, which is never
            // what an operator means by configuring a seller — clamp to serial rather than mute it.
            permits: Arc::new(Semaphore::new(capacity.max(1))),
            parked: Mutex::new(HashMap::new()),
            lapse_after,
            capacity: capacity.max(1),
        }
    }

    /// Reserve a slot for `job_id` at claim time. [`Reserve::Full`] when every slot is busy — the
    /// caller then skips the offer (does not claim), which is exactly how a loaded node stays
    /// invisible. Idempotent for an already-parked job id (a re-seen offer does not double-reserve).
    fn try_reserve(&self, job_id: &str) -> Reserve {
        let mut parked = self.parked.lock().expect("slot gate poisoned");
        if parked.contains_key(job_id) {
            return Reserve::AlreadyParked;
        }
        match self.permits.clone().try_acquire_owned() {
            Ok(permit) => {
                parked.insert(job_id.to_owned(), ParkedSlot { permit, reserved_at: Instant::now() });
                Reserve::Reserved
            }
            Err(_) => Reserve::Full,
        }
    }

    /// Release a reserved slot without executing (claim failed to journal, was a dedup no-op, or the
    /// buyer awarded another seller). Dropping the permit returns it to the pool. Idempotent.
    fn release(&self, job_id: &str) {
        self.parked.lock().expect("slot gate poisoned").remove(job_id);
    }

    /// Acquire a permit for an execution that holds no parked reservation, WAITING for one to free.
    ///
    /// This is the single fallback behind [`Self::take_for_execution`] returning `None` — the
    /// producers of that state are enumerated there. It was first written for the restart producer
    /// alone, and the lapsed-park producer then reproduced the exact `slots + K` overrun the restart
    /// fix had closed (#728). That is why every producer now funnels through this ONE wait via
    /// [`spawn_bounded_execution`], instead of each producer getting its own guard: a guard written
    /// for one instance never covers the next.
    ///
    /// WAITED, not tried: the alternative to waiting is abandoning awarded work, and under
    /// award-is-payment the buyer's sats are already committed. The excess queues here instead —
    /// tokio's semaphore hands permits to waiters in order, so a node that owes more executions than
    /// `slots` runs them in waves rather than all at once or not at all.
    ///
    /// Nothing is parked and no `reserved_at` is seeded, which is the whole answer to the lapse clock:
    /// an awarded job is already past the point the lapse sweep exists to bound (a CLAIM sitting
    /// unawarded), so [`Self::sweep_lapsed`] can never reclaim this permit and there is no timer to
    /// restart. It is released exactly like every other executing slot — by the execution future
    /// returning, unwind included.
    ///
    /// `None` is unreachable in this tree: it requires a closed semaphore, and this gate never calls
    /// `close` or `add_permits`. Kept as an `Option` rather than an `expect` so an impossible case
    /// cannot panic an execution task into silence — the caller logs it and runs anyway, because
    /// briefly exceeding a ceiling is the lesser failure against dropping awarded work.
    async fn acquire_unreserved(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().acquire_owned().await.ok()
    }

    /// Take a reserved slot's permit to hand to the execution task, which holds it until the job is
    /// terminal (drop-on-return releases it). `None` when no permit is parked for this job — which
    /// has THREE producers, not one (#728 was the cost of documenting only the first):
    ///
    ///   1. the restart path: the parked map is in-memory and `reserved_at` is a monotonic
    ///      `Instant`, so a restart cannot inherit reservations — the durable store still has the
    ///      awarded job, and nothing is parked for it;
    ///   2. a lapsed park: [`Self::sweep_lapsed`] reclaimed a claim that sat unawarded past
    ///      `lapse_after`, and the award still arrived afterwards (`record_award` binds a claim in
    ///      ANY state, `released` included);
    ///   3. a redundant second award (#279): the first award already moved this job's permit out,
    ///      so the re-award finds the map empty for it.
    ///
    /// Every producer is answered the same way: [`spawn_bounded_execution`] meets `None` by WAITING
    /// on [`Self::acquire_unreserved`], so no execution ever runs outside slot accounting.
    fn take_for_execution(&self, job_id: &str) -> Option<OwnedSemaphorePermit> {
        self.parked
            .lock()
            .expect("slot gate poisoned")
            .remove(job_id)
            .map(|slot| slot.permit)
    }

    /// Reclaim every slot whose parked claim has sat unawarded longer than `lapse_after`. Returns the
    /// job ids so the caller can release the durable claim to match. Dropping each permit returns it
    /// to the pool.
    ///
    /// A swept job is NOT dead: `record_award` binds a claim in ANY state, so the award can still
    /// arrive after the sweep and the job still executes. That late execution finds nothing parked —
    /// producer 2 on [`Self::take_for_execution`] — and waits for a fresh permit like every other
    /// unreserved execution (#728: treating a missing park as restart-only is what let this run
    /// permitless).
    fn sweep_lapsed(&self, now: Instant) -> Vec<String> {
        let mut parked = self.parked.lock().expect("slot gate poisoned");
        let lapsed: Vec<String> = parked
            .iter()
            .filter(|(_, slot)| now.duration_since(slot.reserved_at) >= self.lapse_after)
            .map(|(job_id, _)| job_id.clone())
            .collect();
        for job_id in &lapsed {
            parked.remove(job_id);
        }
        lapsed
    }

    /// Permits currently free. For logging and tests.
    fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

/// Spawn ONE execution task for `job_id`, bounded by a real slot permit no matter which path
/// produced the call.
///
/// This is the single construction behind "an execution always holds a real permit". Every producer
/// of the nothing-parked state — enumerated on [`SlotGate::take_for_execution`]: restart resume,
/// lapsed park (#728), redundant re-award (#279) — flows through the same wait, so the bound is a
/// property of this primitive rather than a per-caller convention. #728 is what a per-caller
/// convention costs: the restart producer got its own guard, the lapsed-park producer did not, and
/// the node ran `slots + K` executions while `available()` — and therefore status — said it was
/// idle.
///
/// The parked reservation is taken HERE, on the caller's thread — the single event loop — so the
/// take never interleaves with the lapse sweep (which also runs on the loop) and a permit is never
/// both sweepable and executing. When nothing is parked, the task WAITS for a permit INSIDE itself,
/// so the loop is never blocked on capacity (issue #223): tokio's semaphore is fair, the excess
/// queues in waves, and awarded work is never dropped.
fn spawn_bounded_execution<F, Fut>(slots: &Arc<SlotGate>, job_id: String, execute: F)
where
    F: FnOnce(String, Option<OwnedSemaphorePermit>) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let parked = slots.take_for_execution(&job_id);
    if parked.is_none() {
        opline!(
            "seller node execute job_id={job_id}: no parked slot reservation (restart resume, park \
             lapsed unawarded before its award, or a redundant re-award) — the task will WAIT to \
             acquire a real slot before executing ({} free now)",
            slots.available()
        );
    }
    let slots = Arc::clone(slots);
    tokio::task::spawn_local(async move {
        let slot = match parked {
            Some(permit) => Some(permit),
            None => {
                let acquired = slots.acquire_unreserved().await;
                if acquired.is_none() {
                    opline!(
                        "seller node execute job_id={job_id}: slot gate closed; executing WITHOUT a \
                         permit (capacity may be exceeded — dropping awarded work is the worse \
                         outcome)"
                    );
                }
                acquired
            }
        };
        execute(job_id, slot).await;
    });
}

/// Spawn one resume task per job a restart left mid-flight, each bounded by a real slot permit.
///
/// Without this the restart path re-drove every non-terminal job at once with no permit, so a node
/// that came back up holding K such jobs ran `slots + K` executions against a ceiling of `slots` —
/// invisible at `slots = 1`, and growing with every slot count we raise.
///
/// Delegates per job to [`spawn_bounded_execution`]: at boot the parked map is empty by
/// construction, so every resume takes the wait-for-a-permit leg there. Sharing the primitive is
/// the point, not a convenience — the same permitless hazard had a second producer (a lapsed park,
/// #728) that a restart-only guard here never covered.
///
/// Generic over the execution step so the fan-out — the part that carries the bound — is reachable by
/// a test without a live relay, an agent, or a store. What a resumed job DOES is irrelevant to the
/// bound, which is why stubbing it is sound rather than a shortcut.
fn spawn_bounded_resumes<F, Fut>(slots: Arc<SlotGate>, job_ids: Vec<String>, execute: F)
where
    F: Fn(String, Option<OwnedSemaphorePermit>) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let execute = std::rc::Rc::new(execute);
    for job_id in job_ids {
        let execute = std::rc::Rc::clone(&execute);
        spawn_bounded_execution(&slots, job_id, move |job_id, slot| execute(job_id, slot));
    }
}

/// The pure claim/skip decision over a parsed offer — no I/O, so the money-safety ordering
/// (targeting, deadline-expiry, rate floor) is unit-testable. Mirrors the legacy `classify_offer`
/// gates that do not need durable state; the store-backed dedup + capacity checks ride on top in
/// [`SellerNodeRunner::on_offer`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaimDecision {
    /// Claim it — carries the job deadline resolved for execution.
    Claim { deadline_unix: u64 },
    /// Skip it, with a named reason (never a silent drop).
    Skip(SkipReason),
}

/// Why an offer was skipped. A typed reason (not a bare string) so the caller can act on the
/// *kind* of refusal — specifically, only a rate-gate refusal (never a lapsed offer) is eligible for
/// the targeted under-rate buyer feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// The offer's own absolute deadline already passed — dead, never resurrected.
    Lapsed,
    /// #604: the offer was authored too long ago — its WIRE age (`now − created_at`) exceeds
    /// [`MAX_OFFER_ADMIT_AGE_SECS`]. A long-aged, never-awarded historical the backfill keeps
    /// re-surfacing; admitting it only parks an execution slot on work that will not be awarded.
    /// Distinct from [`Self::Lapsed`]: that is the offer's self-declared expiry, this is its age.
    TooOld,
    /// The seller runs a populated `accept_offers_only_from` allowlist, this offer's author (the
    /// buyer) is not on it, the offer TARGETS this seat, and `accept_open_targeted` is false — so no
    /// control admits it. Named (not silent) so the operator log records the declined pubkey; no
    /// buyer feedback is emitted (a private seller does not advertise why it declined a stranger).
    ///
    /// ⛔ NOT A FENCE OVER BOTH SURFACES SINCE #923. It used to be emitted for ANY unlisted buyer the
    /// moment a list existed, targeted or not, ahead of `accept_open_targeted` and the rate gate. It
    /// is now scoped to the targeted surface with the open route CLOSED: an untargeted offer is
    /// answered by [`Self::RateGate`] and `claim_open_pool`, and an offer addressed to another seat
    /// by the rate gate's target mismatch.
    NotAllowlisted,
    /// A buyer this seat has not named targeted it directly, and the seat has not opted in to the
    /// targeted-open surface (`accept_open_targeted = false`, the default).
    ///
    /// DELIBERATELY NOT FOLDED INTO [`Self::NotAllowlisted`], for the reason
    /// [`Self::TakenElsewhere`] is not folded into [`Self::Settled`]: the two answer different
    /// operator questions. `NotAllowlisted` means "this buyer is not on the list you wrote" and the
    /// fix is to edit the list. This means "you named no buyers and you are not open to strangers"
    /// — the operator has NO list to edit, so the same string would send them looking for one that
    /// does not exist. It is also the line that makes the silent migration audible: a seat upgraded
    /// past the three-knob change stops accepting targeted work with no config error, and this
    /// reason is what tells its operator which knob restores it. Like the fence, it emits no buyer
    /// feedback — a seat that is not open does not advertise why.
    OpenTargetedRefused,
    /// Rate-gate refused: untargeted without open-pool opt-in, or below the seller's rate floor.
    RateGate,
    /// The offer asked for a harness this node does not run.
    AgentUnavailable,
    /// Every execution slot is busy — the node is fully loaded and does not claim (reserve-at-claim
    /// back-pressure; a loaded node is invisible to the market by simply not claiming). Emitted by
    /// [`SellerNodeRunner::on_offer`], never by the pure [`classify_offer`], because it depends on
    /// live slot state.
    SlotsBusy,
    /// #541: the offer is already SETTLED — a co-signed kind-3400 receipt authored by the offer's own
    /// buyer has been seen for it (a settlement by any seller is terminal, never claimable). Emitted
    /// by [`SellerNodeRunner::claim_offer`], never by the pure [`classify_offer`], because it depends
    /// on the live relay-derived terminal cache — the same reason `SlotsBusy` lives here, not there.
    Settled,
    /// #814: the offer was AWARDED (or its delivery ACCEPTED) to ANOTHER seller — a buyer-authenticated
    /// kind-3405/3406 naming a claim that is not ours has been seen for an offer we recorded but never
    /// claimed. Emitted by [`SellerNodeRunner::claim_offer`] for the same reason as `Settled`.
    ///
    /// DELIBERATELY NOT FOLDED INTO `Settled`, though both mean "not ours to claim". The whole harm of
    /// #814 is FALSE MARKETPLACE ACTIVITY — a losing claim published after the race was over — so the
    /// operator reading this line is usually asking "did we lose this one, or was it paid and closed?"
    /// One string covering both states answers neither. The two also differ in LIFETIME, which is the
    /// same distinction [`Suppression`] draws in the type: a receipt is terminal forever, an award
    /// binds only until the offer's own deadline.
    TakenElsewhere,
}

impl SkipReason {
    /// The machine-readable log/feedback reason (same string the legacy path logged).
    fn reason(self) -> &'static str {
        match self {
            Self::Lapsed => "offer deadline already passed (lapsed; never resurrected)",
            Self::TooOld => "offer authored too long ago (aged historical; not re-admitted from backfill)",
            Self::NotAllowlisted => "buyer not in accept_offers_only_from allowlist",
            Self::OpenTargetedRefused => {
                "targeted by a buyer this seat has not named, and accept_open_targeted=false \
                 (set accept_open_targeted=true to accept strangers, or list the buyer in \
                 accept_offers_only_from)"
            }
            Self::RateGate => "rate-gate refused (untargeted without opt-in / below rate)",
            Self::AgentUnavailable => "requested agent harness not available on this node",
            Self::SlotsBusy => "all execution slots busy (node fully loaded; not claiming)",
            Self::Settled => "offer already settled (co-signed receipt seen; terminal, never claimed)",
            Self::TakenElsewhere => {
                "offer awarded to another seller (buyer-authenticated award/acceptance seen; not ours \
                 to claim)"
            }
        }
    }
}

/// Upper bound on distinct settled offer ids the terminal cache tracks (#541). FIFO by first-seen: a
/// settled offer is terminal forever, but the set cannot grow without limit, so the oldest ids age
/// out. Eviction is FAIL-OPEN — an aged-out offer becomes claimable again, bounded by its own
/// deadline (and, if genuinely re-awarded, a fresh receipt).
const TERMINAL_OFFERS_CAP: usize = 4096;
/// Upper bound on receipt-author pubkeys cached per offer (#541). A co-signed receipt is buyer-signed
/// (authorize_pay.rs `sign_with_keys(buyer_keys)`), so the honest set is size one; the cap tolerates a
/// few and DROPS-NEWEST on overflow, so once the real buyer's author is recorded a later forged
/// receipt can never displace it.
const TERMINAL_AUTHORS_PER_OFFER: usize = 4;

/// How long a terminal/suppression entry for one (offer, author) stays in force.
///
/// Two buyer-authenticated signals mark a recorded offer as no longer ours to claim, and they differ
/// only in lifetime:
/// - [`Self::Settled`] (#541) — a co-signed kind-3400 receipt. The offer is settled; terminal
///   FOREVER, never re-claimable.
/// - [`Self::TakenElsewhere`] (#814) — a buyer-authenticated AWARD or ACCEPTANCE of the offer to
///   ANOTHER seller. The offer is taken, but suppressed only until its own `param:deadline` (the
///   ONLY deadline the protocol defines — there is no separate award/delivery deadline). Fail-open,
///   mirroring the receipt cache's bounded eviction: a legitimately re-runnable job always carries a
///   NEW offer id (awards are write-once per offer), so expiring at the offer deadline can never drop
///   a real re-claim — an offer past its deadline is already `Lapsed` at the claim gate anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Suppression {
    /// A co-signed receipt was seen — terminal forever.
    Settled,
    /// A buyer-authenticated award/acceptance to another seller was seen — suppressed until
    /// `until_unix` (the offer's own absolute deadline).
    TakenElsewhere { until_unix: u64 },
}

impl Suppression {
    /// The operator line this suppression earns at the claim gate. The two states are reported
    /// separately on purpose — see [`SkipReason::TakenElsewhere`].
    fn skip_reason(self) -> SkipReason {
        match self {
            Self::Settled => SkipReason::Settled,
            Self::TakenElsewhere { .. } => SkipReason::TakenElsewhere,
        }
    }

    /// Whether this suppression still bars a claim at `now_unix`. `Settled` never expires.
    fn in_force(self, now_unix: u64) -> bool {
        match self {
            Self::Settled => true,
            Self::TakenElsewhere { until_unix } => now_unix < until_unix,
        }
    }

    /// Combine two suppressions seen for the SAME (offer, author): the stronger wins. `Settled`
    /// (permanent) always dominates, so a later receipt UPGRADES a prior award-suppression to
    /// terminal and a receipt is never weakened back to an expiring award; two award-suppressions
    /// keep the later expiry.
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Settled, _) | (_, Self::Settled) => Self::Settled,
            (
                Self::TakenElsewhere { until_unix: a },
                Self::TakenElsewhere { until_unix: b },
            ) => Self::TakenElsewhere { until_unix: a.max(b) },
        }
    }
}

/// Offers no longer ours to claim, keyed by offer id to the buyer-authored suppression signals seen
/// for them ([`Suppression`]). Relay-derived, populated by [`SellerNodeRunner::on_receipt`] (the
/// co-signed 3400) and [`SellerNodeRunner::on_award`] / [`SellerNodeRunner::on_accept`] (a
/// buyer-authenticated award/acceptance to another seller, #814) off their live subscriptions plus
/// the boot/reconnect backfill — the offer row that authorizes each award is itself persisted, so a
/// relay redelivery on restart re-derives the suppression, exactly as the receipt path does.
///
/// The gate ([`SellerNodeRunner::claim_offer`]) skips an offer ONLY when an IN-FORCE suppression
/// authored by the offer's OWN buyer is present ([`Self::suppressed_by`]). Deliberately NOT the local
/// `store.has_receipt`, which knows only settlements THIS node collected — an offer another seat won
/// and settled is absent there, so a local check would be vacuous. And buyer-bound: award, acceptance
/// and receipt are all buyer-signed and their outer event signatures are client-verified, so a forged
/// event (author != the offer's buyer) is stored but never matches at claim time, where the offer's
/// real buyer is known.
///
/// Bounded twice so it can neither grow without limit nor be displaced by a flood:
/// - authors-per-offer caps at [`TERMINAL_AUTHORS_PER_OFFER`], DROP-NEWEST, so an established
///   real-buyer entry can never be evicted by later (forged) events;
/// - the offer map caps at [`TERMINAL_OFFERS_CAP`], FIFO by first-seen.
///
/// FAIL-OPEN throughout: an unknown / evicted / flood-displaced / expired offer stays claimable. The
/// worst a forger or a flood achieves is the PRE-#541 behaviour — a wasted slot until the claim
/// lapses — never a spend and never worse than today.
struct TerminalOffers {
    inner: std::sync::Mutex<TerminalOffersInner>,
}

struct TerminalOffersInner {
    /// offer id → the buyer-authored suppressions seen for it (a bounded, drop-newest set keyed by
    /// author pubkey).
    by_offer: std::collections::HashMap<String, Vec<(String, Suppression)>>,
    /// offer ids in first-seen order, for FIFO eviction of the whole map.
    order: std::collections::VecDeque<String>,
    offers_cap: usize,
    authors_cap: usize,
}

impl TerminalOffersInner {
    fn new(offers_cap: usize, authors_cap: usize) -> Self {
        Self {
            by_offer: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            offers_cap: offers_cap.max(1),
            authors_cap: authors_cap.max(1),
        }
    }

    /// Record `author` as having published `suppression` for `offer_id`. Idempotent per
    /// (offer, author): a repeat entry from the same author COMBINES with the existing one
    /// ([`Suppression::combine`]) so a receipt upgrades a prior award-suppression to terminal and is
    /// never weakened back. DROP-NEWEST once an offer holds `authors_cap` distinct authors (so an
    /// established real-buyer entry is never displaced); FIFO-evict the oldest offer when a NEW offer
    /// would exceed `offers_cap`.
    fn record(&mut self, offer_id: &str, author: &str, suppression: Suppression) {
        if let Some(authors) = self.by_offer.get_mut(offer_id) {
            if let Some(entry) = authors.iter_mut().find(|(a, _)| a == author) {
                entry.1 = entry.1.combine(suppression);
            } else if authors.len() < self.authors_cap {
                authors.push((author.to_owned(), suppression));
            }
            return;
        }
        if self.order.len() >= self.offers_cap {
            if let Some(evicted) = self.order.pop_front() {
                self.by_offer.remove(&evicted);
            }
        }
        self.by_offer
            .insert(offer_id.to_owned(), vec![(author.to_owned(), suppression)]);
        self.order.push_back(offer_id.to_owned());
    }

    /// The IN-FORCE suppression authored by `buyer` (the offer's own buyer) barring `offer_id` at
    /// `now_unix` — `None` when the offer is still ours to claim. The buyer-binding is the whole of
    /// the anti-grief property: a forged event authored by any other key is present in the set but
    /// never satisfies this test. An expired award-suppression ([`Suppression::TakenElsewhere`] past
    /// its deadline) also yields `None` — fail-open.
    ///
    /// Returns WHICH suppression rather than a bool so the gate can name it: "another seller won
    /// this" and "this was paid and closed" are different operator lines (see [`SkipReason`]). At most
    /// one entry per author exists — [`Self::record`] combines a repeat from the same author — so the
    /// first match is the whole answer.
    fn suppressed_by(&self, offer_id: &str, buyer: &str, now_unix: u64) -> Option<Suppression> {
        self.by_offer
            .get(offer_id)?
            .iter()
            .find(|(a, _)| a == buyer)
            .map(|(_, s)| *s)
            .filter(|s| s.in_force(now_unix))
    }

    #[cfg(test)]
    fn offer_count(&self) -> usize {
        self.by_offer.len()
    }
}

impl TerminalOffers {
    fn new(offers_cap: usize, authors_cap: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(TerminalOffersInner::new(offers_cap, authors_cap)),
        }
    }

    /// #541: a co-signed receipt makes `offer_id` terminal FOREVER for `author` (the buyer).
    fn record_receipt(&self, offer_id: &str, author: &str) {
        self.inner
            .lock()
            .expect("terminal offers mutex poisoned")
            .record(offer_id, author, Suppression::Settled);
    }

    /// #814: a buyer-authenticated award/acceptance to another seller marks `offer_id` taken by
    /// `author` (the buyer) until `until_unix` (the offer's own deadline). Fail-open past that.
    fn record_taken_elsewhere(&self, offer_id: &str, author: &str, until_unix: u64) {
        self.inner
            .lock()
            .expect("terminal offers mutex poisoned")
            .record(offer_id, author, Suppression::TakenElsewhere { until_unix });
    }

    fn suppressed_by(&self, offer_id: &str, buyer: &str, now_unix: u64) -> Option<Suppression> {
        self.inner
            .lock()
            .expect("terminal offers mutex poisoned")
            .suppressed_by(offer_id, buyer, now_unix)
    }
}

/// Upper bound on distinct offer ids remembered as having already been given the targeted under-rate
/// buyer-feedback (#582). Sized like [`TERMINAL_OFFERS_CAP`] — a few thousand distinct under-rate
/// offers within a single boot is already far past any real buyer's behaviour, and each entry is one
/// 64-hex id, so the whole set stays well under a megabyte at the cap. FIFO by first-seen, FAIL-OPEN:
/// an aged-out id can re-emit ONE more feedback if it is re-ingested, which is exactly the pre-#582
/// duplicate this bounds — never worse.
const FED_UNDER_RATE_OFFERS_CAP: usize = 4096;

/// Offer ids that have ALREADY surfaced the targeted under-rate buyer-feedback this boot (#582), so
/// the #560 offer-backfill re-feeding every stored offer through [`SellerNodeRunner::on_offer`] each
/// tick does not re-emit a duplicate `BelowRate` feedback to the buyer on every pass (~12×/window in
/// prod). A pure wire-noise dedup: it gates ONLY the buyer-feedback emit
/// ([`SellerNodeRunner::publish_under_rate_feedback`]) and never the claim/money path, which is a
/// different, idempotent branch of `on_offer`.
///
/// In-memory and bounded, mirroring [`TerminalOffers`] (the #541 precedent, also in-memory):
/// - the set caps at [`FED_UNDER_RATE_OFFERS_CAP`], FIFO by first-seen — the OLDEST id is evicted when
///   a new one would overflow, so a long-running seller never leaks memory. Evicting the oldest (not
///   the newest) is deliberate: the oldest fed offer is the one most likely to have already aged past
///   the backfill lookback, so dropping it is the least likely to cause a re-emit.
/// - cleared on restart by construction, which the issue explicitly accepts: at most ONE re-emit per
///   offer per boot ("suppress a repeat within the window").
struct FedUnderRateOffers {
    inner: std::sync::Mutex<FedUnderRateOffersInner>,
}

struct FedUnderRateOffersInner {
    /// Offer ids already fed, for O(1) first-sight lookup.
    seen: std::collections::HashSet<String>,
    /// The same ids in first-seen order, for FIFO eviction of the whole set.
    order: std::collections::VecDeque<String>,
    cap: usize,
}

impl FedUnderRateOffersInner {
    fn new(cap: usize) -> Self {
        Self {
            seen: std::collections::HashSet::new(),
            order: std::collections::VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Whether `offer_id` has already surfaced under-rate feedback this boot.
    fn contains(&self, offer_id: &str) -> bool {
        self.seen.contains(offer_id)
    }

    /// Record `offer_id` as fed. Idempotent; FIFO-evicts the oldest id when at `cap`.
    fn record(&mut self, offer_id: &str) {
        if self.seen.contains(offer_id) {
            return;
        }
        if self.order.len() >= self.cap {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        self.seen.insert(offer_id.to_owned());
        self.order.push_back(offer_id.to_owned());
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.seen.len()
    }
}

impl FedUnderRateOffers {
    fn new(cap: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(FedUnderRateOffersInner::new(cap)),
        }
    }

    fn contains(&self, offer_id: &str) -> bool {
        self.inner
            .lock()
            .expect("fed under-rate offers mutex poisoned")
            .contains(offer_id)
    }

    fn record(&self, offer_id: &str) {
        self.inner
            .lock()
            .expect("fed under-rate offers mutex poisoned")
            .record(offer_id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("fed under-rate offers mutex poisoned")
            .len()
    }
}

/// The pure award-match decision over a parked claim — no I/O, so the security-critical rule is
/// unit-testable. An award binds our claim ONLY when its author is the offer's buyer (a third party
/// can never drive execute or release) AND it names OUR published claim id; if it names a different
/// claim the buyer picked another seller and we release; if our claim is not yet on the wire, or the
/// author is not the buyer, we ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AwardMatch {
    /// The award names our claim — bind it and execute.
    Execute,
    /// The award names a different claim — the buyer picked another seller; release ours.
    Release,
    /// Not ours / not the buyer / our claim not yet published — do nothing.
    Ignore,
}

/// Match an award against our parked claim. `our_claim_id` is the published id of our claim for this
/// offer (`None` until it has been confirmed on the relay). Pure over its inputs.
fn match_award(
    award_claim_id: &str,
    our_claim_id: Option<&str>,
    award_author: &str,
    offer_buyer: &str,
) -> AwardMatch {
    // Authorization: only the offer's buyer may award. A spoofed award (author != buyer) can never
    // drive execute OR release.
    if award_author != offer_buyer {
        return AwardMatch::Ignore;
    }
    match our_claim_id {
        Some(id) if id == award_claim_id => AwardMatch::Execute,
        Some(_) => AwardMatch::Release,
        None => AwardMatch::Ignore,
    }
}

/// A REJECT is readable only when its signer is the buyer recorded on this job's AWARD.
fn reject_author_gate(reject_author: &str, awarding_buyer: Option<&str>) -> bool {
    awarding_buyer.is_some_and(|buyer| reject_author == buyer)
}

/// The operator line a re-seen, already-claimed offer earns — `None` when it earns only the verbose
/// dedup no-op.
///
/// Pure over the job's state, so the exact rendered text is assertable rather than eyeballed (the
/// same reason [`crate::oplog::line`] is a function instead of macro-internal). A guard nobody can
/// assert on is the guard this line exists to replace.
///
/// `None` for an absent or unreadable state as well as an unfinished one: the loud line is for a
/// job we KNOW is finished, and an unknown state is not evidence of one.
fn already_handled_skip_line(job_id: &str, state: Option<super::store::JobState>) -> Option<String> {
    let state = state?;
    state.is_finished().then(|| {
        format!(
            "seller node offer skip id={job_id}: already handled (job {}; not re-claiming)",
            state.as_str()
        )
    })
}

/// Build the delivery co-signature preimage. `creq_hash` is derived from the STORED claim-time creq
/// (`stored_creq`) — never a rebuild from live config — so a config change between claim and delivery
/// cannot break the buyer/seller cosignature (audit N-4 / invariant 8). The specific realized mint is
/// deliberately NOT in the preimage (the seller signs at delivery, before the buyer picks a mint); the
/// accepted-mint SET is bound via this creq hash, so buyer/seller cosigs agree for ANY accepted mint.
#[allow(clippy::too_many_arguments)]
fn delivery_receipt_preimage(
    job_id: &str,
    task: &str,
    amount: u64,
    buyer_pubkey: &str,
    seller_pubkey: &str,
    commit_oid: &str,
    delivery_kind: &str,
    stored_creq: &str,
) -> ReceiptPreimage {
    ReceiptPreimage {
        job_hash: job_hash_for_offer(job_id, task, amount),
        offer_id: job_id.to_owned(),
        amount,
        unit: "sat".to_owned(),
        buyer_pubkey: buyer_pubkey.to_owned(),
        seller_pubkey: seller_pubkey.to_owned(),
        delivery_integrity_hash: commit_oid.to_owned(),
        delivery_kind: delivery_kind.to_owned(),
        exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
        creq_hash: Some(gateway::creq_hash_hex(stored_creq)),
    }
}

/// Relay-stall watchdog threshold (seconds): `interval_secs * missed_intervals`, each clamped to at
/// least 1 so the product is always positive (the watchdog can never trip on the first tick). Pure,
/// unit-testable. Ported verbatim from the daemon (#150/#142).
fn stall_threshold_secs(interval_secs: u64, missed_intervals: u32) -> u64 {
    interval_secs
        .max(1)
        .saturating_mul(u64::from(missed_intervals.max(1)))
}

/// Whether the live subscription is presumed dead: no own heartbeat has round-tripped for at least
/// `threshold_secs`. Pure over an elapsed-seconds reading (fake-clock testable).
fn subscription_stalled(elapsed_secs: u64, threshold_secs: u64) -> bool {
    elapsed_secs >= threshold_secs
}

/// Whether a single-relay publish was CONFIRMED accepted by the relay it was sent to (#509).
///
/// `Client::send_event_to` returns `Ok(Output { success, failed })` EVEN WHEN the sole relay
/// REJECTS the write: an `OK: false` lands that relay in `output.failed`, NOT a top-level `Err`. So
/// the old `Ok(_) => true` match read a rejection as success — the "health inferred, not confirmed"
/// defect. A publish is confirmed only when the relay acknowledged it (`success` non-empty) and none
/// rejected it (`failed` empty). Because the seat sends to exactly ONE relay, that is equivalent to
/// "`output.success` contains our relay url", but this form needs no url parse and no re-derivation
/// of which relay we sent to. An empty `success` (relay in `failed`, or nothing acknowledged) is the
/// HEALTH-RED signal — the seat is dark on the relay even though the connection is up.
fn publish_confirmed<T: std::fmt::Debug>(output: &Output<T>) -> bool {
    output.failed.is_empty() && !output.success.is_empty()
}

/// Whether a heartbeat tick observed CONFIRMED relay-observed liveness — the sole condition that may
/// refresh the watchdog clock (`last_liveness_seen*`), #509.
///
/// Relay-observed liveness has TWO independent legs and BOTH must hold this tick:
/// - `probe_ok`: the relay served our `limit(0)` REQ on this authenticated session (READ path,
///   [`probe_relay_serves_our_reqs`]).
/// - `publish_ok`: the relay ACCEPTED our own kind-30340 heartbeat write ([`publish_confirmed`]).
///
/// The bug in #509 is precisely that these two diverge: a seat whose reads keep answering while its
/// heartbeat writes are silently rejected/lost is DARK on the relay yet the read probe alone kept the
/// clock fresh, so the watchdog never fired ("recovery SUCCEEDED" logged across a 2691s outage). AND-
/// gating the clock on the publish leg makes a rejected/absent heartbeat OK leave the clock
/// un-refreshed and drive the RELAY-STALL watchdog, exactly as a failed read probe does.
fn relay_liveness_confirmed(probe_ok: bool, publish_ok: bool) -> bool {
    probe_ok && publish_ok
}

/// Overlap margin (seconds) subtracted from the last-known-good heartbeat timestamp when computing
/// the post-stall resubscribe `since` cursor, so events published during the stall backfill; the
/// idempotent handlers (offer dedup, award match against still-parked claims, wrap pay-once via the
/// receipt dedup) absorb the overlap re-delivery.
const STALL_OVERLAP_MARGIN_SECS: u64 = 60;
/// Bounded connect-phase recovery attempts within ONE stall recovery before yielding to the next
/// heartbeat tick (#162): a relay that drops the socket before completing NIP-42 is retried with a
/// short backoff rather than waiting a whole stall interval.
const RECOVERY_MAX_ATTEMPTS: u32 = 3;
/// Base backoff between the bounded recovery attempts (#162), doubled per attempt by
/// [`recovery_backoff`].
const RECOVERY_BACKOFF: Duration = Duration::from_secs(2);
/// Ceiling on the per-attempt backoff, so one bounded recovery still fits inside a single heartbeat
/// interval and the watchdog stays on cadence.
const RECOVERY_BACKOFF_MAX: Duration = Duration::from_secs(8);

/// Backoff to wait after a failed recovery `attempt` before the next one: exponential from
/// [`RECOVERY_BACKOFF`], capped at [`RECOVERY_BACKOFF_MAX`].
///
/// A flat retry interval re-dials the relay as fast as the socket can be torn down — with #171 in
/// the field that was every wedged node re-dialing shared infrastructure three times a minute,
/// indefinitely. Backing off spaces the attempts; capping them keeps a whole recovery bounded.
fn recovery_backoff(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.saturating_sub(1).min(16);
    RECOVERY_BACKOFF
        .saturating_mul(factor)
        .min(RECOVERY_BACKOFF_MAX)
}

/// Cadence of the periodic payment-wrap backfill.
///
/// A live kind-1059 subscription is not sufficient on its own. Field-observed on the in-memory
/// daemon this node replaces: a fresh subscription delivers a wrap within ~1 min, but a subscription
/// ~10+ minutes old was seen to go deaf and never deliver again — and a payment then sat unredeemed
/// until the process was manually restarted, because the restart re-ran the boot backfill. Re-asking
/// the relay for stored wraps on a timer is what makes that recover WITHOUT a restart.
///
/// Note this is a failure the liveness probe cannot see: the session still answers our REQs, so the
/// relay is "alive" by every measure the watchdog has. The three layers are deliberately distinct —
/// probe = session liveness, this backfill = money-leg recovery, and a subscription-map reconciler
/// (#172) = registration integrity. None of them subsumes another.
const WRAP_BACKFILL_INTERVAL_SECS: u64 = 300;
/// Skew margin subtracted from the oldest delivered-but-unpaid job when clamping the backfill cursor.
const WRAP_BACKFILL_MARGIN_SECS: i64 = 3600;
/// Test-only override of [`WRAP_BACKFILL_INTERVAL_SECS`]. NOT a user config knob; no production path
/// sets it (mirrors the heartbeat cadence seam).
const WRAP_BACKFILL_INTERVAL_ENV: &str = "MAXPLAYER_WRAP_BACKFILL_INTERVAL_SECS";
/// Hard cap on one backfill fetch, so an auth-gated relay that never EOSEs cannot wedge the tick.
const WRAP_BACKFILL_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Effective backfill cadence: the env test seam wins over [`WRAP_BACKFILL_INTERVAL_SECS`]; a `0` or
/// unparseable value is ignored.
fn resolve_wrap_backfill_interval_secs() -> u64 {
    match std::env::var(WRAP_BACKFILL_INTERVAL_ENV) {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|secs| *secs > 0)
            .unwrap_or(WRAP_BACKFILL_INTERVAL_SECS),
        Err(_) => WRAP_BACKFILL_INTERVAL_SECS,
    }
}

/// The `since` cursor for a wrap backfill: the last collected receipt, but never later than the
/// oldest delivered-but-unpaid job (minus a skew margin).
///
/// The last-receipt timestamp alone is wrong: a receipt for a NEWER job would advance the cursor past
/// an OLDER unsettled job and skip its still-uncollected payment wrap forever. Clamping keeps that
/// wrap in range; the per-job idempotency guards (`has_receipt` skip, mint already-spent refuse) make
/// the wider re-scan safe.
///
/// A journal/store READ ERROR must abort the cycle, never fall back to `since = 0` — that would turn a
/// transient read failure into a full-history backfill. Absent data (nothing collected, nothing
/// unsettled) is legitimately `0`; a failure to read is not.
fn resolve_backfill_since(
    last_receipt: Result<Option<i64>, super::store::StoreError>,
    oldest_unsettled: Result<Option<i64>, super::store::StoreError>,
) -> Result<u64, super::store::StoreError> {
    let last_receipt = last_receipt?.unwrap_or(0);
    let cursor = match oldest_unsettled? {
        Some(oldest) => last_receipt.min(oldest.saturating_sub(WRAP_BACKFILL_MARGIN_SECS)),
        None => last_receipt,
    };
    Ok(cursor.max(0) as u64)
}

/// Upper bound on stored open-pool offers a backfilling REQ may return.
const OFFER_BACKFILL_LIMIT: usize = 500;

/// Lookback window (seconds) for the periodic offer backfill's `since` cursor (#560).
///
/// Bounded so a long-lived seat's targeted re-fetch does not grow to its whole history every tick.
/// The classify-level deadline-expiry refusal (see [`offer_subscription_filters`]) discards anything
/// stale the window still returns, so the width only trades relay bandwidth against how long a missed
/// offer stays recoverable — sized to span several backfill intervals so a run of failed or timed-out
/// ticks cannot open a permanent gap.
const OFFER_BACKFILL_WINDOW_SECS: u64 = 3600;

/// #604: the maximum WIRE AGE (`now − event.created_at`) at which an offer is still admitted for a
/// claim, applied in [`classify_offer`] DISTINCT from the offer's self-declared `deadline_unix`. The
/// periodic offer-backfill re-ingests every stored kind-3401 in its lookback window each tick, so a
/// long-aged, never-awarded historical with a FAR-FUTURE deadline would otherwise be (re-)admitted,
/// claimed, and hold an execution slot for the full claim-lapse — starving live offers (`SlotsBusy`)
/// while a genuinely awardable one is refused. Set to the backfill window: everything within the
/// routine recovery horizon stays admitted (so legitimate backfill of a genuinely-recent offer is
/// never defeated), and only offers a WIDENED `offer_backfill_secs` reaches past that horizon are
/// refused. Orthogonal to the live claim-lapse capacity guard — a claimed slot still lapses on time.
const MAX_OFFER_ADMIT_AGE_SECS: u64 = OFFER_BACKFILL_WINDOW_SECS;

/// Hard cap on one offer-backfill fetch, so an auth-gated relay that never EOSEs cannot wedge the
/// tick. Mirrors [`WRAP_BACKFILL_FETCH_TIMEOUT`].
const OFFER_BACKFILL_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Hard bound on the #563 resume-refine settled-elsewhere relay-derive. It runs in the resume/boot
/// path, so a hung relay must never stall boot: the fetch is wrapped in this timeout, and a timeout
/// is treated EXACTLY as absence (⇒ RunAgent, the safe branch here). Kept short — the derive fires
/// only for the narrow live-deadline residual, and a settlement event either returns fast or not at
/// all; over-waiting only delays re-driving a genuine award.
const SETTLED_ELSEWHERE_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The `since` cursor for a periodic offer backfill: a bounded lookback from `now`, never shorter than
/// [`OFFER_BACKFILL_WINDOW_SECS`] and widened to the seller's configured `offer_backfill_secs` when
/// that is larger, so the periodic recovery window is never narrower than the boot backfill's. Unlike
/// the wrap cursor there is no store cursor to clamp to. What a fetched offer is then measured
/// against is [`classify_offer`]'s admission gates: the self-declared `deadline_unix` refusal AND the
/// #604 offer-age refusal ([`MAX_OFFER_ADMIT_AGE_SECS`]) — the latter is why a WIDENED window is safe,
/// since a long-aged historical the wide window re-surfaces is refused rather than re-claimed.
fn resolve_offer_backfill_since(now: u64, offer_backfill_secs: u64) -> nostr_sdk::Timestamp {
    let window = OFFER_BACKFILL_WINDOW_SECS.max(offer_backfill_secs);
    nostr_sdk::Timestamp::from(now.saturating_sub(window))
}

/// Stable per-role subscription ids. Named rather than generated so a relay `CLOSED` says WHICH
/// subscription died — with anonymous ids a closed subscription is indistinguishable in the log,
/// which is how a node could go silently deaf on one leg while heartbeating happily on another.
const OFFER_SUB_ID: &str = "maxplayer-offers";
const AWARD_SUB_ID: &str = "maxplayer-awards";
const WRAP_SUB_ID: &str = "maxplayer-wraps";
/// #541: co-signed settlement receipts (kind-3400). An open-pool seller subscribes to these to learn
/// which offers are already SETTLED (by ANY seller) so it never claims a terminal offer that
/// re-appears via backfill or redelivery. Registered only when open-pool — a targeted seller's own
/// settlements are covered by local claim idempotency, and a targeted-to-it offer can only ever be
/// settled by it.
const RECEIPT_SUB_ID: &str = "maxplayer-receipts";
/// The liveness probe's subscription (see [`probe_relay_serves_our_reqs`]).
const LIVENESS_PROBE_SUB_ID: &str = "maxplayer-liveness-probe";

/// How long the liveness probe waits for its `EOSE`. A `limit(0)` REQ is answered in milliseconds by
/// a healthy relay, so this is generous — it bounds the tick, and a single slow answer is not a stall
/// on its own (it takes `stall_missed_intervals` consecutive failures to trip the watchdog).
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The human label for one of our subscription ids, for logging a relay `CLOSED`.
fn subscription_label(id: &str) -> &'static str {
    match id {
        OFFER_SUB_ID => "offers",
        AWARD_SUB_ID => "awards",
        WRAP_SUB_ID => "payment gift-wraps (kind-1059)",
        RECEIPT_SUB_ID => "settlement receipts (kind-3400)",
        LIVENESS_PROBE_SUB_ID => "liveness probe",
        _ => "unknown (not one of ours)",
    }
}

/// Whether `id` names a long-lived subscription this node registers. Anything else — a transient
/// `fetch_events` REQ, a stale generation, a relay-side artefact — is not a leg of ours, so its
/// closure cannot make us deaf.
fn is_our_subscription(id: &str) -> bool {
    matches!(
        id,
        OFFER_SUB_ID | AWARD_SUB_ID | WRAP_SUB_ID | RECEIPT_SUB_ID | LIVENESS_PROBE_SUB_ID
    )
}

/// The diagnostic for a `CLOSED` naming a subscription id we never registered.
///
/// A function rather than an inline `opline!` because this line is field-facing: the relay owner
/// reads it to tell two hypotheses apart, and neither is visible from the server side. Our periodic
/// backfills (wrap + offer, #560) call `fetch_events`, which GENERATES its subscription id
/// (`pool/mod.rs:815`) and run on exactly the cadence these closes appear on — so a small
/// `last_backfill` age implicates our own transient REQ. A `last_nip42_auth` age near the relay's
/// NIP-42 TTL instead implicates a re-challenge sweep closing auth-scoped subscriptions from the
/// pre-expiry generation. Being a function, its content is pinned by a test instead of drifting
/// silently.
fn unknown_close_diagnostic(
    id: &str,
    last_backfill_secs: u64,
    last_nip42_auth_secs: u64,
    authed: bool,
) -> String {
    format!(
        "seller node RELAY-CLOSED UNKNOWN-ID: id={id} was never in our registry (ours: \
         {OFFER_SUB_ID}, {AWARD_SUB_ID}, {WRAP_SUB_ID}, {RECEIPT_SUB_ID}, {LIVENESS_PROBE_SUB_ID}); \
         no recovery \
         forced. last_backfill={last_backfill_secs}s ago, \
         last_nip42_auth={last_nip42_auth_secs}s ago, authed={authed}"
    )
}

/// Whether EVERY filter on this subscription pins `#p` to our own pubkey.
///
/// This is the precondition for reading a `restricted:` CLOSED as the #189 pre-auth race instead of a
/// gate violation, and the CLOSED-prefix taxonomy stays load-bearing everywhere else: `restricted:`
/// remains permanent-class, and the SDK's `Remove` classification is not softened. The carve-out is
/// sound because maxplayer-relay's p-gate has exactly two ways to refuse a `#p` filter — the `#p` names
/// somebody else, or the connection had no authenticated pubkey to compare it against. We author
/// these filters from `self.seller_pubkey`, so the first is impossible by construction for the ids
/// below; only the second remains, and the second is retryable once auth exists. A subscription
/// carrying ANY un-pinned filter is excluded, because there the refusal may genuinely be about the
/// un-pinned half — that case has its own repair, the targeted-only degrade.
fn subscription_pins_only_our_pubkey(id: &str, claim_open_pool: bool) -> bool {
    match id {
        AWARD_SUB_ID | WRAP_SUB_ID => true,
        OFFER_SUB_ID => !claim_open_pool,
        _ => false,
    }
}

/// Owned ticks to wait before the next open-pool re-arm attempt, after `rejections` consecutive
/// refusals (#190).
///
/// Doubling, capped: a relay that permanently refuses the un-pinned filter must cost one REQ per cap
/// interval, never one REQ per tick. Zero rejections means "attempt on the next tick" — the first
/// try after a degrade is not delayed, because the degrade itself is usually collateral from the
/// #189 race rather than a real refusal of the open-pool half.
fn open_pool_rearm_cooldown_ticks(rejections: u32) -> u32 {
    /// Ceiling on the backoff, in owned ticks.
    const MAX_COOLDOWN_TICKS: u32 = 12;
    match rejections {
        0 => 0,
        n => (1u32 << (n - 1).min(31)).min(MAX_COOLDOWN_TICKS),
    }
}

/// Open-pool degrade bookkeeping (#190). Absent = the open-pool half is live.
///
/// The re-arm this drives is DEFENCE IN DEPTH, not a repair for an observed stuck seat: the reported
/// specimen was withdrawn — every seat seen degraded was flapping on the #189 sawtooth, not stuck.
/// The gap it closes is structural rather than field-observed. A seat that degrades and then never
/// recovers has no path back, because the only re-arm was `open_pool_degraded = false` in the
/// recovery-success arm; a healthy seat produces no recoveries, so it would hold the degraded shape
/// indefinitely. That reasoning survives the #189 fix, which is why the owned schedule stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenPoolDegrade {
    /// Consecutive re-arm attempts the relay refused.
    rejections: u32,
    /// Owned ticks still to skip before the next attempt.
    cooldown_ticks: u32,
    /// An attempt is on the wire, awaiting the relay's verdict: `EOSE` re-arms, `CLOSED` rejects.
    attempt_pending: bool,
}

impl OpenPoolDegrade {
    /// Freshly degraded: attempt the re-arm on the very next owned tick.
    fn new() -> Self {
        Self {
            rejections: 0,
            cooldown_ticks: 0,
            attempt_pending: false,
        }
    }

    /// What the next owned tick should do.
    fn on_tick(&mut self) -> RearmStep {
        if self.attempt_pending {
            // The previous attempt drew neither an EOSE nor a CLOSED within a full tick. Treat the
            // silence as a refusal rather than waiting on it: an attempt with no verdict pending is
            // exactly the timer-less park this fix exists to remove.
            self.reject();
            return RearmStep::Wait;
        }
        if self.cooldown_ticks > 0 {
            self.cooldown_ticks -= 1;
            return RearmStep::Wait;
        }
        self.attempt_pending = true;
        RearmStep::Attempt
    }

    /// The relay refused (or ignored) the re-arm.
    fn reject(&mut self) {
        self.attempt_pending = false;
        self.rejections = self.rejections.saturating_add(1);
        self.cooldown_ticks = open_pool_rearm_cooldown_ticks(self.rejections);
    }
}

/// What an owned tick does about a degraded open-pool half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RearmStep {
    /// Send the grouped offer REQ again.
    Attempt,
    /// Still cooling down, or still waiting on the last attempt's verdict.
    Wait,
}

/// Ask the relay to serve one trivial REQ on the CURRENT session and wait for its `EOSE`. True means
/// the relay is answering OUR subscriptions on THIS authenticated connection — the exact property the
/// #150 watchdog needs, and the one thing a heartbeat cannot demonstrate.
///
/// WHY NOT the own-heartbeat round-trip this replaced: a client cannot observe its own published
/// event coming back. `RelayPool::send_event_to` saves every event it publishes into the client's own
/// database (`pool/mod.rs:767`); when the relay echoes it, the inbound handler sees
/// `DatabaseEventStatus::Saved` and returns without emitting a notification (`relay/inner.rs:1215`,
/// notification only in the `NotExistent` arm). So the old probe could never succeed — the watchdog
/// declared a stall every `stall_threshold` on every node, healthy or not, and then drove a recovery
/// that could not succeed either (#171). A `limit(0)` REQ needs no cooperating publisher and no
/// stored events: the `EOSE` alone carries the proof.
async fn probe_relay_serves_our_reqs(
    client: &Client,
    seller_pubkey: nostr_sdk::PublicKey,
    timeout: Duration,
) -> bool {
    // Receiver BEFORE the REQ — an EOSE that lands first would otherwise be missed.
    let mut notifications = client.notifications();
    let probe_id = nostr_sdk::SubscriptionId::new(LIVENESS_PROBE_SUB_ID);
    // `limit(0)` asks for zero stored events, so the relay's only work is the EOSE. Scoped to our own
    // heartbeat address so the filter is narrow and unambiguous even if it ever did match.
    let probe = Filter::new()
        .kind(Kind::Custom(crate::heartbeat::SELLER_HEARTBEAT_KIND))
        .author(seller_pubkey)
        .identifier(crate::heartbeat::SELLER_HEARTBEAT_D)
        .limit(0);
    if let Err(error) = client.subscribe_with_id(probe_id, probe, None).await {
        opline!("seller node liveness probe: REQ could not be sent ({error})");
        return false;
    }
    tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayPoolNotification::Message {
                    message: nostr_sdk::RelayMessage::EndOfStoredEvents(id),
                    ..
                }) if id.to_string() == LIVENESS_PROBE_SUB_ID => return true,
                Ok(_) => continue,
                // The stream ending is itself a loss of liveness.
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// The seller's LIVE offer filters: the TARGETED (`#p == self`) filter always, plus — under
/// `open_pool` — the un-pinned open-pool filter. BOTH carry the `#t=maxplayer` namespace guard, so a
/// foreign event squatting the offer kind is never even delivered.
///
/// The two ride ONE subscription (a single REQ, OR-matched per NIP-01). Registered as a separate
/// second subscription the un-pinned filter delivers stored events but never LIVE offers, so a
/// running open-pool seller would ignore fresh untargeted offers — grouping them is load-bearing,
/// not tidiness.
///
/// `since` bounds: on a post-stall resubscribe BOTH filters carry the overlap cursor (only the stall
/// gap is missing). At boot the targeted filter is unbounded — stored offers addressed to this
/// seller are always wanted — while the open-pool filter is bounded by `offer_backfill_secs`: `0` is
/// live-only (`since(now)` + `limit(0)`), otherwise `since(now - window)` capped at
/// [`OFFER_BACKFILL_LIMIT`]. The classify-level deadline-expiry refusal is the staleness guard on
/// both paths, so a backfilled offer is never claimed just because it was returned.
fn offer_subscription_filters(
    seller_pubkey: nostr_sdk::PublicKey,
    open_pool: bool,
    offer_backfill_secs: u64,
    since: Option<nostr_sdk::Timestamp>,
    now: nostr_sdk::Timestamp,
) -> Vec<Filter> {
    let targeted = Filter::new()
        .kind(Kind::Custom(JOB_OFFER_KIND))
        .hashtag(crate::gateway::MAXPLAYER_TAG)
        .pubkey(seller_pubkey);
    let mut filters = vec![match since {
        Some(cursor) => targeted.since(cursor),
        None => targeted,
    }];
    if open_pool {
        let untargeted = Filter::new()
            .kind(Kind::Custom(JOB_OFFER_KIND))
            .hashtag(crate::gateway::MAXPLAYER_TAG);
        filters.push(match since {
            Some(cursor) => untargeted.since(cursor).limit(OFFER_BACKFILL_LIMIT),
            None if offer_backfill_secs > 0 => untargeted
                .since(nostr_sdk::Timestamp::from(
                    now.as_secs().saturating_sub(offer_backfill_secs),
                ))
                .limit(OFFER_BACKFILL_LIMIT),
            None => untargeted.since(now).limit(0),
        });
    }
    filters
}

/// The award/accept/reject subscription filter. Buyer-authored decisions about our claims ride ONE REQ
/// — the AWARD that selects a claim, and the ACCEPT that pay-binds a delivered result. A TARGETED
/// seller only claims offers addressed to it, and an award for such an offer p-tags it as the sole
/// winner, so scoping the REQ to its own pubkey suffices. An OPEN-POOL seller ALSO claims untargeted
/// offers it can LOSE; an award p-tags ONLY the winner, so a loser scoped to its own pubkey never
/// receives the award that should release its slot (#456) and holds that capacity until the lapse
/// timeout fires. When open-pool we therefore drop the pubkey scope and match by kind + hashtag alone
/// — mirroring the open-pool OFFER filter above, which is likewise unscoped. An unscoped REQ
/// therefore delivers events about OTHER seats' claims, and what keeps that a VISIBILITY change is
/// the handlers: `on_award` and `on_accept` each match the event's claim id against THIS node's
/// published claim id before binding, so a wider filter changes slot-release LATENCY, never money
/// authorization. That identity match is the load-bearing part — recording and claiming the offer
/// (offer_facts + job_creq) establishes only that the job is one of ours, never that the buyer chose
/// our claim, and a handler resting on those alone binds other seats' wins (#626). The filter is
/// static for the node's lifetime — no per-claim re-subscription to drift or leak.
fn award_filter(seller_pubkey: nostr_sdk::PublicKey, open_pool: bool) -> Filter {
    let base = Filter::new()
        .kinds([
            Kind::Custom(JOB_AWARD_KIND),
            Kind::Custom(JOB_ACCEPT_KIND),
            Kind::Custom(JOB_REJECT_KIND),
        ])
        .hashtag(crate::gateway::MAXPLAYER_TAG);
    if open_pool {
        base
    } else {
        base.pubkey(seller_pubkey)
    }
}

/// Drop the live socket and bring a fresh authenticated one up, returning once NIP-42 has completed
/// on the NEW connection.
///
/// ORDER IS LOAD-BEARING, and it is the whole of #171: `Relay::disconnect` emits
/// `RelayNotification::Shutdown` on the relay's own notification channel. A receiver taken BEFORE
/// the disconnect inherits that Shutdown, and [`relay_auth::wait_for_nip42_auth`] reads it as the
/// fatal "relay shutdown before NIP-42 authentication" — on a socket that in fact authenticated
/// fine. Recovery then failed 100% of the time (0 successes in 969 field attempts) while the node
/// kept heartbeating with dead subscriptions, because that Shutdown is relay-internal and never
/// reaches the pool notifications the run loop watches.
///
/// A `broadcast::Receiver` only observes sends made after it subscribes, so taking it AFTER the
/// disconnect cannot inherit our own teardown — while still taking it BEFORE `connect`, so the
/// one-shot `Authenticated` notification cannot be missed either. Both halves are required; this is
/// a free function so a test can drive exactly this sequence.
async fn reconnect_and_authenticate(
    client: &Client,
    relay: &nostr_sdk::prelude::Relay,
) -> Result<AuthWait, crate::relay_auth::RelayAuthError> {
    client.disconnect().await;
    let mut relay_notifications = relay.notifications();
    client.connect().await;
    client.wait_for_connection(CONNECT_WAIT).await;
    relay_auth::wait_for_nip42_auth(&mut relay_notifications, CONNECT_WAIT).await
}

/// Leave the SDK with nothing to re-`REQ` when the next socket comes up, and return how many
/// registrations survived — which must be zero.
///
/// `RelayPool::unsubscribe_all` is best-effort by construction: the relay-level loop removes each id
/// from the map and then sends its `CLOSE`, propagating the first send error with `?`
/// (`relay/inner.rs:1724-1736`), so one failed send leaves every remaining id registered. A single
/// leftover registration is the whole #189 hazard — it is the thing that gets re-sent pre-auth — so
/// the relay's own view is swept afterwards. `Relay::unsubscribe` removes before it sends, so the
/// sweep empties the map whether or not the socket can carry the `CLOSE`.
async fn clear_subscription_registrations(
    client: &Client,
    relay: &nostr_sdk::prelude::Relay,
) -> usize {
    client.unsubscribe_all().await;
    for id in relay.subscriptions().await.keys() {
        let _ = relay.unsubscribe(id).await;
    }
    let leftover = relay.subscriptions().await.len();
    if leftover > 0 {
        opline!(
            "seller node WARN: {leftover} subscription registration(s) survived the pre-reconnect \
             clear; they will be re-sent before NIP-42 completes"
        );
    }
    leftover
}

/// The refusal reason `on_offer` logs when an offer fails to parse.
///
/// A cross-version offer is a DISTINCT refusal from a malformed one (#146 / #117 refusal taxonomy):
/// it is well-formed under another protocol version, not broken tags, and an operator triaging a
/// quiet seller has to be able to tell those apart. Routing every parse failure through this one
/// function is what makes the taxonomy testable — collapsing the version arm back into the generic
/// bucket changes what this returns, so the tooth goes red instead of quietly passing.
fn offer_parse_refusal(error: &OfferParseError) -> String {
    match error {
        OfferParseError::UnsupportedVersion(version) => {
            format!("unsupported maxplayer protocol version {version:?}")
        }
        other => format!("unparseable ({other})"),
    }
}

/// The seller-receive classification on the node redeem path (finding S, ported from the daemon).
#[derive(Debug)]
enum RedeemDecision {
    /// Receive succeeded — finalize a receipt for this redeemed amount.
    Finalize(u64),
    /// Idempotent re-see: already spent AND a COMPLETED receipt exists — we already collected and
    /// receipted it. No-op; never double-collect / re-receipt.
    IdempotentNoOp,
    /// Fail closed — do NOT finalize; refuse (buffer for manual reconcile), with a named reason.
    Refuse(String),
}

/// True when a receive error is the mint reporting the token already spent — the one idempotent
/// surface on the node redeem path (the node's receipt dedup lives in the store, so there is no
/// journal "already receipted" string). Substring match: cdk surfaces no typed already-spent error.
fn is_already_spent(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("already spent") || lower.contains("already redeemed")
}

/// Classify a seller-receive result (finding S). The load-bearing rule: NEVER infer "our swap already
/// landed" from a pending-receive breadcrumb — the breadcrumb is written before EVERY swap, so it
/// proves only intent. Inferring collection from it would let a malicious buyer replay an
/// already-redeemed seller-locked token against a NEW same-value job and forge a receipt for zero new
/// funds (theft-of-service). The ONLY positive proof of OUR OWN prior collection is a COMPLETED
/// receipt for this job, read FAIL-CLOSED: already-spent + has_receipt(true) ⇒ idempotent no-op;
/// already-spent + has_receipt(false) ⇒ refuse (replay/theft or a genuine interrupted redeem — both
/// indistinguishable, so fail closed); has_receipt read error ⇒ refuse. Any non-already-spent error
/// also refuses. `has_receipt` is a closure so the store is read only on the already-spent branch and
/// the decision is unit-testable without a mint.
fn classify_redeem_outcome(
    receive_result: Result<u64, String>,
    has_receipt: impl FnOnce() -> Result<bool, String>,
) -> RedeemDecision {
    match receive_result {
        Ok(amount) => RedeemDecision::Finalize(amount),
        Err(error) if !is_already_spent(&error) => RedeemDecision::Refuse(error),
        Err(error) => match has_receipt() {
            Ok(true) => RedeemDecision::IdempotentNoOp,
            Ok(false) => RedeemDecision::Refuse(error),
            Err(read_err) => {
                RedeemDecision::Refuse(format!("receipt read failed (fail-closed): {read_err}"))
            }
        },
    }
}

/// The seal-sender guard: a payment settles a job ONLY when the authenticated NIP-17 seal sender is
/// the bound offer buyer (the pubkey folded into the seller-signed receipt preimage). A third party
/// can never pay-once and close someone else's job.
fn seal_sender_is_bound_buyer(seal_sender: &str, offer_buyer: &str) -> bool {
    seal_sender == offer_buyer
}

/// Whether a skipped offer earns a buyer-facing under-rate refusal (a feedback-kind `status=error`):
/// ONLY a rate-gate refusal (never a lapsed offer) that is targeted to THIS seller and priced below
/// its floor. Open-pool under-rate stays log-only (spam guard). Pure so the gate is unit-testable.
fn should_publish_under_rate_feedback(
    skip: SkipReason,
    targeted_to_self: bool,
    amount: u64,
    rate_sats: u64,
) -> bool {
    skip == SkipReason::RateGate && targeted_to_self && amount < rate_sats
}

/// Whether a job resumed from durable state on (re)start OCCUPIES AN EXECUTION SLOT (charter
/// invariant 4, fallback form): `awarded` (award seen, delivery not started) or `executing`
/// (interrupted mid-run). `delivered` jobs are left for the pay path; terminal (`paid`/`failed`)
/// never re-run. Pure so the selection is unit-testable. This is a COARSE state-only pre-filter for
/// the resume loop; [`resume_action`] refines it per-job over the durable delivery markers — because
/// an `awarded`/`executing` row can ALREADY have delivered (its state lagging, or its enqueue
/// interrupted after a push) and must NOT re-run the agent: a re-run re-executes a non-deterministic
/// agent and re-pushes a divergent commit (#552).
fn should_resume_execution(state: super::store::JobState) -> bool {
    // Same predicate the wire's `queue_depth` counts with, so "occupies a slot" has one definition
    // rather than one here and another in the heartbeat path.
    state.occupies_execution_slot()
}

/// The action a restart must take for a job that occupies an execution slot (#552). Pure over the
/// job's DURABLE facts so the whole terminal-shape enumeration is unit-testable in BOTH directions:
/// a replay (already-delivered work) must be caught, and a genuine mid-flight award must still run
/// (over-skipping trades a replay bug for a STALL bug — a lost award). Order matters: a
/// terminal/delivered/receipted row is skipped first; then a LAPSED offer (its absolute deadline has
/// passed) is failed BEFORE a pushed commit is finalized, so a pushed-but-lapsed row is never
/// re-signed into a fresh result after its deadline (which would emit a delivery past settlement —
/// the replay the wire regression forbids); only a slot-occupying, live-deadline row with NO delivery
/// evidence re-runs the agent (or finalizes a pushed commit). A missing/unreadable deadline degrades
/// to "live" — never fail a genuine award on an absent fact. `settled_elsewhere` (#563) is a
/// relay-DERIVED terminal fact — our own already-published result, or a buyer settlement for the offer
/// (settled with us or another seat) — supplied by the caller's bounded resume-refine; it joins the
/// delivery/receipt terminal class (skipped FIRST), so a row settled elsewhere is never re-driven even
/// with a live deadline.
#[derive(Debug, PartialEq, Eq)]
enum ResumeAction {
    /// Genuinely mid-flight (never delivered): re-drive the agent. THE FOIL — a real award, never
    /// abandon it.
    RunAgent,
    /// A delivery was pushed but its sign+enqueue was interrupted (deadline still live): complete it
    /// from the stored commit (re-sign + enqueue — deterministic + idempotent) WITHOUT re-running the
    /// agent or re-pushing.
    FinalizeFromPushed(String),
    /// Already delivered / receipted, or a terminal state: nothing to do, never re-run.
    SkipTerminal,
    /// The offer's own absolute deadline has already passed: the award can no longer be paid (a buyer
    /// will not honour a delivery past its deadline), so fail the job — release the slot, record it
    /// terminal — rather than re-run or finalize. #552's DURABLE primary signal (the deadline is
    /// journaled at claim time), the fix for stale `awarded` rows a restart would otherwise re-drive.
    SkipLapsed,
}

fn resume_action(
    state: super::store::JobState,
    has_delivery: bool,
    has_receipt: bool,
    settled_elsewhere: bool,
    pushed_commit: Option<String>,
    deadline_unix: Option<i64>,
    now_unix: i64,
) -> ResumeAction {
    // Terminal (delivered/paid/failed) never re-runs.
    if !state.occupies_execution_slot() {
        return ResumeAction::SkipTerminal;
    }
    // A delivery row, a collected receipt, or a relay-derived "settled elsewhere" marker (#563) is
    // durable proof the result already exists (ours) or the offer is terminal (a buyer settled — with
    // us or another seat) — skip even if the `state` column lagged behind `deliver_and_enqueue`'s
    // atomic advance. The SAME terminal class as delivery/receipt, so it precedes BOTH the
    // deadline-lapse and the pushed-commit branches: settled means the result exists / the buyer has
    // left, regardless of the deadline.
    if has_delivery || has_receipt || settled_elsewhere {
        return ResumeAction::SkipTerminal;
    }
    // #4 LAPSED — the DURABLE primary (#552): an offer whose own absolute deadline has passed is dead
    // (a buyer will not pay past it), so a stale `awarded`/`executing` row a restart re-reads must be
    // failed, not re-driven — re-running burns compute + re-pushes a divergent commit, and finalizing
    // would re-sign a fresh result AFTER the deadline. Checked BEFORE the pushed-commit branch so a
    // pushed-but-lapsed row fails rather than emitting a post-settlement delivery (the wire regression
    // `count(3403 with earlier 3406) == 0`). Boundary `deadline == now` counts as lapsed, matching
    // `classify_offer`. A `None` deadline (absent/unreadable) is treated as LIVE: never fail a genuine
    // award on a missing fact — over-skipping is the worse (a lost award).
    if let Some(deadline) = deadline_unix
        && deadline <= now_unix
    {
        return ResumeAction::SkipLapsed;
    }
    // Pushed-but-not-enqueued (deadline still live): the commit is on the remote; finalize from it
    // rather than re-running the (non-deterministic) agent, which would diverge the tree and re-push.
    match pushed_commit {
        Some(commit) => ResumeAction::FinalizeFromPushed(commit),
        None => ResumeAction::RunAgent,
    }
}

#[cfg(test)]
mod resume_action_tests {
    use super::{resume_action, ResumeAction};
    use crate::seller_node::store::JobState;

    const NOW: i64 = 1_000_000;
    const LIVE: Option<i64> = Some(NOW + 3_600); // deadline still in the future
    const LAPSED: Option<i64> = Some(NOW - 1); // deadline already passed
    const AT_NOW: Option<i64> = Some(NOW); // boundary: deadline == now counts as lapsed

    #[test]
    fn genuine_mid_flight_runs_the_agent() {
        // THE FOIL (red-provable in BOTH directions): awarded/executing, NO delivery evidence, LIVE
        // deadline ⇒ resume + run. Over-skip here (e.g. a wrong lapse comparison) and a real award
        // stalls — a lost award, worse than a replay — and this goes red.
        assert_eq!(resume_action(JobState::Awarded, false, false, false, None, LIVE, NOW), ResumeAction::RunAgent);
        assert_eq!(resume_action(JobState::Executing, false, false, false, None, LIVE, NOW), ResumeAction::RunAgent);
        // An absent/unreadable deadline degrades to "live": never skip a genuine award on a missing
        // fact.
        assert_eq!(resume_action(JobState::Awarded, false, false, false, None, None, NOW), ResumeAction::RunAgent);
    }

    #[test]
    fn settled_elsewhere_skips_the_live_residual_in_both_directions() {
        // #563 — the belt, red-provable in BOTH directions. The residual `resume_action` would
        // otherwise RunAgent: slot-occupying, no delivery/receipt, no pushed commit, LIVE deadline. A
        // relay-DERIVED settled_elsewhere=true reclassifies it terminal (the result already exists, or
        // the buyer settled — with us or another seat), so it SKIPS instead of re-driving. false leaves
        // THE FOIL untouched: a genuine live award still runs (over-skipping strands a real award —
        // worse than a bounded replay).
        for state in [JobState::Awarded, JobState::Executing] {
            assert_eq!(
                resume_action(state, false, false, true, None, LIVE, NOW),
                ResumeAction::SkipTerminal,
                "settled_elsewhere ⇒ skip the live residual (the result exists / the buyer left)"
            );
            assert_eq!(
                resume_action(state, false, false, false, None, LIVE, NOW),
                ResumeAction::RunAgent,
                "NOT settled ⇒ the FOIL still runs (absence never strands a real award)"
            );
        }
        // settled_elsewhere is terminal-class: it wins over a live deadline AND a pushed commit,
        // exactly like has_delivery/has_receipt — it is evaluated BEFORE the lapse and pushed branches.
        assert_eq!(
            resume_action(JobState::Awarded, false, false, true, Some("abc".into()), LIVE, NOW),
            ResumeAction::SkipTerminal,
            "settled_elsewhere precedes the pushed-commit finalize branch"
        );
    }

    #[test]
    fn lapsed_deadline_fails_the_stale_award() {
        // #4 LAPSED — the durable primary (#552): a slot-occupying row whose offer deadline has passed
        // can never be paid, so it is failed, never re-run/finalized. Holds with NO marker (the
        // dominant stale-`awarded` case) AND with a pushed marker (lapse BEFORE finalize — never emit
        // a 3403 after the deadline). Boundary `deadline == now` counts as lapsed.
        for pushed in [None, Some("abc".to_string())] {
            assert_eq!(
                resume_action(JobState::Awarded, false, false, false, pushed.clone(), LAPSED, NOW),
                ResumeAction::SkipLapsed
            );
            assert_eq!(
                resume_action(JobState::Executing, false, false, false, pushed.clone(), AT_NOW, NOW),
                ResumeAction::SkipLapsed
            );
        }
    }

    #[test]
    fn pushed_but_not_enqueued_finalizes_without_rerun() {
        // Deadline still LIVE + pushed commit + no delivery/receipt ⇒ finalize from the stored commit
        // (no agent re-run, no re-push).
        assert_eq!(
            resume_action(JobState::Executing, false, false, false, Some("abc".into()), LIVE, NOW),
            ResumeAction::FinalizeFromPushed("abc".into())
        );
        assert_eq!(
            resume_action(JobState::Awarded, false, false, false, Some("abc".into()), LIVE, NOW),
            ResumeAction::FinalizeFromPushed("abc".into())
        );
    }

    #[test]
    fn already_delivered_or_receipted_never_reruns() {
        // Delivery/receipt evidence wins over everything else, including a pushed marker and even a
        // lapsed deadline (a delivered row is never reclassified as lapsed).
        for state in [JobState::Awarded, JobState::Executing] {
            assert_eq!(resume_action(state, true, false, false, None, LIVE, NOW), ResumeAction::SkipTerminal);
            assert_eq!(resume_action(state, false, true, false, None, LIVE, NOW), ResumeAction::SkipTerminal);
            assert_eq!(
                resume_action(state, true, false, false, Some("c".into()), LIVE, NOW),
                ResumeAction::SkipTerminal
            );
            assert_eq!(resume_action(state, true, false, false, None, LAPSED, NOW), ResumeAction::SkipTerminal);
        }
    }

    #[test]
    fn terminal_states_never_rerun_regardless_of_markers() {
        for state in [JobState::Delivered, JobState::Paid, JobState::Failed] {
            for hd in [false, true] {
                for hr in [false, true] {
                    for se in [false, true] {
                        for pc in [None, Some("c".to_string())] {
                            for dl in [LIVE, LAPSED, None] {
                                assert_eq!(
                                    resume_action(state, hd, hr, se, pc.clone(), dl, NOW),
                                    ResumeAction::SkipTerminal,
                                    "terminal {state:?} must never re-run (hd={hd} hr={hr} se={se} pc={pc:?} dl={dl:?})"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// #562: the ceiling on how long a single delivery push may hold [`SellerNodeRunner::delivery_push_lock`].
/// The push's network leg is ALREADY bounded by the transport client's own per-leg request timeout
/// (`git_transport::DEFAULT_HTTP_LEG_TIMEOUT`, 120s); this sits ABOVE that so the real transport error
/// surfaces first (and is logged — the LEG-1 conflict detail), and this only fires for a pathological
/// NON-network hang, guaranteeing the lock is released so later deliveries are never starved. Generous
/// by design: a legit push finishes in seconds, so this never false-strands a slow-but-live push
/// (which would be the very strand bug #562 is about). The `whole-op > per-leg` ordering that keeps
/// this safe is no longer prose-only: the `const _` assert below binds the two clocks at COMPILE time.
const DELIVERY_PUSH_TIMEOUT: Duration = Duration::from_secs(150);

/// #563: make the two-clock ordering a COMPILE-TIME invariant instead of the cross-file prose above.
/// git2 has no whole-operation timeout, so `DELIVERY_PUSH_TIMEOUT` is the ONLY whole-op bound on the
/// delivery push; the push's single-leg cap is `git_transport::DEFAULT_HTTP_LEG_TIMEOUT` — the DEFAULT
/// long client, since the push runs with `short = false`, NOT the buyer money-path short client. If
/// that per-leg cap were ever raised to reach or exceed this whole-op bound, a single slow-but-LIVE
/// push leg could trip the `TimedOut` arm above — false-stranding a maybe-accepted delivery and
/// masking the real `Push(error)` (#562). This assert turns such a drift into a BUILD failure rather
/// than a silent live failure. `Duration::as_secs()` is const-stable and `assert!` runs in const
/// context, so this needs no `static_assertions` dependency (stays cleancut).
const _: () = assert!(
    DELIVERY_PUSH_TIMEOUT.as_secs() > crate::git_transport::DEFAULT_HTTP_LEG_TIMEOUT.as_secs(),
    "seller two-clock invariant (#563/#562): DELIVERY_PUSH_TIMEOUT (the delivery-push whole-operation \
     bound) must be strictly GREATER THAN git_transport::DEFAULT_HTTP_LEG_TIMEOUT (the per-HTTP-leg \
     request cap of the DEFAULT transport client the seller push uses); git2 has no whole-op timeout, \
     so if the per-leg cap reaches or exceeds the whole-op bound a single slow-but-live push leg trips \
     the whole-op TimedOut arm and false-strands a maybe-accepted delivery while masking the real Push error"
);

/// #562 delivery-push failure: distinguishes the transport/push error (which carries the reason for
/// the operator log — the LEG-1 detail) from the bounded-timeout firing, so both route to the SINGLE
/// `delivery_failed` handling while logging distinctly (never a new state).
#[derive(Debug)]
enum DeliveryPushErr {
    /// The push itself failed; the inner error carries the transport reason (409 / auth / io).
    Push(seller_git::SellerGitError),
    /// The push did not settle within [`DELIVERY_PUSH_TIMEOUT`] (seconds); the lock was released.
    TimedOut(u64),
}

/// #562: push a delivery under `lock` — serializing concurrent deliveries to this seat's ONE delivery
/// remote (concurrent `git-receive-pack` to one repo is what the relay 409s) — and bounded by
/// `timeout` so a hung push releases the lock rather than starving every later delivery. Pure over
/// (lock, timeout, push) so the serialization + timeout are unit-testable WITHOUT a relay. The lock is
/// held ONLY across the push and released the instant it settles or times out. The push oid is stable
/// (invariant 2), so ORDERING pushes never duplicates a delivery — this is exactly-once.
async fn serialized_bounded_push<Fut>(
    lock: &tokio::sync::Mutex<()>,
    timeout: Duration,
    push: impl FnOnce() -> Fut,
) -> Result<String, DeliveryPushErr>
where
    Fut: std::future::Future<Output = Result<String, seller_git::SellerGitError>>,
{
    let _guard = lock.lock().await;
    match tokio::time::timeout(timeout, push()).await {
        Ok(Ok(oid)) => Ok(oid),
        Ok(Err(error)) => Err(DeliveryPushErr::Push(error)),
        Err(_elapsed) => Err(DeliveryPushErr::TimedOut(timeout.as_secs())),
    }
    // `_guard` drops here — the lock is released the instant the push settles OR times out, never held
    // into the sign/enqueue tail.
}

#[cfg(test)]
mod serialized_bounded_push_tests {
    use super::{serialized_bounded_push, DeliveryPushErr};
    use crate::seller_git::SellerGitError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // #562 core: concurrent deliveries to ONE remote must serialize — the push closure records peak
    // concurrency, and under the lock peak is exactly 1. Red-on-revert: drop the `_guard` in
    // `serialized_bounded_push` and the 8 racers overlap ⇒ peak > 1 ⇒ this fails (that overlap IS the
    // concurrent git-receive-pack the relay 409s).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_pushes_serialize_to_one_at_a_time() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let (lock, inflight, peak) = (lock.clone(), inflight.clone(), peak.clone());
            handles.push(tokio::spawn(async move {
                serialized_bounded_push(&lock, Duration::from_secs(5), || async move {
                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await; // a racer would overlap here if unserialized
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    Ok::<_, SellerGitError>(format!("oid{i}"))
                })
                .await
            }));
        }
        for handle in handles {
            handle.await.expect("task joined").expect("push ok");
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "delivery pushes to one remote must serialize to one at a time (#562)"
        );
    }

    // #562 constraint (lead 37896): a hung push must NOT starve later deliveries — it times out,
    // returns TimedOut (→ delivery_failed at the caller), and RELEASES the lock so the next delivery
    // proceeds promptly rather than blocking behind the hung one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hung_push_times_out_and_frees_the_lock() {
        let lock = tokio::sync::Mutex::new(());
        let hung = serialized_bounded_push(&lock, Duration::from_millis(50), || async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<_, SellerGitError>("never".to_string())
        })
        .await;
        assert!(matches!(hung, Err(DeliveryPushErr::TimedOut(_))), "a hung push must time out");
        // The lock is free again: the next push acquires it and completes promptly (not starved).
        let next = tokio::time::timeout(
            Duration::from_secs(2),
            serialized_bounded_push(&lock, Duration::from_secs(5), || async move {
                Ok::<_, SellerGitError>("next-oid".to_string())
            }),
        )
        .await
        .expect("the next delivery must not be starved behind the timed-out push");
        assert!(matches!(next, Ok(oid) if oid == "next-oid"));
    }
}

/// The journaled offer facts for a parsed offer — the ONE place a wire offer becomes a stored row.
///
/// Extracted from the claim path so this mapping is reachable by a test. Everything downstream
/// reads the ROW, not the event: the award arm authorizes against its buyer, the pay path takes its
/// amount/unit as the redeem terms, and execution (possibly a restart later) takes its
/// `requested_agent` as the harness to dispatch. A field dropped here is a field that silently does
/// not exist for the rest of the job's life, which is why the mapping is a named function with its
/// own tooth rather than a struct literal inlined at the only call site.
fn offer_row(job_id: &str, buyer_pubkey: &str, offer: &ParsedOffer) -> super::store::Offer {
    super::store::Offer {
        offer_id: job_id.to_owned(),
        buyer_pubkey: buyer_pubkey.to_owned(),
        amount_sats: offer.amount,
        unit: offer.unit.clone(),
        task: offer.task.clone(),
        deadline_unix: offer.deadline_unix as i64,
        targeted: offer.is_targeted(),
        requested_agent: offer.requested_agent.clone(),
        // #686: the buyer's declared output type is mandatory on the wire (`parse_offer` refuses an
        // offer without it), so it is always `Some` here. It becomes `None` only for a row written
        // before the column existed.
        output: Some(offer.output.clone()),
    }
}

/// The agent prompt for a stored job — the ONE place a stored offer row becomes the hired agent's
/// prompt.
///
/// Extracted from `execute_job` for the same reason [`offer_row`] was extracted from the claim path:
/// this read is what decides which of the journaled facts the agent is ever told, and inlined at its
/// single call site no test can reach it. It reads the ROW, not the event — execution can be a
/// RESTART away from the claim, so the row is all a resumed job has.
///
/// `pub` so the shipped CLI crate can assert this seam under ITS OWN feature set: `maxplayer-core`'s
/// wallet-gated unit tests do not run under `cargo test -p maxplayer-core` (the `wallet` feature is
/// off by default there), so a tooth that lives only here is invisible to the repo's declared check
/// set. See `crates/maxplayer/tests/seller_declared_output.rs`.
pub fn job_prompt(
    offer: &super::store::Offer,
    git_remote: &str,
    deadline_unix: u64,
    memory_section: Option<&str>,
) -> String {
    compose_agent_prompt(
        &offer.task,
        git_remote,
        deadline_unix,
        offer.output.as_deref(),
        memory_section,
    )
}

/// The rendered read-on-start memory section for a seller at `home_root`, or `None` when there is
/// nothing to inject (#828).
///
/// This is the impure half of the prompt seam, kept OUT of [`job_prompt`] on purpose. `job_prompt`
/// stays pure over the stored row — the property its doc above exists to protect — and the one read
/// that touches the filesystem lives here, where a test can drive it with a real directory.
///
/// **It only ever READS.** It must never call `seller_memory::ensure_memory_dir`: that seeds a
/// NON-EMPTY index (it links `operator-notes.md`), and `memory_enabled` defaults to TRUE, so seeding
/// from this path would flip every existing seller from inert to injecting on its next job without
/// any operator writing a word. Creating memory stays an operator act.
///
/// **It degrades and never propagates.** `read_on_start_section` REFUSES an index over
/// [`MAX_MEMORY_INDEX_BYTES`](crate::seller_memory::MAX_MEMORY_INDEX_BYTES) with `InvalidData`, and
/// an unreadable file is an error too. Neither may fail a job: this is diagnostic/economic context
/// that never feeds the pay gate, the journal or the receipt bind, so a job that would otherwise
/// have been delivered and PAID must not die over it. An error is logged and read as "no memory".
pub fn job_memory_section(
    home_root: &std::path::Path,
    config: &crate::home::SellerMemoryConfig,
) -> Option<String> {
    if !config.memory_enabled {
        return None;
    }
    let dir = crate::seller_memory::memory_dir(home_root);
    match crate::seller_memory::read_on_start_section(
        &dir,
        config.read_on_start_template_path.as_deref(),
    ) {
        Ok(section) => section,
        Err(error) => {
            opline!("seller node memory read skipped ({error}); running the job without memory");
            None
        }
    }
}

/// #591: how a job's delivery workdir is provisioned — a from-scratch empty repo, or a clone of a
/// served contribution's pinned base at `base_oid` (the fork tip the agent extends). Pure over the
/// stored pin so `execute_job`'s routing is unit-testable without a live node.
#[derive(Debug, PartialEq, Eq)]
enum DeliveryWorkdirPlan {
    Empty,
    ContributionClone {
        clone_url: String,
        base_branch: String,
        base_oid: String,
        branch: String,
    },
}

#[derive(Debug)]
enum DeliveryWorkdirError {
    Git(seller_git::SellerGitError),
    Refused(ProvisionRefusal),
}

impl std::fmt::Display for DeliveryWorkdirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git(error) => write!(f, "{error}"),
            Self::Refused(refusal) => write!(f, "environment refused: {refusal:?}"),
        }
    }
}

impl From<seller_git::SellerGitError> for DeliveryWorkdirError {
    fn from(error: seller_git::SellerGitError) -> Self {
        Self::Git(error)
    }
}

impl From<ProvisionRefusal> for DeliveryWorkdirError {
    fn from(refusal: ProvisionRefusal) -> Self {
        Self::Refused(refusal)
    }
}

struct ProcessEnvEffects;

impl EnvEffects for ProcessEnvEffects {
    fn run(&self, argv: &[String]) -> Result<EffectOutput, EnvProvisionError> {
        let (program, args) =
            argv.split_first()
                .ok_or_else(|| EnvProvisionError::EnvUnresolvable {
                    detail: "empty provisioning command".to_owned(),
                })?;
        let output = std::process::Command::new(program)
            .args(args)
            .output()
            .map_err(|_| EnvProvisionError::BackendUnavailable {
                backend: program.clone(),
            })?;
        Ok(EffectOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        })
    }
}

fn available_container_runtime() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    ["podman", "docker"].into_iter().find_map(|runtime| {
        std::env::split_paths(&path)
            .any(|dir| dir.join(runtime).is_file())
            .then(|| runtime.to_owned())
    })
}

fn checks_refusal(error: crate::checks::ChecksError) -> ProvisionRefusal {
    use crate::checks::ChecksError;
    match error {
        ChecksError::ReservedPath(path) => ProvisionRefusal::ReservedPath { path },
        ChecksError::MissingEnvLock(detail) | ChecksError::MissingFlake(detail) => {
            ProvisionRefusal::EnvLockMissing { detail }
        }
        ChecksError::TooLarge => ProvisionRefusal::DeclarationUnparsable {
            detail: "checks declaration exceeds size limit".to_owned(),
        },
        ChecksError::Malformed(detail) => ProvisionRefusal::DeclarationUnparsable { detail },
        ChecksError::UnsupportedSchema(schema) => ProvisionRefusal::DeclarationUnparsable {
            detail: format!("unsupported checks schema {schema}"),
        },
        ChecksError::InvalidFlakePath => ProvisionRefusal::DeclarationUnparsable {
            detail: "invalid flake path".to_owned(),
        },
        ChecksError::InvalidImage => ProvisionRefusal::DeclarationUnparsable {
            detail: "invalid container image".to_owned(),
        },
        ChecksError::EmptyCommand => ProvisionRefusal::DeclarationUnparsable {
            detail: "empty checks command".to_owned(),
        },
        ChecksError::TreeRead(detail) => ProvisionRefusal::DeclarationUnparsable { detail },
    }
}

/// Route a job to its workdir provisioning: a recorded contribution pin ⇒ clone at `base_oid` on a
/// per-job fork branch carrying the full job id; no pin ⇒ the empty-workdir default (unchanged).
fn plan_delivery_workdir(
    pin: Option<super::store::ContributionPin>,
    job_id: &str,
) -> DeliveryWorkdirPlan {
    match pin {
        Some(pin) => DeliveryWorkdirPlan::ContributionClone {
            clone_url: pin.clone_url,
            base_branch: pin.base_branch,
            base_oid: pin.base_oid,
            branch: format!("maxplayer/contribution/{job_id}"),
        },
        None => DeliveryWorkdirPlan::Empty,
    }
}

/// Provision a job's delivery workdir from its STORED contribution pin — the single routing seam
/// `execute_job` uses on BOTH the fresh-award and restart/resume paths. Reads the pin, plans, and
/// initializes: a recorded pin clones the pinned base at `base_oid` onto the per-job fork branch,
/// captures any checks declaration from that commit's tree, provisions its environment, and persists
/// the exact declaration bytes plus resolved environment reference. No pin gives the empty-workdir
/// default (a from-scratch job, unchanged). A pin READ error is mapped to an init error so the caller
/// fails the job rather than silently degrading a served contribution to an empty workdir.
///
/// Returns the base the delivery snapshot must be parented on: `Some(base_oid)` for a contribution,
/// `None` for a from-scratch job. `execute_job` threads it into the snapshot so a contribution's
/// delivery commit DESCENDS from `base_oid` by construction — the invariant the buyer's descendant
/// gate enforces (#616). Single-sourced: the snapshot is parented on exactly the base provisioned.
async fn provision_delivery_workdir(
    store: &super::store::SellerStore,
    home: &MaxplayerHome,
    job_id: &str,
    workdir: std::path::PathBuf,
    identity: DeliveryAgentIdentity,
) -> Result<Option<String>, DeliveryWorkdirError> {
    let pin = store
        .contribution_pin(job_id)
        .map_err(|error| seller_git::SellerGitError::Io(format!("contribution pin read failed: {error}")))?;
    let base_oid = match plan_delivery_workdir(pin, job_id) {
        DeliveryWorkdirPlan::Empty => {
            seller_git::init_empty_delivery_workdir_off_runtime(workdir, identity).await?;
            // From-scratch: no pinned base ⇒ the delivery is a root commit (snapshot base_oid = None).
            return Ok(None);
        }
        DeliveryWorkdirPlan::ContributionClone {
            clone_url,
            base_branch,
            base_oid,
            branch,
        } => {
            // NIP-98 auth for a relay-git base fetch; None for a public https base (libgit2 only
            // invokes the credential when the server demands it). The secret enters the fetch closure
            // only and is never logged (PushAuth: callers must not print it).
            let auth = home::read_secret_key_hex(home)
                .ok()
                .map(|secret_key_hex| seller_git::PushAuth { secret_key_hex });
            seller_git::init_contribution_workdir_off_runtime(
                workdir.clone(), identity, clone_url, base_branch, base_oid.clone(), branch, auth,
            )
            .await?;
            base_oid
        }
    };

    let store = store.clone();
    let checks_workdir = workdir.clone();
    let checks_base_oid = base_oid.clone();
    let checks_job_id = job_id.to_owned();
    tokio::task::spawn_blocking(move || -> Result<(), DeliveryWorkdirError> {
        let repo = git2::Repository::open(&checks_workdir).map_err(|error| {
            seller_git::SellerGitError::Io(format!("open checked workdir: {error}"))
        })?;
        let oid = git2::Oid::from_str(&checks_base_oid).map_err(|error| {
            seller_git::SellerGitError::Io(format!("invalid checked base oid: {error}"))
        })?;
        let commit = repo
            .find_commit(oid)
            .map_err(|error| {
                seller_git::SellerGitError::Io(format!("find checked base commit: {error}"))
            })?;
        let tree = commit.tree().map_err(|error| {
            seller_git::SellerGitError::Io(format!("read checked base tree: {error}"))
        })?;
        let base_tree = (&repo, &tree);
        if crate::checks::BaseTree::blob_at(&base_tree, crate::checks::DECLARATION_PATH)
            .map_err(checks_refusal)?
            .is_none()
        {
            return Ok(());
        }
        let (declaration_bytes, declaration) =
            env_provision::capture_job_checks(&base_tree).map_err(checks_refusal)?;
        let env_lock_ref =
            crate::checks::env_lock_ref(&declaration, &base_tree).map_err(checks_refusal)?;
        let mut backend = env_provision::resolve_backend(&declaration).map_err(|error| {
            DeliveryWorkdirError::Refused(ProvisionRefusal::Unprovisionable(error))
        })?;
        if let EnvBackend::NixFlake { workdir, .. } = &mut backend {
            *workdir = checks_workdir.join(&*workdir);
        }
        let runner = HostEnvRunner {
            container_runtime: available_container_runtime(),
            mount_dir: checks_workdir,
        };
        env_provision::provision(&ProcessEnvEffects, &runner, backend, EnvPosture::Checks)
            .map_err(|error| {
                DeliveryWorkdirError::Refused(ProvisionRefusal::Unprovisionable(error))
            })?;
        store
            .record_job_checks(
                &checks_job_id,
                &declaration_bytes,
                declaration.env_kind,
                &env_lock_ref,
                now_unix(),
            )
            .map_err(|error| {
                DeliveryWorkdirError::Git(seller_git::SellerGitError::Io(format!(
                    "checks persistence failed: {error}"
                )))
            })?;
        Ok(())
    })
    .await
    .map_err(|error| {
        seller_git::SellerGitError::Io(format!("checks provisioning task failed: {error}"))
    })??;
    Ok(Some(base_oid))
}

/// #613 — the seller EMIT dual of the buyer's contribution parse. If `job_id` is a served
/// contribution (a recorded pin), build the contribution result envelope tags to APPEND to the
/// standard git result: the offer echo (`job-class`/`target-repo`/`base`/`accepts`) + the
/// seller-signed `sig/seller-contribution` authorship tuple. The tuple is signed over the SAME fork
/// repo/branch/tip the result carries ([`crate::contribution::seller_contribution_result_parts`]),
/// through the signer actor (the seller key never leaves it), so the buyer's
/// `parse_contribution_result_echo` + pre-pay tuple verify round-trip.
///
/// - `Ok(None)`  — a from-scratch job (no pin): the caller leaves the standard result unchanged.
/// - `Ok(Some)`  — the contribution envelope tags to append.
/// - `Err(msg)`  — a pin read / envelope build / tuple sign failure. Fail-closed: the caller emits a
///   delivery-failed feedback and returns rather than delivering a from-scratch shape the buyer
///   refuses. A free fn (store + signer, not `self`) so the real read -> build -> sign glue is
///   unit-testable without standing up the whole runner.
async fn contribution_result_envelope_tags(
    store: &super::store::SellerStore,
    signer: &super::signer::SignerHandle,
    job_id: &str,
    seller_pubkey_hex: &str,
    fork_repo: &str,
    fork_branch: &str,
    commit_oid: &str,
) -> Result<Option<Vec<gateway::TagSpec>>, String> {
    let Some(pin) = store
        .contribution_pin(job_id)
        .map_err(|error| format!("contribution pin read failed ({error})"))?
    else {
        return Ok(None);
    };
    let (offer, tuple) = crate::contribution::seller_contribution_result_parts(
        job_id,
        seller_pubkey_hex,
        &pin.owner_pubkey,
        &pin.clone_url,
        &pin.base_branch,
        &pin.base_oid,
        fork_repo,
        fork_branch,
        commit_oid,
    )
    .map_err(|error| format!("contribution result envelope build failed ({error})"))?;
    let tuple_sig = signer
        .sign_receipt_hash(tuple.digest_hex())
        .await
        .map_err(|error| format!("contribution tuple sign: signer actor gone ({error})"))?
        .map_err(|error| format!("contribution tuple sign refused ({error})"))?;
    Ok(Some(crate::contribution::contribution_result_tags(
        &offer, &tuple_sig,
    )))
}

/// Decide whether to claim `offer`, applying the always-on money-safety gates in the legacy order:
/// a lapsed offer is refused BEFORE its deadline is re-derived (never resurrect a stale offer with a
/// fresh `now + timeout`), then the #604 offer-age gate (a long-aged historical the backfill keeps
/// re-surfacing is refused so it cannot park a slot on work that will not be awarded), then buyer
/// eligibility, then the targeting/rate gate, then the harness the offer asked for.
/// Pure over (offer, config, registry, buyer, now, offer_created_at).
///
/// Buyer eligibility is ONE clause over three ADDITIVE, INDEPENDENT controls (#923), and no control
/// is inferred from another being empty or populated. On the TARGETED surface admission is the UNION
/// `buyer_is_named || accept_open_targeted`: `accept_offers_only_from` is an always-admits set of
/// buyers the operator chose, and `accept_open_targeted` ADDITIONALLY admits a buyer it did not name.
/// The untargeted (open-pool) surface is left wholly to `claim_open_pool` in the rate gate, so all
/// three controls stay separately switchable.
///
/// ⛔ THE ALLOWLIST IS AN ADMIT-LIST FOR TARGETED WORK, NEVER A VETO OVER THE OPT-IN BESIDE IT. Until
/// #923 the populated-allowlist fence returned BEFORE the targeted opt-in was consulted, so a
/// populated list made `accept_open_targeted` INERT on both surfaces: an operator could not keep
/// trusted buyers while temporarily opening a public route, and the config said one thing while the
/// seat did another. Restoring that precedence re-breaks #923 — the order is asserted, not left to
/// reading. Widening admission BEYOND these three controls is a defect, not an improvement: every
/// clause here is a permission grant an operator cannot take back once a stranger has claimed.
///
/// The harness gate is a CLAIM-time decision, not a delivery-time one: a node that cannot run the
/// requested harness never parks a claim at all, so the buyer's offer stays visible to a seller
/// that can, instead of being answered by one that would fail later.
fn classify_offer(
    offer: &ParsedOffer,
    seller: &crate::home::SellerConfig,
    agents: &LiveRoster,
    seller_pubkey: &str,
    buyer_pubkey: &str,
    now_unix: u64,
    offer_created_at: u64,
) -> ClaimDecision {
    // Offer-freshness (money-safety): an offer whose own absolute deadline already passed is dead,
    // refused here before `job_deadline_unix` could hand it a fresh window.
    if offer.deadline_unix <= now_unix {
        return ClaimDecision::Skip(SkipReason::Lapsed);
    }
    // #604 offer-age gate — DISTINCT from the self-declared deadline above. The periodic backfill
    // re-ingests every stored offer in its lookback each tick; a long-aged, never-awarded historical
    // with a far-future deadline clears the deadline gate but must NOT be (re-)admitted — claiming it
    // holds an execution slot for the full claim-lapse and starves live offers (`SlotsBusy`). Refuse
    // by WIRE age (`now − created_at`); the threshold spans the backfill recovery horizon so a
    // genuinely-recent offer is untouched. Independent of the live claim-lapse guard — a claimed slot
    // still lapses on time; this only stops the offer being claimed in the first place.
    if now_unix.saturating_sub(offer_created_at) > MAX_OFFER_ADMIT_AGE_SECS {
        return ClaimDecision::Skip(SkipReason::TooOld);
    }
    // Buyer-eligibility, in two independent clauses. Consulted after the lapsed refusal but before
    // the rate/harness gates, so an ineligible buyer's offer is declined outright; the caller names
    // the declined pubkey in the skip log (this pure fn cannot, and stays silent to the buyer).
    let buyer_is_named = seller.accept_offers_only_from.iter().any(|allowed| allowed == buyer_pubkey);
    // TARGETED surface (#923): admit on the UNION of the two controls that govern it — a buyer the
    // operator NAMED, or `accept_open_targeted` for one it did not. Neither cancels the other, so
    // toggling the public route leaves the private fallback intact and vice versa.
    //
    // ⛔ SCOPED TO OFFERS WHOSE `p` TAG IS THIS SEAT. That scope is what leaves the untargeted
    // (open-pool) surface wholly to `claim_open_pool` in the rate gate below — the third, separate
    // control. An offer targeting ANOTHER seat is refused there, never admitted here.
    if offer.seller_pubkey.as_deref() == Some(seller_pubkey)
        && !buyer_is_named
        && !seller.accept_open_targeted
    {
        // TWO refusals over one condition, DELIBERATELY NOT FOLDED: an operator who wrote a list
        // must be sent to the list, and one who wrote none must be sent to the flag. The same string
        // would send the second operator hunting a list that does not exist.
        return ClaimDecision::Skip(if seller.accept_offers_only_from.is_empty() {
            SkipReason::OpenTargetedRefused
        } else {
            SkipReason::NotAllowlisted
        });
    }
    if rate_gate_allows(offer, seller_pubkey, seller.rate_sats, seller.claim_open_pool).is_err() {
        return ClaimDecision::Skip(SkipReason::RateGate);
    }
    if !agents.serves(offer.requested_agent.as_deref()) {
        return ClaimDecision::Skip(SkipReason::AgentUnavailable);
    }
    ClaimDecision::Claim {
        deadline_unix: crate::seller::job_deadline_unix(offer, seller, now_unix),
    }
}

/// The boot siren for a seat whose config can claim NOTHING: no buyer named, and neither open
/// surface opted in to. Returns the operator line, or `None` when at least one route in exists.
///
/// ⛔ THIS EXISTS BECAUSE THE THREE-KNOB MIGRATION IS SILENT, AND THE SILENCE IS THE HAZARD, NOT THE
/// STRICTNESS. Before the split, an empty `accept_offers_only_from` meant accept-all on the targeted
/// surface; after it, that same config accepts no one. Every already-deployed seller with no
/// allowlist — including outside operators running our releases — stops accepting targeted work the
/// moment it upgrades, and nothing about it is an error: the config still parses, the node still
/// boots, the relay subscription is still live, and the seat still advertises. It simply never
/// claims again. The strict default is intended and stays; a seat going quiet without saying so is
/// not, so this is REQUIRED rather than advisory.
///
/// Pure over the config so it is testable without a relay, a home lock or a boot. The caller emits.
fn unreachable_seat_warning(seller: &crate::home::SellerConfig) -> Option<String> {
    // Counted, never `is_empty()`. An entry is matched byte-for-byte against a wire pubkey, so one
    // that cannot be a wire pubkey is not a narrow route in — it is no route, while still fencing
    // everyone else out. Keying this on emptiness silenced the siren for exactly the seat it exists
    // to catch: a list of typos claims nothing and looked configured.
    let usable_buyers = seller
        .accept_offers_only_from
        .iter()
        .filter(|entry| crate::home::buyer_pubkey_is_reachable(entry))
        .count();
    if usable_buyers > 0 {
        return None;
    }
    // With NO list, either open flag is a way in. With a populated one the fence shuts both
    // surfaces, so the flags cannot rescue a list that matches nobody.
    if seller.accept_offers_only_from.is_empty()
        && (seller.accept_open_targeted || seller.claim_open_pool)
    {
        return None;
    }
    if !seller.accept_offers_only_from.is_empty() {
        return Some(format!(
            "seller node WARNING: this seat can claim NOTHING as configured — all {} entr(y/ies) in \
             [seller] accept_offers_only_from are unusable, and a populated allowlist fences out \
             everyone else on BOTH surfaces, so no offer can reach this seat at all. {}. Correct \
             the entries, or remove them. THREE ROUTES BACK IN: {}.",
            seller.accept_offers_only_from.len(),
            crate::home::USABLE_BUYER_ENTRY,
            crate::home::ROUTES_BACK_IN
        ));
    }
    Some(format!(
        "seller node WARNING: this seat can claim NOTHING as configured — it names no buyers \
         (accept_offers_only_from is empty), does not accept targeted offers from buyers it has not \
         named (accept_open_targeted=false), and does not claim the open pool \
         (claim_open_pool=false). It will advertise and stay connected, but never claim a job. If \
         this seat used to serve, an upgrade closed the targeted surface that an empty allowlist \
         used to leave open. THREE ROUTES BACK IN: {}.",
        crate::home::ROUTES_BACK_IN
    ))
}

/// Resolve + report the harness registry at boot: one PASS/FAIL line per configured preset, then
/// either a loud degrade line (some resolved) or a refusal (none did).
///
/// The three outcomes are deliberately distinct. ALL configured presets failing REFUSES the boot —
/// a node with no launchable harness that still claimed work would take jobs it must then fail.
/// SOME failing DEGRADES loudly and serves with the remainder, advertising only those, because a
/// two-harness seller that loses one is still a working one-harness seller. A node with no `agents`
/// list at all resolves to its single `agent_command` and prints nothing new.
fn boot_agent_registry(home: &MaxplayerHome) -> Result<AgentRegistry, NodeError> {
    let Some(seller) = home.config.seller.as_ref() else {
        // No `[seller]` section: nothing serves offers, and the run loop already no-ops. An empty
        // registry keeps that path unchanged rather than turning it into a boot failure.
        return Ok(AgentRegistry::new(Vec::new()));
    };
    let resolved = crate::seller_agents::resolve(
        seller,
        &home.config.agents,
        crate::agent_presets::AdapterHost::for_sandbox(home.config.sandbox.as_ref()),
    )
    .map_err(NodeError::Agents)?;
    for verdict in &resolved.verdicts {
        opline!("seller node agent {}", verdict.line());
    }
    if let Some(degraded) = resolved.degrade_line() {
        opline!("{degraded}");
    } else if !resolved.registry.advertised().is_empty() {
        opline!(
            "seller node agents ready: {:?} (execution concurrency set by [seller] slots)",
            resolved.registry.advertised()
        );
    }
    Ok(resolved.registry)
}

/// Which agent-run failures implicate the HARNESS, and how.
///
/// Attribution, not severity, decides this. `None` means the failure says nothing about the harness,
/// so the roster must not narrow — a gate that drops a harness for our own refusal is an outage we
/// inflict on ourselves.
///
/// - [`ExecError::AcpRequired`] — the binary has no `acp` feature, so NO harness here can run a turn.
///   A named capability, and no probe is scheduled: retrying cannot add a build feature.
/// - [`ExecError::Config`] — a misconfiguration surfaced before the run. Also structural, and the
///   remedy is derived from the reported detail rather than assumed to be a rebuild: a harness whose
///   barrier is an unset provider is not fixed by rebuilding, and saying so would be a lie.
/// - [`ExecError::DeadlineExceeded`] — the deadline-derived clock expired. Typed upstream, so it is
///   attributable to the job budget and reaches the roster as a non-striking failure.
/// - [`ExecError::Agent`] — deliberately UNPROVEN. Remaining agent errors do not carry enough
///   evidence to distinguish transient from structural, so the self-probe decides.
/// - [`ExecError::Policy`] — OUR OWN refusal (e.g. an un-typeable delivery oid). Never the harness.
fn harness_fault_for(error: &ExecError) -> Option<ExecutionFailure> {
    match error {
        ExecError::AcpRequired => Some(ExecutionFailure::Harness(Fault::Incapable(
            MissingCapability::AcpFeature,
        ))),
        ExecError::Config(detail) => Some(ExecutionFailure::Harness(Fault::Incapable(
            MissingCapability::HarnessConfig(detail.clone()),
        ))),
        ExecError::DeadlineExceeded => Some(ExecutionFailure::DeadlineExceeded),
        ExecError::Agent(_) => Some(ExecutionFailure::Harness(Fault::Unproven)),
        ExecError::Policy(_) => None,
    }
}

/// How long a harness self-probe turn may take. Deliberately short: the probe asks for one tiny file,
/// so a harness that cannot manage that inside this window is not one to hand a paid job to either.
///
/// ⚠ This is an ACP *idle* timeout: it resets on every stream event (`driver`/`acp_driver`), so it
/// bounds the gap BETWEEN updates, not the whole turn. A harness that drip-feeds a status update every
/// few seconds never trips it and can hold a probe open forever — which is exactly why the restore path
/// wraps the probe in an outer wall-clock ceiling too ([`HARNESS_PROBE_WALL_TIMEOUT`], #301).
const HARNESS_PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// Outer WALL-CLOCK ceiling on a single restore self-probe, wrapping the whole `run_harness_probe`
/// turn. Strictly above [`HARNESS_PROBE_TIMEOUT`] (asserted below) because the idle timeout structurally
/// cannot bound a harness that keeps its stream alive with periodic events: without this ceiling such a
/// hang leaves the harness marked `probing` forever and it is never re-probed (#301). Set well clear of
/// the idle cap so a merely SLOW-but-live probe still completes and is not mis-faulted as hung.
const HARNESS_PROBE_WALL_TIMEOUT: Duration = Duration::from_secs(300);

// The wall ceiling only helps if it sits ABOVE the idle cap: below it, it would kill live-but-slow
// probes the idle timeout was content with. A compile-time guard, since both are constants (#301).
const _: () = assert!(
    HARNESS_PROBE_WALL_TIMEOUT.as_secs() > HARNESS_PROBE_TIMEOUT.as_secs(),
    "the wall-clock probe ceiling must be strictly above the ACP idle timeout"
);

/// Prefix of a probe sentinel.
///
/// Called a SENTINEL and never a "token" on purpose: in this crate a token is **Cashu ecash**, i.e.
/// money (`payload.to_token()`, `token_hash`, "already-spent token" — all in this same file). A probe
/// sentinel is an opaque nonce that spends nothing, and naming it "token" made a reviewer stop and ask
/// whether probing costs sats. A name that forces that question on a money path is wrong whatever the
/// answer is.
const PROBE_SENTINEL_PREFIX: &str = "maxplayer-probe";

/// Prefix of the NON-SECRET label naming a probe's workdir. Distinct from the sentinel's prefix so no
/// sentinel value can ever be a substring of a workdir path.
const PROBE_DIR_PREFIX: &str = "maxplayer-selfprobe";

/// One probe's two values: a non-secret label for its workdir, and the secret it must produce.
///
/// **They are separate, and that separation is the property.** A workdir path is trivially observable
/// by the harness — it *is* its own cwd — so any harness that echoes a path into a file (an error
/// trace, a log header, a partial write) would reproduce a sentinel that lived in that path, passing
/// the probe **without doing the task**. A discriminator must not appear in the environment it is
/// discriminating. Keying the workdir off the sentinel is exactly that mistake.
struct ProbeIdentity {
    /// Names the workdir. The harness sees it and may echo it freely; nothing depends on its secrecy.
    dir_label: String,
    /// The secret the harness must write. Reaches it ONLY through the prompt — never through the
    /// filesystem, the cwd, or any argv.
    sentinel: String,
}

/// Mint one probe's identity — the ONE place either value's shape is decided.
///
/// Both are then PASSED to the prompt, the workdir name and the readback, so no second literal exists
/// anywhere that could drift out of step. The sentinel carries sub-second entropy the label does not,
/// so it is neither equal to nor derivable from anything the harness can read off its own path. The
/// `attempt` index distinguishes the retries of one harness's probe (#472), so two turns that fall in
/// the same second never share a workdir and no attempt can inherit an earlier one's artifact.
fn mint_probe_identity(harness: usize, attempt: usize, now_unix: u64) -> ProbeIdentity {
    let entropy = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or(0);
    ProbeIdentity {
        dir_label: format!("{PROBE_DIR_PREFIX}-{harness}-{attempt}-{now_unix}"),
        sentinel: format!("{PROBE_SENTINEL_PREFIX}-{harness}-{attempt}-{now_unix}-{entropy:09}"),
    }
}

/// The self-probe prompt. Asks for ONE artifact carrying the sentinel minted for this probe.
fn harness_probe_prompt(sentinel: &str) -> String {
    format!(
        "Create a file named `probe.txt` in your current working directory whose contents are \
         exactly this line:\n\n{sentinel}\n\n\
         Do nothing else. Do not explain, and do not ask a question — write the file."
    )
}

/// How many self-probe turns the FLAKY shape gets before the pre-advertise gate gives up on a harness.
///
/// Only the "completed the turn but produced no artifact" shape is retried, and only up to this many
/// turns total (#472). A model that flakes on one turn often delivers on the next, so grounding a seat
/// on a single empty turn would drop a working harness; but one that never produces the sentinel across
/// three turns is not flaky, it is broken. A launcher/exec failure is NEVER counted here — it is
/// structural (a permission or containment barrier does not clear by re-asking) and stops on the first
/// attempt. See [`probe_step`].
const HARNESS_PROBE_MAX_ATTEMPTS: usize = 3;

/// One self-probe turn's outcome, keeping WHICH shape of failure occurred — the one bit a single
/// `Result` throws away, and the bit the prove-before-advertise diagnostic needs (#472).
///
/// - [`Self::CompletedNoArtifact`] — the launcher ran the turn and the turn completed, but no artifact
///   carries the sentinel. RETRIED, because a model that flakes on one turn often delivers on the next.
/// - [`Self::Unrunnable`] — the turn never ran (a typed launcher/exec [`ExecError`], or our own workdir
///   could not be created). The remedy is to fix the launcher/containment, not the model. NOT retried:
///   re-asking a launcher that refused only delays a fail-closed boot.
///
/// ⛔ THE SHAPE BOUNDS THE RETRY POLICY, NOT THE CAUSE, AND AN EARLIER VERSION OF THIS COMMENT CLAIMED
/// OTHERWISE. It read "the turn completed, the model just did not do the task. Flaky" — an exhaustive
/// dichotomy (turn-never-ran ⇒ containment, turn-ran ⇒ model) that the code does not have. A THIRD
/// state exists and was measured on 2026-08-21: a contained cursor seat ran the turn to completion and
/// the fault WAS containment — its model host would not resolve, cursor folded `getaddrinfo EAI_AGAIN`
/// into ordinary assistant text, and returned `stopReason: end_turn`. Every egress gap produces this
/// same completed-with-no-artifact shape.
///
/// ⇒ so `CompletedNoArtifact` carries the agent's own last message, which is the only thing in the turn
/// that can name the cause. Retrying it is still right; asserting the model's mood is not.
///
/// `Debug` so a failing probe assertion names the shape it actually got. The three shapes are what
/// the diagnostic is ABOUT, and a bare "assertion failed" cannot tell an unrunnable launcher from a
/// harness that ran and wrote nothing.
#[derive(Debug)]
enum ProbeAttempt {
    /// The harness produced the sentinel artifact: a proven turn.
    ///
    /// Carries the model the harness reported for this turn, which is the ONLY moment it is
    /// observable: the probe is the one place the node runs the harness and reads its usage back
    /// before serving. Dropping it here is what leaves a production roster's `models` empty, and an
    /// empty roster cannot emit `harness_model` on a heartbeat or a claim.
    ///
    /// `None` is a real answer, not a gap: a harness that exposes no usage has no model to
    /// advertise, and absent-stays-absent all the way to the wire rather than being filled in.
    Proven { model: Option<String> },
    /// The ACP turn completed but left no artifact carrying the sentinel. Carries the agent's own last
    /// message when it said anything — the cause is in there or nowhere.
    CompletedNoArtifact { agent_message: Option<String> },
    /// A failure retrying cannot fix. Carries the operator reason (which NAMES the launcher/containment
    /// remedy) and the fault to record.
    Unrunnable { reason: String, fault: Fault },
}

/// What the retry loop does after one attempt: try again, or stop with a verdict. Pure — the decision
/// is a function of (attempt index, cap, this turn's shape) and nothing else — so the whole retry
/// policy is unit-tested without spawning an agent. See [`probe_step`].
enum ProbeStep {
    /// The flaky shape, with turns still left: probe again.
    Retry,
    /// Stop: this is the verdict the gate records (an `Err` is fail-closed). The `Ok` payload is the
    /// model this turn's `session/new` reported, carried forward so the roster records a
    /// MACHINE-SOURCED value rather than an operator-typed one.
    ///
    /// ⚠ Reported, not executed. [`crate::driver::AcpDriver`] captures the resolved session model off
    /// the `session/new` response, from either wire shape it carries (#896); it does not select, pin
    /// or verify what the harness runs. The
    /// value's worth is its PROVENANCE — the harness said it about itself — never a record of
    /// execution.
    Done(Result<Option<String>, (String, Fault)>),
}

/// How much of an agent's message an operator line carries. Long enough for a vendor error string
/// (`getaddrinfo EAI_AGAIN <host>` and friends), short enough to stay one readable line.
const AGENT_MESSAGE_QUOTE_CHARS: usize = 400;

/// One agent message, prepared for an operator line: trimmed, quoted, and truncated ONLY for length.
///
/// `None` when the agent said nothing usable, so every caller renders that case in its own words
/// rather than printing empty quotes. Truncation counts CHARS, not bytes — a vendor error can carry any
/// UTF-8 and slicing bytes would panic mid-codepoint.
fn quoted_agent_message(message: Option<&str>) -> Option<String> {
    let text = message.map(str::trim).filter(|text| !text.is_empty())?;
    let head: String = text.chars().take(AGENT_MESSAGE_QUOTE_CHARS).collect();
    let elided = if text.chars().count() > AGENT_MESSAGE_QUOTE_CHARS {
        " […]"
    } else {
        ""
    };
    Some(format!("\"{head}\"{elided}"))
}

/// The refusal reason once every retry completed a turn and still produced no artifact.
///
/// ⛔ THIS FUNCTION USED TO NAME A CAUSE IT COULD NOT KNOW — "a FLAKY harness/model, not a containment
/// fault" — and that sentence cost a full day on 2026-08-21: the fault WAS containment (a model host
/// that would not resolve), and the string told the operator not to look there. It now reports what was
/// observed and hands over the only evidence the turn contains: the agent's own words.
///
/// The agent's message is quoted rather than summarised, and truncated only for line length, because
/// the useful part is a vendor error string nobody can predict.
fn flaky_harness_reason(attempts: usize, agent_message: Option<&str>) -> String {
    let head = format!(
        "completed {attempts} self-probe turn(s), every turn ran but none produced the sentinel \
         artifact — a completed turn does NOT establish the cause: an unreachable model host, an \
         exhausted plan and an idle model all end a turn this way"
    );
    match quoted_agent_message(agent_message) {
        Some(quoted) => {
            format!(
                "{head}. The agent's own last message was: {quoted} (remedy: read that message \
                 first — it names the fault whenever the agent knew it)"
            )
        }
        None => format!(
            "{head}, and the agent produced NO message this turn, so nothing in the turn names the \
             cause (remedy: check the launcher, the sandbox egress and the harness credential — none \
             of them is ruled out by the turn completing)"
        ),
    }
}

/// The refusal reason for the UNRUNNABLE launcher/exec shape.
///
/// The opposite remedy to [`flaky_harness_reason`]: the turn never executed, so the fault is the
/// launcher or its containment, never the model — retrying it would only defer a fail-closed boot.
fn launcher_unrunnable_reason(error: &ExecError) -> String {
    format!(
        "the launcher could not run a probe turn ({error}) — the harness never executed, so this is a \
         containment/permission/launcher fault, not a flaky model (remedy: fix the launcher/sandbox \
         config, then restart)"
    )
}

/// Whether an [`ExecError`] is an AUTHENTICATION failure rather than a containment/launcher one (#555).
///
/// The agent CLI surfaces "not signed in" as an ACP JSON-RPC error whose text reaches us inside
/// [`ExecError::Agent`] — e.g. `ACP request 3 failed: {"code":-32000,"message":"Authentication
/// required"}`. It shares the [`ProbeAttempt::Unrunnable`] shape with a launcher fault (the turn
/// never ran), but the remedies are OPPOSITE: this one is fixed by signing in, never by editing a
/// sandbox/launcher config.
///
/// Keyed on the ACP auth MESSAGE ("authentication required", case-insensitive), NOT on the `-32000`
/// code: `-32000` is the generic JSON-RPC server-error code, so a bare match on it would
/// false-positive on any other agent error that happens to carry it.
fn is_auth_class(error: &ExecError) -> bool {
    matches!(
        error,
        ExecError::Agent(message)
            if message.to_ascii_lowercase().contains("authentication required")
    )
}

/// The refusal reason for the AUTHENTICATION-failure Unrunnable shape (#555).
///
/// Like [`launcher_unrunnable_reason`] the turn never ran, so this is not a flaky model — but the
/// fault is a signed-OUT agent CLI, not the sandbox/launcher, so the remedy is to sign in. Pointing
/// an operator with a login problem at a containment config sends them to the one subsystem that was
/// never at fault.
fn auth_unrunnable_reason(error: &ExecError) -> String {
    format!(
        "this is an AUTHENTICATION failure ({error}), not a containment/launcher fault — sign in to \
         the agent CLI on this machine (e.g. run `claude`, then `/login`), then restart"
    )
}

/// The refusal reason for an Unrunnable probe turn, ROUTED by the error's class (#555).
///
/// An authentication failure and a containment failure are both [`ProbeAttempt::Unrunnable`] — the
/// turn never ran — but their remedies are opposite (see [`auth_unrunnable_reason`] vs
/// [`launcher_unrunnable_reason`]). Routing here keeps an operator with a login problem from being
/// sent to edit a launcher/sandbox config that was never at fault. The shape and retry policy are
/// unchanged; only the remedy STRING branches.
fn unrunnable_reason(error: &ExecError) -> String {
    if is_auth_class(error) {
        auth_unrunnable_reason(error)
    } else {
        launcher_unrunnable_reason(error)
    }
}

/// The retry policy, as a pure decision over one attempt's shape (#472).
///
/// - `Proven` → `Done(Ok)`.
/// - `Unrunnable` → `Done(Err)`, always, whatever the attempt index: a launcher that refused does not
///   start working because we asked again.
/// - `CompletedNoArtifact` → `Retry` while turns remain, else `Done(Err(flaky))`. This is the ONLY
///   shape that retries, and only up to `max_attempts`.
fn probe_step(attempt: usize, max_attempts: usize, outcome: ProbeAttempt) -> ProbeStep {
    match outcome {
        ProbeAttempt::Proven { model } => ProbeStep::Done(Ok(model)),
        ProbeAttempt::Unrunnable { reason, fault } => ProbeStep::Done(Err((reason, fault))),
        ProbeAttempt::CompletedNoArtifact { agent_message } => {
            if attempt + 1 < max_attempts {
                ProbeStep::Retry
            } else {
                ProbeStep::Done(Err((
                    flaky_harness_reason(max_attempts, agent_message.as_deref()),
                    Fault::Unproven,
                )))
            }
        }
    }
}

/// Run ONE self-probe turn and decide it on the ARTIFACT, reporting WHICH shape occurred.
///
/// A positive control, not a liveness check, and the distinction is the entire reason this exists. A
/// harness whose account is exhausted ends its turn `completed`, exits 0, and returns a perfectly
/// non-empty message explaining that you should upgrade your plan — so exit status, turn state and
/// response length are ALL green for exactly the harness we most need to catch. The sentinel is the only
/// signal that goes red, because a harness that cannot work cannot produce it.
///
/// The probe runs under the same sandbox policy as a real job. Probing an unsandboxed path while jobs
/// run sandboxed would verify a path no paid job ever takes.
///
/// The two failure shapes are returned APART (see [`ProbeAttempt`]) so the caller can retry the flaky
/// one and fail the structural one fast. Typed launcher/exec failures go through [`harness_fault_for`],
/// the same classifier a real job's failure uses, so a probe against an `acp`-less binary marks the
/// harness INCAPABLE and stops being probed at all rather than re-asking a settled question.
async fn run_harness_probe_once(
    argv: &[String],
    sandbox: &SandboxPolicy,
    identity: &DeliveryAgentIdentity,
    workdir: &std::path::Path,
    sentinel: &str,
) -> ProbeAttempt {
    if let Err(error) =
        seller_git::init_empty_delivery_workdir_off_runtime(workdir.to_path_buf(), identity.clone())
            .await
    {
        // Our own filesystem, not the harness: record nothing against its capability, and do NOT
        // retry — re-minting a workdir cannot fix a filesystem that just refused one.
        return ProbeAttempt::Unrunnable {
            reason: format!("probe workdir init failed ({error}) — our filesystem, not the harness"),
            fault: Fault::Unproven,
        };
    }

    let report = match run_agent_job(
        argv,
        sandbox,
        &harness_probe_prompt(sentinel),
        workdir,
        identity,
        AgentRunTimeout::HarnessProbe(HARNESS_PROBE_TIMEOUT),
    )
    .await
    {
        Ok(report) => report,
        Err(error) => {
            // The turn never ran: structural, so do not retry. The remedy STRING is routed by class —
            // an auth failure needs a sign-in, not a containment fix (#555) — but the shape and fault
            // are unchanged either way.
            let fault = match harness_fault_for(&error) {
                Some(ExecutionFailure::Harness(fault)) => fault,
                Some(ExecutionFailure::DeadlineExceeded) | None => Fault::Unproven,
            };
            return ProbeAttempt::Unrunnable {
                reason: unrunnable_reason(&error),
                fault,
            };
        }
    };

    // The turn "succeeded" — now ask the only question that separates a working harness from an
    // exhausted one: is the sentinel actually here?
    if probe_sentinel_present(workdir, sentinel) {
        ProbeAttempt::Proven {
            model: report.usage.and_then(|usage| usage.model),
        }
    } else {
        ProbeAttempt::CompletedNoArtifact {
            agent_message: report.last_agent_message,
        }
    }
}

/// One-shot probe verdict, no retry: run a single turn and collapse its shape to pass/fail.
///
/// The pre-advertise gate uses [`run_harness_probe_once`] directly so it can tell the two failure
/// shapes apart and retry the flaky one (#472). The restore-timer path has not adopted the retry, so it
/// keeps the single-turn collapse here — the same verdict it recorded before, now carrying the shape's
/// sharper reason string.
async fn run_harness_probe(
    argv: &[String],
    sandbox: &SandboxPolicy,
    identity: &DeliveryAgentIdentity,
    workdir: &std::path::Path,
    sentinel: &str,
) -> Result<Option<String>, (String, Fault)> {
    match run_harness_probe_once(argv, sandbox, identity, workdir, sentinel).await {
        ProbeAttempt::Proven { model } => Ok(model),
        ProbeAttempt::Unrunnable { reason, fault } => Err((reason, fault)),
        ProbeAttempt::CompletedNoArtifact { agent_message } => Err((
            flaky_harness_reason(1, agent_message.as_deref()),
            Fault::Unproven,
        )),
    }
}

/// Releases a claimed probe's in-flight mark if — and only if — no verdict already did.
///
/// A restore self-probe runs as a `spawn_local` task whose `JoinHandle` is DROPPED (see
/// [`SellerNode::start_due_harness_probes`]), so a panic inside the probe is otherwise swallowed
/// entirely: neither the `Ok`→restore nor the `Err`→fault arm runs, and the harness would stay marked
/// `probing` for the life of the process, never re-probed. This guard is armed BEFORE the probe future
/// is polled, so the panic unwinding through it still releases the mark. On the normal paths the verdict
/// arms clear `probing` first, which makes [`LiveRoster::abandon_probe`] a no-op here — the guard fires
/// on every path but only DOES anything on the one that reached no verdict (#301).
struct ProbeInFlightGuard {
    roster: Arc<LiveRoster>,
    harness: usize,
}

impl Drop for ProbeInFlightGuard {
    fn drop(&mut self) {
        self.roster.abandon_probe(self.harness, Instant::now());
    }
}

/// What a supervised restore probe did. The roster mutation (restore / fault / re-arm) has already
/// happened by the time this is returned; the variant only tells the caller which line to log.
#[derive(Debug)]
enum ProbeOutcome {
    /// The probe proved its sentinel — the harness is back in service.
    Restored,
    /// The probe reached a verdict of failure — the harness is dropped with `state`.
    Faulted { reason: String, state: Unavailable },
    /// The outer wall-clock ceiling elapsed before the probe returned — a live-but-endless stream the
    /// idle timeout could not bound. Faulted `Unproven`, exactly as a failed turn would be.
    WallTimeout { state: Unavailable },
}

/// Run ONE restore self-probe under two guarantees the raw `run_harness_probe` future lacks:
///
/// 1. An outer WALL-CLOCK timeout ([`HARNESS_PROBE_WALL_TIMEOUT`]). The 120s cap inside the probe is an
///    ACP *idle* timeout that resets on every stream event, so a harness that drip-feeds updates hangs
///    the probe forever without ever tripping it. `tokio::time::timeout` bounds the whole turn.
/// 2. A Drop guard armed BEFORE the probe is polled, so a PANIC in the probe task (whose `JoinHandle`
///    the spawner drops) still releases the harness's in-flight mark as the stack unwinds.
///
/// Together they close both paths on which the old spawn body left a harness marked `probing` forever
/// and thus never re-probed — stuck `Dropped` for the life of the process (#301). Neither path can
/// restore a harness: a hang faults it `Unproven`, a panic re-arms it through the guard, and both leave
/// it `Dropped`, so a probe that failed to prove anything can never leave a dead harness serving.
///
/// The probe future is taken as a parameter (rather than built here) purely so a test can inject one
/// that hangs or panics; production passes `run_harness_probe(..)`, which is not polled until awaited
/// below, inside the guard's scope.
async fn supervise_harness_probe(
    roster: Arc<LiveRoster>,
    harness: usize,
    probe: impl std::future::Future<Output = Result<Option<String>, (String, Fault)>>,
) -> ProbeOutcome {
    // Armed before the await: releases `probing` even if `probe` panics through it. Idempotent with the
    // verdict arms below, which clear the mark themselves on the paths that reach a verdict.
    let _guard = ProbeInFlightGuard {
        roster: Arc::clone(&roster),
        harness,
    };
    match tokio::time::timeout(HARNESS_PROBE_WALL_TIMEOUT, probe).await {
        Ok(Ok(model)) => {
            // RECORD BEFORE RESTORE, and the order is the property. `fault` leaves `model` untouched
            // and `restore` clears only availability, so the model standing here is the one from
            // BEFORE the harness was dropped — and a harness is dropped for exactly as long as an
            // operator might re-point it at something else. Restoring first would publish that stale
            // value on any beat landing between the two calls: the roster read takes its own lock, so
            // the window is real, and what it emits is a filterable claim a buyer can be awarded on.
            //
            // `None` is written THROUGH rather than skipped, for the reason the setter takes an
            // `Option` at all: a harness that has stopped reporting a model must come back stating
            // none, not still advertising what it last said a fault ago.
            roster.record_model(harness, model);
            roster.restore(harness);
            ProbeOutcome::Restored
        }
        Ok(Err((reason, fault))) => {
            let state = roster.fault(harness, fault, Instant::now());
            ProbeOutcome::Faulted { reason, state }
        }
        Err(_elapsed) => {
            // The idle timeout never fired because the harness kept its stream alive; the wall ceiling
            // did. Record it as an unattributable failure so the harness backs off and re-arms rather
            // than staying `probing` forever — the exact stuck state #301 is about.
            let state = roster.fault(harness, Fault::Unproven, Instant::now());
            ProbeOutcome::WallTimeout { state }
        }
    }
}

/// Whether any file the harness left in `workdir` carries the probe sentinel.
///
/// Content, not filename: a harness that wrote the sentinel somewhere sensible has demonstrated the
/// capability being tested, and failing it for choosing a different filename would report a working
/// harness as broken. `.git` is skipped — the workdir is a fresh repo and its own metadata is ours.
///
/// **The workdir's own path is subtracted from the content before matching.** A harness that echoes
/// its cwd has demonstrated nothing about the task, so a sentinel reachable only through that echo
/// must not count. [`mint_probe_identity`] already keeps the sentinel out of the path, which is the
/// real fix; this is the belt to that braces, so a future caller that reintroduces the leak cannot
/// silently turn a dead harness into a serving one.
fn probe_sentinel_present(workdir: &std::path::Path, sentinel: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(workdir) else {
        return false;
    };
    let path_text = workdir.to_string_lossy().to_string();
    entries.flatten().any(|entry| {
        if entry.file_name() == ".git" {
            return false;
        }
        std::fs::read_to_string(entry.path())
            .map(|content| content.replace(&path_text, "").contains(sentinel))
            .unwrap_or(false)
    })
}

/// One configured harness's pre-advertise probe verdict: which registry index it is, its name (for
/// logs), and whether it PROVED it can deliver. `Err` carries the operator reason and the `Fault` to
/// record against the roster — the same `(reason, Fault)` a real job's failure produces, so a dead
/// harness narrows the roster identically whether the fault is found here or at runtime.
#[derive(Debug)]
pub struct HarnessProbeVerdict {
    pub index: usize,
    pub name: Option<String>,
    /// `Ok` carries the model the harness reported on its proving turn's `session/new`, or `None`
    /// when it exposed no usage. This is the value the roster records, and it is sourced from the
    /// harness itself rather than from configuration.
    ///
    /// ⚠ The reason is PROVENANCE, not enforcement. Reading it from config would destroy the one
    /// property this value has — that the harness said it about itself — and substitute a number an
    /// operator typed, which can drift from the harness with nothing to notice. Neither form is a
    /// promise about execution: ACP reports the model before any work happens, and nothing here pins
    /// it.
    pub result: Result<Option<String>, (String, Fault)>,
}

/// Probe ONE configured harness under the retry policy (#472), returning the verdict the gate records.
///
/// Each attempt gets a FRESH identity and workdir, so a retry must be earned by THIS turn — no stale
/// artifact or replayed transcript can satisfy a later attempt. Only the flaky `CompletedNoArtifact`
/// shape loops; [`probe_step`] returns `Done` immediately for a proven or unrunnable turn, so a bogus
/// launcher costs exactly one spawn, not three.
async fn probe_one_harness(
    index: usize,
    argv: &[String],
    label: &str,
    sandbox: &SandboxPolicy,
    identity: &DeliveryAgentIdentity,
    home: &MaxplayerHome,
) -> Result<Option<String>, (String, Fault)> {
    for attempt in 0..HARNESS_PROBE_MAX_ATTEMPTS {
        let probe = mint_probe_identity(index, attempt, now_unix() as u64);
        let workdir = job_workdir(home, &probe.dir_label);
        let outcome =
            run_harness_probe_once(argv, sandbox, identity, &workdir, &probe.sentinel).await;
        let _ = std::fs::remove_dir_all(&workdir);
        // Kept before `probe_step` consumes the outcome, so a retry can print what the agent said on
        // THIS turn instead of making the operator wait for the final verdict to learn it.
        let said = match &outcome {
            ProbeAttempt::CompletedNoArtifact { agent_message } => {
                quoted_agent_message(agent_message.as_deref())
            }
            _ => None,
        };
        match probe_step(attempt, HARNESS_PROBE_MAX_ATTEMPTS, outcome) {
            ProbeStep::Done(result) => return result,
            ProbeStep::Retry => opline!(
                "seller node harness probe {label}: turn {} completed with NO artifact, retrying — \
                 the agent said: {}",
                attempt + 1,
                said.as_deref().unwrap_or("<nothing>")
            ),
        }
    }
    // Unreachable: probe_step returns Done on the final attempt. Kept as a fail-closed verdict so a
    // future change to the policy cannot fall through to a panic on a money path. No agent message is
    // available here precisely because this path never runs a turn.
    Err((
        flaky_harness_reason(HARNESS_PROBE_MAX_ATTEMPTS, None),
        Fault::Unproven,
    ))
}

/// Probe EVERY configured harness before anything goes on the wire.
///
/// Local compute only — no sats, no mint, no award ([`probe_one_harness`] runs the harness in a
/// throwaway workdir to write a sentinel and decides on the artifact). It is ours to run and ours to
/// pay for, exactly like the restore-timer self-probe — but here it gates the FIRST advertisement
/// rather than a restoration, so a seat that cannot deliver never advertises at all (#357). Each
/// harness is probed under the retry policy, so a merely FLAKY model is not mistaken for a broken one
/// and grounded before it advertises (#472). Inputs are derived from `home` the same way
/// `start_due_harness_probes` derives them for the restore probe.
pub async fn probe_configured_harnesses(
    home: &MaxplayerHome,
) -> Result<Vec<HarnessProbeVerdict>, NodeError> {
    let registry = boot_agent_registry(home)?;
    // A `[sandbox]` that does not resolve into an executor refuses the boot gate outright: probing
    // under a pass-through fallback would prove a harness the awarded job will never run under.
    let sandbox = SandboxPolicy::from_config(home.config.sandbox.as_ref())
        .map_err(|error| NodeError::Sandbox(error.to_string()))?;
    // #647 credential-containment scope (P2): every KNOWN model-credential variable is contained by
    // the proxy. What can still cross RAW is an operator-added `[sandbox] forward_env` variable the
    // daemon cannot recognize — it may be a credential, and the daemon has no way to know. Say so
    // LOUDLY at boot — the same gap the doctor WARN reports.
    for var in crate::seller_exec::uncontained_forwarded_credentials(&sandbox, |key| {
        std::env::var(key).ok()
    }) {
        opline!(
            "seller node SECURITY: [sandbox] forward_env carries {var} into the container \
             UNCONTAINED — the credential proxy contains only the known model-credential variables, \
             so if {var} is a secret a stranger's job can read and reuse it. Remove it from \
             forward_env, or treat that credential as compromised and spend-cap it at the provider."
        );
    }
    // The seat's own identity, established BEFORE the reap below because it is what scopes it. A
    // daemon that cannot name itself reaps nothing: the `?` here is the gate.
    let identity = DeliveryAgentIdentity::for_seller(&home::public_key_hex(home)?);
    // Reap THIS SEAT'S containment holders that no job is attached to (#797 R6). Both legs are
    // required. An unattached holder is NOT evidence of a dead daemon: the holder is created before
    // the job joins it and outlives the job after it exits, so unattached is a normal state twice in
    // every job's life. What makes a holder ours to remove is the seat label it carries — several
    // seller daemons share a host, and the query is host-wide. Best-effort and never a gate: a leaked
    // holder owns a namespace and holds no policy, so it wastes a container rather than opening
    // anything.
    // `#[cfg(acp)]` because `reap_orphans` needs the docker runner behind that feature, while this
    // module only needs `wallet`. A `wallet`-without-`acp` build (the money-path row, and the workspace
    // default build via the bin crate) compiles this function and would not find the call.
    #[cfg(feature = "acp")]
    if sandbox.sandbox_network().is_some() {
        match crate::sandbox_netns::reap_orphans(identity.seller_pubkey_hex()).await {
            Ok(report) => {
                if !report.removed.is_empty() {
                    opline!(
                        "seller node: reaped {} containment holder(s) left by an earlier run of this seat",
                        report.removed.len()
                    );
                }
                // Still never a gate — see above. `reap_orphans` returns its per-holder failures
                // (#905) instead of printing them, so they now reach the operator log this daemon
                // already writes rather than raw process stderr. Reporting them, not acting on them,
                // is the whole difference from the operator command, which exits nonzero on the same
                // input.
                for (holder, error) in &report.failed {
                    opline!(
                        "seller node: could not reap this seat's stale containment holder {holder} \
                         ({error}) — harmless to this boot, but it will accumulate until it succeeds"
                    );
                }
            }
            Err(error) => opline!(
                "seller node: could not reap this seat's stale containment holders ({error}) — \
                 harmless to this boot, but they will accumulate until it succeeds"
            ),
        }
    }

    let mut verdicts = Vec::with_capacity(registry.entries().len());
    for (index, entry) in registry.entries().iter().enumerate() {
        let label = entry.name.clone().unwrap_or_else(|| "<unlabelled>".to_owned());
        let result = probe_one_harness(index, &entry.argv, &label, &sandbox, &identity, home).await;
        verdicts.push(HarnessProbeVerdict {
            index,
            name: entry.name.clone(),
            result,
        });
    }
    Ok(verdicts)
}

/// The pure decision the pre-advertise gate turns on: the registry indices that PROVED they can
/// serve. No I/O, so the healthy direction is testable by injecting outcomes — no agent spawn.
pub fn proven_serving_indices(verdicts: &[HarnessProbeVerdict]) -> Vec<usize> {
    verdicts
        .iter()
        .filter(|verdict| verdict.result.is_ok())
        .map(|verdict| verdict.index)
        .collect()
}

/// The operator lines for a pre-advertise probe: one FAILED line per harness that did not prove,
/// then a serving `n/m` count.
///
/// ⛔ THIS EXISTS BECAUSE A PARTIAL FAILURE USED TO NARROW THE ROSTER IN SILENCE. The FAILED lines
/// used to live inside the `proven_serving_indices(..).is_empty()` branch, so they printed only when
/// NOTHING proved and the seat refused to boot. A seat with at least one prover booted, dropped the
/// rest, and advertised the survivors — with no line naming which harnesses dropped, or why. The
/// roster narrowing is what determines the kind-30340 announcement, so the log was silent about the
/// thing that changed what the seat advertised (#773).
///
/// Pure over the verdicts so it is testable without a relay, a home lock or a boot. The caller emits.
fn pre_advertise_probe_lines(verdicts: &[HarnessProbeVerdict]) -> Vec<String> {
    let mut lines = Vec::new();
    for verdict in verdicts {
        if let Err((reason, _)) = &verdict.result {
            let label = verdict.name.as_deref().unwrap_or("<unlabelled>");
            lines.push(format!(
                "seller node pre-advertise probe FAILED {label}: {reason}"
            ));
        }
    }
    let proven = proven_serving_indices(verdicts).len();
    lines.push(format!(
        "seller node pre-advertise probe: serving {proven}/{} configured harness(es)",
        verdicts.len()
    ));
    lines
}

/// Prove-before-advertise: publish discoverability and boot serving ONLY the harnesses that proved
/// they can deliver.
///
/// `verdicts` is the outcome of [`probe_configured_harnesses`], taken as a parameter so a caller can
/// inject a passing result without spawning an agent. Two outcomes:
///
/// - **None proved out** → publish NOTHING (0×kind-0) and refuse to boot (0×kind-30340). Fail
///   loud, non-zero: a seat that cannot deliver any harness must not advertise, and lingering while
///   advertising nothing only hides the fault from the operator.
/// - **Some proved out** → publish the kind-0 identity, boot, then pre-narrow the live roster to
///   the provers so the kind-30340 announcement (which reads the roster) is honest for free.
///
/// The gate is the `is_empty` block below; reverting it — publishing unconditionally — reproduces the
/// #357 bug (advertise, then fail every job).
pub async fn boot_advertising_only_proven(
    mut home: MaxplayerHome,
    verdicts: Vec<HarnessProbeVerdict>,
) -> Result<SellerNodeRunner, NodeError> {
    // Report every verdict BEFORE the gate, so a seat that boots with a narrowed roster still names
    // every harness that dropped — the kind-30340 announcement reads that roster. Emitted on every
    // outcome, not only the all-failed refusal: a mixed probe used to narrow and advertise in
    // silence (#773). Composition is in `pre_advertise_probe_lines`; the caller emits.
    for line in pre_advertise_probe_lines(&verdicts) {
        opline!("{line}");
    }
    if proven_serving_indices(&verdicts).is_empty() {
        return Err(NodeError::NoProvenHarness(format!(
            "none of {} configured harness(es) produced a probe artifact; refusing to advertise \
             (fix the harness/launcher, then restart)",
            verdicts.len()
        )));
    }

    // Say it BEFORE the wire work, so an operator watching a boot scroll past sees it whether or not
    // the relay legs succeed — a seat that will never claim is worth knowing about even if boot then
    // fails for an unrelated reason. Emitted every boot, not once: the condition is a standing state
    // of the config, not an event, and a seat can be restarted long after the upgrade that closed it.
    if let Some(warning) = home
        .config
        .seller
        .as_ref()
        .and_then(unreachable_seat_warning)
    {
        opline!("{warning}");
    }

    // Take the home lock BEFORE anything reaches the relay. The publish below is the first thing this
    // path puts on the wire, and a second seller started on the same home used to reach it, announce
    // its identity, and only then fail the lock inside boot — leaving a kind-0 on the relay for a node
    // that never served a single job. The lock is the claim on the home, so it has to precede the
    // claim on the wire. It is threaded into boot rather than re-taken, which would deadlock.
    let lock =
        crate::seller_node::lock::HomeLock::acquire(home.root.join(crate::seller_node::LOCK_FILE))?;

    // Prove seat CAPABILITY before anything reaches the wire (#784). The probe runs in the SAME
    // executor a job runs in, in a throwaway workdir under the seller-jobs root, so it answers for the
    // machine jobs land on rather than the seller host. A capability that cannot be MEASURED — a render
    // failure, a launcher that will not spawn, a probe that times out — fails boot LOUDLY here, before
    // the kind-0 publish, rather than advertising a silently shorter set: a buyer commits sats on this
    // field, so "we could not check" must never look like "checked, and no". The proven set is recorded
    // into the roster below, before the first heartbeat or claim can read it.
    let sandbox = crate::seller_exec::SandboxPolicy::from_config(home.config.sandbox.as_ref())
        .map_err(|error| NodeError::Sandbox(format!("capability probe policy: {error}")))?;
    let probe_dir = crate::seller_exec::ProbeWorkdir::create(&home)
        .map_err(|error| NodeError::Sandbox(format!("capability probe workdir: {error}")))?;
    let capabilities = crate::capability::probe_seat_capabilities(&sandbox, probe_dir.path())
        .map_err(|error| {
            NodeError::Sandbox(format!(
                "capability probe could not be measured; refusing to advertise: {error}"
            ))
        })?;
    drop(probe_dir);
    opline!("seller node capability probe proved: [{}]", capabilities.join(", "));

    let disco = crate::profile::publish_seller_discoverability_async(&mut home)
        .await
        .map_err(|error| NodeError::Relay(format!("discoverability publish failed: {error}")))?;
    opline!(
        "seller node discoverable kind0={} name={} pubkey={}",
        disco.kind0_event_id,
        disco.name.as_deref().unwrap_or(""),
        disco.pubkey
    );

    let runner = SellerNodeRunner::boot_with_lock(home, lock).await?;
    runner.narrow_roster_to(&verdicts);
    // Record the proven capability set into the live roster BEFORE the runner is handed back and can
    // serve. The first heartbeat and the first claim both read the roster, so recording here — not on
    // first use — is what makes the very first advertisement honest.
    runner.agents.record_capabilities(capabilities);
    Ok(runner)
}

/// How long boot waits for the relay connection and the NIP-42 challenge.
const CONNECT_WAIT: Duration = Duration::from_secs(20);
/// Ceiling on the #747 terminal `accepting=n` publish, which runs on the EXIT path.
///
/// Bounded because an unbounded one would be self-defeating: a stop that never returns gets a
/// SIGKILL from the operator or the supervisor, and SIGKILL is exactly the exit no retraction can
/// cover. Long enough for a sign + one round trip to a healthy relay, short enough to stay well
/// inside a supervisor's own stop grace period (systemd's default `TimeoutStopSec` is 90s, and
/// `docker stop` allows 10s before it escalates).
const RETRACTION_PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);
/// Cadence of the outbox drain / housekeeping tick.
const DRAIN_INTERVAL: Duration = Duration::from_secs(5);

/// A booted seller node with its live relay surface.
pub struct SellerNodeRunner {
    node: SellerNode,
    client: Client,
    publisher: RelayPublisher,
    relay_url: String,
    seller_pubkey: nostr_sdk::PublicKey,
    /// Outcome of the boot NIP-42 handshake, which seeds the run loop's view of whether the current
    /// socket is authenticated. `NoChallenge` is not authentication.
    boot_auth: AuthWait,
    /// The harnesses this node is serving with. Resolved once at boot, then narrowed at RUNTIME as
    /// harnesses fail: every claim decision, every advertisement, and every dispatch reads THIS —
    /// never the config, and never the boot registry directly — so what the node advertises is what
    /// it still believes it can deliver with, not merely what it once launched.
    ///
    /// Behind an `Arc` because execution runs off the loop: the task that discovers a harness is
    /// broken is not the task that publishes the next advertisement. See [`LiveRoster`].
    agents: Arc<LiveRoster>,
    /// Homogeneous execution-slot admission (reserve-at-claim). Behind an `Arc` so it is shared with
    /// the off-loop execution tasks; see [`SlotGate`].
    slots: Arc<SlotGate>,
    /// #450: armed when an offer is skipped because every slot is busy (`SlotsBusy`). The drain tick
    /// consumes it once a slot frees to re-run the offer backfill, so a capacity-skipped offer is
    /// reconsidered without waiting for a restart. A flag, not a queue: `on_offer` is idempotent
    /// (`claim_and_enqueue` dedups an already-claimed offer), so re-delivering every stored offer
    /// safely re-claims only the ones still open.
    capacity_skip_pending: std::sync::atomic::AtomicBool,
    /// #562: serializes delivery pushes to this seat's ONE `seller.git_remote`. Every awarded job
    /// executes on its own task and pushes a per-job branch to the SAME delivery repo; concurrent
    /// `git-receive-pack` to one repo is what the relay 409s (the multi-slot delivery hazard). Held
    /// ONLY across the push (execution stays parallel) and bounded by [`DELIVERY_PUSH_TIMEOUT`], so a
    /// hung push releases it rather than starving every later delivery behind the lock.
    delivery_push_lock: tokio::sync::Mutex<()>,
    /// #541: relay-derived set of SETTLED offer ids (a co-signed kind-3400 receipt has been seen).
    /// Read before any claim to skip a terminal offer that re-appears via backfill or redelivery.
    /// Populated by [`Self::on_receipt`] from the live receipt subscription and its boot/reconnect
    /// backfill. Stays empty when the seller is not open-pool (the receipt sub is registered only then).
    terminal_offers: TerminalOffers,
    /// #582: offer ids already given the targeted under-rate buyer-feedback this boot. First-sight
    /// gate on the emit in [`Self::on_offer`], so the #560 offer-backfill re-ingesting a stored
    /// under-rate offer every tick does not re-emit a duplicate `BelowRate` feedback to the buyer on
    /// every pass. Feedback-only wire-noise dedup — never consulted on the claim/money path. See
    /// [`FedUnderRateOffers`].
    fed_under_rate_offers: FedUnderRateOffers,
    /// #747: how this node is asked to leave the selling role, so it can publish its terminal
    /// `accepting=n` beat before exiting. See [`shutdown`] and [`Self::shutdown_handle`].
    shutdown: shutdown::ShutdownChannel,
}

impl SellerNodeRunner {
    /// Boot the node and connect its authenticated relay client.
    ///
    /// Custody rule: the seller key lives in exactly two places — the signer actor (opened by
    /// [`SellerNode::open`]) and THIS authenticated relay client, constructed once below. It is never
    /// exposed by an accessor, logged, or serialized. The client holds it because maxplayer-relay
    /// authenticates the seller via NIP-42 (signing the challenge) before it will deliver the
    /// p-gated kind-1059 payment wraps.
    pub async fn boot(home: MaxplayerHome) -> Result<Self, NodeError> {
        let lock = crate::seller_node::lock::HomeLock::acquire(
            home.root.join(crate::seller_node::LOCK_FILE),
        )?;
        Self::boot_with_lock(home, lock).await
    }

    /// [`Self::boot`] for a caller that already holds the home lock, so the lock can be taken before
    /// the caller publishes anything. See [`SellerNode::open_with_lock`].
    pub async fn boot_with_lock(
        home: MaxplayerHome,
        lock: crate::seller_node::lock::HomeLock,
    ) -> Result<Self, NodeError> {
        let relay_url = home.config.relay_url.clone();

        // Read the seller secret ONCE, here, to build the authenticated client (single construction
        // site — see the custody rule above). Dropped as soon as the client owns the keys.
        let secret = home::read_secret_key_hex(&home)?;
        let keys = Keys::parse(&secret)
            .map_err(|error| NodeError::Relay(format!("seller key parse: {error}")))?;
        drop(secret);

        let node = SellerNode::open_with_lock(home, lock).await?;

        // Resolve the harness registry BEFORE anything goes on the wire: a node that cannot launch
        // a single harness must refuse to boot rather than claim work it can never run. Boot can
        // only verify LAUNCHABILITY, so the resolved set becomes the live roster's starting point
        // and narrows from there as harnesses prove they cannot deliver.
        let agents = Arc::new(LiveRoster::new(boot_agent_registry(node.home())?));

        // Reconcile durable state before serving anything live: expire stale outbox rows, report the
        // non-terminal jobs that resume. Reconcile must NOT release parked claims (invariant 5).
        match node.reconcile_on_start(now_unix()) {
            Ok(report) => opline!(
                "seller node reconcile: resumed_jobs={} expired_outbox={} pending_outbox={}",
                report.resumed_jobs.len(),
                report.expired_outbox,
                report.pending_outbox
            ),
            Err(error) => opline!("seller node reconcile failed on startup (continuing): {error}"),
        }

        let seller_pubkey = keys.public_key();
        let client = Client::new(keys);
        // Seller receive depends on NIP-42; keep auto-auth ON so a relay that challenges on the REQ
        // (not just connect) still authenticates.
        client.automatic_authentication(true);
        client
            .pool()
            .add_relay(&relay_url, RelayOptions::default().reconnect(true))
            .await
            .map_err(|error| NodeError::Relay(format!("add relay: {error}")))?;

        // Subscribe the relay's notification stream BEFORE connect — `Authenticated` is emitted once
        // and never re-emitted, so a receiver created after connect could miss it.
        let parsed_relay = RelayUrl::parse(&relay_url)
            .map_err(|error| NodeError::Relay(format!("parse relay url: {error}")))?;
        let relay = client
            .relays()
            .await
            .get(&parsed_relay)
            .cloned()
            .ok_or_else(|| NodeError::Relay("relay missing after add_relay".into()))?;
        let mut relay_notifications = relay.notifications();
        client.connect().await;
        client.wait_for_connection(CONNECT_WAIT).await;
        let boot_auth =
            match relay_auth::wait_for_nip42_auth(&mut relay_notifications, CONNECT_WAIT).await {
                Ok(AuthWait::Authenticated) => {
                    opline!("seller node relay authenticated (NIP-42)");
                    AuthWait::Authenticated
                }
                Ok(AuthWait::NoChallenge) => {
                    opline!(
                        "seller node WARN: no NIP-42 challenge within {CONNECT_WAIT:?}; proceeding \
                     (auto-auth stays ON — a challenge on the REQ still authenticates). p-gated \
                     kind-1059 receive may be degraded until auth completes."
                    );
                    AuthWait::NoChallenge
                }
                Err(error) => return Err(NodeError::Relay(format!("NIP-42 auth: {error}"))),
            };

        let publisher = RelayPublisher::new(node.signer().clone(), client.clone(), &relay_url);

        // Execution-slot admission from config: `slots` (default 1 = serial) and the claim-lapse
        // timeout (default when unset). A node with no `[seller]` block never claims, so its slot
        // count is immaterial — default to serial.
        let (capacity, lapse_secs) = node
            .home()
            .config
            .seller
            .as_ref()
            .map(|s| (s.slots, s.claim_award_timeout_secs))
            .unwrap_or((1, None));
        let slots = Arc::new(SlotGate::new(
            capacity,
            Duration::from_secs(lapse_secs.unwrap_or(DEFAULT_CLAIM_AWARD_TIMEOUT_SECS)),
        ));
        opline!(
            "seller node execution slots: {} (claim-lapse timeout {}s)",
            capacity.max(1),
            lapse_secs.unwrap_or(DEFAULT_CLAIM_AWARD_TIMEOUT_SECS)
        );

        Ok(Self {
            node,
            client,
            publisher,
            relay_url,
            seller_pubkey,
            boot_auth,
            agents,
            slots,
            capacity_skip_pending: std::sync::atomic::AtomicBool::new(false),
            delivery_push_lock: tokio::sync::Mutex::new(()),
            terminal_offers: TerminalOffers::new(TERMINAL_OFFERS_CAP, TERMINAL_AUTHORS_PER_OFFER),
            fed_under_rate_offers: FedUnderRateOffers::new(FED_UNDER_RATE_OFFERS_CAP),
            shutdown: shutdown::ShutdownChannel::new(),
        })
    }

    /// The seller public key (hex).
    pub fn seller_pubkey(&self) -> String {
        self.seller_pubkey.to_hex()
    }

    /// A handle asking this node to leave the selling role: the run loop stops, publishes its
    /// terminal `accepting=n` beat (#747), and [`Self::run`] returns `Ok(())`.
    ///
    /// Take it BEFORE [`Self::run`], which consumes the runner. The daemon wires it to SIGTERM/SIGINT
    /// via [`shutdown::spawn_os_signal_listener`]; an embedder can drive it directly.
    pub fn shutdown_handle(&self) -> shutdown::ShutdownHandle {
        self.shutdown.handle()
    }

    /// Subscribe (or re-subscribe) the offer REQ. `open_pool` false forces the targeted-only shape —
    /// used by the boot/recovery path when the seller has not opted into the open pool, and by the
    /// `CLOSED` degrade, which keeps targeted claiming alive after the relay refuses the grouped REQ.
    async fn subscribe_offers(
        &self,
        since: Option<nostr_sdk::Timestamp>,
        open_pool: bool,
    ) -> Result<(), NodeError> {
        let filters = offer_subscription_filters(
            self.seller_pubkey,
            open_pool,
            self.node
                .home()
                .config
                .seller
                .as_ref()
                .map(|seller| seller.offer_backfill_secs)
                .unwrap_or(0),
            since,
            nostr_sdk::Timestamp::from(now_unix().max(0) as u64),
        );
        self.client
            .pool()
            .subscribe_with_id(
                nostr_sdk::SubscriptionId::new(OFFER_SUB_ID),
                filters,
                nostr_sdk::pool::SubscribeOptions::default(),
            )
            .await
            .map_err(|error| NodeError::Relay(format!("subscribe offers: {error}")))?;
        Ok(())
    }

    /// #541: subscribe (or re-subscribe) the settlement-receipt REQ — one kind-3400 + hashtag filter,
    /// bounded like the open-pool offer filter so it is never a firehose. At boot (`since` None) it
    /// backfills the `offer_backfill_secs` window — the window offers themselves re-appear from, so
    /// exactly the receipts needed to gate them — capped at [`OFFER_BACKFILL_LIMIT`]; a `0` window is
    /// live-only. On a post-stall resubscribe it carries the overlap cursor. Only ever registered for
    /// an open-pool seller (see [`Self::subscribe_all`]).
    async fn subscribe_receipts(
        &self,
        since: Option<nostr_sdk::Timestamp>,
    ) -> Result<(), NodeError> {
        let base = Filter::new()
            .kind(Kind::Custom(JOB_RECEIPT_KIND))
            .hashtag(crate::gateway::MAXPLAYER_TAG);
        let filter = match since {
            Some(cursor) => base.since(cursor),
            None => {
                let backfill_secs = self
                    .node
                    .home()
                    .config
                    .seller
                    .as_ref()
                    .map(|seller| seller.offer_backfill_secs)
                    .unwrap_or(0);
                let now = now_unix().max(0) as u64;
                if backfill_secs > 0 {
                    base.since(nostr_sdk::Timestamp::from(now.saturating_sub(backfill_secs)))
                        .limit(OFFER_BACKFILL_LIMIT)
                } else {
                    base.since(nostr_sdk::Timestamp::from(now))
                }
            }
        };
        self.client
            .subscribe_with_id(nostr_sdk::SubscriptionId::new(RECEIPT_SUB_ID), filter, None)
            .await
            .map_err(|error| NodeError::Relay(format!("subscribe receipts: {error}")))?;
        Ok(())
    }

    /// #450: reconsider offers we skipped for capacity once a slot frees. An offer skipped because
    /// every slot was busy (`SlotsBusy`) is recorded but parks no claim, and was previously only
    /// revisited by a RESTART's offer backfill. When a slot later frees, re-drive those recorded
    /// offers straight from the store — NOT by re-subscribing the relay: nostr-relay-pool emits an
    /// event notification only the FIRST time it sees an id (an already-seen replay is swallowed by
    /// its per-connection seen-cache), so a re-subscribe re-REQs but never re-fires `on_offer`. Only
    /// a restart's fresh connection would, which is exactly the gap this closes.
    ///
    /// Each candidate is re-classified at re-drive time — freshness/expiry, the buyer allowlist, the
    /// rate gate, and the requested harness can all have changed in the seconds since the skip — and
    /// only those still `Claim` are re-driven through [`Self::claim_offer`]. Idempotency there
    /// (`claim_and_enqueue` dedups) is the double-claim guard. The pending flag is consumed only when
    /// a slot is actually free, and re-armed if more recorded offers remain than freed slots.
    async fn reconsider_capacity_skips(&self) {
        if self.slots.available() == 0
            || !self
                .capacity_skip_pending
                .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let Some(seller) = self.node.home().config.seller.clone() else {
            return;
        };
        let now = now_unix();
        let pending = match self.node.store().offers_awaiting_claim(now) {
            Ok(offers) => offers,
            Err(error) => {
                opline!("seller node capacity-skip reconsider: store read failed ({error}); re-arming for the next tick");
                self.capacity_skip_pending
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        if pending.is_empty() {
            return;
        }
        let seller_pubkey = self.seller_pubkey.to_hex();
        let mut reclaimed = 0usize;
        for row in pending {
            if self.slots.available() == 0 {
                // More recorded-unclaimed offers than freed slots: re-arm so the next freed slot
                // revisits the remainder rather than dropping them on the floor.
                self.capacity_skip_pending
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                break;
            }
            // Reconstruct the parsed offer from the stored row. Only offers that already passed the
            // targeting gate are ever recorded (`rate_gate_allows` refuses a foreign `p`-tag BEFORE
            // `record_offer`), so `targeted ⇒ p-tag == self` and the reconstruction is exact.
            let offer = ParsedOffer {
                task: row.task.clone(),
                output: String::new(),
                amount: row.amount_sats,
                unit: row.unit.clone(),
                deadline_unix: row.deadline_unix as u64,
                seller_pubkey: row.targeted.then(|| seller_pubkey.clone()),
                requested_agent: row.requested_agent.clone(),
                // The seller's own claim decision is capability-blind: the buyer's request filters
                // which claims may be AWARDED, and it is judged buyer-side against what this seat
                // advertises. Reconstructing it here would be a second reading of the request that
                // could disagree with the one that decides.
                requested_harness_family: None,
                requested_model: None,
                required_capabilities: Vec::new(),
            };
            match classify_offer(
                &offer,
                &seller,
                &self.agents,
                &seller_pubkey,
                &row.buyer_pubkey,
                now as u64,
                // #604: the age gate is a BACKFILL admission concern. A reconsider re-drives an
                // already-recorded, already-age-vetted capacity-skip (an offer is recorded only after
                // `classify_offer` returned `Claim`, so an aged historical never lands in this set),
                // so pass `now` — the gate is a no-op here and cannot refuse a legitimately-vetted
                // offer that is merely waiting on a slot.
                now as u64,
            ) {
                ClaimDecision::Claim { deadline_unix } => {
                    self.claim_offer(
                        &row.offer_id,
                        &row.buyer_pubkey,
                        &offer,
                        &seller_pubkey,
                        deadline_unix,
                        now,
                        // A recorded offer re-driven from the store carries no tags; its pin (if any)
                        // was already written at the original claim (INSERT OR IGNORE — idempotent).
                        None,
                    )
                    .await;
                    reclaimed += 1;
                }
                ClaimDecision::Skip(skip) => {
                    opline_verbose!(
                        "seller node capacity-skip reconsider: offer id={} no longer claimable ({})",
                        row.offer_id,
                        skip.reason()
                    );
                }
            }
        }
        if reclaimed > 0 {
            opline!(
                "seller node reconsidered capacity-skipped offers: re-drove {reclaimed} recorded offer(s) after a slot freed ({} slot(s) free)",
                self.slots.available()
            );
        }
    }

    /// Subscribe the marketplace filters: offers, awards, and payment gift-wraps. `since` is
    /// `Some(overlap)` on a post-stall resubscribe so events published during the stall backfill;
    /// `None` at boot. Reused by boot and by the watchdog's reconnect so both paths subscribe the SAME
    /// set — including re-arming the open-pool half after a `CLOSED` degrade.
    ///
    /// There is no own-heartbeat subscription: a client cannot be delivered its own published event
    /// (see [`probe_relay_serves_our_reqs`]), so that REQ could only ever have returned nothing.
    /// Liveness is asserted by the probe instead.
    async fn subscribe_all(&self, since: Option<nostr_sdk::Timestamp>) -> Result<(), NodeError> {
        for id in [OFFER_SUB_ID, AWARD_SUB_ID, WRAP_SUB_ID] {
            self.subscribe_one(id, since).await?;
        }
        // #541: the settlement-receipt sub feeds the terminal-offer gate, and the gate only matters for
        // offers we can LOSE — the open pool. A targeted seller is covered by local claim idempotency
        // (its own settlements are in its store) and cannot lose a targeted-to-it offer to another
        // seat, so it never carries the sub. Registered here so boot AND the watchdog's reconnect keep
        // the same set.
        if self.claim_open_pool() {
            self.subscribe_one(RECEIPT_SUB_ID, since).await?;
        }
        Ok(())
    }

    /// Issue (or re-issue) the REQ for ONE named subscription, so a single leg can be repaired
    /// without re-dialing the relay or disturbing the others.
    async fn subscribe_one(
        &self,
        id: &str,
        since: Option<nostr_sdk::Timestamp>,
    ) -> Result<(), NodeError> {
        // The offer REQ has its own entry point: it is the only subscription with a meaningful
        // partial form, and it carries two filters rather than one.
        if id == OFFER_SUB_ID {
            return self.subscribe_offers(since, self.claim_open_pool()).await;
        }
        // #541: receipts have their own entry point too — one kind+hashtag filter bounded by the same
        // backfill window as offers, so a cold cache re-fills from the relay's stored receipts on
        // boot/reconnect (the relay-verify-on-restart) rather than a firehose of every receipt ever.
        if id == RECEIPT_SUB_ID {
            return self.subscribe_receipts(since).await;
        }
        let base = match id {
            // One subscription carries both buyer-authored decisions about our claims: the AWARD
            // that selects one, and the ACCEPT that pay-binds a delivered result. Sharing the REQ
            // (rather than adding a sub id) keeps them under the same CLOSED handling and the same
            // stall watchdog — a second subscription would be a second thing that can die quietly.
            // Award/accept visibility (#456): an open-pool seller subscribes UNSCOPED so a losing
            // claimant still receives the award that releases its slot (an award p-tags only the
            // winner). See `award_filter` — on_award/on_accept still bind only offers we claimed.
            AWARD_SUB_ID => award_filter(self.seller_pubkey, self.claim_open_pool()),
            WRAP_SUB_ID => Filter::new()
                .kind(Kind::GiftWrap)
                .pubkey(self.seller_pubkey),
            other => {
                return Err(NodeError::Relay(format!(
                    "subscribe {other}: not one of ours"
                )));
            }
        };
        let filter = match since {
            Some(cursor) => base.since(cursor),
            None => base,
        };
        self.client
            .subscribe_with_id(nostr_sdk::SubscriptionId::new(id), filter, None)
            .await
            .map_err(|error| NodeError::Relay(format!("subscribe {id}: {error}")))?;
        Ok(())
    }

    /// Whether this seller has opted into claiming untargeted (open-pool) offers.
    fn claim_open_pool(&self) -> bool {
        self.node
            .home()
            .config
            .seller
            .as_ref()
            .is_some_and(|seller| seller.claim_open_pool)
    }

    /// Run the live loop until the relay pool closes or a shutdown is requested: ingests
    /// offers/awards/gift-wraps, drains the outbox on a periodic tick, and — when heartbeat is
    /// enabled — publishes an own-heartbeat each heartbeat tick and runs the #150 relay-stall
    /// watchdog (reconnect + resubscribe-with-overlap if no own heartbeat has round-tripped within
    /// the stall threshold), with #162 bounded recovery retries.
    ///
    /// However it ends, the seat publishes one terminal `accepting=n` beat on the way out (#747).
    pub async fn run(self) -> Result<(), NodeError> {
        // Consume the runner into an `Arc` so each awarded job's execution runs as its own task (see
        // [`SlotGate`]) while this loop stays responsive to new offers, awards, and payments — the
        // loop never runs a job inline, which is the multi-slot change.
        //
        // Jobs run as `spawn_local` tasks under a LocalSet, NOT `tokio::spawn`: `execute_job` holds
        // `&self` (node store and friends, not `Sync`) across awaits, so its future is `!Send` and
        // cannot cross threads. A LocalSet runs the jobs cooperatively on THIS thread, interleaving
        // with the loop at every await. That is the right model here: execution is I/O-bound (it
        // awaits the agent subprocess over ACP — the driver's waits genuinely yield, issue #223), so
        // one thread suffices, and it keeps the nostr client and signer on their original runtime —
        // no cross-runtime client calls.
        let local = tokio::task::LocalSet::new();
        local.run_until(Arc::new(self).run_loop()).await
    }

    /// [`Self::serve`], plus the one thing that must happen no matter how serving ended: the
    /// terminal `accepting=n` beat (#747).
    ///
    /// Shaped like the #729 driver-shutdown seam for the same reason. The retraction sits on the
    /// ONE exit path rather than beside each `break`/`?`, so a `?` added to the loop tomorrow still
    /// funnels through it. The serving outcome is carried across untouched — a failing seat must
    /// still report WHY it failed, and a retraction publish is never allowed to mask that.
    ///
    /// ⛔ This runs only when the process is still alive to run it. SIGKILL, a panic that skips
    /// unwinding, an OOM kill and a power cut reach no exit path at all, and leave the seat's last
    /// `accepting=y` standing exactly as before. Consumer-side recency filtering stays the only
    /// cover for those. Belt AND braces, never a replacement.
    async fn run_loop(self: Arc<Self>) -> Result<(), NodeError> {
        let served = Arc::clone(&self).serve().await;
        self.publish_retraction().await;
        served
    }

    async fn serve(self: Arc<Self>) -> Result<(), NodeError> {
        // Heartbeat + relay-stall watchdog config. Disabled ⇒ no heartbeat publish and the watchdog
        // branch is inert (the loop only waits on the drain tick + relay stream).
        let hb = &self.node.home().config.seller_heartbeat;
        let heartbeat_enabled = crate::heartbeat::resolve_enabled(hb);
        let heartbeat_interval_secs = crate::heartbeat::resolve_interval_secs(hb);
        let stall_missed_intervals = crate::heartbeat::resolve_stall_missed_intervals(hb);
        let stall_threshold = stall_threshold_secs(heartbeat_interval_secs, stall_missed_intervals);

        // The relay handle for the watchdog's reconnect (fresh notification receiver + NIP-42 re-auth).
        let parsed_relay = RelayUrl::parse(&self.relay_url)
            .map_err(|error| NodeError::Relay(format!("parse relay url: {error}")))?;
        let relay = self
            .client
            .relays()
            .await
            .get(&parsed_relay)
            .cloned()
            .ok_or_else(|| NodeError::Relay("relay missing in run loop".into()))?;

        let mut notifications = self.client.notifications();
        // #814: refill the suppression cache BEFORE any REQ goes out, so the very first offer the
        // backfill re-feeds is already gated. After the subscribe it would be a race against our own
        // recovery path.
        self.rehydrate_suppressions();
        self.subscribe_all(None).await?;
        opline!(
            "seller node live: pubkey={} relay={}",
            self.seller_pubkey.to_hex(),
            self.relay_url
        );
        if heartbeat_enabled {
            opline!(
                "seller node heartbeat+watchdog enabled: kind-30340 every {heartbeat_interval_secs}s; \
                 reconnect if the relay stops serving our REQs for {stall_threshold}s \
                 ({stall_missed_intervals} missed intervals)"
            );
        }
        opline!(
            "seller node wrap backfill enabled: re-fetching stored kind-1059(s) every {}s (recovers a \
             silently-deaf payment subscription without a restart; its log line is the periodic \
             liveness signal)",
            resolve_wrap_backfill_interval_secs()
        );

        // Drain anything reconcile left pending before the first tick.
        self.drain().await;

        // Resume execution for jobs a process restart left mid-flight (invariant 4, fallback form):
        // an `awarded`/`executing` job is re-driven through execute_job so a crash mid-job resumes
        // instead of losing the award. Idempotent — a re-created delivery lands exactly once
        // (deterministic snapshot + deliver_and_enqueue dedup). Runs once at boot, before the loop.
        match self.node.store().resumable_jobs() {
            Ok(jobs) => {
                let mut resumable = Vec::new();
                for (job_id, state) in jobs {
                    if should_resume_execution(state) {
                        opline!(
                            "seller node resume: re-driving execution for job_id={job_id} (state={state:?})"
                        );
                        resumable.push(job_id);
                    }
                }
                if !resumable.is_empty() {
                    // The count WITH its denominator: a restart that caught more jobs than `slots`
                    // runs them in waves, and an operator seeing only the count cannot tell whether
                    // that is happening. While the backlog drains the node also stops claiming new
                    // offers (no free permit ⇒ `SlotsBusy`), which is the intended back-pressure.
                    opline!(
                        "seller node resume: {} job(s) to re-drive, bounded to {} execution slot(s)",
                        resumable.len(),
                        self.slots.capacity
                    );
                }
                // Resume off the loop, each holding a real permit so a restart honors `slots`.
                // `resume = true` arms the #563 relay-derive belt: these are exactly the stale rows a
                // restart re-reads, the only place a job could have been settled elsewhere or delivered
                // by a pre-#552 binary.
                let runner = Arc::clone(&self);
                spawn_bounded_resumes(Arc::clone(&self.slots), resumable, move |job_id, slot| {
                    let runner = Arc::clone(&runner);
                    async move { runner.execute_job(&job_id, slot, true).await }
                });
            }
            Err(error) => {
                opline!("seller node resume: resumable_jobs read failed (continuing): {error}")
            }
        }

        let mut drain_tick = tokio::time::interval(DRAIN_INTERVAL);
        let wrap_backfill_interval_secs = resolve_wrap_backfill_interval_secs();
        let mut wrap_backfill_tick =
            tokio::time::interval(Duration::from_secs(wrap_backfill_interval_secs));
        let mut heartbeat_tick =
            tokio::time::interval(Duration::from_secs(heartbeat_interval_secs.max(1)));
        // Watchdog liveness clocks: monotonic instant (staleness measure, robust to wall-clock jumps)
        // + unix stamp (resubscribe `since` cursor). Refreshed whenever the relay answers our liveness
        // probe. Seeded to "now" so a healthy node never trips before its first probe.
        let mut last_liveness_seen = tokio::time::Instant::now();
        let mut last_liveness_seen_unix = now_unix();
        // Set while the offer REQ is running in its degraded targeted-only shape after a relay
        // `CLOSED`. Carries its own re-arm schedule (#190) — see [`OpenPoolDegrade`].
        let mut open_pool: Option<OpenPoolDegrade> = None;
        // A repair the CLOSED arm has asked for, run on the next heartbeat tick through the ONE
        // paced recovery path rather than an off-cadence ad-hoc resubscribe.
        let mut forced_recovery: Option<String> = None;
        // NIP-42 state of the CURRENT socket, and when it was last established.
        //
        // Tracked here because `Authenticated` is a RELAY notification that never becomes a pool
        // notification (`relay/inner.rs:418` maps it to `None`), so the pool stream the loop already
        // watches cannot see it. Seeded from the boot handshake. Both stale readings are bounded and
        // safe: stale-false only declines a cheap retry and falls through to the paced recovery,
        // while stale-true spends the single retry this session allows and then does the same.
        let mut nip42_authed = matches!(self.boot_auth, AuthWait::Authenticated);
        let mut last_authenticated_at = tokio::time::Instant::now();
        let mut relay_notifications = relay.notifications();
        // Subscriptions that have already spent their one post-auth retry on this session (#189
        // belt). Cleared whenever a new session authenticates, so the budget is per-session and can
        // never become a loop.
        let mut restricted_retry_used: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // When the last periodic backfill ran (wrap + offer, #560 — both fetch on this tick). Reported
        // alongside an unknown-id `CLOSED` so the relay owner can tell a refusal of our transient
        // `fetch_events` REQ (which uses a generated id, and runs on exactly this cadence) from a
        // relay-side sweep of a stale generation.
        let mut last_backfill_at = tokio::time::Instant::now();
        // Which path actually restored the receive leg. Manual recovery and the SDK's background
        // reconnect were previously indistinguishable in the log — which is how a manual path that
        // never once succeeded went unnoticed (#171). The next answered probe names it.
        let mut stalled_since_recovery = false;
        let mut manual_recovery_succeeded = false;
        // #747: the departure request (SIGTERM/SIGINT via
        // [`shutdown::spawn_os_signal_listener`], or an embedder's handle). Taken once — a second
        // loop on the same node would get `None` and simply never see a request.
        let mut shutdown_rx = self.shutdown.take_receiver();
        loop {
            tokio::select! {
                // #747: leave the selling role. Breaking here — rather than exiting the process
                // where the signal arrived — is the whole point: the ONE exit path in
                // [`Self::run_loop`] then publishes the terminal `accepting=n` beat before this
                // seat goes quiet, instead of leaving its last `accepting=y` standing forever on a
                // replaceable event no later beat will ever correct.
                //
                // Jobs still executing on the LocalSet stop with the loop. Their durable rows stay
                // exactly as they are and the boot resume path re-drives them on the next start, so
                // a graceful stop loses no awarded work — it is strictly gentler than the SIGKILL
                // an unlistened SIGTERM used to be.
                reason = shutdown::next_request(&mut shutdown_rx) => {
                    opline!("seller node: shutdown requested ({reason}); retracting the seat and ending the loop");
                    break;
                }
                _ = drain_tick.tick() => {
                    self.sweep_lapsed_claims();
                    self.reconsider_capacity_skips().await;
                    self.start_due_harness_probes();
                    self.drain().await;
                    continue;
                }
                // Re-ask the relay for stored payment wraps AND stored offers, so a silently-deaf 1059
                // or offer subscription recovers without a restart (#560). Also the node's only
                // periodic log lines, and therefore the positive signal external supervision watches.
                _ = wrap_backfill_tick.tick() => {
                    self.run_wrap_backfill().await;
                    // #560: the offers analog, on the SAME owned cadence. It rides this tick (not the
                    // heartbeat) for the same reason the wrap backfill does — a recovery leg must not
                    // depend on a tick config can disable.
                    self.run_offer_backfill().await;
                    last_backfill_at = tokio::time::Instant::now();
                    // #190: the open-pool half is re-armed on THIS tick, which is owned and
                    // unconditional. It rides the backfill rather than the heartbeat because the
                    // heartbeat is disableable by config, and a repair must not depend on a tick that
                    // may never fire. Acceptance is the relay's EOSE below — a response the protocol
                    // owes us — never the fact that we managed to send the REQ.
                    if let Some(state) = open_pool.as_mut()
                        && state.on_tick() == RearmStep::Attempt
                    {
                        let overlap = nostr_sdk::Timestamp::from(
                            last_liveness_seen_unix
                                .saturating_sub(STALL_OVERLAP_MARGIN_SECS as i64)
                                .max(0) as u64,
                        );
                        match self.subscribe_offers(Some(overlap), true).await {
                            Ok(()) => opline!(
                                "seller node RELAY-CLOSED RE-ARM: retrying the open-pool half of \
                                 the offer subscription (attempt after {} rejection(s), since={} \
                                 overlap); the relay's EOSE confirms it",
                                state.rejections,
                                overlap.as_secs()
                            ),
                            Err(error) => {
                                state.reject();
                                opline!(
                                    "seller node RELAY-CLOSED RE-ARM failed to send ({error}); next \
                                     attempt in {} backfill tick(s)",
                                    state.cooldown_ticks
                                );
                            }
                        }
                    }
                    continue;
                }
                // The heartbeat tick rides the SAME loop (never a blocking side-thread). Probe the
                // READ leg first, evaluate staleness, then publish the heartbeat (the WRITE leg) —
                // both bounded so the tick cannot hang on a dead link. #509: relay-observed liveness
                // needs BOTH legs, so the watchdog clock is refreshed only after the publish, and
                // only when read AND publish both confirmed this tick (see below).
                _ = heartbeat_tick.tick(), if heartbeat_enabled => {
                    let probe_ok = probe_relay_serves_our_reqs(
                        &self.client,
                        self.seller_pubkey,
                        LIVENESS_PROBE_TIMEOUT,
                    )
                    .await;
                    if probe_ok {
                        if stalled_since_recovery {
                            if manual_recovery_succeeded {
                                opline!(
                                    "seller node subscription RESTORED via MANUAL recovery (relay \
                                     is serving our REQs again)"
                                );
                            } else {
                                opline!(
                                    "seller node subscription RESTORED via SDK BACKGROUND reconnect \
                                     — no manual recovery had succeeded (relay is serving our REQs \
                                     again)"
                                );
                            }
                            stalled_since_recovery = false;
                            manual_recovery_succeeded = false;
                        }
                        // #509: the read leg passed, but the clock refresh is DEFERRED to after the
                        // publish leg below. A seat whose reads answer while its heartbeat writes are
                        // silently rejected is dark on the relay yet used to keep the clock fresh
                        // here — the exact 2691s blind spot. The clock now moves only when both legs
                        // confirm.
                    }
                    let stall_elapsed = last_liveness_seen.elapsed().as_secs();
                    let stalled = subscription_stalled(stall_elapsed, stall_threshold);
                    let forced = forced_recovery.take();
                    if stalled || forced.is_some() {
                        let overlap_since = nostr_sdk::Timestamp::from(
                            last_liveness_seen_unix
                                .saturating_sub(STALL_OVERLAP_MARGIN_SECS as i64)
                                .max(0) as u64,
                        );
                        if stalled {
                            opline!(
                                "seller node RELAY-STALL detected: relay has not served our REQs in \
                                 {stall_elapsed}s (threshold {stall_threshold}s); reconnecting + \
                                 resubscribing with since={} overlap",
                                overlap_since.as_secs()
                            );
                        } else {
                            opline!(
                                "seller node RELAY-RECOVERY triggered: {}; reconnecting + \
                                 resubscribing with since={} overlap",
                                forced.unwrap_or_default(),
                                overlap_since.as_secs()
                            );
                        }
                        stalled_since_recovery = true;
                        match self.recover_stall(&relay, overlap_since).await {
                            Ok(attempts) => {
                                let outage = now_unix().saturating_sub(last_liveness_seen_unix);
                                // Grace: reset the watchdog clock so it does not immediately re-fire
                                // before the next tick's probe can answer.
                                last_liveness_seen = tokio::time::Instant::now();
                                last_liveness_seen_unix = now_unix();
                                // The full set was re-subscribed, so the open-pool half is back.
                                open_pool = None;
                                manual_recovery_succeeded = true;
                                opline!(
                                    "seller node RELAY-STALL recovery SUCCEEDED (attempts={attempts}, \
                                     outage={outage}s): reconnected + resubscribed \
                                     (offers+awards+1059, since={} overlap)",
                                    overlap_since.as_secs()
                                );
                            }
                            Err(error) => {
                                // Leave the clocks untouched so the next heartbeat tick retries.
                                opline!(
                                    "seller node RELAY-STALL recovery FAILED (will retry next heartbeat tick): {error}"
                                );
                            }
                        }
                    }
                    // The WRITE leg: publish the heartbeat and CONFIRM the relay accepted it (#509),
                    // not merely that the SDK send returned `Ok`. Only a tick where the relay both
                    // served our REQs (`probe_ok`) AND acknowledged our heartbeat (`publish_ok`)
                    // refreshes the watchdog clock; a rejected/absent OK therefore leaves the clock
                    // to age and drives the RELAY-STALL branch above on a later tick, exactly as a
                    // failed read probe does.
                    let publish_ok = self.publish_heartbeat().await;
                    if relay_liveness_confirmed(probe_ok, publish_ok) {
                        last_liveness_seen = tokio::time::Instant::now();
                        last_liveness_seen_unix = now_unix();
                    }
                    continue;
                }
                recv = notifications.recv() => {
                    match recv {
                        Ok(RelayPoolNotification::Event { event, .. }) => {
                            match event.kind {
                                k if k.as_u16() == JOB_OFFER_KIND => self.on_offer(&event).await,
                                k if k.as_u16() == JOB_AWARD_KIND => self.on_award(&event).await,
                                k if k.as_u16() == JOB_ACCEPT_KIND => self.on_accept(&event).await,
                                k if k.as_u16() == JOB_REJECT_KIND => self.on_reject(&event).await,
                                k if k.as_u16() == JOB_RECEIPT_KIND => self.on_receipt(&event).await,
                                Kind::GiftWrap => self.on_gift_wrap(&event).await,
                                _ => {}
                            }
                            self.drain().await;
                        }
                        Ok(RelayPoolNotification::Shutdown) => {
                            opline!("seller node: relay pool shutdown; loop ending");
                            break;
                        }
                        // A relay `CLOSED` kills ONE subscription while the socket stays up, so the
                        // heartbeat watchdog cannot see it: close the 1059 leg and the node keeps
                        // heartbeating happily while every payment silently misses. Never fatal —
                        // always loud, always repaired.
                        Ok(RelayPoolNotification::Message {
                            message: nostr_sdk::RelayMessage::Closed { subscription_id, message: reason },
                            ..
                        }) => {
                            let id = subscription_id.to_string();
                            let label = subscription_label(&id);
                            opline!(
                                "seller node RELAY-CLOSED: relay closed the {label} subscription \
                                 (id={id}): {reason}"
                            );

                            // An id we never registered cannot be a leg of ours going deaf, so it
                            // must not cost a reconnect — and escalating it did exactly that. Field
                            // seats open every cycle with a CLOSED for an unknown id; that forced a
                            // full recovery, and the recovery then re-closed the 1059 leg. A
                            // self-inflicted sawtooth on a socket that was never broken.
                            //
                            // The two ages are for the relay owner, who cannot see either from the
                            // server side. Our periodic wrap backfill uses `fetch_events`, which
                            // GENERATES its subscription id (`pool/mod.rs:815`) and runs on exactly
                            // this cadence, so a small backfill age implicates our own transient REQ;
                            // an auth age near the relay's NIP-42 TTL instead implicates a
                            // re-challenge sweep closing auth-scoped subs from the pre-expiry
                            // generation.
                            if !is_our_subscription(&id) {
                                opline!(
                                    "{}",
                                    unknown_close_diagnostic(
                                        &id,
                                        last_backfill_at.elapsed().as_secs(),
                                        last_authenticated_at.elapsed().as_secs(),
                                        nip42_authed,
                                    )
                                );
                                continue;
                            }

                            // Whether the offer REQ currently on the wire carries the un-pinned
                            // open-pool filter: either it was never dropped, or a re-arm attempt has
                            // just put it back. This is what decides whether a refusal can be ABOUT
                            // the un-pinned half — while degraded to targeted-only, it cannot.
                            let offer_req_carries_unpinned = self.claim_open_pool()
                                && open_pool.is_none_or(|state| state.attempt_pending);

                            // The offer REQ is the one subscription with a meaningful partial form:
                            // drop the un-pinned open-pool filter and re-subscribe targeted-only, so
                            // a relay that refuses the grouped REQ still leaves targeted claiming
                            // alive rather than taking the whole offer leg down.
                            if id == OFFER_SUB_ID && offer_req_carries_unpinned {
                                // A CLOSED landing while a re-arm attempt is on the wire IS that
                                // attempt's verdict, and it is what advances the backoff (#190).
                                let refused = open_pool.as_mut().map(|state| {
                                    state.reject();
                                    (state.rejections, state.cooldown_ticks)
                                });
                                match self.subscribe_offers(None, false).await {
                                    Ok(()) => {
                                        let (rejections, cooldown) = refused.unwrap_or_else(|| {
                                            open_pool = Some(OpenPoolDegrade::new());
                                            (0, 0)
                                        });
                                        opline!(
                                            "seller node RELAY-CLOSED DEGRADE: offer subscription \
                                             re-armed TARGETED-ONLY (open-pool half dropped after \
                                             {rejections} consecutive refusal(s); the open-pool half \
                                             is retried on the \
                                             {wrap_backfill_interval_secs}s backfill tick, next \
                                             attempt in {cooldown} tick(s) — no reconnect required)"
                                        );
                                    }
                                    Err(error) => {
                                        opline!(
                                            "seller node RELAY-CLOSED degrade failed ({error}); \
                                             forcing full recovery on the next heartbeat tick"
                                        );
                                        forced_recovery = Some(format!(
                                            "offer subscription CLOSED and the targeted-only degrade failed: {error}"
                                        ));
                                    }
                                }
                                continue;
                            }

                            // #189 BELT. A `restricted:` CLOSED of a subscription whose filters all
                            // pin `#p` to our OWN pubkey, on a session that has authenticated, is the
                            // pre-auth REQ race — not a gate violation. It arrives mostly from the
                            // SDK's own background reconnect, which resubscribes on socket-up before
                            // AUTH exists (`relay/inner.rs:748-752`) and is not a path we can order
                            // from out here. So re-issue that ONE REQ, at most once per authenticated
                            // session (`insert` returns false the second time, and the budget is
                            // cleared only when a NEW session authenticates). The taxonomy is not
                            // softened: a genuine wrong-`#p` `restricted:` cannot reach this branch,
                            // because we author these filters from our own pubkey — and a second
                            // refusal falls through to the paced recovery below rather than looping.
                            let restricted = matches!(
                                nostr_sdk::prelude::MachineReadablePrefix::parse(&reason),
                                Some(nostr_sdk::prelude::MachineReadablePrefix::Restricted)
                            );
                            if restricted
                                && nip42_authed
                                && subscription_pins_only_our_pubkey(&id, offer_req_carries_unpinned)
                                && restricted_retry_used.insert(id.clone())
                            {
                                let overlap = nostr_sdk::Timestamp::from(
                                    last_liveness_seen_unix
                                        .saturating_sub(STALL_OVERLAP_MARGIN_SECS as i64)
                                        .max(0) as u64,
                                );
                                match self.subscribe_one(&id, Some(overlap)).await {
                                    Ok(()) => {
                                        opline!(
                                            "seller node RELAY-CLOSED RETRY: the {label} \
                                             subscription pins #p to our OWN pubkey and this session \
                                             authenticated {}s ago, so `restricted:` here is the \
                                             pre-auth REQ race (#189) and not a gate violation; \
                                             re-subscribed ONCE with since={} overlap",
                                            last_authenticated_at.elapsed().as_secs(),
                                            overlap.as_secs()
                                        );
                                        continue;
                                    }
                                    Err(error) => opline!(
                                        "seller node RELAY-CLOSED retry failed ({error}); forcing \
                                         full recovery on the next heartbeat tick"
                                    ),
                                }
                            }

                            // Awards / 1059 / probe have no partial form — repair them through the
                            // one paced recovery path so nothing re-dials the relay off-cadence.
                            forced_recovery =
                                Some(format!("relay CLOSED the {label} subscription: {reason}"));
                        }
                        // An EOSE for the offer subscription while a re-arm attempt is on the wire is
                        // the relay ACCEPTING the grouped REQ. Acceptance is read from this response
                        // — which NIP-01 owes us — and never from our own send having succeeded: a
                        // REQ that left the socket proves nothing about whether the relay took it.
                        Ok(RelayPoolNotification::Message {
                            message: nostr_sdk::RelayMessage::EndOfStoredEvents(eose_id),
                            ..
                        }) if eose_id.to_string() == OFFER_SUB_ID => {
                            if open_pool.is_some_and(|state| state.attempt_pending) {
                                open_pool = None;
                                opline!(
                                    "seller node RELAY-CLOSED RE-ARMED: the open-pool half of the \
                                     offer subscription is live again (the relay served the grouped \
                                     REQ); no reconnect was required"
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            // A broadcast lag is recoverable — never go permanently deaf.
                            opline!("seller node WARN: notification stream {error}; continuing");
                            continue;
                        }
                    }
                }
                // The relay's OWN notification stream, watched only to know whether the current
                // socket has completed NIP-42. `Authenticated` never reaches the pool stream above
                // (`relay/inner.rs:418` maps it to `None`), so this is the only way to see it.
                relay_event = relay_notifications.recv() => {
                    use nostr_sdk::pool::RelayNotification;
                    match relay_event {
                        Ok(RelayNotification::Authenticated) => {
                            nip42_authed = true;
                            last_authenticated_at = tokio::time::Instant::now();
                            // A newly authenticated session earns a fresh retry budget: the budget
                            // exists to bound retries WITHIN a session, not to spend one forever.
                            restricted_retry_used.clear();

                            // #429 FIX. A completed NIP-42 auth is the only reliable signal that this
                            // socket can carry our `#p`-gated REQs again — so RE-ISSUE the long-lived
                            // subscriptions here, on EVERY completed auth, re-armed per-expiry (no
                            // once-per-process latch).
                            //
                            // WHY it is needed: the relay refuses an unauthenticated `#p`-self REQ with
                            // `auth-required:` (retryable, NOT `restricted:` — so the #189 belt above
                            // never even fires for the money leg) and closes the auth-scoped subs when
                            // auth lapses on a LIVE socket. nostr-sdk's only live-socket repair is the
                            // post-auth `resubscribe()` (`relay/inner.rs:941`), gated by
                            // `should_resubscribe` to re-send only subs already marked `closed==true`;
                            // it races the CLOSED that sets that flag and, field-observed, LOSES — the
                            // 1059/awards/offers legs stay registered-but-deaf with no reconnect and no
                            // restart. Re-issuing here does not depend on that race, on the sub's
                            // closed flag, or on a reconnect: `subscribe_with_id` re-sends the REQ
                            // unconditionally (`pool/mod.rs:603`), AUTHENTICATED, so the relay accepts
                            // it and the leg is durable — robust to whichever SDK path is broken.
                            //
                            // No loop: the re-sends go out on an already-authenticated socket, so the
                            // relay serves them (no fresh challenge). A harmless idempotent re-send at
                            // boot is accepted rather than guarded — the loop rarely even sees the boot
                            // auth (the notification stream is taken after `boot()` completes), and a
                            // duplicate REQ is answered the same as the first. The overlap cursor lets
                            // events published while a leg was deaf backfill on the re-send.
                            let overlap = nostr_sdk::Timestamp::from(
                                last_liveness_seen_unix
                                    .saturating_sub(STALL_OVERLAP_MARGIN_SECS as i64)
                                    .max(0) as u64,
                            );
                            match self.subscribe_all(Some(overlap)).await {
                                Ok(()) => opline!(
                                    "seller node RELAY-AUTH RESUBSCRIBE: re-issued offers+awards+\
                                     kind-1059 on a completed NIP-42 auth (since={} overlap), so a \
                                     relay that re-challenged auth on this live socket does not leave \
                                     the money leg silently deaf",
                                    overlap.as_secs()
                                ),
                                Err(error) => opline!(
                                    "seller node RELAY-AUTH RESUBSCRIBE failed ({error}); the next \
                                     completed auth or the wrap backfill retries"
                                ),
                            }
                        }
                        Ok(RelayNotification::AuthenticationFailed) => nip42_authed = false,
                        // A socket that went away takes its NIP-42 state with it — whatever comes
                        // back starts unauthenticated.
                        Ok(RelayNotification::RelayStatus { status })
                            if status != nostr_sdk::prelude::RelayStatus::Connected =>
                        {
                            nip42_authed = false;
                        }
                        Ok(RelayNotification::Shutdown) => nip42_authed = false,
                        Ok(_) => {}
                        // Lagging this stream costs only auth-state precision, and both stale
                        // readings are bounded (see the declaration). Never go deaf over it.
                        Err(_) => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Publish a feedback-kind (`status=error`) event to the buyer explaining why the seller will not
    /// deliver — so the buyer learns the reason instead of getting silence. Best-effort: signed
    /// through the signer actor and sent on the shared client; a failure is logged, never wedges the
    /// loop. Used for both the targeted under-rate refusal and an execution failure.
    ///
    /// Returns whether the feedback reached the relay (`send_event_to` resolved `Ok`). The under-rate
    /// dedup (#582) records an offer as fed ONLY on a `true`, so a transient publish failure is retried
    /// by the next backfill re-ingest rather than being permanently suppressed. Callers that do not
    /// dedup (execution/delivery failure) ignore the return.
    async fn publish_buyer_feedback(
        &self,
        offer_id: &str,
        buyer_pubkey: &str,
        reason_code: ReasonCode,
        reason: &str,
        reason_detail: Option<&str>,
    ) -> bool {
        let mut draft = error_draft(
            offer_id,
            buyer_pubkey,
            &self.seller_pubkey.to_hex(),
            reason_code,
            reason,
        );
        if let Some(reason_detail) = reason_detail {
            draft
                .tags
                .push(gateway::TagSpec::new(["reason_detail", reason_detail]));
        }
        match self.node.signer().sign(draft, now_unix()).await {
            Ok(Ok(signed)) => {
                use nostr_sdk::JsonUtil as _;
                match nostr_sdk::Event::from_json(&signed.json) {
                    Ok(feedback) => match self.client.send_event_to([&self.relay_url], &feedback).await {
                        Ok(_) => {
                            opline!(
                                "seller node buyer feedback surfaced: offer={offer_id} reason_code={} reason={reason}",
                                reason_code.as_str()
                            );
                            true
                        }
                        Err(error) => {
                            opline!(
                                "seller node WARN: buyer feedback publish failed offer={offer_id} ({error})"
                            );
                            false
                        }
                    },
                    Err(error) => {
                        opline!("seller node buyer feedback encode failed (continuing): {error}");
                        false
                    }
                }
            }
            Ok(Err(error)) => {
                opline!("seller node buyer feedback sign failed (continuing): {error}");
                false
            }
            Err(error) => {
                opline!("seller node signer actor gone at buyer feedback (continuing): {error}");
                false
            }
        }
    }

    /// The targeted under-rate refusal feedback (see [`should_publish_under_rate_feedback`]) — a price
    /// decline, so it carries the `below_rate` reason_code (§10 `refusal` class, does not score).
    /// Returns whether it reached the relay, so the #582 dedup records the offer only on a real emit.
    async fn publish_under_rate_feedback(&self, event: &nostr_sdk::Event, reason: &str) -> bool {
        self.publish_buyer_feedback(
            &event.id.to_hex(),
            &event.pubkey.to_hex(),
            ReasonCode::BelowRate,
            reason,
            None,
        )
        .await
    }

    /// The "is it working" line (#489): one periodic line that answers the operator's question
    /// directly, instead of leaving them to infer health from an internal `kind-1059` fetch line.
    ///
    /// A healthy idle seat is otherwise almost silent, so the honest answer had to be stated rather
    /// than implied by absence of errors. Rides the wrap-backfill tick because that timer already
    /// exists at the cadence an operator wants, and is emitted BEFORE the fetch so it still appears
    /// on a cycle whose cursor read aborts.
    ///
    /// `serving` is the authority on whether the seat can take work — NOT whether `names` is empty,
    /// which is also true for an unlabelled `--agent-argv` hatch that is serving perfectly well.
    /// Both come from ONE `advertisement()` snapshot, as that method requires.
    fn report_status(&self) {
        let roster = self.agents.advertisement();
        let busy = self.slots.capacity.saturating_sub(self.slots.available());
        let harnesses = if roster.names.is_empty() {
            "unnamed (argv hatch)".to_owned()
        } else {
            roster.names.join(", ")
        };
        opline!(
            "seller node status: {} · harness: {harnesses} · {busy}/{} job slot(s) busy",
            if roster.serving {
                "ADVERTISING, ready for work"
            } else {
                "NOT serving — no live harness"
            },
            self.slots.capacity
        );
    }

    /// Re-ask the relay for stored payment gift-wraps and ingest whatever comes back.
    ///
    /// This is the money leg's recovery path, and it is response-based: we make a request and read
    /// what is returned, rather than waiting on a broadcast that may never come. A live kind-1059
    /// subscription that has silently gone deaf strands a payment indefinitely otherwise — see
    /// [`WRAP_BACKFILL_INTERVAL_SECS`] for the field case.
    ///
    /// Every wrap is routed through the normal `on_gift_wrap` path, so all the pay-once guards apply
    /// unchanged: a re-seen wrap hits the receipt dedup, and an already-spent token fails closed.
    /// Re-scanning a wide window is therefore safe by construction.
    ///
    /// LOAD-BEARING LOG: the "fetching" line is emitted unconditionally, BEFORE the fetch. It is the
    /// only periodic line a healthy idle node produces, which makes it the positive liveness signal
    /// external supervision has — a parked process satisfies pid-presence, so absence of failures is
    /// not evidence of health. Do not make it conditional to reduce noise.
    async fn run_wrap_backfill(&self) {
        self.report_status();
        let since = match resolve_backfill_since(
            self.node.store().last_receipt_unix(),
            self.node.store().oldest_unsettled_delivery_unix(),
        ) {
            Ok(since) => since,
            Err(error) => {
                opline!(
                    "seller node wrap backfill: ABORT — cursor read failed (retrying next cycle, \
                     NOT defaulting to since=0): {error}"
                );
                return;
            }
        };
        opline!("seller node wrap backfill (periodic): fetching stored kind-1059(s) since ts={since}");
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.seller_pubkey)
            .since(nostr_sdk::Timestamp::from(since));
        match tokio::time::timeout(
            WRAP_BACKFILL_FETCH_TIMEOUT,
            self.client
                .fetch_events(filter, WRAP_BACKFILL_FETCH_TIMEOUT / 2),
        )
        .await
        {
            Ok(Ok(events)) => {
                opline!(
                    "seller node wrap backfill (periodic): {} stored kind-1059(s) returned since ts={since}",
                    events.len()
                );
                for event in events {
                    self.on_gift_wrap(&event).await;
                }
                self.drain().await;
            }
            Ok(Err(error)) => opline!(
                "seller node WARN: wrap backfill fetch failed (continuing; live 1059 subscription \
                 active): {error}"
            ),
            Err(_) => opline!(
                "seller node WARN: wrap backfill fetch timed out after {}s (continuing; live 1059 \
                 subscription active)",
                WRAP_BACKFILL_FETCH_TIMEOUT.as_secs()
            ),
        }
    }

    /// Re-ask the relay for stored job OFFERS and ingest whatever comes back — the offers analog of
    /// [`Self::run_wrap_backfill`], recovering the offer leg the way that recovers the money leg.
    ///
    /// Same failure it repairs (#560): a registered offer subscription that has silently gone deaf —
    /// the session still answers our REQs, so the liveness probe cannot see it — strands every new
    /// offer until a restart. `fetch_events` issues a TRANSIENT REQ under a pool-GENERATED id and
    /// returns the events off the call itself, so it bypasses the per-connection seen-cache that
    /// swallows an already-seen id on the notification path (see [`Self::reconsider_capacity_skips`]);
    /// a plain re-subscribe re-REQs but never re-fires `on_offer`, which is why recovery has to be a
    /// fetch and not a re-subscribe.
    ///
    /// This RECOVERS MISSED OFFERS; it does not DETECT a registered sub's silent death. Telling a
    /// dead leg from a merely-idle one is the subscription-map reconciler's job (#172, unimplemented).
    /// The layering mirrors the wrap side (see [`WRAP_BACKFILL_INTERVAL_SECS`]): probe = session
    /// liveness, backfill = leg recovery, #172 = registration integrity — none subsumes another.
    ///
    /// The fetch filters MATCH the live offer subscription ([`offer_subscription_filters`]) so a
    /// backfill covers exactly what the live leg does — the `#t=maxplayer` guard and the `#p` targeting
    /// included — bounded to [`resolve_offer_backfill_since`]. `report_status` is left to the wrap
    /// backfill that precedes it on the shared tick, so the pair emits one status line, not two; the
    /// "fetching" line here is still emitted UNCONDITIONALLY, as the positive signal this leg ran.
    async fn run_offer_backfill(&self) {
        let open_pool = self.claim_open_pool();
        let now = now_unix().max(0) as u64;
        let offer_backfill_secs = self
            .node
            .home()
            .config
            .seller
            .as_ref()
            .map(|seller| seller.offer_backfill_secs)
            .unwrap_or(0);
        let since = resolve_offer_backfill_since(now, offer_backfill_secs);
        let filters = offer_subscription_filters(
            self.seller_pubkey,
            open_pool,
            offer_backfill_secs,
            Some(since),
            nostr_sdk::Timestamp::from(now),
        );
        opline!(
            "seller node offer backfill (periodic): fetching stored kind-{}(s) since ts={} ({} filter(s))",
            JOB_OFFER_KIND,
            since.as_secs(),
            filters.len()
        );
        for filter in filters {
            match tokio::time::timeout(
                OFFER_BACKFILL_FETCH_TIMEOUT,
                self.client
                    .fetch_events(filter, OFFER_BACKFILL_FETCH_TIMEOUT / 2),
            )
            .await
            {
                Ok(Ok(events)) => {
                    opline!(
                        "seller node offer backfill (periodic): {} stored kind-{}(s) returned",
                        events.len(),
                        JOB_OFFER_KIND
                    );
                    for event in events {
                        self.on_offer(&event).await;
                    }
                    self.drain().await;
                }
                Ok(Err(error)) => opline!(
                    "seller node WARN: offer backfill fetch failed (continuing; live offer \
                     subscription active): {error}"
                ),
                Err(_) => opline!(
                    "seller node WARN: offer backfill fetch timed out after {}s (continuing; live \
                     offer subscription active)",
                    OFFER_BACKFILL_FETCH_TIMEOUT.as_secs()
                ),
            }
        }
    }

    /// Publish one own-heartbeat (kind-30340) — best-effort liveness/discovery + the watchdog's
    /// publish-liveness leg. Signed through the signer actor and sent on the shared client; a failure
    /// is logged and never wedges the loop.
    ///
    /// Returns whether the relay CONFIRMED the write (#509) — the publish leg of relay-observed
    /// liveness the watchdog AND-gates its clock on. A `true` from the no-seller no-op path means
    /// "nothing to publish", never "publish landed": with no `[seller]` config there is no heartbeat
    /// to lose, so it must not spuriously drive the watchdog RED.
    async fn publish_heartbeat(&self) -> bool {
        let Some(seller) = self.node.home().config.seller.clone() else {
            return true;
        };
        let in_flight = self.live_in_flight("heartbeat");
        // ONE roster read for both wire signals: a seat that has dropped every harness advertises
        // `accepting=n`, so it stops attracting work instead of looking open and declining later.
        let roster = self.agents.advertisement();
        // The mints ride the announcement every beat (§4.2). They come from the SAME config field
        // the pay path validates against, so what a buyer reads here is what this seat can settle
        // on — before #645 they were written once into a kind-31990 content at boot, which no
        // config change ever revisited.
        // `names` is CLONED rather than moved because the capability comes off the SAME
        // `advertisement()` snapshot: a second call could observe a different roster, and then the
        // advertised names and the advertised models would describe two different moments. One
        // snapshot, both reads.
        let draft = crate::heartbeat::heartbeat_for_state(
            in_flight,
            roster.serving,
            seller.rate_sats,
            self.node.home().config.accepted_mints.clone(),
            roster.names.clone(),
            roster.capability(&self.node.home().config.seat),
        )
        .to_event_draft();
        self.publish_seat_announcement(draft, "heartbeat").await
    }

    /// #747 — publish the seat's TERMINAL announcement: the same kind-30340, `accepting=n`, once,
    /// as this node leaves the selling role. Called from the single exit path in [`Self::run_loop`],
    /// so it covers a requested shutdown, a relay-pool close, and a loop that ended on an error
    /// alike.
    ///
    /// WHY IT MATTERS THAT THIS IS THE SAME EVENT: kind-30340 is addressable, so the relay keeps one
    /// announcement per `(pubkey, d)` and the last one published stands as the seat's permanent
    /// public answer. A seat that just stops beating leaves `accepting=y` there forever — no newer
    /// event exists to correct it, and none ever will, so the lie is stable rather than transient.
    /// Overwriting that slot is the only correction available, and this is it.
    ///
    /// ⛔ **INSURANCE, NOT REPAIR.** Nothing here runs unless the process is alive to run it. SIGKILL,
    /// a panic that skips unwinding, an OOM kill and a power cut leave the stale `accepting=y`
    /// standing exactly as before, and consumer-side recency filtering remains the only thing
    /// covering those. This narrows the window; it does not make the directory truthful.
    ///
    /// Best-effort and BOUNDED. It is on the exit path, where a hung relay must not turn a stop into
    /// a hang — an operator whose `Ctrl-C` did not return would send SIGKILL, which is precisely the
    /// exit no retraction can cover. So a slow relay costs at most
    /// [`RETRACTION_PUBLISH_TIMEOUT`] and then the node leaves anyway, as loudly as it failed.
    async fn publish_retraction(&self) {
        let Some(seller) = self.node.home().config.seller.clone() else {
            return;
        };
        // A node configured to publish no seat announcements publishes no terminal one either: with
        // heartbeats off this run, nothing of ours is in the directory to retract. Residue from an
        // EARLIER run that had them on is not corrected here — that case is left to consumer-side
        // recency filtering, exactly as a crash is.
        if !crate::heartbeat::resolve_enabled(&self.node.home().config.seller_heartbeat) {
            return;
        }
        // The roster still names what this seat can run: leaving the market is not a claim to have
        // forgotten how to work, and `accepting` is the field that carries "not taking work".
        let roster = self.agents.advertisement();
        // The capability rides the terminal beat for the same reason the roster does: leaving the
        // market is not a claim to have forgotten what this seat could run. `accepting=n` is what
        // carries "not taking work", and it is passed as a literal by `retraction_for_state`.
        let draft = crate::heartbeat::retraction_for_state(
            self.live_in_flight("retraction"),
            seller.rate_sats,
            self.node.home().config.accepted_mints.clone(),
            roster.names.clone(),
            roster.capability(&self.node.home().config.seat),
        )
        .to_event_draft();

        opline!("seller node: publishing terminal kind-30340 (accepting=n) — retracting this seat");
        match tokio::time::timeout(
            RETRACTION_PUBLISH_TIMEOUT,
            self.publish_seat_announcement(draft, "retraction"),
        )
        .await
        {
            Ok(true) => opline!(
                "seller node: seat retracted (accepting=n published); a reader must still treat an \
                 old announcement as stale — this covers a graceful exit only"
            ),
            // Both failures are the SAME field outcome as before #747 — the seat's last
            // `accepting=y` stands — so say so rather than let a quiet exit imply it was corrected.
            Ok(false) => opline!(
                "seller node WARN: seat retraction did not reach the relay; this seat's last \
                 accepting=y announcement stands until a future run replaces it"
            ),
            Err(_) => opline!(
                "seller node WARN: seat retraction timed out after {}s; leaving anyway — this \
                 seat's last accepting=y announcement stands until a future run replaces it",
                RETRACTION_PUBLISH_TIMEOUT.as_secs()
            ),
        }
    }

    /// A LIVE count of jobs occupying execution capacity — never `health().jobs`, which counts
    /// every row ever written and so never returns to zero (#313).
    fn live_in_flight(&self, what: &str) -> u32 {
        match self.node.store().jobs_in_flight() {
            Ok(count) => count,
            Err(error) => {
                // Fail toward AVAILABLE, as this path always has, but say so: a silent read failure
                // that parked the seat would be the same invisible-refusal shape as #313 itself.
                // On the retraction path the fallback costs nothing either way — that beat says
                // `accepting=n` whatever the count is (`retraction_for_state` passes
                // `anything_serving = false` as a literal), so only `queue_depth` is affected.
                opline!(
                    "seller node {what}: in-flight count unavailable ({error}); \
                     advertising as free this tick"
                );
                0
            }
        }
    }

    /// Sign one kind-30340 seat announcement through the signer actor and put it on the wire.
    /// Returns whether it reached the relay. Every failure is logged and none ever wedges the
    /// caller.
    ///
    /// Shared by the periodic beat and the #747 terminal beat so both leave by exactly one path: the
    /// retraction has to be the ordinary publisher told `accepting=false`, or it is not a
    /// replacement for what the ordinary publisher put there.
    async fn publish_seat_announcement(&self, draft: gateway::EventDraft, what: &str) -> bool {
        match self.node.signer().sign(draft, now_unix()).await {
            Ok(Ok(signed)) => {
                use nostr_sdk::JsonUtil as _;
                match nostr_sdk::Event::from_json(&signed.json) {
                    Ok(event) => match self.client.send_event_to([&self.relay_url], &event).await {
                        // #509: an `Ok(Output)` is NOT proof the relay stored the event — a single
                        // relay that rejects the write returns `Ok` with itself in `output.failed`.
                        // Confirm the relay actually acknowledged it; an `OK: false`, an empty
                        // `success`, or a top-level `Err`/timeout is the health-RED signal.
                        Ok(output) if publish_confirmed(&output) => true,
                        Ok(output) => {
                            let reasons: Vec<String> = output
                                .failed
                                .iter()
                                .map(|(url, reason)| format!("{url}: {reason}"))
                                .collect();
                            let detail = if reasons.is_empty() {
                                "no relay acknowledged the event".to_string()
                            } else {
                                reasons.join("; ")
                            };
                            opline!(
                                "seller node {what} publish NOT confirmed by relay (continuing): \
                                 {detail}"
                            );
                            false
                        }
                        Err(error) => {
                            opline!("seller node {what} publish failed (continuing): {error}");
                            false
                        }
                    },
                    Err(error) => {
                        opline!("seller node {what} encode failed (continuing): {error}");
                        false
                    }
                }
            }
            Ok(Err(error)) => {
                opline!("seller node {what} sign failed (continuing): {error}");
                false
            }
            Err(error) => {
                opline!("seller node signer actor gone at {what} (continuing): {error}");
                false
            }
        }
    }

    /// One stall recovery, with #162 bounded retries: a connect-phase failure (relay drops the socket
    /// before NIP-42 completes) is retried up to [`RECOVERY_MAX_ATTEMPTS`] with a short backoff WITHIN
    /// this recovery before yielding to the next heartbeat tick. Returns the attempt count on success.
    async fn recover_stall(
        &self,
        relay: &nostr_sdk::prelude::Relay,
        overlap_since: nostr_sdk::Timestamp,
    ) -> Result<u32, NodeError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self
                .reconnect_and_resubscribe(relay, overlap_since)
                .await
            {
                Ok(()) => return Ok(attempt),
                Err(error) if attempt < RECOVERY_MAX_ATTEMPTS => {
                    let backoff = recovery_backoff(attempt);
                    opline!(
                        "seller node RELAY-STALL recovery attempt {attempt} failed ({error}); \
                         retrying in {}s",
                        backoff.as_secs()
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Tear down the silently-dead connection and rebuild it: drop the stale registrations, reconnect,
    /// re-run NIP-42 (the p-gated kind-1059 resubscribe depends on it, same as boot), then resubscribe
    /// ALL filters with `since = overlap`.
    ///
    /// CLEARING BEFORE THE RECONNECT IS THE WHOLE OF #189. `RelayInner::post_connection` re-sends every
    /// registered `REQ` as its first act on socket-up (`relay/inner.rs:748-752`), before that
    /// connection has any NIP-42 state at all; auth only happens later, in the ingester
    /// (`inner.rs:936`). maxplayer-relay evaluates its p-gate against the empty authed pubkey of that
    /// unauthenticated session and answers `restricted:` — the PERMANENT prefix — where the truth is
    /// the retryable `auth-required:`. nostr-sdk takes `restricted:` at its word and DELETES the
    /// subscription (`inner.rs:1028` → `remove_subscription`), so the post-auth `resubscribe()` at
    /// `inner.rs:941` cannot see it and never restores it. Carrying registrations across the socket
    /// boundary therefore kills the kind-1059 money leg on every single recovery. With nothing
    /// registered, that first resubscribe has nothing to send and the REQs go out below — after auth,
    /// the same order boot has always had.
    async fn reconnect_and_resubscribe(
        &self,
        relay: &nostr_sdk::prelude::Relay,
        overlap_since: nostr_sdk::Timestamp,
    ) -> Result<(), NodeError> {
        clear_subscription_registrations(&self.client, relay).await;
        match reconnect_and_authenticate(&self.client, relay).await {
            Ok(AuthWait::Authenticated) => self.subscribe_all(Some(overlap_since)).await,
            Ok(AuthWait::NoChallenge) => {
                // Same posture as boot: proceed, loudly. Auto-auth stays on, so a challenge raised on
                // the REQ itself still authenticates — but a p-gated resubscribe issued before that
                // completes is exactly the condition above, so say so rather than report a clean
                // recovery.
                opline!(
                    "seller node WARN: recovery saw no NIP-42 challenge within {CONNECT_WAIT:?}; \
                     resubscribing anyway (auto-auth stays ON). p-gated kind-1059 receive may be \
                     degraded until auth completes."
                );
                self.subscribe_all(Some(overlap_since)).await
            }
            Err(error) => {
                // The registrations are gone and the new socket never authenticated. Put them back:
                // the SDK's own background reconnect is a real recovery path in the field (the run
                // loop distinguishes it in the RESTORED line) and it can only restore subscriptions it
                // still knows about. Re-registering makes a failed attempt no worse than not having
                // tried; the next heartbeat tick retries the whole recovery.
                if let Err(restore) = self.subscribe_all(Some(overlap_since)).await {
                    opline!(
                        "seller node WARN: subscriptions could not be restored after a failed \
                         recovery ({restore}); the next heartbeat tick retries"
                    );
                }
                Err(NodeError::Relay(format!("reconnect NIP-42 auth: {error}")))
            }
        }
    }

    /// Consider one offer event: parse it, apply the money-safety gates, and — if admitted — journal
    /// the claim (creq + claim event) into the store in one transaction, then drain so the claim is
    /// published. Every non-claim path logs a named reason; there is no silent drop.
    ///
    /// The claim-time creq is authored here from the seller's OWN config (accepted mints + rate) and
    /// journaled via `claim_and_enqueue`, so delivery later signs the STORED creq's hash (invariant
    /// 8) and the restart redeem-guard reads its mints (Fix Q) — never a rebuild from live config.
    /// Reclaim execution slots reserved by claims that have sat unawarded past the lapse timeout,
    /// releasing the durable claim to match. Runs on the drain tick. Without it, reserve-at-claim
    /// would let a claim a buyer never awards hold its slot for the full (long) publish window —
    /// permanently shrinking a busy node's capacity. Runs on the event loop, so it never races the
    /// reserve/take done by `on_offer`/`on_award`.
    fn sweep_lapsed_claims(&self) {
        for job_id in self.slots.sweep_lapsed(Instant::now()) {
            match self.node.store().release_claim(&job_id, now_unix()) {
                Ok(1..) => opline!(
                    "seller node slot reclaimed job_id={job_id}: parked claim lapsed unawarded ({} slot(s) free)",
                    self.slots.available()
                ),
                // The slot came back but no claim row moved: the row had already left `claimed`.
                // Reported as what it is, so the log never credits a release that did not happen.
                Ok(_) => opline!(
                    "seller node slot reclaimed job_id={job_id}: no claim row to release (state={}) ({} slot(s) free)",
                    self.claim_state_label(&job_id),
                    self.slots.available()
                ),
                Err(error) => {
                    opline!("seller node slot reclaim job_id={job_id}: release_claim failed ({error})")
                }
            }
        }
    }

    /// The claim row's state, for a LOG that must name why a release moved nothing. Never fails the
    /// caller: a read error or a missing row becomes a label, because a logging path that can return
    /// an error invites a caller to drop the line entirely and go back to saying nothing.
    fn claim_state_label(&self, job_id: &str) -> String {
        match self.node.store().claim_row_state(job_id) {
            Ok(Some(state)) => state,
            Ok(None) => "absent".to_owned(),
            Err(error) => format!("unreadable ({error})"),
        }
    }

    async fn on_offer(&self, event: &nostr_sdk::Event) {
        let Some(seller) = self.node.home().config.seller.clone() else {
            opline!("seller node offer skipped: no [seller] config");
            return;
        };
        let draft = event_to_draft(event);
        let offer = match parse_offer(&draft) {
            Ok(offer) => offer,
            Err(error) => {
                opline!(
                    "seller node offer skip id={}: {}",
                    event.id,
                    offer_parse_refusal(&error)
                );
                return;
            }
        };
        // Contribution offers are gated by the operator's `[seller] contribution_enabled` flag
        // (default on): served when enabled, refused when the operator turns them off, and a
        // malformed contribution is refused either way (never run as from-scratch). #591: a served
        // contribution's pin is captured HERE — the only site with the offer tags — and persisted at
        // claim, so execute clones the target at base_oid instead of an empty workdir.
        let contribution = match contribution_serve_gate(&draft.tags, seller.contribution_enabled) {
            ContributionServeGate::NotContribution => None,
            ContributionServeGate::Serve => {
                opline!("seller node serving contribution offer id={}", event.id);
                // The gate already confirmed a well-formed contribution; re-parse to carry the pin
                // without widening #590's pure gate return. Total + cheap (contribution offers only).
                parse_contribution_offer(&draft.tags).ok().flatten()
            }
            ContributionServeGate::RefuseDisabled => {
                opline!(
                    "seller node offer skip id={}: contribution offers disabled (contribution_enabled=false)",
                    event.id
                );
                return;
            }
            ContributionServeGate::Malformed(error) => {
                opline!("seller node offer skip id={}: malformed contribution ({error})", event.id);
                return;
            }
        };

        let seller_pubkey = self.seller_pubkey.to_hex();
        // The buyer is the offer's author. Resolved here — ahead of the classify call — so the
        // allowlist fence can test it and the skip log can name the declined pubkey.
        let buyer_pubkey = event.pubkey.to_hex();
        let now = now_unix();
        let deadline_unix = match classify_offer(&offer, &seller, &self.agents, &seller_pubkey, &buyer_pubkey, now as u64, event.created_at.as_secs())
        {
            ClaimDecision::Claim { deadline_unix } => deadline_unix,
            ClaimDecision::Skip(skip) => {
                // Every skip is named, never silent. The two buyer-eligibility refusals additionally
                // name the declined buyer (the other reasons are offer-intrinsic and need no
                // identity). Both are spelled out rather than left to the catch-all: a refusal the
                // operator can only fix by knowing WHICH buyer was turned away is useless without it.
                match skip {
                    SkipReason::NotAllowlisted | SkipReason::OpenTargetedRefused => opline!(
                        "seller node offer skip id={}: {} (buyer={})",
                        event.id,
                        skip.reason(),
                        buyer_pubkey
                    ),
                    _ => opline!("seller node offer skip id={}: {}", event.id, skip.reason()),
                }
                // Buyer-visibility: a TARGETED-to-self under-rate refusal also emits a feedback-kind
                // `status=error` so the buyer learns WHY (distinguishes rate-refusal from a crash /
                // silence). Open-pool under-rate stays log-only (spam guard); a lapsed offer never
                // emits (only RateGate). Mirrors the legacy under-rate feedback dropped at cutover.
                let targeted_to_self = offer.seller_pubkey.as_deref() == Some(seller_pubkey.as_str());
                if should_publish_under_rate_feedback(skip, targeted_to_self, offer.amount, seller.rate_sats)
                {
                    // #582: the classifier decides this skip EARNS feedback; the dedup, layered on top,
                    // decides whether we have already SENT it for this offer. The #560 offer-backfill
                    // re-feeds every stored offer through `on_offer` each tick (~300s) across the whole
                    // lookback, so without this gate a targeted under-rate offer in that window re-emits
                    // an identical `BelowRate` feedback to the buyer on every pass (~12×/window). First
                    // sight per offer id ⇒ the buyer hears the price refusal once. RECORD ONLY AFTER a
                    // successful emit (the `&&` short-circuits the record on a `false`), so a transient
                    // publish failure retries on the next re-ingest (arm-after-the-event) instead of
                    // being suppressed forever. Feedback-only: the claim/money path is untouched.
                    let offer_id = event.id.to_hex();
                    if !self.fed_under_rate_offers.contains(&offer_id)
                        && self.publish_under_rate_feedback(event, skip.reason()).await
                    {
                        self.fed_under_rate_offers.record(&offer_id);
                    }
                }
                return;
            }
        };

        // The job id IS the offer event id (as on the legacy path); the buyer (its author) was
        // resolved above for the allowlist fence.
        let job_id = event.id.to_hex();
        self.claim_offer(
            &job_id,
            &buyer_pubkey,
            &offer,
            &seller_pubkey,
            deadline_unix,
            now,
            contribution.as_ref(),
        )
        .await;
    }

    /// #541: a co-signed kind-3400 settlement receipt marks its offer terminal. Cache
    /// `(offer_id → receipt author)` so the claim path skips an offer already settled by another seat
    /// (see [`Self::claim_offer`] and [`TerminalOffers`]). The author is the event's own pubkey,
    /// already signature-verified by the client, so no crypto runs here; the buyer-binding that makes
    /// a forged receipt inert is applied at claim time, where the offer's real buyer is known. A
    /// receipt with no root offer tag is ignored — fail-open, the offer stays claimable.
    async fn on_receipt(&self, event: &nostr_sdk::Event) {
        let draft = event_to_draft(event);
        let Some(offer_id) = crate::gateway::settled_offer_id(&draft) else {
            opline_verbose!("seller node receipt id={}: no root offer e-tag; ignoring", event.id);
            return;
        };
        let author = event.pubkey.to_hex();
        self.terminal_offers.record_receipt(&offer_id, &author);
        opline_verbose!("seller node receipt: offer {offer_id} settled by author={author} (terminal)");
    }

    /// The claim tail: capacity back-pressure → journal the offer facts → build the creq/claim →
    /// reserve a slot → park the claim. Shared by [`Self::on_offer`] (a wire offer just classified
    /// `Claim`) and [`Self::reconsider_capacity_skips`] (a RECORDED offer re-driven from the store
    /// once a slot frees). Idempotent via `claim_and_enqueue`: an offer we already claimed is a
    /// `Claimed::Idempotent` no-op and its fresh reservation is released, so re-driving a recorded
    /// offer can never double-claim — the same property the restart backfill relies on.
    async fn claim_offer(
        &self,
        job_id: &str,
        buyer_pubkey: &str,
        offer: &ParsedOffer,
        seller_pubkey: &str,
        deadline_unix: u64,
        now: i64,
        contribution: Option<&ContributionOffer>,
    ) {
        // #541/#814: refuse an offer that is no longer ours to claim, before any work. Two
        // buyer-authenticated signals land here (see [`Suppression`]): a co-signed kind-3400 receipt
        // means the offer was awarded + settled (to us or another seat) and is terminal; a kind-3405
        // award or kind-3406 acceptance naming another seller's claim means the race is already over.
        // Either way, claiming would park a slot on decided work, mask real availability, and — the
        // whole of #814 — publish a LOSING CLAIM into the market after the fact.
        //
        // THE SINGLE CHOKE POINT, and that is why the check lives here rather than in `on_offer`:
        // `claim_offer` is shared by `on_offer` (the wire path) and `reconsider_capacity_skips` (a
        // recorded offer re-driven once a slot frees). #814's repro runs through the SECOND of those,
        // so a gate on the first alone would not have caught it.
        //
        // The buyer-binding is load-bearing: a forged event (author != buyer) sits in the cache but
        // never matches here, so the worst a forger or a flood achieves is the pre-#541 wasted slot,
        // never a spend. FAIL-OPEN: an unknown / cold / evicted / EXPIRED offer is not skipped —
        // over-suppression would strand a real award (compute spent, nothing paid), which is the one
        // outcome worse than the bug. Consulted only on the event loop, before record_offer /
        // try_reserve, so nothing is reserved for a decided offer.
        if let Some(suppression) = self
            .terminal_offers
            .suppressed_by(job_id, buyer_pubkey, now as u64)
        {
            opline!(
                "seller node offer skip id={job_id}: {}",
                suppression.skip_reason().reason()
            );
            return;
        }
        // Capacity back-pressure: never hold unbounded parked claims.
        match self.node.store().health() {
            Ok(health) if health.open_claims >= AWAITING_AWARD_CAP => {
                opline!(
                    "seller node offer skip id={job_id}: awaiting-award backlog full (cap {AWAITING_AWARD_CAP})"
                );
                return;
            }
            Ok(_) => {}
            Err(error) => {
                opline!("seller node offer skip id={job_id}: store health read failed ({error})");
                return;
            }
        }

        // #591: persist the contribution pin BEFORE record_offer. execute_job only runs on an awarded
        // claim, which requires the recorded offer — so writing the pin first makes pin ≤ offer ≤
        // claim: a crash can never leave an offer recorded (hence claimable) without its pin, which
        // would silently fall back to an empty workdir and mis-deliver. A pin-write error fails the
        // claim (fail-closed); the offer re-ingest retries both (INSERT OR IGNORE).
        if let Some(contribution) = contribution {
            let pin = super::store::ContributionPin {
                owner_pubkey: contribution.target.owner_pubkey().to_owned(),
                clone_url: contribution.target.clone_url().to_owned(),
                base_branch: contribution.base.branch().to_owned(),
                base_oid: contribution.base.oid().to_owned(),
            };
            if let Err(error) = self.node.store().record_contribution_pin(job_id, &pin, now) {
                opline!("seller node offer skip id={job_id}: contribution pin persist failed ({error})");
                return;
            }
        }

        // Journal the offer facts BEFORE claiming: the award arm reads the buyer to authorize an
        // award (author MUST be the offer's buyer), and the pay path reads amount/unit as the redeem
        // terms. Idempotent — a re-seen offer is a no-op (so a reconsider re-drive never re-writes an
        // already-recorded offer).
        if let Err(error) = self
            .node
            .store()
            .record_offer(&offer_row(job_id, buyer_pubkey, offer), now)
        {
            opline!("seller node offer skip id={job_id}: record offer failed ({error})");
            return;
        }

        let creq = match gateway::creq::build_seller_creq(
            job_id,
            offer.amount,
            &offer.unit,
            &self.node.home().config.accepted_mints,
            seller_pubkey,
        ) {
            Ok(creq) => creq,
            Err(error) => {
                opline!("seller node offer skip id={job_id}: creq build failed ({error})");
                return;
            }
        };
        // The claim advertises what this node can run, so the buyer's award filter can hold it to
        // the harness its job asked for.
        //
        // ONE `advertisement()` snapshot feeds both the names and the capability, rather than two
        // reads of the live roster. The award decides on what THIS claim carries, so a claim whose
        // names came from one moment and whose models came from another would attribute a model to a
        // harness the seat was not offering when it said so — and nothing downstream could detect it,
        // because both halves parse.
        let roster = self.agents.advertisement();
        let claim = claim_draft(
            job_id,
            buyer_pubkey,
            seller_pubkey,
            &creq,
            &roster.names,
            // The claim holds the declared colour and emits none of it: `claim_draft` asks only for
            // the filterable tags. Passing it is not a leak, and passing a stub here instead would
            // make this the one site whose capability came from somewhere other than the config.
            &roster.capability(&self.node.home().config.seat),
        );
        // Reserve-at-claim: a fully loaded node has no free slot and simply does not claim, which is
        // how it stays invisible to the market. The gate is consulted only here on the event loop,
        // never concurrently with itself, so two offers can never both take the last slot.
        let reserved = self.slots.try_reserve(job_id);
        if reserved == Reserve::Full {
            opline!("seller node offer skip id={job_id}: {}", SkipReason::SlotsBusy.reason());
            // #450: remember the capacity skip so a freed slot re-drives the recorded offer straight
            // from the store (see reconsider_capacity_skips) — the offer is recorded but unclaimed
            // until then, and was previously only revisited by a restart.
            self.capacity_skip_pending
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        // Release a FRESH reservation if the claim does not actually become a new parked claim (dedup
        // no-op or journal error). An already-parked job id keeps its existing slot untouched.
        let release_on_no_claim = |runner: &Self| {
            if reserved == Reserve::Reserved {
                runner.slots.release(job_id);
            }
        };
        match self.node.store().claim_and_enqueue(
            job_id,
            job_id,
            &creq,
            &claim,
            now,
            now + CLAIM_PUBLISH_WINDOW_SECS,
            now,
        ) {
            Ok(super::store::Claimed::New) => {
                opline!(
                    "seller node claimed job_id={job_id} buyer={buyer_pubkey} amount={} deadline={deadline_unix} slot-reserved (awaiting award; {} slot(s) free)",
                    offer.amount,
                    self.slots.available()
                );
                // The caller drains after dispatch, publishing the just-enqueued claim.
            }
            Ok(super::store::Claimed::Idempotent) => {
                // Verbose-only (#489): a re-seen offer we already claimed is nothing happening.
                // It reports no state change and prompts no operator decision, and relays redeliver
                // often enough that it crowds out lines that do.
                //
                // One case is exempt and logs at normal level: an offer whose job we already
                // FINISHED. A buyer re-serves an offer for its whole deadline window, so a
                // delivered/paid/failed job keeps arriving and THIS dedup is what declines it. The
                // only skip reason visible on this path is slot exhaustion, which is a different
                // guard — so an operator watching a seat with free slots decline its own completed
                // work sees the decision and no reason for it, and would credit the wrong
                // protector. A guard that is silent while it works is equally silent while it
                // degrades. Naming it costs one line on a bounded population (finished jobs), not
                // on the redelivery firehose #489 was written to suppress.
                let finished = self.node.store().job_state(&job_id).ok().flatten();
                match already_handled_skip_line(&job_id, finished) {
                    Some(line) => opline!("{line}"),
                    None => opline_verbose!(
                        "seller node offer id={job_id}: already claimed (dedup no-op)"
                    ),
                }
                release_on_no_claim(self);
            }
            Err(error) => {
                opline!("seller node claim failed job_id={job_id}: {error}");
                release_on_no_claim(self);
            }
        }
    }

    /// Handle one ACCEPT (kind-3406): the buyer's pay-bind against a delivered result. Binds a
    /// still-unbound claim and **never executes**.
    ///
    /// The seller subscribes to ACCEPT for exactly one reason: it is a second, later event naming
    /// our claim, so it is the only remaining way to bind a claim whose AWARD never reached us —
    /// the across-restart re-bind of TOOTH 3 (#143), where the award is delivered only to the
    /// reopened node. While ACCEPT shared the award kind, `on_award` absorbed it and that re-bind
    /// worked by accident; splitting the kinds would have narrowed it silently, so the path is made
    /// explicit here instead.
    ///
    /// Bind-if-unbound is a real precondition, not a formality. `record_award` is unconditional
    /// once entered — it `UPDATE claims SET state = 'awarded'`, which for an already-delivered job
    /// would regress a terminal claim row — and it inserts an `awards` row keyed by award id, so a
    /// second row for one job makes [`Store::job_award_time`] depend on which row SQLite returns
    /// first. That value is the delivery commit's authored-at, the thing that keeps a re-created
    /// delivery byte-identical (invariant 2). So: read first, write only when there is nothing there.
    ///
    /// Never executes, in either branch. Execution follows the AWARD. An ACCEPT arrives after a
    /// delivery the buyer has already verified, so there is nothing left to run, and
    /// `execute_job`'s state guard would refuse it anyway — this handler does not lean on that
    /// guard, it simply has no execute path to reach.
    async fn on_accept(self: &Arc<Self>, event: &nostr_sdk::Event) {
        let draft = event_to_draft(event);
        let Some(accept) = crate::gateway::parse_accept(&draft) else {
            return;
        };
        let job_id = accept.offer_id.clone();

        // Same authorization as the award path: only the offer's own buyer may bind it.
        let buyer = match self.node.store().offer_facts(&job_id) {
            Ok(Some((buyer, _, _))) => buyer,
            Ok(None) => {
                opline!("seller node accept ignore job_id={job_id}: no offer of ours recorded");
                return;
            }
            Err(error) => {
                opline!("seller node accept ignore job_id={job_id}: offer read failed ({error})");
                return;
            }
        };
        if event.pubkey.to_hex() != buyer {
            opline!(
                "seller node accept ignore job_id={job_id}: author is not the offer's buyer"
            );
            return;
        }
        match self.node.store().job_creq(&job_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                // #814 requirement (2), the BACKUP suppression: an acceptance is buyer-authenticated
                // evidence that this offer was decided, and it is the only such evidence when the
                // award itself never reached us. Same treatment as the award path — the author gate
                // above has already run, and the helper re-checks it rather than trusting the caller.
                self.suppress_taken_elsewhere(&job_id, &buyer, event, "accept");
                return;
            }
            Err(error) => {
                opline!("seller node accept ignore job_id={job_id}: claim read failed ({error})");
                return;
            }
        }

        match self.node.store().job_award_time(&job_id) {
            // The overwhelmingly common case: the AWARD already bound this job. The ACCEPT is
            // information, not instruction — record nothing, run nothing.
            Ok(Some(_)) => {
                opline!(
                    "seller node accept job_id={job_id} buyer={buyer}: pay-bind observed (already awarded — no action)"
                );
            }
            // The award never reached us, so this ACCEPT may be the only evidence of a selection —
            // but of WHOSE claim is a question the guards above cannot answer. `job_creq` proves we
            // claimed this job; it does not prove the buyer chose our claim. On an untargeted offer
            // a LOSING claimant satisfies every guard so far while holding its own losing claim, so
            // binding here on existence alone parks a slot on work another seat won (#626). The
            // accepted claim id arrives in the same event, so ask the identity question directly.
            Ok(None) => {
                // Ensure our claim is on the wire, then read its published id for the win check —
                // the same order `on_award` uses, for the same reason.
                self.drain().await;
                let our_claim_id = match self.node.store().outbox_row(&format!("claim:{job_id}")) {
                    Ok(Some((_, _, published))) => published,
                    _ => None,
                };
                let accept_author = event.pubkey.to_hex();
                match match_award(
                    &accept.claim_id,
                    our_claim_id.as_deref(),
                    &accept_author,
                    &buyer,
                ) {
                    // The matcher answers identity, never action: `Execute` here means only "this
                    // ACCEPT names our claim". The AWARD path turns that answer into execution; an
                    // ACCEPT binds and stops, exactly as this handler's doc states.
                    AwardMatch::Execute => {
                        match self.node.store().record_award(
                            &event.id.to_hex(),
                            &job_id,
                            &buyer,
                            now_unix(),
                        ) {
                            Ok(outcome) => opline!(
                                "seller node accept job_id={job_id} buyer={buyer}: bound from ACCEPT with no prior award ({outcome:?}) — NOT executing"
                            ),
                            Err(error) => opline!(
                                "seller node accept job_id={job_id}: bind from accept failed ({error})"
                            ),
                        }
                    }
                    // The buyer accepted another seat's claim. We lost, and this event says so as
                    // conclusively as the award would: release the claim and the slot rather than
                    // hold capacity for work that is already someone else's.
                    AwardMatch::Release => {
                        self.slots.release(&job_id);
                        match self.node.store().release_claim(&job_id, now_unix()) {
                            Ok(1..) => opline!(
                                "seller node released claim job_id={job_id}: buyer accepted another seller's claim (bound nothing)"
                            ),
                            Ok(_) => opline!(
                                "seller node accept job_id={job_id}: buyer accepted another seller's claim, but no claim row was in 'claimed' (state={}) — nothing released, bound nothing",
                                self.claim_state_label(&job_id)
                            ),
                            Err(error) => opline!(
                                "seller node accept release failed job_id={job_id}: {error}"
                            ),
                        }
                    }
                    // FAIL CLOSED. Our published claim id is unreadable, so nothing here can show
                    // the buyer chose us — and binding on an unproven identity is the whole defect.
                    // (The author leg of the match cannot fire: this handler already returned above
                    // unless the author IS the offer's buyer.)
                    AwardMatch::Ignore => opline!(
                        "seller node accept ignore job_id={job_id}: our published claim id is unreadable — not binding"
                    ),
                }
            }
            Err(error) => {
                opline!("seller node accept ignore job_id={job_id}: award read failed ({error})")
            }
        }
    }

    /// #814 — an offer we RECORDED but never claimed has just been decided by its buyer. Mark it
    /// taken so the claim gate stops re-driving it, and persist that fact so a restart does not undo
    /// the decision.
    ///
    /// The bug this closes: at capacity we record an offer and skip the claim; the buyer awards
    /// another seller; a slot frees; `reconsider_capacity_skips` re-drives the offer and we publish a
    /// claim for a race that is already over. Nothing downstream catches it — the award named someone
    /// else's claim, so no release fires — and the losing claim sits in the market as false activity
    /// until it lapses. Both handlers previously DISCARDED this event at "no claim of ours".
    ///
    /// OPEN-POOL ONLY, by construction rather than by choice: `award_filter` scopes a targeted
    /// seller's REQ to its own pubkey, and an award p-tags only the WINNER, so a targeted seller never
    /// receives another seat's award and no design could suppress on one. An open-pool seller drops
    /// that scope (#456) and does receive them.
    ///
    /// AUTHORIZATION IS RE-CHECKED HERE rather than assumed from the caller. `on_accept` has already
    /// gated author == buyer; `on_award` has NOT (its check lives inside `match_award`, which this
    /// path returns before ever reaching). A string compare is cheap, and the alternative is a guard
    /// whose correctness depends on which of two callers you arrived from. `buyer` is read from OUR
    /// OWN recorded offer, never from the event — that non-circularity is what makes a forged award
    /// inert: it is refused here, and even a row that somehow reached the table re-hydrates under the
    /// real buyer's key and never matches the buyer-bound gate.
    ///
    /// FAIL-OPEN on every unknown: a foreign author, an unreadable or vanished offer row, or a failed
    /// persist all leave the offer claimable. Over-suppression is the one direction that costs money
    /// (a stranded award = compute spent, nothing paid); under-suppression is only the status quo.
    ///
    /// The caller guarantees the #814 HARD INVARIANT: both call sites are the `job_creq` == `None`
    /// arm, so we hold NO claim for this job, and neither reads it again across an await. A job we DO
    /// hold is never suppressed — that is the #563 foil, where suppressing would strand our own award.
    fn suppress_taken_elsewhere(
        &self,
        job_id: &str,
        buyer: &str,
        event: &nostr_sdk::Event,
        signal: &str,
    ) {
        let author = event.pubkey.to_hex();
        if author != buyer {
            opline_verbose!(
                "seller node {signal} job_id={job_id}: author={author} is not the offer's buyer; \
                 not suppressing (a forged decision can never take an offer off us)"
            );
            return;
        }
        // The offer's own `param:deadline` bounds the suppression — the only deadline the protocol
        // defines. A legitimately re-runnable job carries a NEW offer id (an award is write-once per
        // offer), and an offer past its deadline is already `Lapsed` at the gate, so expiring here can
        // never drop a real re-claim.
        let deadline_unix = match self.node.store().offer_row(job_id) {
            Ok(Some(offer)) => offer.deadline_unix as u64,
            Ok(None) => {
                opline!(
                    "seller node {signal} job_id={job_id}: offer row vanished before suppression; \
                     leaving it claimable (fail-open)"
                );
                return;
            }
            Err(error) => {
                opline!(
                    "seller node {signal} job_id={job_id}: offer read failed ({error}); leaving it \
                     claimable (fail-open)"
                );
                return;
            }
        };
        // DURABLE leg first, then the hot path. The `awards` row is what survives a restart with no
        // relay dependency — the in-memory cache alone would lose this on every bounce and the #560
        // offer-backfill would re-drive the offer straight back into a losing claim. A relay
        // redelivery would usually re-derive it, but "relay deafness manufactures absence" (#560/#563)
        // makes that recovery, not durability. `record_award` writes the row and creates NO job when
        // we hold no claim — the primitive already existed; only the early return kept it unreachable.
        let recorded = self
            .node
            .store()
            .record_award(&event.id.to_hex(), job_id, buyer, now_unix());
        match recorded {
            // The expected arm: award recorded, no claim of ours to bind, no job created.
            Ok(super::store::Awarded::NoClaim) => opline!(
                "seller node {signal} job_id={job_id} buyer={buyer}: decided in another seller's \
                 favour — not claiming (suppressed until the offer's deadline {deadline_unix})"
            ),
            // A redelivery of a decision we already recorded. Verbose-only, the #489 convention the
            // award path already follows: the FIRST sighting logs, the duplicate stays quiet.
            Ok(super::store::Awarded::Duplicate) => opline_verbose!(
                "seller node {signal} dedup job_id={job_id} (decision already recorded)"
            ),
            // ⛔ Unreachable: `record_award` returns `New` only when a claim row EXISTS, and both call
            // sites just read `job_creq` == `None` with no await in between on a single-threaded
            // LocalSet. If it ever fires, a claim appeared underneath us and this call bound an award
            // to it WITHOUT the `match_award` identity check that proves the buyer chose OUR claim
            // (#626). Say so loudly and do NOT suppress: we now hold a claim, and suppressing a job we
            // hold is precisely what strands a real award.
            Ok(super::store::Awarded::New) => {
                opline!(
                    "seller node {signal} job_id={job_id}: BUG — a claim appeared during suppression \
                     and the award bound it unchecked; not suppressing"
                );
                return;
            }
            // The suppression still holds for THIS process off the cache below; it just will not
            // survive a restart. Logged, never propagated — the same shape as the #563 marker's
            // persist failure.
            Err(error) => opline!(
                "seller node {signal} job_id={job_id}: decision persist failed ({error}); \
                 suppressing in memory only (re-derives from the relay next restart)"
            ),
        }
        self.terminal_offers
            .record_taken_elsewhere(job_id, buyer, deadline_unix);
    }

    /// #814 — refill the suppression cache from the store before the first event is served.
    ///
    /// This is the leg that makes the fix survive a restart. The cache is in-memory, so a bounce
    /// empties it, and the #560 periodic offer-backfill re-feeds EVERY stored offer through
    /// [`Self::on_offer`] each tick — so without this, a restart puts every recorded-but-lost offer
    /// straight back on the claim path and #814 returns intact. A relay redelivery of the award would
    /// usually re-derive the suppression, but that is RECOVERY, NOT DURABILITY: "relay deafness
    /// manufactures absence" (#560/#563), and an award the relay does not redeliver is
    /// indistinguishable from one that never happened.
    ///
    /// Runs BEFORE `subscribe_all`, so the gate is right on the FIRST event rather than only once a
    /// backfill lands. Read-only — no migration, no write, just the cache — and FAIL-OPEN: a store
    /// error leaves the cache cold, which is exactly today's behaviour.
    ///
    /// Restoring more than [`TERMINAL_OFFERS_CAP`] live offers would FIFO-evict the earliest of them,
    /// which is fail-open in the same direction as every other bound here: an evicted offer is
    /// claimable again, still bounded by its own deadline.
    fn rehydrate_suppressions(&self) {
        let taken = match self.node.store().offers_awarded_elsewhere(now_unix()) {
            Ok(rows) => rows,
            Err(error) => {
                opline!(
                    "seller node suppression re-hydrate: store read failed ({error}); starting cold \
                     (fail-open — offers stay claimable, as before #814)"
                );
                return;
            }
        };
        if taken.is_empty() {
            return;
        }
        for (offer_id, buyer, deadline_unix) in &taken {
            self.terminal_offers
                .record_taken_elsewhere(offer_id, buyer, *deadline_unix as u64);
        }
        opline!(
            "seller node suppression re-hydrate: {} live offer(s) already awarded to another seller \
             restored from the store — not re-claiming them after this restart (#814)",
            taken.len()
        );
    }

    /// Handle one award event: authorize it (author must be the offer's buyer), decide whether it
    /// names OUR claim, and bind or release accordingly. Binding records the award (which moves the
    /// claim → awarded and creates the job row); execution of the awarded job is the next port step.
    async fn on_award(self: &Arc<Self>, event: &nostr_sdk::Event) {
        let draft = event_to_draft(event);
        let Some(award) = parse_award(&draft) else {
            return;
        };
        let job_id = award.offer_id.clone();

        // Only an offer we recorded can be awarded to us; its buyer is the sole authorized awarder.
        let buyer = match self.node.store().offer_facts(&job_id) {
            Ok(Some((buyer, _, _))) => buyer,
            Ok(None) => {
                opline!("seller node award ignore job_id={job_id}: no offer of ours recorded");
                return;
            }
            Err(error) => {
                opline!("seller node award ignore job_id={job_id}: offer read failed ({error})");
                return;
            }
        };
        // We must hold a parked claim for this job (journaled creq present ⇒ we claimed).
        match self.node.store().job_creq(&job_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                // #814: nothing of ours to bind — but this is NOT a nothing-event. An authentic award
                // for an offer WE RECORDED means the race is over, and the capacity-skip reconsider
                // would otherwise re-drive that offer into a losing claim. Discarding it here is the
                // bug. `buyer` is from our own recorded offer, so the authorization stays non-circular.
                self.suppress_taken_elsewhere(&job_id, &buyer, event, "award");
                return;
            }
            Err(error) => {
                opline!("seller node award ignore job_id={job_id}: claim read failed ({error})");
                return;
            }
        }

        // Ensure our claim is on the wire, then read its published id for the win check.
        self.drain().await;
        let our_claim_id = match self.node.store().outbox_row(&format!("claim:{job_id}")) {
            Ok(Some((_, _, published))) => published,
            _ => None,
        };
        let award_author = event.pubkey.to_hex();
        match match_award(&award.claim_id, our_claim_id.as_deref(), &award_author, &buyer) {
            AwardMatch::Execute => {
                match self
                    .node
                    .store()
                    .record_award(&event.id.to_hex(), &job_id, &buyer, now_unix())
                {
                    Ok(super::store::Awarded::New) => {
                        // `requested_agent=` is the offer's REQUEST (preset-label vocabulary,
                        // normalized at parse) — deliberately named as a request, never `agent=`:
                        // nothing has run yet, and a request is not an attribution. The dispatched
                        // truth lands on the delivered line (#261). `any` = the offer stated no
                        // preference; `unknown` = the journal could not be read — absence and
                        // failure are never conflated.
                        let requested_agent = match self.node.store().offer_row(&job_id) {
                            Ok(Some(offer)) => {
                                offer.requested_agent.unwrap_or_else(|| "any".to_owned())
                            }
                            Ok(None) | Err(_) => "unknown".to_owned(),
                        };
                        opline!(
                            "seller node awarded job_id={job_id} buyer={buyer} requested_agent={requested_agent} — executing (spawned; {} slot(s) free)",
                            self.slots.available()
                        );
                        // Decouple execution from the loop: the permit rides the spawned task and
                        // releases on drop — covering delivery, every fail_job path, and a panic
                        // (unwind drops it). The parked reservation can be gone even for a FRESH
                        // award: the lapse sweep reclaims a park that sat unawarded past the
                        // timeout, and `record_award` still binds that claim when its award arrives
                        // late (#728) — and a redundant re-award finds the permit already moved out
                        // by the first (#279). `spawn_bounded_execution` answers every missing-park
                        // producer the same way — the task WAITS for a real permit — so an awarded
                        // job can never run outside slot accounting.
                        //
                        // `resume = false`: a FRESH award, never a stale restart re-drive — the #563
                        // belt does not query the relay (nothing is settled the instant we are awarded).
                        let runner = Arc::clone(self);
                        spawn_bounded_execution(&self.slots, job_id.clone(), move |job, slot| {
                            async move { runner.execute_job(&job, slot, false).await }
                        });
                    }
                    Ok(super::store::Awarded::Duplicate) => {
                        // Verbose-only (#489): the award was already recorded — a redelivery, not
                        // an event. The FIRST award still logs; only the duplicate is quiet.
                        opline_verbose!(
                            "seller node award dedup job_id={job_id} (already recorded)"
                        )
                    }
                    Ok(super::store::Awarded::NoClaim) => {
                        opline!("seller node award job_id={job_id}: no claim to bind");
                        // No claim to bind ⇒ any slot we reserved for it is orphaned; return it.
                        self.slots.release(&job_id);
                    }
                    Err(error) => opline!("seller node award record failed job_id={job_id}: {error}"),
                }
            }
            AwardMatch::Release => {
                // The buyer picked another seller: release the durable claim AND its reserved slot.
                self.slots.release(&job_id);
                match self.node.store().release_claim(&job_id, now_unix()) {
                    Ok(1..) => opline!(
                        "seller node released claim job_id={job_id}: buyer picked another seller's claim"
                    ),
                    // The claim had already left `claimed`, so this release moved nothing. Say so:
                    // announcing a release here on a 0 is how a bound row survived a loss unnoticed.
                    Ok(_) => opline!(
                        "seller node release job_id={job_id}: buyer picked another seller's claim, but no claim row was in 'claimed' (state={}) — nothing released",
                        self.claim_state_label(&job_id)
                    ),
                    Err(error) => opline!("seller node release failed job_id={job_id}: {error}"),
                }
            }
            AwardMatch::Ignore => opline!(
                "seller node award ignore job_id={job_id}: author not the offer buyer, or our claim not yet published"
            ),
        }
    }

    /// Surface a REJECT only after joining it to the recorded AWARD author. Phase 0 deliberately
    /// performs no terminal-state, execution, or payment action here.
    async fn on_reject(&self, event: &nostr_sdk::Event) {
        let Some(job_id) = event.tags.iter().find_map(|tag| {
            let fields = tag.as_slice();
            (fields.first().map(String::as_str) == Some("e")
                && fields.get(3).map(String::as_str) == Some("root"))
            .then(|| fields.get(1).cloned())
            .flatten()
        }) else { return; };
        let awarding_buyer = match self.node.store().job_award_buyer(&job_id) {
            Ok(buyer) => buyer,
            Err(error) => {
                opline!("seller node reject ignore job_id={job_id}: award read failed ({error})");
                return;
            }
        };
        if !reject_author_gate(&event.pubkey.to_hex(), awarding_buyer.as_deref()) {
            return;
        }
        opline!("seller node reject observed job_id={job_id}: buyer rejected delivered result");
    }

    /// Start a self-probe for every dropped harness whose window has passed, each OFF the event loop.
    ///
    /// Probes are spawned, never awaited here: a probe runs a whole agent turn, and awaiting one on
    /// the loop would deafen the node to offers, awards and payments for its duration — the failure
    /// mode #223 exists to prevent. [`LiveRoster::claim_due_probes`] marks each harness in-flight as
    /// it hands it over, so this tick firing every few seconds cannot stack probes on one harness.
    fn start_due_harness_probes(&self) {
        for harness in self.agents.claim_due_probes(Instant::now()) {
            let Some(argv) = self.agents.argv(harness) else {
                // No argv means no harness at that index, which cannot happen for a claimed probe;
                // release it rather than leaving the mark set forever.
                self.agents.fault(harness, Fault::Unproven, Instant::now());
                continue;
            };
            let roster = Arc::clone(&self.agents);
            // A `[sandbox]` that does not resolve grounds the harness rather than restoring it under
            // a pass-through: a probe that ran unsandboxed would prove nothing about the executor an
            // awarded job actually gets.
            let sandbox = match SandboxPolicy::from_config(self.node.home().config.sandbox.as_ref()) {
                Ok(sandbox) => sandbox,
                Err(error) => {
                    if let Some(failure) = harness_fault_for(&error) {
                        self.agents.execution_failure(harness, failure, Instant::now());
                    }
                    continue;
                }
            };
            let identity = DeliveryAgentIdentity::for_seller(&self.seller_pubkey.to_hex());
            // Minted per probe, so neither a stale workdir nor a replayed transcript can satisfy one:
            // the artifact has to be produced by THIS turn. The workdir is named after the NON-SECRET
            // label — never the sentinel, which the harness must not be able to read off its own cwd.
            // Attempt 0: the restore path runs a single turn (it has not adopted the retry).
            let probe = mint_probe_identity(harness, 0, now_unix() as u64);
            let workdir = job_workdir(self.node.home(), &probe.dir_label);
            let sentinel = probe.sentinel;
            tokio::task::spawn_local(async move {
                let label = roster
                    .label(harness)
                    .unwrap_or_else(|| "<unlabelled>".to_owned());
                // Supervised: an outer wall-clock ceiling bounds a live-but-endless-stream hang the
                // idle timeout cannot, and a Drop guard releases the in-flight mark even if the probe
                // panics — both close the "stuck probing, never re-probed" paths of #301. The probe
                // future is not polled until `supervise_harness_probe` awaits it, inside the guard.
                let outcome = supervise_harness_probe(
                    Arc::clone(&roster),
                    harness,
                    run_harness_probe(&argv, &sandbox, &identity, &workdir, &sentinel),
                )
                .await;
                match outcome {
                    ProbeOutcome::Restored => opline!(
                        "seller node harness RESTORED {label}: self-probe delivered its sentinel — \
                         now advertising {:?} of {} resolved",
                        roster.advertised(),
                        roster.entry_count()
                    ),
                    ProbeOutcome::Faulted { reason, state } => opline!(
                        "seller node harness probe FAILED {label}: {reason} — {}",
                        state.reason()
                    ),
                    ProbeOutcome::WallTimeout { state } => opline!(
                        "seller node harness probe TIMED OUT {label}: exceeded {}s wall clock (the ACP \
                         idle timeout cannot bound a drip-feeding stream) — {}",
                        HARNESS_PROBE_WALL_TIMEOUT.as_secs(),
                        state.reason()
                    ),
                }
                let _ = std::fs::remove_dir_all(&workdir);
            });
        }
    }

    /// Pre-narrow the live roster to the harnesses that PROVED they can deliver: every FAILED
    /// pre-advertise probe faults its index with the fault it produced, exactly as the restore-timer
    /// verdict does — so the kind-30340 heartbeat, which reads the roster, advertises only provers
    /// from the very first tick (#357). Provers are left untouched (boot already starts them serving).
    fn narrow_roster_to(&self, verdicts: &[HarnessProbeVerdict]) {
        for verdict in verdicts {
            match &verdict.result {
                // The proving turn is the one moment the node has run this harness and read its
                // usage back, so it is the only honest source for the model. Recorded BEFORE the
                // runner serves, which is what makes the first heartbeat and the first claim carry
                // the same answer every later one does — an empty roster advertises no
                // `harness_model` at all, and a buyer filtering on it never matches this seat.
                //
                // `None` is written through deliberately rather than skipped: a harness that stopped
                // reporting usage must not keep advertising the model it reported last boot.
                Ok(model) => self.agents.record_model(verdict.index, model.clone()),
                Err((reason, fault)) => {
                    let state = self.agents.fault(verdict.index, fault.clone(), Instant::now());
                    let label = self
                        .agents
                        .label(verdict.index)
                        .unwrap_or_else(|| "<unlabelled>".to_owned());
                    opline!(
                        "seller node pre-advertise DROPPED {label}: {reason} — {}",
                        state.reason()
                    );
                }
            }
        }
    }

    /// Take a harness out of service after a failure attributed to IT, and say so in one line naming
    /// both the drop and what would fix it.
    ///
    /// `None` is the deliberate no-op: a failure that does not implicate the harness must not narrow
    /// the roster, or the node inflicts its own outage. Only the sites that can attribute a failure
    /// call this at all — see [`harness_fault_for`].
    fn drop_harness(&self, harness: usize, fault: Option<ExecutionFailure>) {
        let Some(fault) = fault else {
            return;
        };
        let Some(state) = self
            .agents
            .execution_failure(harness, fault, Instant::now())
        else {
            return;
        };
        let label = self.agents.label(harness).unwrap_or_else(|| "<unlabelled>".to_owned());
        // The denominator belongs in the line: "1 harness dropped" means nothing without how many
        // this node had, and a roster that has reached 0 is a node that has gone quiet on the market.
        opline!(
            "seller node harness DROPPED {label}: {} — now advertising {:?} of {} resolved",
            state.reason(),
            self.agents.advertised(),
            self.agents.entry_count()
        );
    }

    /// #563 — the bounded relay-derive belt: does the relay hold POSITIVE evidence that this job's
    /// result already exists, or that the buyer has SETTLED it (with us or another seat)? Returns
    /// `true` only on the positive presence of a settlement event actually returned by the relay:
    ///   - our own already-published RESULT (kind-3403 authored by THIS seller), or
    ///   - a buyer RECEIPT (kind-3400 authored by the offer's buyer) — the co-signed settlement, which
    ///     is terminal whether it settled with us or another seat (mirrors the #541 terminal-offer gate).
    ///
    /// The AWARD (3405) and ACCEPT (3406) kinds are deliberately NOT positive evidence here: in this
    /// residual we HOLD an awarded row, so an award/accept rooting this offer is almost always OUR OWN
    /// selection (or the #143 re-bind) — matching it would strand the very award we are resuming (THE
    /// FOIL). Only our own result or a buyer receipt is unambiguous "already produced / already settled".
    ///
    /// ABSENCE is NEVER a skip: a timeout, a fetch error, an unparseable id, or a relay-deaf empty
    /// return (#560) all yield `false` ⇒ RunAgent. Relay deafness MANUFACTURES absence, so absence must
    /// never drive a skip — over-skipping a live award STRANDS it, the one outcome worse than a bounded
    /// replay (the receipt gate still holds the money line against double-pay; the lapse check bounds
    /// wasted compute). This is the money-adjacent inversion of fail-closed: RunAgent is the safe branch.
    ///
    /// The read uses `fetch_events` (like the #560 offer/wrap backfills): a TRANSIENT REQ under a
    /// pool-GENERATED sub id, returning events off the call itself and BYPASSING nostr-relay-pool's
    /// per-connection seen-cache (a plain re-subscribe would be swallowed for already-seen ids). The
    /// job is matched by an EXACT `#e` tag == the offer event id (never a substring — the #562 hazard
    /// of a numeric token matching inside a 64-hex id), plus the `#t=maxplayer` namespace guard.
    ///
    /// arm-after-the-event: the durable marker is written ONLY once the settlement event is in hand,
    /// never on the way INTO the query, so a crash mid-derive leaves the row re-checkable next restart.
    /// A marker-persist failure does NOT change THIS run's decision (still skip) — it just re-derives
    /// next restart, which is still correct.
    async fn settled_elsewhere_on_relay(
        &self,
        job_id: &str,
        buyer_pubkey: Option<&str>,
        now: i64,
    ) -> bool {
        let offer_id = match EventId::from_hex(job_id) {
            Ok(id) => id,
            Err(error) => {
                opline!(
                    "seller node execute job_id={job_id}: settled-elsewhere derive skipped (offer id not hex: {error}); running agent"
                );
                return false;
            }
        };
        // kind-3403 (our result) OR kind-3400 (buyer receipt), rooted at THIS offer, maxplayer namespace.
        let filter = Filter::new()
            .kinds([Kind::Custom(JOB_RESULT_KIND), Kind::Custom(JOB_RECEIPT_KIND)])
            .event(offer_id)
            .hashtag(crate::gateway::MAXPLAYER_TAG);
        let events = match tokio::time::timeout(
            SETTLED_ELSEWHERE_FETCH_TIMEOUT,
            self.client
                .fetch_events(filter, SETTLED_ELSEWHERE_FETCH_TIMEOUT / 2),
        )
        .await
        {
            Ok(Ok(events)) => events,
            Ok(Err(error)) => {
                opline!(
                    "seller node execute job_id={job_id}: settled-elsewhere derive fetch failed ({error}); running agent (absence never strands a live award)"
                );
                return false;
            }
            Err(_) => {
                opline!(
                    "seller node execute job_id={job_id}: settled-elsewhere derive timed out after {}s; running agent (absence never strands a live award)",
                    SETTLED_ELSEWHERE_FETCH_TIMEOUT.as_secs()
                );
                return false;
            }
        };
        // POSITIVE presence only. Our result is authored by THIS seller; a buyer receipt is authored by
        // the offer's buyer (the #541 buyer-binding — a forged receipt with a foreign author never
        // counts). Absence of BOTH ⇒ false ⇒ RunAgent.
        let settled = events.iter().any(|event| {
            let kind = event.kind.as_u16();
            (kind == JOB_RESULT_KIND && event.pubkey == self.seller_pubkey)
                || (kind == JOB_RECEIPT_KIND
                    && buyer_pubkey.is_some_and(|buyer| event.pubkey.to_hex() == buyer))
        });
        if settled {
            // arm-after-the-event: the settlement event is in hand — persist the durable marker so a
            // later restart short-circuits without re-querying. A persist failure does not change THIS
            // run's skip (it re-derives next restart), so it is logged, not propagated.
            if let Err(error) = self.node.store().mark_settled_elsewhere(job_id, now) {
                opline!(
                    "seller node execute job_id={job_id}: settled-elsewhere marker persist failed ({error}); skipping this run anyway (re-derives next restart)"
                );
            }
            opline!(
                "seller node execute skip job_id={job_id}: settled elsewhere (relay-derived: our result or a buyer receipt present) — NOT re-running the agent (#563)"
            );
        }
        settled
    }

    /// Execute an awarded job end to end: run the agent in a fresh empty-base workdir, snapshot its
    /// output into ONE delivery commit dated at the STORED award time (so a re-created commit after a
    /// restart keeps the same oid — invariant 2), push it under the seller's NIP-98 auth, then bind
    /// the trade + the delivered commit + the STORED claim-time creq's hash (audit N-4 / invariant 8)
    /// into a co-signature the seller signs through its actor, and journal + enqueue the result event
    /// in one transaction. Every failure path fails the job with a named reason and publishes nothing
    /// partial; the delivery journal is idempotent, so a resumed job never double-publishes.
    ///
    /// `resume` marks the BOOT/restart re-drive path (vs a fresh award off `on_award`). It gates the
    /// #563 relay-derive belt: only a resumed row can be stale enough to have been settled elsewhere or
    /// delivered by a pre-#552 binary, so a fresh award NEVER queries the relay (nothing is settled the
    /// instant we are awarded, and the hot award path must not wait on a relay round-trip).
    async fn execute_job(&self, job_id: &str, _slot: Option<OwnedSemaphorePermit>, resume: bool) {
        // `_slot` is the reserved execution permit, moved in and held for the whole call. It is
        // released the instant this function returns — on delivery, on any `fail_job*` path, on an
        // early idempotency return, or on a panic (unwind drops it). This RAII pairing is the single
        // release site for an executing slot; there is no explicit release to forget. Every caller
        // goes through `spawn_bounded_execution`, which hands over the parked reservation or WAITS
        // to acquire a fresh permit when nothing is parked (restart resume, lapsed park (#728), or
        // a redundant re-award (#279) — enumerated on `SlotGate::take_for_execution`). `None` here
        // therefore requires a closed semaphore, which this gate never produces
        // (`SlotGate::acquire_unreserved`); the impossible case runs anyway rather than dropping
        // awarded work, and the caller has already logged it.
        //
        // Idempotency guard: only a job still `awarded`/`executing` runs. A REDUNDANT award (a second
        // award event with a different award_id for a job already delivered/paid — seen live in the
        // smoke) or any re-drive must NOT re-run the agent: a duplicate execute burns operator compute
        // for nothing and its push is rejected non-fast-forward. It must also never clobber a terminal
        // state (delivered/paid/failed). Delivered/paid/failed ⇒ early-return, no second execute.
        // #552: decide from the job's DURABLE facts whether to re-drive the agent, finalize an
        // interrupted delivery, or skip already-delivered/terminal work. A marker READ ERROR degrades
        // to the legacy behavior (re-drive) rather than risk STALLING a genuine mid-flight award — a
        // wasted re-run is recoverable (the enqueue dedups), a silent skip of a real award is not.
        let state = match self.node.store().job_state(job_id) {
            Ok(Some(state)) => state,
            Ok(None) => {
                opline!("seller node execute skip job_id={job_id}: no job row (idempotent)");
                return;
            }
            Err(error) => {
                opline!("seller node execute job_id={job_id}: job_state read failed ({error}); not executing");
                return;
            }
        };
        let has_delivery = match self.node.store().has_delivery(job_id) {
            Ok(v) => v,
            Err(error) => {
                opline!("seller node execute job_id={job_id}: has_delivery read failed ({error}); assuming not delivered");
                false
            }
        };
        let has_receipt = match self.node.store().has_receipt(job_id) {
            Ok(v) => v,
            Err(error) => {
                opline!("seller node execute job_id={job_id}: has_receipt read failed ({error}); assuming not receipted");
                false
            }
        };
        let pushed = match self.node.store().pushed_commit(job_id) {
            Ok(v) => v,
            Err(error) => {
                opline!("seller node execute job_id={job_id}: pushed_commit read failed ({error}); assuming not pushed");
                None
            }
        };
        // Offer facts (task + amount + buyer + absolute deadline) were journaled at claim time. Read
        // them BEFORE the resume decision: the absolute deadline is #552's DURABLE terminal signal — a
        // restart must not re-drive (or finalize) a job whose offer deadline has already passed (the
        // buyer will not pay past it). A read error or a missing row degrades the deadline to "unknown
        // ⇒ live" so a genuine award is never skipped on an absent fact; the RunAgent path below still
        // fails the job if the offer is truly unavailable.
        let offer = match self.node.store().offer_row(job_id) {
            Ok(offer) => offer,
            Err(error) => {
                opline!("seller node execute job_id={job_id}: offer read failed ({error}); treating deadline as live");
                None
            }
        };
        let deadline_unix = offer.as_ref().map(|offer| offer.deadline_unix);
        // #563 — the relay-derive belt. `resume_action` returns RunAgent for a slot-occupying row with
        // no delivery/receipt, no pushed commit, and a live deadline — right for a genuine mid-flight
        // award (THE FOIL), but WRONG for a row actually settled ELSEWHERE (a buyer settled with
        // another seat) or delivered by a pre-#552 binary (before the pushed_commit marker existed).
        // Only on a RESUME (`resume`), and only for that exact residual, refine the decision from the
        // relay. A durable marker from a PRIOR derive short-circuits without re-querying; a fresh award
        // (`resume == false`) never queries — nothing is settled the instant we are awarded, and the
        // hot award path must not wait on a relay round-trip. `now` is captured ONCE so this liveness
        // gate and `resume_action`'s lapse check read the same clock.
        let now = now_unix();
        let settled_marked = match self.node.store().has_settled_elsewhere(job_id) {
            Ok(v) => v,
            Err(error) => {
                opline!("seller node execute job_id={job_id}: has_settled_elsewhere read failed ({error}); assuming not settled");
                false
            }
        };
        let deadline_live = deadline_unix.is_none_or(|deadline| deadline > now);
        let is_live_residual = state.occupies_execution_slot()
            && !has_delivery
            && !has_receipt
            && pushed.is_none()
            && deadline_live;
        let settled_elsewhere = if settled_marked {
            // A prior resume already relay-derived this settled — trust the durable marker, never re-query.
            true
        } else if resume && is_live_residual {
            self.settled_elsewhere_on_relay(job_id, offer.as_ref().map(|offer| offer.buyer_pubkey.as_str()), now)
                .await
        } else {
            // A fresh award, or any row that already skips/finalizes/fails on local markers: no derive.
            false
        };
        match resume_action(state, has_delivery, has_receipt, settled_elsewhere, pushed, deadline_unix, now) {
            ResumeAction::RunAgent => {
                // #628: an `awarded` row records that a job was bound, never WHICH claim the buyer
                // chose, so a resume cannot separate a job this seat won from one it lost in an
                // open-pool race and bound anyway (#626). Both re-run here, and no local fact can
                // tell them apart — declining unmarked rows would drop real awards. Said out loud
                // at normal level so an operator watching a seat start work at boot can see which
                // job it is, rather than inferring it from a busy seat that produces no result.
                if resume && matches!(state, super::store::JobState::Awarded) {
                    opline!(
                        "seller node resume job_id={job_id}: re-driving an awarded row; if this seat lost an open-pool race for this offer, the run produces a result nobody awarded (#628)"
                    );
                }
            }
            ResumeAction::FinalizeFromPushed(commit) => {
                opline!(
                    "seller node execute job_id={job_id}: delivery already pushed (commit={commit}) — finalizing from the stored commit, NOT re-running the agent (#552)"
                );
                self.finalize_pushed_delivery(job_id, &commit).await;
                return;
            }
            ResumeAction::SkipTerminal => {
                opline!(
                    "seller node execute skip job_id={job_id}: job already {state:?}/delivered (idempotent — not re-run)"
                );
                return;
            }
            ResumeAction::SkipLapsed => {
                opline!(
                    "seller node execute skip job_id={job_id}: offer deadline already passed (lapsed) — failing the stale award, NOT re-running or finalizing (#552)"
                );
                self.fail_job(job_id).await;
                return;
            }
        }

        // RunAgent: a genuinely mid-flight award with a live deadline — it now needs its seller config
        // and full offer facts to execute.
        let Some(seller) = self.node.home().config.seller.clone() else {
            opline!("seller node execute skip job_id={job_id}: no [seller] config");
            self.fail_job(job_id).await;
            return;
        };
        // `offer` was read tolerantly above (for the deadline); its true absence fails the job here.
        let Some(offer) = offer else {
            opline!("seller node execute fail job_id={job_id}: offer facts missing");
            self.fail_job(job_id).await;
            return;
        };
        // The claim-time creq is the single source of truth for the payment terms (audit N-4): the
        // delivery cosignature signs ITS hash, never a rebuild from live config.
        let stored_creq = match self.node.store().job_creq(job_id) {
            Ok(Some(creq)) => creq,
            _ => {
                opline!("seller node execute fail job_id={job_id}: stored creq missing");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None).await;
                return;
            }
        };
        // The delivery commit's author date is the STORED award time (stable across restarts), so a
        // re-created delivery commit is byte-identical and the re-push is a no-op (invariant 2).
        let author_date = match self.node.store().job_award_time(job_id) {
            Ok(Some(award_time)) => award_time,
            _ => now_unix(),
        };

        // Which harness runs this job. Read from the STORED offer (not live config), so a job
        // resumed after a restart still dispatches to the harness its buyer asked for. A request
        // this node cannot serve fails the job rather than substituting another harness — the
        // claim gate should already have refused it, and quietly running the wrong agent is the
        // one outcome the registry exists to prevent.
        //
        // The reason_code is `capability_missing`, not `execution_failed` (#821): nothing ran here.
        // `run_agent_job` is below this arm and is never reached, so there is no execution to have
        // failed — the seat could not START. `execution_failed` reads as *tried and broke*, which
        // attributes a fault to a run that never happened and points the buyer at a retry, when the
        // only move that can succeed is a seat that serves this harness.
        //
        // ⛔ This is a LABEL, not a guard. It changes nothing about whether the job is paid: under
        // award-is-payment the sats were committed at award, upstream of this arm. Never read this
        // code as protecting money (see `ReasonCode::CapabilityMissing`).
        let requested_agent = offer.requested_agent.clone();
        let Some(selected) = self.agents.dispatch(requested_agent.as_deref()) else {
            opline!(
                "seller node execute fail job_id={job_id}: requested agent {:?} is not available on \
                 this node (never substituted)",
                requested_agent.as_deref().unwrap_or("<any>")
            );
            self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, UNDISPATCHABLE_REASON_CODE, CAPABILITY_MISSING_FEEDBACK, None).await;
            return;
        };
        let agent_command = selected.agent.argv.clone();
        let agent_label = selected.agent.name.clone();
        // The registry index travels with the run: reporting a fault happens long after dispatch, and
        // a name would not do — the unlabelled `--agent-argv` hatch has none, and it is exactly as
        // capable of being a black hole as a named harness.
        let harness = selected.index;
        // Journal WHICH harness ran it before the run starts, so the row exists even if the job
        // then fails — the journal answers "what ran this", not only "what finished it".
        if let Some(label) = agent_label.as_deref()
            && let Err(error) = self.node.store().assign_agent(job_id, label)
        {
            opline!(
                "seller node execute job_id={job_id}: agent journal write failed (continuing): {error}"
            );
        }

        // Move awarded -> executing (idempotent). A failed mark is logged, never fatal.
        if let Err(error) = self.node.store().mark_executing(job_id, now_unix()) {
            opline!("seller node execute job_id={job_id}: mark_executing failed (continuing): {error}");
        }

        let seller_pubkey = self.seller_pubkey.to_hex();
        let identity = DeliveryAgentIdentity::for_seller(&seller_pubkey);
        let workdir = job_workdir(self.node.home(), job_id);
        // #591: provision the delivery workdir from the job's STORED contribution pin. The pin was
        // written at claim (pin ≤ offer ≤ claim), so it is present on BOTH the fresh-award and the
        // restart/resume path — the same durable-facts re-read the rest of execute_job relies on. A
        // recorded pin ⇒ clone the pinned base at base_oid (the fork tip the agent extends); no pin ⇒
        // the empty-workdir default (a from-scratch job, unchanged). A pin read error is fatal here
        // rather than a silent degrade to an empty workdir: no fund risk either way (buyer verify is
        // fail-closed pre-pay), but a loud fail is recoverable whereas an empty-workdir mis-delivery
        // hides the fault. Routing lives in `provision_delivery_workdir` so the real read→plan→init
        // path is unit-testable.
        let base_oid = match provision_delivery_workdir(
            self.node.store(),
            self.node.home(),
            job_id,
            workdir.clone(),
            identity.clone(),
        )
        .await
        {
            Ok(base_oid) => base_oid,
            Err(DeliveryWorkdirError::Refused(refusal)) => {
                let (reason_code, reason_detail) = env_provision::refusal_feedback(&refusal);
                opline!(
                    "seller node execute fail job_id={job_id}: environment provisioning refused ({refusal:?})"
                );
                self.fail_job_with_feedback(
                    job_id,
                    &offer.buyer_pubkey,
                    reason_code,
                    EXEC_FAILURE_FEEDBACK,
                    Some(reason_detail),
                )
                .await;
                return;
            }
            Err(error) => {
                opline!("seller node execute fail job_id={job_id}: workdir init failed ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None).await;
                return;
            }
        };

        // Run the agent under the job's remaining deadline, retrying a transient error while the
        // deadline has room. The agent edits files in `workdir`; the node owns commit + push. The
        // configured `[sandbox]` policy launches the command (pass-through when absent).
        let deadline = offer.deadline_unix.max(0) as u64;
        // #828: operator-authored context (brand guidelines, house style) loads with the job. Inert
        // for a seller that has never written a MEMORY.md, and it never blocks a job — see
        // `job_memory_section`.
        let memory_section = job_memory_section(
            &self.node.home().root,
            &self.node.home().config.seller_memory,
        );
        let prompt = job_prompt(&offer, &seller.git_remote, deadline, memory_section.as_deref());
        // Resolve the sandbox executor before the run; a misconfigured `[sandbox]` fails the job
        // rather than silently running the agent unsandboxed.
        let sandbox = match SandboxPolicy::from_config(self.node.home().config.sandbox.as_ref()) {
            Ok(sandbox) => sandbox,
            Err(error) => {
                opline!("seller node execute fail job_id={job_id}: sandbox config invalid ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None).await;
                return;
            }
        };
        let run_started = std::time::Instant::now();
        let run_result = run_agent_with_retry(
            deadline,
            MAX_AGENT_ATTEMPTS,
            || now_unix() as u64,
            |_attempt| {
                let job_timeout = unified_job_timeout(deadline, now_unix() as u64);
                run_agent_job(
                    &agent_command,
                    &sandbox,
                    &prompt,
                    &workdir,
                    &identity,
                    AgentRunTimeout::JobDeadline(job_timeout),
                )
            },
        )
        .await;
        let wall_time_ms = run_started.elapsed().as_millis() as u64;
        let report = match run_result {
            Ok(report) => report,
            Err(error) => {
                opline!("seller node execute fail job_id={job_id}: agent run failed ({error})");
                self.drop_harness(harness, harness_fault_for(&error));
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None).await;
                return;
            }
        };
        // The agent's own account of the run, on the REAL job path and not only the probe: it was
        // discarded here too, so a job that delivered nothing left the operator the same guess the
        // probe used to. Logged whenever the agent said anything — one line per job, and it is the
        // line that names a blocked host or an exhausted plan.
        if let Some(quoted) = quoted_agent_message(report.last_agent_message.as_deref()) {
            opline!("seller node execute job_id={job_id} agent last message: {quoted}");
        }
        let usage = report.usage;
        // Refresh the advertised model from THIS run (#784). The boot probe answers for the moment
        // the node started; an operator can re-point a harness at a different default while it runs,
        // and an advertisement that sits behind the last restart is a claim the seat no longer keeps.
        // A real run is the freshest evidence available, and it costs nothing to read here.
        //
        // A `None` observation CLEARS rather than preserves, which is the roster's documented
        // contract: a harness that stops reporting a model must stop advertising one, or the last
        // value it ever gave outlives the truth. That is the drift this field exists to bound.
        self.agents
            .record_model(harness, usage.as_ref().and_then(|u| u.model.clone()));

        // Snapshot the agent's final workdir tree into ONE delivery commit at the stored author date.
        // §19: the snapshot writes the execution sentinel into the delivered tree, seeded from this
        // job's job_hash (replay-resistant; the buyer holds the same value on its accept-bind). When
        // the node observed no genuine execution — the quota-dead case, an empty / base-identical tree
        // — the snapshot refuses `NoExecutionObserved` and writes no sentinel, which is mapped here to
        // the `no_sentinel` refusal so the buyer learns delivery was refused for want of a sentinel
        // (distinct from a crash). The gate, not an unconditional write, is the check.
        let branch = format!("maxplayer/{}", &job_id[..8.min(job_id.len())]);
        // Single source for the delivery ref. The branch-scoped push token (below) is minted for
        // THIS refname, and the push refspec `push_branch_with_header` builds is
        // `refs/heads/{branch}:refs/heads/{branch}` — both derive from `branch`, so the token scope
        // and the ref actually pushed cannot drift apart. The relay (PR #929) demands the scope be
        // fully qualified (`refs/heads/…`); a bare branch name is rejected.
        let push_ref = crate::git_transport::delivery_ref(&branch);
        let message = delivery_message(&offer.task);
        let job_hash = job_hash_for_offer(job_id, &offer.task, offer.amount_sats);
        if let Err(error) = seller_git::snapshot_delivery_at_off_runtime(
            workdir.clone(),
            identity.clone(),
            // #616: parent the delivery commit on the base the workdir was provisioned at. A
            // contribution (Some(base_oid)) then descends from base_oid by construction; the buyer's
            // descendant gate refuses a commit that doesn't. From-scratch (None) stays a root commit.
            base_oid,
            branch.clone(),
            message,
            author_date,
            job_hash,
        )
        .await
        {
            // Harness-attributable: the agent returned success having left nothing to deliver. This
            // is the site that fires on a quota-dead harness — its turn "completes", so the agent-run
            // arm above sees no error at all — which is why the trigger cannot live at one site.
            self.drop_harness(
                harness,
                Some(ExecutionFailure::Harness(Fault::Unproven)),
            );
            let (reason_code, feedback) = match error {
                seller_git::SellerGitError::NoExecutionObserved(_) => {
                    opline!(
                        "seller node execute fail job_id={job_id}: delivery refused no_sentinel — {error}"
                    );
                    (ReasonCode::NoSentinel, NO_SENTINEL_FEEDBACK)
                }
                _ => {
                    opline!(
                        "seller node execute fail job_id={job_id}: delivery snapshot failed ({error})"
                    );
                    (ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK)
                }
            };
            self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, reason_code, feedback, None)
                .await;
            return;
        }

        // Push under the seller's NIP-98 auth. The push authorization is signed THROUGH the signer
        // actor (which owns the seller key), so the push path is NOT a third custody site — the key
        // stays confined to the actor + the authenticated relay client, never re-read here. A
        // public/anonymous https remote takes no header (auth applies to relay-git remotes only).
        let push_header = if crate::delivery_transport::is_relay_git_locator(&seller.git_remote) {
            match self.node.signer().http_auth_header(seller.git_remote.clone(), Some(push_ref.clone()), None).await {
                Ok(Ok(header)) => Some(header),
                Ok(Err(error)) => {
                    opline!("seller node execute fail job_id={job_id}: push auth sign failed ({error})");
                    self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                    return;
                }
                Err(error) => {
                    opline!("seller node execute fail job_id={job_id}: signer actor gone ({error})");
                    self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                    return;
                }
            }
        } else {
            None
        };
        // #562: serialize the delivery push to this seat's ONE delivery remote, bounded so a hung
        // push frees the lock instead of starving every later delivery. Concurrent awarded jobs push
        // per-job branches to the same repo, and concurrent git-receive-pack to one repo is what the
        // relay 409s (surfaced as terminal delivery_failed before this). Serializing removes the race;
        // the push oid is stable (invariant 2), so ordering never duplicates a delivery.
        // Harden the workdir's `.git/config` (a whole-file replacement) BEFORE pushing, so an
        // `insteadOf` the agent planted cannot redirect the seller's token to a host it chose
        // (tests/hostile_local_git_config.rs). Safe here: the job container has already exited, so no
        // agent process is alive to re-plant the redirect between the rewrite and the push. Both run
        // in one blocking op inside `neutralize_then_push_off_runtime`.
        let commit = match serialized_bounded_push(
            &self.delivery_push_lock,
            DELIVERY_PUSH_TIMEOUT,
            || {
                seller_git::neutralize_then_push_off_runtime(
                    workdir.clone(),
                    seller.git_remote.clone(),
                    branch.clone(),
                    push_header,
                )
            },
        )
        .await
        {
            Ok(oid) => oid,
            Err(DeliveryPushErr::Push(error)) => {
                opline!("seller node execute fail job_id={job_id}: git push failed ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
            Err(DeliveryPushErr::TimedOut(secs)) => {
                // Timeout lands in the SAME delivery_failed handling (lead 37896 — no new state); the
                // lock is already released, so later deliveries are not starved behind this one.
                opline!("seller node execute fail job_id={job_id}: git push exceeded {secs}s (delivery-push lock released; treated as delivery_failed)");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        };

        // #552: arm the durable pushed-delivery marker IMMEDIATELY after the push (arm-state-after-
        // the-event) and BEFORE the sign+enqueue — so a crash in that window is a RESUMABLE delivery
        // (finalized from this commit) rather than a re-run that re-executes the agent and re-pushes.
        if let Err(error) = self.node.store().mark_pushed(job_id, &commit, now_unix()) {
            opline!("seller node execute job_id={job_id}: mark_pushed failed (continuing): {error}");
        }

        // Bind the trade + delivered commit + STORED creq hash into the co-signature preimage and
        // sign it through the signer actor (the seller key never leaves the actor).
        let delivery_kind = match seller_delivery_kind(&seller.git_remote, &branch, &commit) {
            Ok(kind) => kind,
            Err(error) => {
                opline!("seller node execute fail job_id={job_id}: delivery kind typing failed ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        };
        let preimage = delivery_receipt_preimage(
            job_id,
            &offer.task,
            offer.amount_sats,
            &offer.buyer_pubkey,
            &seller_pubkey,
            &commit,
            delivery_kind.as_str(),
            &stored_creq,
        );
        let seller_sig = match self.node.signer().sign_receipt_hash(preimage.digest_hex()).await {
            Ok(Ok(sig)) => sig,
            Ok(Err(error)) => {
                opline!("seller node execute fail job_id={job_id}: receipt sign refused ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
            Err(error) => {
                opline!("seller node execute fail job_id={job_id}: signer actor gone ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        };

        // Harness-generic PUBLIC seller-claimed usage block (opportunistic; absent fields stay
        // absent). `usage` carries what the ACP driver surfaced this run — `None` when it exposed none.
        let exec_metadata = seller_exec_metadata(
            &agent_command,
            agent_label.as_deref(),
            wall_time_ms,
            usage.as_ref(),
        );
        let mut draft = git_result_draft(
            job_id,
            &offer.buyer_pubkey,
            &seller.git_remote,
            &branch,
            &commit,
            offer.amount_sats,
            &preimage.job_hash,
            &seller_sig,
            format!("delivery commit {commit}"),
            &exec_metadata,
        );
        // #613: a served contribution (recorded pin) delivers a CONTRIBUTION result envelope — the
        // offer echo + the seller-signed authorship tuple — additive to the standard git result.
        // Without it the buyer refuses a correctly-delivered fork ("...requires a contribution
        // result..."). A from-scratch job (no pin) leaves the result unchanged; a build/sign failure
        // is fail-closed (delivery_failed), never a from-scratch shape the buyer would refuse.
        match contribution_result_envelope_tags(
            self.node.store(),
            self.node.signer(),
            job_id,
            &seller_pubkey,
            &seller.git_remote,
            &branch,
            &commit,
        )
        .await
        {
            Ok(Some(extra)) => draft.tags.extend(extra),
            Ok(None) => {}
            Err(reason) => {
                opline!("seller node execute fail job_id={job_id}: {reason}");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        }
        // Journal the delivery + enqueue the result in one transaction. Idempotent: a resumed job
        // that already delivered re-enqueues nothing (invariant 2 — no divergent double-publish).
        let now = now_unix();
        match self.node.store().deliver_and_enqueue(
            job_id,
            &commit,
            &draft,
            now,
            now + RESULT_PUBLISH_WINDOW_SECS,
            now,
        ) {
            Ok(true) => {
                // `agent=` is the RESOLVED harness id for this run, read from the SAME
                // exec-metadata block stamped on the outgoing result — so the log provably
                // matches the wire byte-for-byte (one vocabulary with the buyer's settled line;
                // no second derivation that could drift). The preset LABEL (what `assign_agent`
                // journals and `requested_agent=` prints) is the request-side vocabulary; the
                // two relate only through `harness_and_transport`, never by string equality
                // (#261).
                let harness_id = exec_metadata
                    .iter()
                    .find(|tag| tag.first() == Some("harness"))
                    .and_then(|tag| tag.value())
                    .unwrap_or("unknown");
                opline!(
                    "seller node delivered job_id={job_id} commit={commit} agent={harness_id} result enqueued"
                )
            }
            Ok(false) => opline!(
                "seller node execute job_id={job_id}: delivery already journaled (dedup no-op)"
            ),
            Err(error) => {
                opline!("seller node execute fail job_id={job_id}: deliver journal failed ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        }
        self.drain().await;
    }

    /// Complete an interrupted delivery from its journaled pushed commit (#552), WITHOUT re-running
    /// the agent or re-pushing. Reached only from the resume path via [`resume_action`] =
    /// `FinalizeFromPushed`: the delivery commit is already on the remote, but the sign+enqueue was
    /// interrupted (crash after push, before `deliver_and_enqueue`). Re-derives the SAME signed
    /// digest — the delivery-receipt preimage is deterministic in the STORED offer facts + creq +
    /// pushed commit, identical to what the interrupted run would have signed — signs it, and
    /// enqueues (idempotent: `deliver_and_enqueue` dedups). Mirrors the sign+enqueue tail of
    /// `execute_job`; keep the two in sync — both sign the SAME preimage digest, so the buyer accepts
    /// the receipt whichever pass produced it (the schnorr signature BYTES differ per signing via
    /// random aux_rand; BIP340 verification is over the digest, not the bytes). Exec-metadata is
    /// degraded (no agent ran this pass) and rides only as UNSIGNED result tags — it is not in the
    /// signed digest, so its absence cannot make the buyer reject the receipt.
    async fn finalize_pushed_delivery(&self, job_id: &str, commit: &str) {
        let Some(seller) = self.node.home().config.seller.clone() else {
            opline!("seller node finalize skip job_id={job_id}: no [seller] config");
            self.fail_job(job_id).await;
            return;
        };
        let offer = match self.node.store().offer_row(job_id) {
            Ok(Some(offer)) => offer,
            _ => {
                opline!("seller node finalize fail job_id={job_id}: offer facts missing");
                self.fail_job(job_id).await;
                return;
            }
        };
        let stored_creq = match self.node.store().job_creq(job_id) {
            Ok(Some(creq)) => creq,
            _ => {
                opline!("seller node finalize fail job_id={job_id}: stored creq missing");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        };
        let seller_pubkey = self.seller_pubkey.to_hex();
        let branch = format!("maxplayer/{}", &job_id[..8.min(job_id.len())]);
        let delivery_kind = match seller_delivery_kind(&seller.git_remote, &branch, commit) {
            Ok(kind) => kind,
            Err(error) => {
                opline!("seller node finalize fail job_id={job_id}: delivery kind typing failed ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        };
        let preimage = delivery_receipt_preimage(
            job_id,
            &offer.task,
            offer.amount_sats,
            &offer.buyer_pubkey,
            &seller_pubkey,
            commit,
            delivery_kind.as_str(),
            &stored_creq,
        );
        let seller_sig = match self.node.signer().sign_receipt_hash(preimage.digest_hex()).await {
            Ok(Ok(sig)) => sig,
            Ok(Err(error)) => {
                opline!("seller node finalize fail job_id={job_id}: receipt sign refused ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
            Err(error) => {
                opline!("seller node finalize fail job_id={job_id}: signer actor gone ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        };
        // No agent ran this pass, so usage/timing are absent (opportunistic — absent stays absent).
        let exec_metadata = seller_exec_metadata(&[], None, 0, None);
        let mut draft = git_result_draft(
            job_id,
            &offer.buyer_pubkey,
            &seller.git_remote,
            &branch,
            commit,
            offer.amount_sats,
            &preimage.job_hash,
            &seller_sig,
            format!("delivery commit {commit}"),
            &exec_metadata,
        );
        // #613: mirror the execute path — a resumed contribution delivery finalizes the SAME
        // contribution result envelope (echo + seller-signed tuple) it would have on the live path,
        // so the buyer accepts whichever pass produced it. From-scratch (no pin) is unchanged.
        match contribution_result_envelope_tags(
            self.node.store(),
            self.node.signer(),
            job_id,
            &seller_pubkey,
            &seller.git_remote,
            &branch,
            commit,
        )
        .await
        {
            Ok(Some(extra)) => draft.tags.extend(extra),
            Ok(None) => {}
            Err(reason) => {
                opline!("seller node finalize fail job_id={job_id}: {reason}");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        }
        let now = now_unix();
        match self.node.store().deliver_and_enqueue(
            job_id,
            commit,
            &draft,
            now,
            now + RESULT_PUBLISH_WINDOW_SECS,
            now,
        ) {
            Ok(true) => opline!(
                "seller node finalized interrupted delivery job_id={job_id} commit={commit} result enqueued (no agent re-run) (#552)"
            ),
            Ok(false) => opline!(
                "seller node finalize job_id={job_id}: delivery already journaled (dedup no-op)"
            ),
            Err(error) => {
                opline!("seller node finalize fail job_id={job_id}: deliver journal failed ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, ReasonCode::DeliveryFailed, DELIVERY_FAILURE_FEEDBACK, None).await;
                return;
            }
        }
        self.drain().await;
    }

    /// Settle one gift-wrapped payment: decode it (through the signer actor — the NIP-44 decrypt
    /// needs the seller key, which never leaves the actor), authenticate the buyer by the seal,
    /// enforce the money-safety guards (seal sender == bound buyer, realized mint ∈ the STORED
    /// claim-time creq per Fix Q, `allow_real_mints` fence), then — in the invariant-3 order — write
    /// the intent breadcrumb BEFORE swapping at the mint, classify the swap FAIL-CLOSED (never infer
    /// collection from the breadcrumb), and only then record the receipt (deduped by the wrap id, so
    /// a replayed wrap credits the job at most once). Every refusal is logged with a named reason.
    async fn on_gift_wrap(&self, event: &nostr_sdk::Event) {
        let event_id = event.id.to_hex();
        // Log EVERY wrap seen — silence must mean "no wraps", never "lost money".
        opline!("seller node wrap seen event={event_id}");

        let received = match self.node.signer().unwrap_payment_wrap(event.clone()).await {
            Ok(Ok(Some(received))) => received,
            Ok(Ok(None)) => {
                opline!("seller node wrap event={event_id}: not a decodable own-payment wrap (skipped)");
                return;
            }
            Ok(Err(error)) => {
                opline!("seller node wrap event={event_id}: decode failed ({error})");
                return;
            }
            Err(error) => {
                opline!("seller node wrap event={event_id}: signer actor gone ({error})");
                return;
            }
        };
        let job_id = received.payload.job_id().to_owned();
        if job_id.is_empty() {
            opline!("seller node wrap event={event_id}: payment carries no job id (skipped)");
            return;
        }

        // Already-paid job: a re-see of consumed money — skip (do not re-redeem). Fail closed on a
        // read error (never read an unreadable journal as "not paid ⇒ safe to redeem again").
        match self.node.store().has_receipt(&job_id) {
            Ok(true) => {
                opline!("seller node wrap event={event_id}: job {job_id} already receipted, skipping");
                return;
            }
            Ok(false) => {}
            Err(error) => {
                opline!("seller node wrap event={event_id}: has_receipt read failed for {job_id} (fail-closed, skipping): {error}");
                return;
            }
        }

        // Bind to a job we recorded (offer facts). No offer ⇒ early pay for a still-unknown job or
        // not ours — leave it (buffered by re-delivery), never misattribute.
        let offer = match self.node.store().offer_row(&job_id) {
            Ok(Some(offer)) => offer,
            Ok(None) => {
                opline!("seller node wrap event={event_id}: no offer recorded for job {job_id} (skipped)");
                return;
            }
            Err(error) => {
                opline!("seller node wrap event={event_id}: offer read failed for {job_id} ({error})");
                return;
            }
        };

        // Seal-sender guard: the authenticated buyer MUST be the bound offer buyer.
        let buyer = received.buyer_pubkey.to_hex();
        if !seal_sender_is_bound_buyer(&buyer, &offer.buyer_pubkey) {
            opline!(
                "seller node wrap event={event_id}: payment sender {buyer} is not the bound offer buyer {} for job {job_id} — refused",
                offer.buyer_pubkey
            );
            return;
        }

        // Fix Q — settle against the mints the seller ORIGINALLY advertised (the STORED claim-time
        // creq), never live config: a config change across the trade can neither strand this payment
        // nor let a newly-added mint settle it.
        let stored_creq = match self.node.store().job_creq(&job_id) {
            Ok(Some(creq)) => creq,
            _ => {
                opline!("seller node wrap event={event_id}: no stored creq for job {job_id} (skipped)");
                return;
            }
        };
        let request = match crate::gateway::creq::parse_creq(&stored_creq) {
            Ok(request) => request,
            Err(error) => {
                opline!("seller node wrap event={event_id}: stored creq unparseable for job {job_id} ({error})");
                return;
            }
        };
        let payload_mint = received.payload.payload.mint.clone();
        let mint_str = payload_mint.to_string();
        // Redeem guard: the realized mint MUST be one the STORED creq advertised.
        if !request.mints.contains(&payload_mint) {
            opline!(
                "seller node wrap event={event_id}: realized mint {mint_str} outside the stored creq's accepted mints for job {job_id} — refused"
            );
            return;
        }
        // Real-mint fence: a real mint can never settle unless the operator opted in.
        if !crate::home::mint_allowed(&mint_str, self.node.home().config.allow_real_mints) {
            opline!(
                "seller node wrap event={event_id}: mint {mint_str} not allowed (allow_real_mints={}) for job {job_id} — refused",
                self.node.home().config.allow_real_mints
            );
            return;
        }

        // Payment terms over the stored-creq accepted set (amount == offer.amount, unit == sat). The
        // ParsedOffer is reconstructed from the stored offer; a targeted offer we hold was targeted to
        // US, so its seller target is our own pubkey (open-pool = untargeted).
        let seller_pubkey = self.seller_pubkey.to_hex();
        let parsed_offer = ParsedOffer {
            task: offer.task.clone(),
            output: String::new(),
            amount: offer.amount_sats,
            unit: offer.unit.clone(),
            deadline_unix: offer.deadline_unix.max(0) as u64,
            seller_pubkey: offer.targeted.then(|| seller_pubkey.clone()),
            // The pay path is harness-blind: which harness ran the job never changes the terms. It is
            // capability-blind for the same reason — the request decided WHO could be awarded, and
            // that decision is upstream of and independent from what the agreed terms are.
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: Vec::new(),
        };
        let accepted_mints: std::collections::HashSet<cashu::MintUrl> =
            request.mints.iter().cloned().collect();
        let policy = crate::payment_wallet::PaymentPolicy::new(accepted_mints.iter().cloned());
        let terms = match policy.terms_for_offer(payload_mint.clone(), &parsed_offer, &seller_pubkey) {
            Ok(terms) => terms,
            Err(error) => {
                opline!("seller node wrap event={event_id}: payment terms refused for job {job_id} ({error})");
                return;
            }
        };

        // Derive the cashu P2PK key through the actor and open a wallet at the REALIZED mint (the
        // buyer paid seller-locked ecash there; the wallet must be bound to that same mint).
        let cashu_key = match self.node.signer().cashu_p2pk_secret().await {
            Ok(Ok(key)) => key,
            Ok(Err(error)) => {
                opline!("seller node wrap event={event_id}: cashu key derive failed for job {job_id} ({error})");
                return;
            }
            Err(error) => {
                opline!("seller node wrap event={event_id}: signer actor gone ({error})");
                return;
            }
        };
        let wallet = match crate::buyer_fund::open_wallet_at_mint_async(self.node.home(), &mint_str).await {
            Ok(wallet) => wallet,
            Err(error) => {
                opline!("seller node wrap event={event_id}: open wallet at {mint_str} failed for job {job_id} ({error})");
                return;
            }
        };
        let adapter = crate::payment_wallet::CdkSellerReceive::new(&wallet, cashu_key);
        let token = received.payload.to_token();
        let expected = offer.amount_sats;

        // Intent-to-receive breadcrumb BEFORE the swap (invariant 3). token_hash is SHA-256 of the
        // token string — no proof/secret material is stored.
        let token_hash = {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(token.to_string().as_bytes());
            hex::encode(hasher.finalize())
        };
        if let Err(error) =
            self.node
                .store()
                .append_pending_receive(&job_id, &token_hash, &buyer, &mint_str, expected, now_unix())
        {
            opline!("seller node wrap event={event_id}: breadcrumb write failed for job {job_id} ({error}) — refusing to receive");
            return;
        }

        // Swap at the mint, then classify FAIL-CLOSED (never infer prior collection from the
        // breadcrumb — the only proof is a COMPLETED receipt read fail-closed).
        let receive_result = adapter
            .receive(&token, &terms, &accepted_mints, &payload_mint)
            .await
            .map(|amount| amount.to_u64())
            .map_err(|error| error.to_string());
        let amount_received = match classify_redeem_outcome(receive_result, || {
            self.node.store().has_receipt(&job_id).map_err(|error| error.to_string())
        }) {
            RedeemDecision::Finalize(amount) => amount,
            RedeemDecision::IdempotentNoOp => {
                opline!("seller node wrap event={event_id}: idempotent no-op (already spent AND a completed receipt exists) for job {job_id}");
                return;
            }
            RedeemDecision::Refuse(reason) => {
                opline!("seller node wrap event={event_id}: receive refused for job {job_id} ({reason}) — buffered for reconcile");
                return;
            }
        };
        opline!(
            "seller node collect ok: job_id={job_id} amount_received={amount_received} expected={expected} mint={mint_str}"
        );

        // Record the receipt AFTER the money landed (invariant 3 order) — deduped on the wrap id, so a
        // replayed wrap marks the job paid at most once.
        match self
            .node
            .store()
            .collect_receipt(&event_id, &job_id, amount_received, now_unix())
        {
            Ok(super::store::Collected::New) => {
                // `event_id` is the kind-1059 payment gift-wrap — the id this collection is
                // journaled and deduped under. It is NOT the co-signed kind-3400 receipt (the buyer
                // publishes that; the seller never sees its id on this path), so name it for what it
                // is rather than inviting an operator to grep the relay for a 3400 that will not
                // match.
                opline!(
                    "seller node paid job_id={job_id} amount={amount_received} payment_wrap={event_id}"
                )
            }
            Ok(super::store::Collected::Duplicate) => opline!(
                "seller node wrap event={event_id}: receipt already collected for job {job_id} (dedup no-op)"
            ),
            Err(error) => {
                opline!("seller node wrap event={event_id}: receipt write failed for job {job_id} ({error})")
            }
        }
    }

    /// Mark a job failed (best-effort; a fail-mark that itself errors is logged, never propagated —
    /// the loop keeps serving).
    async fn fail_job(&self, job_id: &str) {
        match self.node.store().fail_job(job_id, now_unix()) {
            // The callers announce WHY they are failing a job before they get here, so a successful
            // write needs no line of its own. A write that moved NOTHING does: this is the same
            // store call `SkipLapsed` uses to heal a stale awarded row, and a silent zero there
            // would leave an operator reading a heal that did not happen.
            Ok(0) => opline!(
                "seller node job_id={job_id}: no job row moved to failed (state={}) — nothing was healed",
                self.job_state_label(job_id)
            ),
            Ok(_) => {}
            Err(error) => {
                opline!("seller node job_id={job_id}: fail_job write error (continuing): {error}")
            }
        }
    }

    /// The job row's state, for a LOG that must name why a write moved nothing. Never fails the
    /// caller, for the same reason as [`Self::claim_state_label`].
    fn job_state_label(&self, job_id: &str) -> String {
        match self.node.store().job_state(job_id) {
            Ok(Some(state)) => state.as_str().to_owned(),
            Ok(None) => "absent".to_owned(),
            Err(error) => format!("unreadable ({error})"),
        }
    }

    /// Fail the job AND tell the buyer why (a feedback-kind carrying the §10 `reason_code` tag), so a
    /// failure is not silent on the wire (the buyer waits on a delivery that will never come
    /// otherwise). Used at the post-offer execute fail points where the offer buyer is known; the
    /// caller passes the reason_code that names WHICH failure this is (execution vs delivery vs
    /// no_sentinel), so the buyer can class it without parsing the human-readable reason.
    async fn fail_job_with_feedback(
        &self,
        job_id: &str,
        buyer_pubkey: &str,
        reason_code: ReasonCode,
        reason: &str,
        reason_detail: Option<&str>,
    ) {
        self.fail_job(job_id).await;
        self.publish_buyer_feedback(job_id, buyer_pubkey, reason_code, reason, reason_detail)
            .await;
    }

    /// One outbox drain pass over the shared authenticated client. Log-and-continue: a publish
    /// failure leaves the row pending for the next tick (never wedges the loop).
    async fn drain(&self) {
        let now = now_unix();
        match drain_once(self.node.store(), &self.publisher, now).await {
            Ok(report) if report.confirmed > 0 || report.failed > 0 => opline!(
                "seller node outbox drain: confirmed={} failed={} expired={}",
                report.confirmed, report.failed, report.expired
            ),
            Ok(_) => {}
            Err(error) => opline!("seller node outbox drain error (continuing): {error}"),
        }
    }
}

#[cfg(test)]
mod slot_gate_tests {
    //! The execution-slot admission algebra — reserve-at-claim, release on every terminal path,
    //! and the lapse sweep. These drive [`SlotGate`] directly (no relay, no agent), so they are the
    //! deterministic core of the multi-slot correctness claim: the acquire/release PAIRING is what
    //! keeps a busy node from silently shrinking its own capacity. Each `available()` assertion is a
    //! revert-red tripwire — neuter a release path and the matching assertion fails.
    use super::{spawn_bounded_execution, spawn_bounded_resumes, Reserve, SlotGate};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const LONG: Duration = Duration::from_secs(3600);

    #[test]
    fn one_slot_is_serial_admits_one_then_blocks() {
        // slots=1 reproduces today's behavior exactly: one claim admitted, the next refused as
        // SlotsBusy — so a single-slot node never parks a second concurrent claim.
        let gate = SlotGate::new(1, LONG);
        assert_eq!(gate.available(), 1);
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        assert_eq!(gate.available(), 0);
        assert_eq!(gate.try_reserve("b"), Reserve::Full, "second claim must be refused at slots=1");
    }

    #[test]
    fn zero_config_clamps_to_serial() {
        // A misconfigured `slots = 0` is clamped to serial rather than muting the node entirely.
        let gate = SlotGate::new(0, LONG);
        assert_eq!(gate.available(), 1);
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        assert_eq!(gate.try_reserve("b"), Reserve::Full);
    }

    #[test]
    fn two_slots_admit_two_then_block_the_third() {
        // slots=2: two concurrent claims parked; a third arriving while both are held is NOT
        // claimed. This is the gate half of the "3rd offer while both busy is not claimed" property
        // (the on-the-wire half is the live capstone, P5).
        let gate = SlotGate::new(2, LONG);
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        assert_eq!(gate.try_reserve("b"), Reserve::Reserved);
        assert_eq!(gate.available(), 0);
        assert_eq!(
            gate.try_reserve("c"),
            Reserve::Full,
            "third claim must be refused while both slots busy"
        );
    }

    #[test]
    fn release_returns_the_slot() {
        // The award-elsewhere / dedup-no-op path: releasing a reserved slot returns capacity.
        let gate = SlotGate::new(1, LONG);
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        gate.release("a");
        assert_eq!(gate.available(), 1);
        assert_eq!(
            gate.try_reserve("b"),
            Reserve::Reserved,
            "a released slot must admit the next claim"
        );
    }

    /// #251 — a restart that caught K jobs mid-flight must still honor `slots`.
    ///
    /// Four resumed jobs, two slots. Peak concurrency must be 2, and all four must still run: the
    /// excess queues, it is never dropped, because under award-is-payment abandoning a resumed job
    /// abandons work a buyer already committed sats to.
    ///
    /// ⚠ RED ON REVERT, and this is the pre-fix code exactly: make `spawn_bounded_execution` pass
    /// its `parked` (always `None` at boot) straight to the execution step instead of waiting on
    /// `acquire_unreserved`. All four then run at once and `peak` reaches 4 — `left: 4, right: 2`.
    ///
    /// The execution step is stubbed on purpose. The bound is a property of the FAN-OUT, not of what a
    /// job does, and stubbing is what makes peak concurrency observable at all — a real `execute_job`
    /// would need a relay, an agent and a store, none of which the bound depends on. What this test
    /// does NOT cover: that the production loop passes `execute_job` as its step. That is one call
    /// site, checked by the compiler and by reading, and it is the only gap.
    #[tokio::test]
    async fn a_restart_resumes_more_jobs_than_slots_without_exceeding_capacity() {
        use std::cell::Cell;
        use std::rc::Rc;

        let gate = Arc::new(SlotGate::new(2, LONG));
        let live = Rc::new(Cell::new(0usize));
        let peak = Rc::new(Cell::new(0usize));
        let done = Rc::new(Cell::new(0usize));

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (live_t, peak_t, done_t) =
                    (Rc::clone(&live), Rc::clone(&peak), Rc::clone(&done));
                spawn_bounded_resumes(
                    Arc::clone(&gate),
                    vec!["a".into(), "b".into(), "c".into(), "d".into()],
                    move |_job_id, slot| {
                        let (live, peak, done) =
                            (Rc::clone(&live_t), Rc::clone(&peak_t), Rc::clone(&done_t));
                        async move {
                            // Held for the whole stub execution, exactly as `execute_job` holds it.
                            let _slot = slot;
                            live.set(live.get() + 1);
                            peak.set(peak.get().max(live.get()));
                            // Yield across an await so admitted tasks genuinely overlap: a bound that
                            // held only because nothing ever ran concurrently would prove nothing.
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            live.set(live.get() - 1);
                            done.set(done.get() + 1);
                        }
                    },
                );
                // Bounded drain rather than a fixed sleep, so a slow box cannot flake this.
                for _ in 0..200 {
                    if done.get() == 4 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;

        assert_eq!(
            done.get(),
            4,
            "every resumed job must run — the excess queues behind a permit, it is never dropped"
        );
        assert_eq!(
            peak.get(),
            2,
            "concurrent resumed executions must never exceed the configured slots"
        );
        assert_eq!(
            gate.available(),
            2,
            "every permit returns to the pool once the resumed jobs drain"
        );
    }

    /// #728 — the lapsed-park producer of the permitless state (the restart producer is the test
    /// above; both flow through the same `spawn_bounded_execution`). A claim's park lapses
    /// unawarded, the sweep reclaims the slot, and the award still arrives (`record_award` binds a
    /// claim in ANY state, `released` included). Pre-fix, the award path passed
    /// `take_for_execution`'s `None` straight into execution: the job ran holding nothing, the
    /// semaphore never decremented, and a 50-minute 8.1 GB build executed while status computed
    /// `busy = capacity - available()` as 0/3 and advertised `accepting=y` — measured live.
    ///
    /// ⚠ RED ON REVERT, pre-fix shape exactly: make `spawn_bounded_execution` hand its absent
    /// `parked` straight to the execution step. The lapsed-award job then starts immediately while
    /// the other job holds the only slot, and the "must wait" assertion fails.
    #[tokio::test]
    async fn an_award_after_its_park_lapsed_waits_for_a_real_slot_never_runs_permitless() {
        use std::cell::Cell;
        use std::rc::Rc;

        // Zero lapse window: the park for "lapsed" is immediately sweepable.
        let gate = Arc::new(SlotGate::new(1, Duration::ZERO));
        assert_eq!(gate.try_reserve("lapsed"), Reserve::Reserved);
        assert_eq!(gate.sweep_lapsed(Instant::now()), vec!["lapsed".to_owned()]);

        // Another job takes the freed slot and is executing — the node is genuinely full.
        assert_eq!(gate.try_reserve("occupier"), Reserve::Reserved);
        let occupying = gate.take_for_execution("occupier").expect("occupier's permit");

        let started = Rc::new(Cell::new(false));
        let held_real_permit = Rc::new(Cell::new(false));
        let done = Rc::new(Cell::new(false));

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (started_t, held_t, done_t) =
                    (Rc::clone(&started), Rc::clone(&held_real_permit), Rc::clone(&done));
                spawn_bounded_execution(&gate, "lapsed".into(), move |_job_id, slot| async move {
                    started_t.set(true);
                    // `Some` means a real `OwnedSemaphorePermit`: capacity is decremented for the
                    // whole run and status stops advertising a free node.
                    held_t.set(slot.is_some());
                    done_t.set(true);
                });
                // Give the spawned task every chance to (wrongly) run: it must be WAITING.
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(
                    !started.get(),
                    "the lapsed-award job must queue for a slot, never run permitless past the ceiling"
                );

                drop(occupying); // the executing job finishes; the fair semaphore hands over
                for _ in 0..200 {
                    if done.get() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;

        assert!(done.get(), "the awarded job runs once a slot frees — awarded work is never dropped");
        assert!(held_real_permit.get(), "and it holds a REAL permit, so slot accounting sees it");
        assert_eq!(gate.available(), 1, "which returns to the pool when the execution ends");
    }

    /// The fresh-award hot path through the same primitive: the parked reservation is taken
    /// SYNCHRONOUSLY on the caller's thread, before the spawned task first polls. That ordering is
    /// what keeps the take race-free with the lapse sweep (both run on the event loop) — the permit
    /// is never both sweepable and executing — and keeps the hot path free of any wait.
    #[tokio::test]
    async fn a_fresh_award_takes_its_park_synchronously_out_of_the_sweeps_reach() {
        let gate = Arc::new(SlotGate::new(1, Duration::ZERO)); // zero window: any park is sweepable
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, rx) = tokio::sync::oneshot::channel();
                spawn_bounded_execution(&gate, "a".into(), move |_job_id, slot| async move {
                    let _ = tx.send(slot.is_some());
                });
                // BEFORE the spawned task has run at all: the park is already gone, so a sweep
                // firing between spawn and first poll finds nothing to reclaim.
                assert!(
                    gate.sweep_lapsed(Instant::now()).is_empty(),
                    "the take happened on the caller's thread — nothing left for the sweep"
                );
                assert_eq!(gate.available(), 0, "the moved-out permit still counts as busy");
                assert!(
                    rx.await.expect("the execution step ran"),
                    "the execution holds the parked permit, no wait on the hot path"
                );
            })
            .await;
        assert_eq!(gate.available(), 1);
    }

    /// The THIRD producer of the missing-park state, named: a redundant second award (#279) finds
    /// the permit already moved out by the first award. The double-execution itself remains #279's
    /// open defect (its fix is a per-job in-flight lock, and in production the terminal-state guard
    /// refuses the rerun of a finished job) — but the second run is now SLOT-ACCOUNTED: it queues
    /// for and holds a real permit instead of bypassing the ceiling, so at full capacity it cannot
    /// even start until the first run has finished.
    #[tokio::test]
    async fn a_redundant_second_award_queues_for_a_real_slot_instead_of_bypassing_the_ceiling() {
        use std::cell::Cell;
        use std::rc::Rc;

        let gate = Arc::new(SlotGate::new(1, LONG));
        assert_eq!(gate.try_reserve("job"), Reserve::Reserved);

        let live = Rc::new(Cell::new(0usize));
        let peak = Rc::new(Cell::new(0usize));
        let done = Rc::new(Cell::new(0usize));

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Two award events for the SAME job id: the first takes the park, the second must
                // find it gone and wait.
                for _award in 0..2 {
                    let (live, peak, done) =
                        (Rc::clone(&live), Rc::clone(&peak), Rc::clone(&done));
                    spawn_bounded_execution(&gate, "job".into(), move |_job_id, slot| async move {
                        let _slot = slot; // held for the whole stub run, as execute_job holds it
                        live.set(live.get() + 1);
                        peak.set(peak.get().max(live.get()));
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        live.set(live.get() - 1);
                        done.set(done.get() + 1);
                    });
                }
                for _ in 0..200 {
                    if done.get() == 2 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;

        assert_eq!(
            done.get(),
            2,
            "both spawned executions ran — refusing the redundant one is the STATE guard's job, not the gate's"
        );
        assert_eq!(
            peak.get(),
            1,
            "the redundant execution held a real permit: never past the ceiling, never outside slot accounting"
        );
        assert_eq!(gate.available(), 1);
    }

    /// A resumed job's permit is never parked, so the lapse sweep cannot reclaim it mid-execution.
    ///
    /// This is the answer to #251's second question — the lapse clock for a re-acquired permit — and it
    /// is "there isn't one". `reserved_at` bounds a CLAIM waiting to be awarded; a resumed job is
    /// already awarded and executing, so seeding a fresh `Instant` would start a timer over a state the
    /// sweep was never meant to measure.
    #[tokio::test]
    async fn a_resumed_permit_is_not_parked_so_the_lapse_sweep_cannot_reclaim_it() {
        let gate = SlotGate::new(1, Duration::from_millis(0));
        let permit = gate
            .acquire_unreserved()
            .await
            .expect("the gate is never closed, so a permit is always eventually available");

        assert_eq!(gate.available(), 0, "a resumed job holds real capacity");
        assert!(
            gate.sweep_lapsed(Instant::now()).is_empty(),
            "an executing resumed job is not a parked claim and must never be swept"
        );
        assert_eq!(gate.available(), 0, "and the sweep must not have freed its permit");

        drop(permit);
        assert_eq!(gate.available(), 1, "released by the execution ending, like every other slot");
    }

    /// A queued resume waiter is not starved by incoming claims — the back-pressure #251's fix relies
    /// on, measured rather than assumed.
    ///
    /// The claim path uses `try_acquire` and the resume path `acquire().await`, so "a restarted node
    /// stops claiming while its backlog drains" holds only if tokio's semaphore is FAIR: a permit
    /// released while a waiter is queued must go to that waiter, not to a barging `try_acquire`. If it
    /// barged, resumed jobs could be starved indefinitely by a busy market and the bound would weaken
    /// to "`slots` per wave".
    ///
    /// The discriminating instant is a release with a waiter queued AND the waiter not yet polled.
    /// Asserting `Full` merely while the permit is still held would prove nothing — that is just "no
    /// permits available", one state next to the one that matters.
    #[tokio::test]
    async fn a_queued_resume_waiter_is_not_starved_by_a_barging_claim() {
        let gate = Arc::new(SlotGate::new(1, LONG));
        let held = gate.acquire_unreserved().await.expect("the gate is never closed");

        let waiting = Arc::clone(&gate);
        let waiter = tokio::spawn(async move { waiting.acquire_unreserved().await.is_some() });
        tokio::time::sleep(Duration::from_millis(50)).await; // let the waiter reach the queue

        drop(held);
        assert!(
            !waiter.is_finished(),
            "the waiter must still be unpolled, or this is not the contested instant"
        );
        assert_eq!(
            gate.try_reserve("an offer arriving the instant a slot frees"),
            Reserve::Full,
            "a released permit belongs to the queued resume, never to a barging claim"
        );

        assert!(
            waiter.await.expect("waiter task"),
            "and the queued resume is the one that gets it"
        );
    }

    #[test]
    fn release_is_idempotent() {
        let gate = SlotGate::new(1, LONG);
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        gate.release("a");
        gate.release("a"); // second release is a no-op, not a double-free / capacity inflation
        assert_eq!(gate.available(), 1);
    }

    #[test]
    fn re_seen_offer_does_not_double_reserve() {
        // A re-seen offer for an already-parked claim must not take a second permit — otherwise a
        // duplicate offer would leak a slot.
        let gate = SlotGate::new(1, LONG);
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        assert_eq!(gate.try_reserve("a"), Reserve::AlreadyParked);
        assert_eq!(gate.available(), 0);
    }

    #[test]
    fn taken_permit_holds_capacity_until_dropped() {
        // Models execution: the award moves the permit out into the job task, which holds it until
        // the job is terminal. Capacity stays consumed while the permit lives and returns on drop —
        // this is the RAII release that covers delivery, every fail_job path, and a panic.
        let gate = SlotGate::new(1, LONG);
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        let permit = gate.take_for_execution("a").expect("a reserved permit to take");
        assert_eq!(gate.available(), 0, "the executing job still holds its slot");
        assert_eq!(gate.try_reserve("b"), Reserve::Full, "no free slot while the job runs");
        drop(permit); // job reaches a terminal outcome (or panics — same drop)
        assert_eq!(gate.available(), 1, "the slot returns when execution ends");
        assert_eq!(gate.try_reserve("b"), Reserve::Reserved);
    }

    #[test]
    fn take_for_execution_is_none_whenever_nothing_is_parked_not_only_on_restart() {
        // "None ⇒ the restart path" was the exact false claim that armed #728: a lapsed park yields
        // the SAME None as a restart, and a comment documenting only the first producer is what
        // ended the search for the second. Neither producer fabricates capacity; both are answered
        // by `spawn_bounded_execution` waiting for a real permit.

        // Producer 1 — restart: durable store has the job, in-memory map is empty.
        let gate = SlotGate::new(1, LONG);
        assert!(gate.take_for_execution("never-reserved").is_none());
        assert_eq!(gate.available(), 1);

        // Producer 2 — lapsed park: reserved, swept unawarded, then the award arrives anyway.
        let lapsing = SlotGate::new(1, Duration::ZERO);
        assert_eq!(lapsing.try_reserve("lapsed"), Reserve::Reserved);
        assert_eq!(lapsing.sweep_lapsed(Instant::now()), vec!["lapsed".to_owned()]);
        assert!(
            lapsing.take_for_execution("lapsed").is_none(),
            "a swept park yields the same None as a restart — a fresh award is NOT exempt"
        );
        assert_eq!(lapsing.available(), 1);
    }

    #[test]
    fn lapse_sweep_reclaims_expired_and_leaves_fresh() {
        // A zero lapse window makes every parked claim immediately eligible; the sweep returns its
        // id (so the caller releases the durable claim) and frees the slot.
        let expiring = SlotGate::new(2, Duration::ZERO);
        assert_eq!(expiring.try_reserve("a"), Reserve::Reserved);
        assert_eq!(expiring.try_reserve("b"), Reserve::Reserved);
        let mut reclaimed = expiring.sweep_lapsed(Instant::now());
        reclaimed.sort();
        assert_eq!(reclaimed, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(expiring.available(), 2, "lapsed claims return their slots");

        // A long lapse window: a just-reserved claim is not swept.
        let fresh = SlotGate::new(1, LONG);
        assert_eq!(fresh.try_reserve("a"), Reserve::Reserved);
        assert!(fresh.sweep_lapsed(Instant::now()).is_empty(), "a fresh claim must not lapse");
        assert_eq!(fresh.available(), 0, "the un-lapsed claim keeps its slot");
    }

    #[test]
    fn every_release_path_returns_capacity() {
        // Consolidated revert-red over the three ways a reserved slot is returned: explicit release
        // (award-elsewhere / dedup), permit-drop (execution terminal / panic), and the lapse sweep.
        // If any one path stopped returning the permit, the final assertion would fail.
        let gate = SlotGate::new(1, Duration::ZERO);

        // 1. explicit release
        assert_eq!(gate.try_reserve("release"), Reserve::Reserved);
        gate.release("release");
        assert_eq!(gate.available(), 1);

        // 2. permit drop (models a job finishing or panicking)
        assert_eq!(gate.try_reserve("execute"), Reserve::Reserved);
        let permit = gate.take_for_execution("execute").expect("permit");
        drop(permit);
        assert_eq!(gate.available(), 1);

        // 3. lapse sweep
        assert_eq!(gate.try_reserve("lapse"), Reserve::Reserved);
        let _ = gate.sweep_lapsed(Instant::now());
        assert_eq!(gate.available(), 1, "all three release paths must return the slot");
    }

    #[test]
    fn double_release_cannot_exceed_capacity() {
        // Double-release is worse than a leak: a leak under-counts (safe-ish), but inflating the
        // count would over-commit and take jobs the node cannot deliver — where money exposure
        // enters. The permit lives in the parked map XOR in the execution task (an atomic
        // remove-and-return), and it is an `OwnedSemaphorePermit` released exactly once on drop; the
        // gate NEVER calls `add_permits`. So a stray release can only ever be a no-op, and the free
        // count can never exceed the configured capacity.
        let gate = SlotGate::new(1, LONG);
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        // Award moves the permit out into execution.
        let permit = gate.take_for_execution("a").expect("permit");
        // A stray release for the executing job id must NOT fabricate a slot (nothing parked).
        gate.release("a");
        assert_eq!(gate.available(), 0, "releasing an executing job's id must not free a slot");
        // The job ends: exactly one slot returns.
        drop(permit);
        assert_eq!(gate.available(), 1);
        // A second release after the drop is still a no-op — the count cannot exceed capacity.
        gate.release("a");
        assert_eq!(gate.available(), 1);
        assert!(gate.available() <= 1, "live slot count can never exceed configured capacity");
    }

    #[test]
    fn awarded_job_is_out_of_lapse_sweep_reach() {
        // The lapse timer keys off "no award yet": it only sees the parked map. An award moves the
        // permit into the execution task (removing it from parked), so an awarded job is out of the
        // sweep's reach entirely — ownership passes from the lapse-timer to the execution lifecycle.
        // This is why a slow-but-successful delivery can never have its slot freed out from under it,
        // and why there is no double-count: the sweep and the award both run on the single event
        // loop, so they never interleave, and the permit is never both sweepable and executing.
        let gate = SlotGate::new(1, Duration::ZERO); // zero window ⇒ everything parked is eligible
        assert_eq!(gate.try_reserve("a"), Reserve::Reserved);
        let permit = gate.take_for_execution("a").expect("permit");
        assert!(
            gate.sweep_lapsed(Instant::now()).is_empty(),
            "an awarded (executing) job must never be swept, even past the timeout"
        );
        assert_eq!(gate.available(), 0, "the executing job keeps its slot despite the elapsed timer");
        drop(permit);
        assert_eq!(gate.available(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELLER: &str = "aa";
    const BUYER: &str = "bb";
    const NOW: u64 = 10_000;

    // #591: PLANNER unit — plan_delivery_workdir maps a stored pin to a base_oid clone plan (on the
    // per-job fork branch) and no pin to Empty. This is the pure mapping ONLY; that execute_job
    // actually CALLS this routing is proven by the real-path test `clone_at_base_oid_not_empty_workdir`
    // (an isolated planner assertion like this one is exactly what let the dead wiring slip green).
    #[test]
    fn plan_delivery_workdir_maps_pin_to_clone_and_none_to_empty() {
        let base_oid = "a".repeat(40);
        let pin = crate::seller_node::store::ContributionPin {
            owner_pubkey: "b".repeat(64),
            clone_url: "https://relay.maxplayer.ai/git/owner/repo.git".to_owned(),
            base_branch: "main".to_owned(),
            base_oid: base_oid.clone(),
        };
        match plan_delivery_workdir(Some(pin), "job-42") {
            DeliveryWorkdirPlan::ContributionClone {
                clone_url,
                base_branch,
                base_oid: planned_oid,
                branch,
            } => {
                assert_eq!(planned_oid, base_oid, "the workdir clones AT the pinned base_oid");
                assert_eq!(base_branch, "main");
                assert_eq!(clone_url, "https://relay.maxplayer.ai/git/owner/repo.git");
                assert_eq!(
                    branch, "maxplayer/contribution/job-42",
                    "the per-job fork branch carries the full job id"
                );
            }
            DeliveryWorkdirPlan::Empty => {
                panic!("a served contribution pin must NOT provision an empty workdir")
            }
        }
        // A from-scratch job (no pin) stays on the empty-workdir path — the pin never touches it.
        assert_eq!(plan_delivery_workdir(None, "job-99"), DeliveryWorkdirPlan::Empty);
    }

    // RED-PROVE: a seller that does not own the home must put NOTHING on the relay.
    //
    // `boot_advertising_only_proven` published kind-0 discoverability and only then took the home lock,
    // inside boot. So a second seller started on a home another node already owns announced its
    // identity to the relay and then exited at the lock — leaving a kind-0 for a node that never
    // served a job. The lock is the claim on the home; it has to precede the claim on the wire.
    //
    // No relay is needed to observe the order. The publish path calls `home::save_config` to persist a
    // generated display name BEFORE it contacts any relay (`profile.rs`), so "did the publish path
    // run" is a question about the config file on disk. Restore the old order and the name appears
    // there even though the relay is unreachable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_seller_that_loses_the_home_lock_reaches_no_wire() {
        let root = temp_dir("lock-precedes-wire");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        // Deliberately unreachable: the config write under test happens before any relay contact, so
        // a regression is still visible without a fixture relay.
        home.config.relay_url = "ws://127.0.0.1:1".to_owned();
        let seller = seller_cfg(1, false);
        crate::home::save_config(&mut home, |config| {
            config.seller = Some(seller);
            // No display name, so the publish path would generate one and SAVE it.
            config.profile = None;
        })
        .expect("persist seller config");

        let config_path = root.join("config.toml");
        let before = std::fs::read_to_string(&config_path).expect("read config");
        assert!(
            !before.contains("maxplayer-seller-"),
            "precondition: no generated display name yet, or this test proves nothing"
        );

        // Another node already owns this home.
        let held = crate::seller_node::lock::HomeLock::acquire(
            home.root.join(crate::seller_node::LOCK_FILE),
        )
        .expect("first holder takes the lock");

        let verdicts = vec![HarnessProbeVerdict {
            index: 0,
            name: Some("claude".to_owned()),
            result: Ok(None),
        }];
        let outcome = boot_advertising_only_proven(home, verdicts).await;

        assert!(
            matches!(outcome, Err(NodeError::Lock(_))),
            "a seller that does not own the home must fail AT THE LOCK, before anything else"
        );

        let after = std::fs::read_to_string(&config_path).expect("read config after");
        assert!(
            !after.contains("maxplayer-seller-"),
            "the discoverability publish path ran despite the home being owned by another node — \
             its config write is the proof it got that far:\n{after}"
        );

        drop(held);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A seat that is REACHABLE by an unnamed buyer, which is deliberately NOT the product default.
    ///
    /// `accept_open_targeted` is forced true here so the gate a given test is actually about — rate,
    /// lapse, age, harness, slots — is the one that decides its outcome. Leave it at the product
    /// default and every such test would skip on buyer eligibility instead, passing or failing for a
    /// reason it never meant to exercise.
    ///
    /// ⛔ This fixture is therefore NOT evidence about the shipped default, and no test may cite it
    /// as such. The default that ships is pinned by `open_targeted_is_off_by_default_on_the_wire`
    /// (deserialization, the only thing an operator's `config.toml` actually goes through) and its
    /// behavioural twin `default_seat_refuses_a_targeted_offer_from_an_unnamed_buyer`.
    fn seller_cfg(rate_sats: u64, claim_open_pool: bool) -> crate::home::SellerConfig {
        crate::home::SellerConfig {
            agent_command: vec!["claude".to_owned()],
            rate_sats,
            git_remote: "https://example.invalid/repo.git".to_owned(),
            job_timeout_secs: None,
            agents: Vec::new(),
            claim_open_pool,
            accept_open_targeted: true,
            accept_offers_only_from: Vec::new(),
            offer_backfill_secs: 0,
            contribution_enabled: true,
            slots: 1,
            claim_award_timeout_secs: None,
        }
    }

    /// The registry an existing single-preset (`agent = "claude"`) seller resolves to.
    fn claude_only() -> LiveRoster {
        LiveRoster::new(AgentRegistry::new(vec![
            crate::seller_agents::RegisteredAgent {
                name: Some("claude".to_owned()),
                argv: vec!["claude-agent-acp".to_owned()],
            },
        ]))
    }

    fn offer(amount: u64, targeted_to: Option<&str>, deadline_unix: u64) -> ParsedOffer {
        ParsedOffer {
            task: "do the thing".to_owned(),
            output: String::new(),
            amount,
            unit: "sat".to_owned(),
            deadline_unix,
            seller_pubkey: targeted_to.map(str::to_owned),
            requested_agent: None,
            requested_harness_family: None,
            requested_model: None,
            required_capabilities: Vec::new(),
        }
    }

    // A fresh, in-rate, targeted offer is claimed and carries the resolved deadline.
    #[test]
    fn claims_fresh_targeted_offer_at_rate() {
        let decision = classify_offer(&offer(5, Some(SELLER), NOW + 600), &seller_cfg(2, false), &claude_only(), SELLER, BUYER, NOW, NOW);
        assert_eq!(decision, ClaimDecision::Claim { deadline_unix: NOW + 600 });
    }

    // MONEY-SAFETY ORDER: a lapsed offer (deadline already passed) is refused BEFORE the rate gate —
    // it is never resurrected with a fresh window, even though it clears the rate floor.
    #[test]
    fn refuses_lapsed_offer_before_rate() {
        let decision = classify_offer(&offer(100, Some(SELLER), NOW), &seller_cfg(2, false), &claude_only(), SELLER, BUYER, NOW, NOW);
        assert_eq!(decision, ClaimDecision::Skip(SkipReason::Lapsed));
    }

    // #604 OFFER-AGE GATE — an offer authored longer ago than MAX_OFFER_ADMIT_AGE_SECS is refused as a
    // long-aged historical, DISTINCT from the deadline gate: the deadline is FAR IN THE FUTURE here, so
    // only the age gate can produce this skip. The foil (an offer authored one second INSIDE the
    // horizon, same far-future deadline) still claims — the gate refuses by age, never a blanket veto.
    #[test]
    fn refuses_an_offer_authored_before_the_admit_horizon() {
        assert_eq!(
            classify_offer(&offer(100, Some(SELLER), NOW + 3_600), &seller_cfg(2, false), &claude_only(), SELLER, BUYER, NOW, NOW - (MAX_OFFER_ADMIT_AGE_SECS + 1)),
            ClaimDecision::Skip(SkipReason::TooOld),
            "an offer older than the admit horizon is refused (aged historical)"
        );
        assert!(
            matches!(
                classify_offer(&offer(100, Some(SELLER), NOW + 3_600), &seller_cfg(2, false), &claude_only(), SELLER, BUYER, NOW, NOW - (MAX_OFFER_ADMIT_AGE_SECS - 1)),
                ClaimDecision::Claim { .. }
            ),
            "an offer within the horizon (same far-future deadline) still claims — refused by age, not by a blanket veto"
        );
    }

    // Below the rate floor ⇒ skip (never claim work priced under the seller's floor).
    #[test]
    fn refuses_below_rate() {
        let decision = classify_offer(&offer(1, Some(SELLER), NOW + 600), &seller_cfg(5, false), &claude_only(), SELLER, BUYER, NOW, NOW);
        assert_eq!(decision, ClaimDecision::Skip(SkipReason::RateGate));
    }

    // #482 FENCE (the reject leg) — a populated `accept_offers_only_from` claims ONLY from a named
    // buyer; an UNLISTED buyer's otherwise-claimable offer is skipped with the dedicated reason. The
    // foil is real: identical offer, identical seller, the ONLY difference is the buyer's pubkey (one
    // on the list, one — STRANGER — deliberately not).
    #[test]
    fn allowlist_fences_out_an_unlisted_buyer() {
        const ALLOWED: &str = "cafe01";
        const STRANGER: &str = "dead02"; // ≠ ALLOWED — the real foil, never vacuous-green
        let mut cfg = seller_cfg(2, false);
        cfg.accept_offers_only_from = vec![ALLOWED.to_owned()];
        // #923: STATED, not inherited. This test is about the FENCE, so the targeted route must be
        // shut for the fence to be the gate that decides. The fixture forces `accept_open_targeted`
        // TRUE for reachability, and until #923 a populated allowlist made that flag inert — so this
        // test used to read as a fence test while silently depending on the precedence bug. Now that
        // the two controls are additive, leaving it inherited would assert the OPPOSITE of #923.
        cfg.accept_open_targeted = false;

        // Listed buyer ⇒ still claims (same offer an empty allowlist would claim).
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, ALLOWED, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "a listed buyer's offer must claim"
        );
        // Unlisted buyer ⇒ fenced out, named reason (NOT a silent drop, NOT a rate/harness reason).
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, STRANGER, NOW, NOW),
            ClaimDecision::Skip(SkipReason::NotAllowlisted),
            "an unlisted buyer's offer must be fenced — only the buyer changed"
        );
    }

    /// The already-handled suppression must NAME itself, and only for a job that is actually done.
    ///
    /// Checked over every `JobState` rather than the finished ones alone: the operative guard on the
    /// re-serve path used to emit nothing at default verbosity, while the only visible skip reason
    /// there is slot exhaustion — a different guard entirely. A reader watching a seat with free
    /// slots decline its own completed work would credit the wrong protector, so the line must fire
    /// on exactly the finished states and stay quiet on the rest.
    #[test]
    fn the_already_handled_skip_names_itself_on_finished_jobs_only() {
        use super::super::store::JobState;

        for state in JobState::ALL {
            let line = already_handled_skip_line("job-7", Some(state));
            if state.is_finished() {
                assert_eq!(
                    line.as_deref(),
                    Some(
                        format!(
                            "seller node offer skip id=job-7: already handled (job {}; not re-claiming)",
                            state.as_str()
                        )
                        .as_str()
                    ),
                    "a re-served offer whose job is {state:?} must say why it is not re-claimed"
                );
            } else {
                assert_eq!(
                    line, None,
                    "{state:?} still occupies a slot — it is not `already handled` and must stay on \
                     the verbose dedup line"
                );
            }
        }
        // An absent or unreadable state is not evidence of a finished job.
        assert_eq!(
            already_handled_skip_line("job-7", None),
            None,
            "no job row ⇒ no already-handled claim"
        );
    }

    // THE DEFAULT SEAT IS CLOSED ON THE TARGETED SURFACE. This test replaces the #482-era
    // `empty_allowlist_accepts_any_buyer`, which asserted the OPPOSITE — that an empty allowlist is
    // accept-all — and whose precondition line ("default allowlist is empty") read the emptiness as
    // if it carried the policy. It does not any more: emptiness means the operator named no buyers,
    // and who else may reach the seat is decided by the two surface flags alone.
    //
    // The foil is the same offer from the same buyer at the same instant, with the ONE new flag
    // flipped — so a gate that stopped consulting `accept_open_targeted` cannot pass this.
    #[test]
    fn default_seat_refuses_a_targeted_offer_from_an_unnamed_buyer() {
        let mut cfg = seller_cfg(2, false);
        cfg.accept_open_targeted = false; // the SHIPPED default; the fixture deliberately differs
        assert!(cfg.accept_offers_only_from.is_empty(), "precondition: no buyer is named");

        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, "anybody99", NOW, NOW),
            ClaimDecision::Skip(SkipReason::OpenTargetedRefused),
            "a seat that named no buyers and did not open the targeted surface must refuse a stranger"
        );
        cfg.accept_open_targeted = true;
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, "anybody99", NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "opting in to the targeted surface must let exactly that same offer through"
        );
    }

    // The refusal reports its OWN reason, not the allowlist fence's. An operator with no allowlist
    // told "buyer not in accept_offers_only_from allowlist" goes looking for a list that does not
    // exist; the two skips answer different questions and must stay distinguishable. Also pins that
    // the line names the knob that restores service — this is the migration's only audible signal.
    #[test]
    fn the_closed_targeted_refusal_names_its_own_knob_not_the_allowlist() {
        let refusal = SkipReason::OpenTargetedRefused.reason();
        assert!(
            refusal.contains("accept_open_targeted"),
            "the refusal must name the knob that reopens the seat, got: {refusal}"
        );
        assert_ne!(
            refusal,
            SkipReason::NotAllowlisted.reason(),
            "a closed seat and a fenced-out buyer must not share one string — they need different fixes"
        );
    }

    // THE SHIPPED DEFAULT, read through the path an operator's `config.toml` actually takes. The
    // fixture above forces this flag TRUE for test reachability, so nothing in this module's
    // behavioural tests can attest the default; deserialization is the only thing that can.
    #[test]
    fn open_targeted_is_off_by_default_on_the_wire() {
        let cfg: crate::home::SellerConfig = toml::from_str(
            r#"
            agent_command = ["claude"]
            rate_sats = 2
            git_remote = "https://example.invalid/repo.git"
            "#,
        )
        .expect("a [seller] block with no open-surface keys must still parse");

        assert!(
            !cfg.accept_open_targeted,
            "a config that never mentions accept_open_targeted must default to CLOSED"
        );
        // The foil: the field is genuinely wired to serde, not merely absent-and-false by accident.
        let opted_in: crate::home::SellerConfig = toml::from_str(
            r#"
            agent_command = ["claude"]
            rate_sats = 2
            git_remote = "https://example.invalid/repo.git"
            accept_open_targeted = true
            "#,
        )
        .expect("an explicit opt-in must parse");
        assert!(opted_in.accept_open_targeted, "an explicit true must survive deserialization");
    }

    // KNOB INDEPENDENCE, both directions. The whole point of three knobs is that no one of them is
    // inferred from another, so each surface must be openable WITHOUT opening the other.
    #[test]
    fn the_two_open_surfaces_are_independent() {
        // Targeted opt-in alone must NOT open the pool — the untargeted offer still needs
        // `claim_open_pool`, and it is the rate gate (not the buyer gate) that says so.
        let mut targeted_only = seller_cfg(2, false);
        targeted_only.accept_open_targeted = true;
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &targeted_only, &claude_only(), SELLER, BUYER, NOW, NOW),
            ClaimDecision::Skip(SkipReason::RateGate),
            "accept_open_targeted must not silently enrol the seat in the open pool"
        );

        // Pool opt-in alone must NOT open the targeted surface, and must still claim the pool —
        // `claim_open_pool` is UNCHANGED by the three-knob split.
        let mut pool_only = seller_cfg(2, true);
        pool_only.accept_open_targeted = false;
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &pool_only, &claude_only(), SELLER, BUYER, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "claim_open_pool must still claim an untargeted offer from an unnamed buyer, exactly as before"
        );
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &pool_only, &claude_only(), SELLER, BUYER, NOW, NOW),
            ClaimDecision::Skip(SkipReason::OpenTargetedRefused),
            "claiming the open pool must not also invite strangers to target the seat directly"
        );
    }

    // #923 — THE ALLOWLIST AND THE TARGETED OPT-IN ADMIT ADDITIVELY.
    //
    // ⛔ THIS REPLACES `a_populated_allowlist_wins_over_the_targeted_opt_in`, DELIBERATELY. That test
    // ran on this exact fixture and asserted the OPPOSITE: that the stranger below reports
    // `NotAllowlisted`, and that `accept_open_targeted` is "set, and deliberately INERT while a list
    // exists". That assertion IS the defect #923 reports — it locked in a config whose `true` did
    // nothing, so an operator could not keep trusted buyers while temporarily opening the public
    // route, and the config file said one thing while the seat did another.
    //
    // Its stated fear was that flipping the clause order "silently dissolves the #482 fence". It does
    // not, and that distinction is the whole of this change: #923 removes the list's VETO over the
    // flag beside it, never the list's own refusal. The fence survives as the reject leg and is still
    // asserted — by `allowlist_fences_out_an_unlisted_buyer`, and by every `accept_open_targeted =
    // false` row of `the_three_admission_controls_are_additive_and_independent` below.
    //
    // RED ON REVERT: reinstate the standalone `!seller.accept_offers_only_from.is_empty() &&
    // !buyer_is_named` early return in `classify_offer` (the pre-#923 clause 1). The stranger then
    // reports `NotAllowlisted` and the first assertion fails.
    #[test]
    fn the_allowlist_and_the_targeted_opt_in_admit_additively() {
        const ALLOWED: &str = "cafe01";
        const STRANGER: &str = "dead02"; // ≠ ALLOWED — the foil, so neither leg is vacuous-green
        let mut cfg = seller_cfg(2, false);
        cfg.accept_offers_only_from = vec![ALLOWED.to_owned()];
        cfg.accept_open_targeted = true; // set, and now EFFECTIVE alongside the list

        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, STRANGER, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "accept_open_targeted must ADDITIONALLY admit an unnamed targeted buyer — a populated \
             allowlist may not cancel it (#923)"
        );
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, ALLOWED, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "the named buyer keeps its own route in — the private fallback is not traded away"
        );
    }

    // THE #923 ADMISSION MATRIX — all eight combinations of the three controls, each probed on BOTH
    // surfaces from BOTH a named and an unnamed buyer. Thirty-two expectations, transcribed as
    // literal outcomes from the issue's truth table and NEVER computed from the gate's own predicate:
    // a table derived from the implementation restates the bug whenever there is one.
    //
    // ⛔ A MATRIX, NOT THREE CASES, BECAUSE THE DEFECT WAS A PRECEDENCE BUG. Every control tested
    // ALONE was already correct before #923 — an allowlist alone fenced, `accept_open_targeted` alone
    // opened the targeted route, `claim_open_pool` alone claimed the pool. Only the COMBINATION was
    // wrong. A suite of one-knob-at-a-time tests is fully green on the bug, which is how it shipped.
    //
    // ⛔ THIS IS AN ADMISSION CONTROL, SO THE ROWS THAT REFUSE CARRY THE SAME WEIGHT AS THE ROWS THAT
    // ADMIT. Sixteen of the thirty-two expectations are refusals. A widening that admitted more than
    // these three controls describe would pass every Claim row here and fail those.
    //
    // RED ON REVERT, two independent ways:
    //  - drop the `offer.seller_pubkey.as_deref() == Some(seller_pubkey) &&` scope from the
    //    eligibility clause in `classify_offer` ⇒ the (list, targeted-closed, pool-open) untargeted
    //    rows turn from Claim into a buyer-eligibility refusal.
    //  - reinstate the pre-#923 `!seller.accept_offers_only_from.is_empty() && !buyer_is_named` early
    //    return ⇒ every populated-list row with an unnamed buyer turns into NotAllowlisted.
    #[test]
    fn the_three_admission_controls_are_additive_and_independent() {
        const ALLOWED: &str = "cafe01";
        const STRANGER: &str = "dead02";

        let claim = ClaimDecision::Claim { deadline_unix: NOW + 600 };
        let shut = ClaimDecision::Skip(SkipReason::OpenTargetedRefused); // no list to edit
        let fenced = ClaimDecision::Skip(SkipReason::NotAllowlisted); // a list exists, buyer not on it
        let no_pool = ClaimDecision::Skip(SkipReason::RateGate); // untargeted without claim_open_pool

        // Columns are the four probes in order: named+targeted, stranger+targeted, named+untargeted,
        // stranger+untargeted. With an EMPTY list nobody is named, so the first two columns of an
        // empty-list row are identical by construction — that is a property of the fixture, not a
        // duplicated assertion.
        let matrix = [
            (false, false, false, [&shut, &shut, &no_pool, &no_pool],
             "no list, both routes closed ⇒ no work reaches this seat at all"),
            (false, true, false, [&claim, &claim, &no_pool, &no_pool],
             "no list, targeted route open ⇒ any buyer may target; the pool stays shut"),
            (false, false, true, [&shut, &shut, &claim, &claim],
             "no list, pool open ⇒ untargeted claims only; targeting still refused"),
            (false, true, true, [&claim, &claim, &claim, &claim],
             "no list, both routes open ⇒ both public surfaces serve"),
            (true, false, false, [&claim, &fenced, &no_pool, &no_pool],
             "list only ⇒ the named buyer targets and nobody else does (the #482 fence, intact)"),
            (true, true, false, [&claim, &claim, &no_pool, &no_pool],
             "list + targeted route ⇒ ADDITIVE: any buyer may target, and the pool is untouched"),
            (true, false, true, [&claim, &fenced, &claim, &claim],
             "list + pool ⇒ the pool is INDEPENDENT of the list, and targeting is still fenced"),
            (true, true, true, [&claim, &claim, &claim, &claim],
             "list + both routes ⇒ all three admissions serve at once"),
        ];

        // ⛔ COUNTED, because `zip` TRUNCATES SILENTLY. Add a fifth probe without a fifth expected
        // outcome and the pair below still runs, still passes, and quietly stops testing the new
        // probe — a green matrix that covers less than it says. The count is the only thing between
        // that and a false all-clear, and this is an admission control.
        let mut checked = 0usize;
        for (populated, open_targeted, open_pool, expected, what) in matrix {
            let mut cfg = seller_cfg(2, open_pool);
            cfg.accept_open_targeted = open_targeted;
            cfg.accept_offers_only_from =
                if populated { vec![ALLOWED.to_owned()] } else { Vec::new() };

            let probes = [
                (ALLOWED, Some(SELLER), "the listed buyer, targeting this seat"),
                (STRANGER, Some(SELLER), "an unlisted buyer, targeting this seat"),
                (ALLOWED, None, "the listed buyer's UNTARGETED open-pool offer"),
                (STRANGER, None, "an unlisted buyer's UNTARGETED open-pool offer"),
            ];
            for ((buyer, target, probe), want) in probes.into_iter().zip(expected) {
                assert_eq!(
                    &classify_offer(&offer(5, target, NOW + 600), &cfg, &claude_only(), SELLER, buyer, NOW, NOW),
                    want,
                    "accept_offers_only_from populated={populated} accept_open_targeted=\
                     {open_targeted} claim_open_pool={open_pool} — {what}. Probe: {probe}"
                );
                checked += 1;
            }
        }
        assert_eq!(
            checked, 32,
            "the matrix must run all 8 control settings x 4 probes — a short count means a row or a \
             probe stopped being exercised"
        );
    }

    // THE THIRD OFFER SHAPE, AND THE ONE THIS CHANGE COULD MOST EASILY HAVE OPENED. #923 narrows the
    // buyer-eligibility clause to offers whose `p` tag is THIS seat, so an offer targeted at SOMEONE
    // ELSE now bypasses that clause entirely and is refused only by the rate gate. Nothing in the
    // suite exercised that shape through `classify_offer` before this test, on any config — so the
    // narrowing was load-bearing and unproven, which is the pair a security-shaped diff must not
    // ship. Swept over all eight control settings and both buyers: sixteen refusals, no exceptions.
    //
    // ⛔ DISCLOSED REASON CHANGE, NOT A DECISION CHANGE. Before #923 a populated allowlist reported
    // `NotAllowlisted` for this shape, because the fence ran ahead of everything. It now reports the
    // rate gate's refusal — which is exactly what a seat with an EMPTY allowlist already reported for
    // the same offer. The refusal itself is identical either way; all seats now agree on the reason.
    //
    // RED ON REVERT: reinstate the pre-#923 `!seller.accept_offers_only_from.is_empty() &&
    // !buyer_is_named` early return ⇒ the populated-list rows report NotAllowlisted, not RateGate.
    #[test]
    fn an_offer_targeted_at_another_seat_is_refused_under_every_control_setting() {
        const ALLOWED: &str = "cafe01";
        const STRANGER: &str = "dead02";
        const OTHER_SEAT: &str = "beef03"; // ≠ SELLER — the offer is addressed elsewhere

        let mut checked = 0usize;
        for populated in [false, true] {
            for open_targeted in [false, true] {
                for open_pool in [false, true] {
                    let mut cfg = seller_cfg(2, open_pool);
                    cfg.accept_open_targeted = open_targeted;
                    cfg.accept_offers_only_from =
                        if populated { vec![ALLOWED.to_owned()] } else { Vec::new() };

                    for buyer in [ALLOWED, STRANGER] {
                        assert_eq!(
                            classify_offer(&offer(5, Some(OTHER_SEAT), NOW + 600), &cfg, &claude_only(), SELLER, buyer, NOW, NOW),
                            ClaimDecision::Skip(SkipReason::RateGate),
                            "an offer addressed to another seat must never be claimed \
                             (list_populated={populated} accept_open_targeted={open_targeted} \
                             claim_open_pool={open_pool} buyer={buyer}) — opening a route for THIS \
                             seat may not admit work addressed to a different one"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 16, "the sweep must cover all 8 control settings x 2 buyers");
    }

    // ROUND TRIP — the operator workflow #923 exists to enable: open a public route, then close it
    // and be back to allowlist-only targeted work with the list never rewritten. Without this, the
    // matrix above is satisfied by a gate that reaches the right states but cannot get back.
    //
    // ⛔ BOUNDED CLAIM: `classify_offer` is pure over `&SellerConfig`, so this attests the ADMISSION
    // round trip and the in-memory list, not the config-file writeback. That the writeback preserves
    // an operator's allowlist across a bare relaunch is a separate surface, already pinned by
    // `sell_writeback_preserves_operator_accept_offers_only_from` in `crates/maxplayer/src/sell.rs`.
    //
    // RED ON REVERT: reinstate the pre-#923 `!seller.accept_offers_only_from.is_empty() &&
    // !buyer_is_named` early return ⇒ the two flags-open assertions fail before the round trip runs.
    #[test]
    fn closing_both_open_routes_restores_allowlist_only_admission_with_the_list_intact() {
        const ALLOWED: &str = "cafe01";
        const STRANGER: &str = "dead02";
        let mut cfg = seller_cfg(2, true); // claim_open_pool = true
        cfg.accept_open_targeted = true;
        cfg.accept_offers_only_from = vec![ALLOWED.to_owned()];
        let list_before = cfg.accept_offers_only_from.clone();

        // Both public routes open: the stranger reaches BOTH surfaces.
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, STRANGER, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "with the targeted route open, an unlisted buyer may target the seat"
        );
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &cfg, &claude_only(), SELLER, STRANGER, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "with the pool open, an unlisted buyer's untargeted offer is claimable"
        );

        // Close both. Nothing reconstructs buyer identities, and the private fallback is immediate.
        cfg.accept_open_targeted = false;
        cfg.claim_open_pool = false;
        assert_eq!(
            cfg.accept_offers_only_from, list_before,
            "toggling an open route must never rewrite or clear the allowlist"
        );
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, ALLOWED, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "the listed buyer is served again the moment the public routes close"
        );
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, STRANGER, NOW, NOW),
            ClaimDecision::Skip(SkipReason::NotAllowlisted),
            "closing the targeted route must fence the stranger out again"
        );
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &cfg, &claude_only(), SELLER, ALLOWED, NOW, NOW),
            ClaimDecision::Skip(SkipReason::RateGate),
            "closing the pool refuses untargeted work even from the listed buyer — the pool is its \
             own control, not a property of the list"
        );
    }

    // A NAMED BUYER REACHES A CLOSED SEAT — the migration path an operator is told to take. Without
    // this, every assertion above is satisfied by a gate that simply refuses everything.
    #[test]
    fn a_named_buyer_reaches_a_seat_with_the_targeted_surface_closed() {
        const ALLOWED: &str = "cafe01";
        let mut cfg = seller_cfg(2, false);
        cfg.accept_offers_only_from = vec![ALLOWED.to_owned()];
        cfg.accept_open_targeted = false;
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &cfg, &claude_only(), SELLER, ALLOWED, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "listing a buyer must be sufficient to keep working with the targeted surface closed"
        );
    }

    // THE MIGRATION SIREN. The condition is exactly the config an already-deployed seller upgrades
    // INTO: it had no allowlist (which used to mean accept-all) and never opted in to either surface,
    // so after the split it claims nothing while looking entirely healthy. Fires there and nowhere
    // else — each of the three routes in is checked to SILENCE it, so a warning that simply always
    // fired (or a predicate that lost a clause) fails here rather than training operators to ignore it.
    #[test]
    fn the_unreachable_seat_warning_fires_on_exactly_the_closed_config() {
        let closed = seller_cfg_closed();
        let warning = unreachable_seat_warning(&closed).expect("a seat with no way in must warn");
        assert!(
            warning.contains("accept_open_targeted") && warning.contains("accept_offers_only_from"),
            "the warning must name the knobs that restore service, got: {warning}"
        );

        // Route 1 — name a buyer. ⛔ A WIRE-FORM pubkey: the fixture here used to be `cafe01`,
        // which is fenced-but-unmatchable and therefore NOT a way in. Asserting `None` against it
        // pinned the bug as the specification.
        let mut listed = seller_cfg_closed();
        listed.accept_offers_only_from = vec!["a1".repeat(32)];
        assert_eq!(unreachable_seat_warning(&listed), None, "a named buyer is a way in");

        // Route 2 — open the targeted surface.
        let mut open_targeted = seller_cfg_closed();
        open_targeted.accept_open_targeted = true;
        assert_eq!(unreachable_seat_warning(&open_targeted), None, "the targeted surface is a way in");

        // Route 3 — claim the open pool.
        let mut open_pool = seller_cfg_closed();
        open_pool.claim_open_pool = true;
        assert_eq!(unreachable_seat_warning(&open_pool), None, "the open pool is a way in");
    }

    /// ⛔ ASSERTED ON THE REMEDY CLAUSE ALONE, NEVER THE WHOLE STRING. The diagnosis already names
    /// all three knobs — as the settings that are OFF — so `warning.contains("claim_open_pool")` is
    /// satisfied by text that tells the operator nothing about how to fix it. The needle is present
    /// in the WRONG ROLE, and splitting on the marker is what gives this test access to the claim
    /// it is actually making.
    /// Extraction only — deliberately NOT an assertion helper. Each branch keeps its own `#[test]`
    /// and its own assertions, because the runner stops at the first failing assertion: folding the
    /// two branches into one test would let whichever ran second go unexercised while the suite
    /// still reported a failure for the right-looking reason.
    fn remedy_clause_of(warning: &str) -> String {
        warning
            .split_once("THREE ROUTES BACK IN:")
            .expect(
                "the remedy must be delimited so it can be read apart from the diagnosis, which \
                 names the same knobs as the settings that are OFF",
            )
            .1
            .to_owned()
    }

    /// ⛔ ASSERTED ON THE REMEDY CLAUSE ALONE, NEVER THE WHOLE STRING. The diagnosis already names
    /// all three knobs — as the settings that are OFF — so `warning.contains("claim_open_pool")` is
    /// satisfied by text that tells the operator nothing about how to fix it. The needle is present
    /// in the WRONG ROLE.
    #[test]
    fn the_empty_list_warning_offers_all_three_routes_back_in() {
        let warning =
            unreachable_seat_warning(&seller_cfg_closed()).expect("a closed seat must warn");
        let remedy = remedy_clause_of(&warning);
        for route in ["accept_offers_only_from", "accept_open_targeted", "claim_open_pool"] {
            assert!(remedy.contains(route), "the remedy must offer `{route}`, got: {remedy}");
        }
    }

    /// ⛔ THE BRANCH THAT WAS UNCOVERED, AND THE REASON THIS IS A SEPARATE TEST. The remedy used to
    /// be spelled literally in each branch, and the only test that read one read the EMPTY branch:
    /// deleting a route from the junk-list copy alone left the suite green. A mutation proves the
    /// line it touched, never the claim it was chosen to represent — so each branch is asserted
    /// where it lives, and both now read the one shared constant.
    #[test]
    fn the_junk_list_warning_offers_all_three_routes_back_in() {
        let mut junk = seller_cfg_closed();
        junk.accept_offers_only_from = vec!["cafe01".to_owned()];
        let warning =
            unreachable_seat_warning(&junk).expect("an all-unusable allowlist must warn");
        let remedy = remedy_clause_of(&warning);
        for route in ["accept_offers_only_from", "accept_open_targeted", "claim_open_pool"] {
            assert!(remedy.contains(route), "the remedy must offer `{route}`, got: {remedy}");
        }
    }

    /// The two branches must not drift apart again: whatever the diagnosis says, the remedy is the
    /// SAME text in both. This is the assertion a shared constant makes cheap and two literals make
    /// impossible — it fails the moment someone re-inlines either copy.
    #[test]
    fn both_unreachable_branches_share_one_remedy_text() {
        let mut junk = seller_cfg_closed();
        junk.accept_offers_only_from = vec!["cafe01".to_owned()];
        let empty = remedy_clause_of(
            &unreachable_seat_warning(&seller_cfg_closed()).expect("closed seat warns"),
        );
        let unusable =
            remedy_clause_of(&unreachable_seat_warning(&junk).expect("junk allowlist warns"));
        assert_eq!(empty, unusable, "both branches must offer the identical remedy");
        assert!(!empty.is_empty(), "and it must not be vacuously equal by both being empty");
    }

    /// ⛔ THE EXPLANATION MUST NAME THE RULE THAT ACTUALLY REFUSED THE ENTRY. Every other test here
    /// uses a fixture rejected for its SHAPE — `cafe01` is too short, `A1…` is capitalised — so none
    /// of them can tell a shape-only message from a correct one. This one is rejected for its CURVE
    /// and nothing else, which is the only fixture that puts the wording under test.
    ///
    /// ⛔ WHY IT MATTERS MORE THAN A WORDING NIT: an operator whose entries are all curve-invalid is
    /// told every entry is unusable and handed a rule their entries already pass. A right diagnosis
    /// with an unusable correction is worse than a vague one — they have a stated rule that
    /// demonstrably fails on their input and no way to see why, because this string is their only
    /// interface to the predicate.
    ///
    /// ⛔ THE PRECONDITION ASSERTS ARE THE TEST. Without them this passes on any junk fixture and
    /// proves nothing about the curve wording — the same trap as picking a negative control by eye.
    #[test]
    fn the_unusable_list_explanation_names_the_curve_not_only_the_shape() {
        let curve_rejected = "0123456789abcdef".repeat(4);
        assert!(
            crate::home::buyer_pubkey_is_wire_shaped(&curve_rejected),
            "precondition: `{curve_rejected}` must be SHAPE-valid, or a shape-only message would be \
             a correct explanation for it and this test asserts nothing"
        );
        assert!(
            !crate::home::buyer_pubkey_is_reachable(&curve_rejected),
            "precondition: `{curve_rejected}` must be refused for its CURVE, not its shape"
        );

        let mut junk = seller_cfg_closed();
        junk.accept_offers_only_from = vec![curve_rejected.clone()];
        let warning =
            unreachable_seat_warning(&junk).expect("a curve-rejected allowlist must warn");

        // Pinned twice, and both are needed: the first fails if this site re-inlines its own copy,
        // the second fails if the shared constant is weakened back to shape-only. Either alone
        // leaves one of the two ways this regressed still open.
        assert!(
            warning.contains(crate::home::USABLE_BUYER_ENTRY),
            "the warning must read the SHARED criterion, not a local copy of it, got: {warning}"
        );
        assert!(
            warning.contains("secp256k1"),
            "the criterion must still name what rejected `{curve_rejected}`, got: {warning}"
        );
    }

    /// ⛔ THE FENCE OPENS ON `is_empty()`, SO A LIST OF TYPOS ADMITS NOBODY AND SHUTS BOTH SURFACES.
    /// The siren keyed on emptiness and therefore stayed silent for precisely the seat it exists to
    /// catch. Asserted separately from the mixed case below: they are opposite outcomes, and the
    /// runner stops at the first failing assertion.
    #[test]
    fn the_siren_fires_for_an_allowlist_that_can_never_match() {
        let mut junk = seller_cfg_closed();
        junk.accept_offers_only_from = vec!["cafe01".to_owned(), "A1".repeat(32)];
        // Both open flags ON: the fence shuts them anyway, so they must not rescue the seat.
        junk.accept_open_targeted = true;
        junk.claim_open_pool = true;
        let warning = unreachable_seat_warning(&junk)
            .expect("a seat whose every allowlist entry is unusable can claim nothing");
        assert!(
            warning.contains("unusable"),
            "the operator must be told the entries are the problem, got: {warning}"
        );
    }

    /// The foil for the test above: ONE usable entry among unusable ones is a real route in, so the
    /// siren must stay silent. Without this, a predicate that always fired would pass.
    #[test]
    fn the_siren_stays_silent_when_one_allowlist_entry_is_usable() {
        let mut mixed = seller_cfg_closed();
        mixed.accept_offers_only_from = vec!["cafe01".to_owned(), "a1".repeat(32)];
        assert_eq!(
            unreachable_seat_warning(&mixed),
            None,
            "one buyer that can actually match is a way in, whatever else is listed"
        );
    }

    /// The fully-closed seat: the config an upgrading seller with no allowlist lands on. Written out
    /// rather than derived from `seller_cfg`, which deliberately ships an OPEN targeted surface.
    fn seller_cfg_closed() -> crate::home::SellerConfig {
        let mut cfg = seller_cfg(2, false);
        cfg.accept_open_targeted = false;
        cfg.accept_offers_only_from = Vec::new();
        cfg
    }

    // #482 ORDER — the fence is consulted AFTER the lapsed refusal: a dead offer from an unlisted
    // buyer reports Lapsed (money-safety ordering preserved), not NotAllowlisted.
    #[test]
    fn lapsed_is_refused_before_the_allowlist_fence() {
        let mut cfg = seller_cfg(2, false);
        cfg.accept_offers_only_from = vec!["cafe01".to_owned()];
        assert_eq!(
            classify_offer(&offer(100, Some(SELLER), NOW), &cfg, &claude_only(), SELLER, "dead02", NOW, NOW),
            ClaimDecision::Skip(SkipReason::Lapsed),
            "a lapsed offer is refused before the allowlist is consulted"
        );
    }

    // TOOTH (invariant 8 / audit N-4) — the delivery cosignature signs the hash of the STORED
    // claim-time creq, never a rebuild from live config. Author a creq under one accepted-mint set
    // (what the buyer read off the claim), then "drift" the config to a different mint set: the
    // stored creq's hash is unchanged, and the drifted config would produce a DIFFERENT hash that
    // delivery must NOT use. Signing the stored value is what keeps buyer/seller cosigs agreeing.
    #[test]
    fn delivery_hash_binds_stored_creq_not_drifted_config() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let mints_claim = vec!["https://testnut.cashudevkit.org".to_owned()];
        let stored_creq =
            gateway::creq::build_seller_creq("job-1", 21, "sat", &mints_claim, &seller).expect("creq");
        let signed_hash = gateway::creq_hash_hex(&stored_creq);

        // Config drifts to a different accepted-mint set after the claim.
        let mints_drifted = vec![
            "https://testnut.cashudevkit.org".to_owned(),
            "https://mint.example.invalid".to_owned(),
        ];
        let drifted_creq =
            gateway::creq::build_seller_creq("job-1", 21, "sat", &mints_drifted, &seller).expect("creq2");

        // The stored creq's hash is stable; the drifted config yields a DIFFERENT hash — which the
        // delivery path must never sign (it reads store.job_creq, not live config).
        assert_eq!(gateway::creq_hash_hex(&stored_creq), signed_hash);
        assert_ne!(
            gateway::creq_hash_hex(&drifted_creq),
            signed_hash,
            "a config-drifted creq hashes differently; delivery must sign the STORED creq's hash"
        );
    }

    // #456 — an open-pool seller's award/accept subscription must NOT be scoped to its own pubkey: an
    // award p-tags only the WINNER, so a loser scoped to itself never sees the award that frees its
    // slot and waits out the lapse timeout. The open-pool filter matches by kind + hashtag alone.
    #[test]
    fn award_filter_is_unscoped_for_open_pool_so_losers_see_the_award() {
        let pk = nostr_sdk::prelude::Keys::generate().public_key();
        let json = serde_json::to_value(award_filter(pk, true)).expect("serialize open-pool filter");
        assert!(
            json.get("#p").is_none(),
            "open-pool award filter must not scope by pubkey, or a losing claimant never receives the award: {json}"
        );
        assert!(json.get("kinds").is_some(), "must still bound to the award/accept kinds: {json}");
        assert!(json.get("#t").is_some(), "must still bound to the maxplayer hashtag: {json}");
    }

    // The FOIL: a targeted-only seller KEEPS the pubkey scope (it only claims offers addressed to it;
    // an award for such an offer p-tags it as the sole winner). Without this leg the test above would
    // still pass if the scope were dropped for everyone.
    #[test]
    fn award_filter_is_pubkey_scoped_for_targeted() {
        let pk = nostr_sdk::prelude::Keys::generate().public_key();
        let json = serde_json::to_value(award_filter(pk, false)).expect("serialize targeted filter");
        assert!(
            json.get("#p").is_some(),
            "targeted award filter must keep the pubkey scope: {json}"
        );
    }

    #[test]
    fn reject_reader_gate_refuses_non_awarding_author_and_live_filter_includes_3407() {
        assert!(reject_author_gate("buyer", Some("buyer")), "the buyer recorded on the award may author a rejection");
        assert!(!reject_author_gate("attacker", Some("buyer")), "a non-awarding author is void even when the relay delivered kind 3407");
        assert!(!reject_author_gate("buyer", None), "a rejection with no joined award is void");
        let pk = nostr_sdk::prelude::Keys::generate().public_key();
        let json = serde_json::to_value(award_filter(pk, true)).expect("serialize live seller filter");
        let kinds = json.get("kinds").and_then(serde_json::Value::as_array).expect("live filter has kind list");
        assert!(kinds.iter().any(|kind| kind.as_u64() == Some(JOB_REJECT_KIND.into())), "the live seller-node subscription must actually receive kind 3407: {json}");
    }

    // AWARD AUTHORIZATION (security-critical): only the offer's buyer may drive execute or release.
    #[test]
    fn award_from_non_buyer_is_ignored_even_when_claim_matches() {
        // Author != buyer ⇒ Ignore, regardless of a matching claim id — a third party can neither
        // execute nor release our claim.
        assert_eq!(
            match_award("claim1", Some("claim1"), "attacker", "buyer"),
            AwardMatch::Ignore
        );
    }

    #[test]
    fn award_binds_our_claim_and_releases_on_a_different_one() {
        // Buyer awards OUR published claim ⇒ Execute.
        assert_eq!(match_award("claim1", Some("claim1"), "buyer", "buyer"), AwardMatch::Execute);
        // Buyer awards a DIFFERENT claim ⇒ Release ours (another seller won).
        assert_eq!(match_award("claim2", Some("claim1"), "buyer", "buyer"), AwardMatch::Release);
        // Our claim not yet on the wire ⇒ Ignore (never act on an unpublished claim).
        assert_eq!(match_award("claim1", None, "buyer", "buyer"), AwardMatch::Ignore);
    }

    // ---- #541 terminal-offer cache (relay-derived settlement gate) --------------------------------

    #[test]
    fn terminal_offers_buyer_binds_the_settlement() {
        // The gate skips ONLY on a receipt authored by the offer's OWN buyer. A settlement recorded
        // for buyer B makes the offer terminal for B; a forged receipt authored by anyone else is
        // stored but never satisfies the buyer-bound test — the whole of the anti-grief property.
        let mut t = TerminalOffersInner::new(16, 4);
        t.record("offerX", "buyerB", Suppression::Settled);
        assert!(
            t.suppressed_by("offerX", "buyerB", 0).is_some(),
            "the offer's own buyer settled it ⇒ terminal"
        );
        assert!(
            t.suppressed_by("offerX", "forgerF", 0).is_none(),
            "a receipt author who is not the offer's buyer never matches"
        );
        assert!(
            t.suppressed_by("otherOffer", "buyerB", 0).is_none(),
            "an unrelated offer is not terminal (fail-open on the unknown)"
        );

        // A forger publishing alone cannot make the offer terminal for its real buyer.
        let mut g = TerminalOffersInner::new(16, 4);
        g.record("offerY", "forgerF", Suppression::Settled);
        assert!(
            g.suppressed_by("offerY", "buyerB", 0).is_none(),
            "a forger-only receipt does NOT suppress the real buyer's offer"
        );
    }

    #[test]
    fn terminal_offers_drop_newest_protects_an_established_buyer() {
        // Once the real buyer's author is recorded, a flood of later (forged) authors can NEVER
        // displace it: the per-offer author set is DROP-NEWEST at its cap.
        let mut sticky = TerminalOffersInner::new(16, 2); // authors_cap = 2
        sticky.record("offerX", "buyerB", Suppression::Settled);
        sticky.record("offerX", "f1", Suppression::Settled); // fills the 2-cap
        sticky.record("offerX", "f2", Suppression::Settled); // over cap ⇒ dropped (newest)
        sticky.record("offerX", "f3", Suppression::Settled); // over cap ⇒ dropped
        assert!(
            sticky.suppressed_by("offerX", "buyerB", 0).is_some(),
            "an established real-buyer entry is never evicted by a later flood"
        );

        // The documented residual, asserted as REAL behaviour (never a silent claim of safety): a
        // flood that fills the cap with fakes BEFORE the buyer's receipt blocks the buyer entry, so
        // the offer degrades to PRE-#541 (claimable, wasted slot until lapse) — never a spend.
        let mut flooded = TerminalOffersInner::new(16, 2);
        flooded.record("offerZ", "f1", Suppression::Settled);
        flooded.record("offerZ", "f2", Suppression::Settled); // cap full with fakes
        // over cap ⇒ dropped-newest ⇒ buyer blocked
        flooded.record("offerZ", "buyerB", Suppression::Settled);
        assert!(
            flooded.suppressed_by("offerZ", "buyerB", 0).is_none(),
            "first-flood degrades that offer to pre-fix (fail-open), by design"
        );
    }

    #[test]
    fn terminal_offers_fifo_bounds_the_offer_map() {
        // The offer map is FIFO-bounded: a new offer past the cap evicts the OLDEST, so memory is
        // bounded and an aged-out offer fails open — claimable again.
        let mut t = TerminalOffersInner::new(2, 4); // offers_cap = 2
        t.record("o1", "b1", Suppression::Settled);
        t.record("o2", "b2", Suppression::Settled);
        assert_eq!(t.offer_count(), 2);
        t.record("o3", "b3", Suppression::Settled); // evicts o1 (oldest first-seen)
        assert_eq!(t.offer_count(), 2, "the map never exceeds its cap");
        assert!(
            t.suppressed_by("o1", "b1", 0).is_none(),
            "the oldest offer aged out ⇒ claimable again (fail-open)"
        );
        assert!(
            t.suppressed_by("o2", "b2", 0).is_some() && t.suppressed_by("o3", "b3", 0).is_some(),
            "the two newest offers stay terminal"
        );
    }

    #[test]
    fn terminal_offers_record_is_idempotent() {
        // A replayed receipt (same offer + same author) is a no-op: the offer is tracked once, not
        // once per redelivery, and the replay does not consume the per-offer author cap.
        let mut t = TerminalOffersInner::new(4, 2);
        t.record("offerX", "buyerB", Suppression::Settled);
        t.record("offerX", "buyerB", Suppression::Settled);
        t.record("offerX", "buyerB", Suppression::Settled);
        assert_eq!(t.offer_count(), 1, "the offer is tracked once, not once per redelivery");
        assert!(t.suppressed_by("offerX", "buyerB", 0).is_some());
        // The idempotent replays did not consume the cap: a second DISTINCT author still fits.
        t.record("offerX", "buyerB-2nd-device", Suppression::Settled);
        assert!(
            t.suppressed_by("offerX", "buyerB-2nd-device", 0).is_some(),
            "a distinct author still fits under the cap after idempotent replays"
        );
    }

    // ---- #814 suppression semantics (award/acceptance to another seller) --------------------------

    // The lifetime distinction, which is the whole reason `Suppression` is an enum rather than a bool:
    // a receipt is terminal FOREVER, an award binds only until the offer's own deadline. Both legs
    // asserted — a test that only proves the permanent one would pass with `in_force` hard-coded true.
    #[test]
    fn taken_elsewhere_expires_at_the_offer_deadline_and_settled_never_does() {
        let taken = Suppression::TakenElsewhere { until_unix: 1_000 };
        assert!(taken.in_force(999), "before the offer deadline the award still bars a claim");
        assert!(
            !taken.in_force(1_000),
            "AT the deadline it lapses — fail-open, and the gate's own Lapsed check already refuses \
             a past-deadline offer"
        );
        assert!(!taken.in_force(5_000), "and stays lapsed after it");

        assert!(Suppression::Settled.in_force(0), "a settlement is terminal from the moment it lands");
        assert!(
            Suppression::Settled.in_force(u64::MAX),
            "…and never expires: #541's terminality must not be weakened by #814's expiry"
        );
    }

    // `combine` is the guard on requirement (3): a later receipt UPGRADES an award-suppression to
    // terminal, and a later award NEVER weakens a receipt back to something that expires. Asserted in
    // BOTH orders, because a `combine` that simply returned its second argument would pass one of them.
    #[test]
    fn combine_upgrades_to_settled_and_never_weakens_back() {
        let award = Suppression::TakenElsewhere { until_unix: 1_000 };
        assert_eq!(
            award.combine(Suppression::Settled),
            Suppression::Settled,
            "a receipt arriving after an award upgrades the offer to terminal"
        );
        assert_eq!(
            Suppression::Settled.combine(award),
            Suppression::Settled,
            "an award arriving after a receipt must NOT downgrade it to an expiring suppression"
        );
        assert_eq!(
            award.combine(Suppression::TakenElsewhere { until_unix: 2_000 }),
            Suppression::TakenElsewhere { until_unix: 2_000 },
            "two awards keep the LATER expiry"
        );
        assert_eq!(
            Suppression::TakenElsewhere { until_unix: 2_000 }.combine(award),
            Suppression::TakenElsewhere { until_unix: 2_000 },
            "…in either order — the later expiry wins, not the last writer"
        );
    }

    // The buyer-binding that makes a FORGED award inert, and the expiry fail-open, both through the
    // real cache rather than the pure enum.
    #[test]
    fn taken_elsewhere_is_buyer_bound_and_fails_open_once_expired() {
        let mut t = TerminalOffersInner::new(16, 4);
        t.record("offerX", "buyerB", Suppression::TakenElsewhere { until_unix: 1_000 });
        assert!(
            t.suppressed_by("offerX", "buyerB", 500).is_some(),
            "the offer's own buyer decided it ⇒ suppressed"
        );
        assert!(
            t.suppressed_by("offerX", "forgerF", 500).is_none(),
            "a forged award (author != the offer's buyer) is stored but never suppresses"
        );
        assert!(
            t.suppressed_by("offerX", "buyerB", 1_500).is_none(),
            "past the offer's deadline the suppression lapses — an unknown state never suppresses"
        );
    }

    // #814 §5.4: the operator line must distinguish "another seller won this" from "this was paid and
    // closed". One string covering both is what makes an incident unreadable after the fact.
    #[test]
    fn skip_reason_names_taken_elsewhere_apart_from_settled() {
        let settled = Suppression::Settled.skip_reason();
        let taken = Suppression::TakenElsewhere { until_unix: 1 }.skip_reason();
        assert_eq!(settled, SkipReason::Settled);
        assert_eq!(taken, SkipReason::TakenElsewhere);
        assert_ne!(
            settled.reason(),
            taken.reason(),
            "the two states must not collapse to one operator string"
        );
        assert!(
            taken.reason().contains("another seller"),
            "the awarded-elsewhere line must say another seller won it: {}",
            taken.reason()
        );
        assert!(
            settled.reason().contains("settled"),
            "the settled line must keep naming settlement: {}",
            settled.reason()
        );
    }

    // Untargeted offers are refused unless open-pool is opted in; with it, they claim.
    #[test]
    fn untargeted_needs_open_pool_opt_in() {
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &seller_cfg(2, false), &claude_only(), SELLER, BUYER, NOW, NOW),
            ClaimDecision::Skip(SkipReason::RateGate)
        );
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &seller_cfg(2, true), &claude_only(), SELLER, BUYER, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 }
        );
    }

    // TOOTH (charter invariant 3) — a node that cannot run the requested harness never CLAIMS.
    // The refusal is a decision over the offer, not an outcome discovered at delivery: the offer
    // stays available to a seller that can serve it, instead of being answered by one that would
    // then fail. Bite: drop the `agents.serves(...)` arm from `classify_offer` and the codex offer
    // below is claimed by a claude-only node.
    #[test]
    fn a_node_without_the_requested_harness_never_claims() {
        let mut wants_codex = offer(5, Some(SELLER), NOW + 600);
        wants_codex.requested_agent = Some("codex".to_owned());
        assert_eq!(
            classify_offer(&wants_codex, &seller_cfg(2, false), &claude_only(), SELLER, BUYER, NOW, NOW),
            ClaimDecision::Skip(SkipReason::AgentUnavailable)
        );

        // The same offer at a node that DOES run codex is claimed — the gate is the harness, not
        // the presence of a request.
        let both = LiveRoster::new(AgentRegistry::new(vec![
            crate::seller_agents::RegisteredAgent {
                name: Some("claude".to_owned()),
                argv: vec!["claude-agent-acp".to_owned()],
            },
            crate::seller_agents::RegisteredAgent {
                name: Some("codex".to_owned()),
                argv: vec!["codex-acp".to_owned()],
            },
        ]));
        assert_eq!(
            classify_offer(&wants_codex, &seller_cfg(2, false), &both, SELLER, BUYER, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 }
        );

        // And an offer asking for nothing is claimed by the claude-only node exactly as before.
        assert_eq!(
            classify_offer(&offer(5, Some(SELLER), NOW + 600), &seller_cfg(2, false), &claude_only(), SELLER, BUYER, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 }
        );
    }

    // TOOTH (#254) — a harness that FAILED stops the node claiming for it, not merely fails the next
    // job. Boot verified the harness launches; launching is not delivering, and a node that keeps
    // claiming for a harness it cannot deliver with is a black hole for awards — under
    // award-is-payment the buyer's sats are committed before the failure is visible.
    // Bite: drop the availability filter from `LiveRoster::dispatch` and the SAME offer that was
    // refused below is claimed again, by a node that just proved it cannot serve it.
    #[test]
    fn a_dropped_harness_stops_the_node_claiming_for_it() {
        let roster = LiveRoster::new(AgentRegistry::new(vec![
            crate::seller_agents::RegisteredAgent {
                name: Some("claude".to_owned()),
                argv: vec!["claude-agent-acp".to_owned()],
            },
        ]));
        let untargeted = offer(5, Some(SELLER), NOW + 600);

        // Precondition — the SAME offer is claimable before the drop, so the assertion below cannot
        // pass for some unrelated reason.
        assert_eq!(
            classify_offer(&untargeted, &seller_cfg(2, false), &roster, SELLER, BUYER, NOW, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 },
            "the offer must be claimable first, or the drop below proves nothing"
        );

        roster.fault(0, Fault::Unproven, std::time::Instant::now());

        assert_eq!(
            classify_offer(&untargeted, &seller_cfg(2, false), &roster, SELLER, BUYER, NOW, NOW),
            ClaimDecision::Skip(SkipReason::AgentUnavailable),
            "a node whose only harness is dropped must stop claiming"
        );
        assert!(
            roster.advertised().is_empty(),
            "and it must stop advertising in the same motion — the wire and the dispatch table are \
             one set, so a buyer can never read a harness this node would refuse"
        );
    }

    // TOOTH (#254) — the self-probe is decided by the ARTIFACT, and nothing else it could be decided
    // by would work. A harness whose account is exhausted ends its turn `completed`, exits 0, and
    // returns a non-empty message telling you to upgrade your plan: exit status, turn state and
    // response length are all GREEN for precisely the harness this exists to catch. Only a sentinel it
    // cannot produce goes red.
    // Bite: decide the probe on turn completion, or on the workdir merely being non-empty, and the
    // billing-notice case below passes — restoring a harness that cannot do any work to the roster,
    // where it will rank as the FASTEST seat on it.
    #[test]
    fn the_self_probe_is_decided_by_the_sentinel_not_by_a_completed_turn() {
        // Deliberately NOT sharing the sentinel prefix: a grep for it must return the ONE definition.
        let dir = std::env::temp_dir().join(format!("maxplayer-selfprobe-td-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("probe dir");
        // Minted through the SAME function the probe path uses, so this test cannot pass against a
        // sentinel shape the node would never actually produce.
        let sentinel = mint_probe_identity(0, 0, 1_785_400_000).sentinel;

        // Nothing written at all — a turn that completed having done nothing.
        assert!(
            !probe_sentinel_present(&dir, &sentinel),
            "an empty workdir is not a delivered artifact"
        );

        // The quota-dead case: a completed turn whose ONLY output is a billing notice. Non-empty,
        // plausible, and worthless.
        std::fs::write(dir.join("probe.txt"), "Upgrade your plan to continue")
            .expect("write notice");
        assert!(
            !probe_sentinel_present(&dir, &sentinel),
            "a non-empty file lacking the sentinel must NOT pass — this is the whole failure mode"
        );

        // The working case. Content, not filename: a harness that put the sentinel somewhere sensible
        // has shown the capability, and failing it over a filename would report it broken.
        std::fs::write(dir.join("notes.md"), format!("done: {sentinel}\n")).expect("write sentinel");
        assert!(
            probe_sentinel_present(&dir, &sentinel),
            "the sentinel present in any file is the capability being tested"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // A probe sentinel must never be confusable with ecash. `token` in this crate means Cashu money
    // (`payload.to_token()`, `token_hash`, "already-spent token" — same file), so the sentinel carries
    // its own prefix from ONE definition shared by the mint site and the readback.
    // Bite: reintroduce a second literal for the prefix and the mint and the readback can drift apart
    // silently — which is why the assertion runs through the shared function.
    #[test]
    fn a_probe_sentinel_is_minted_from_one_definition_and_is_not_ecash() {
        let a = mint_probe_identity(0, 0, 1_785_400_000);
        let b = mint_probe_identity(0, 0, 1_785_400_001);
        let other_harness = mint_probe_identity(1, 0, 1_785_400_000);
        let other_attempt = mint_probe_identity(0, 1, 1_785_400_000);

        assert!(a.sentinel.starts_with(PROBE_SENTINEL_PREFIX), "{}", a.sentinel);
        assert_ne!(
            a.sentinel, b.sentinel,
            "a sentinel is per-probe, so a replay cannot satisfy a later one"
        );
        assert_ne!(a.sentinel, other_harness.sentinel, "and it is per-harness");
        // Per-attempt too (#472): a retry gets its own sentinel AND its own workdir, so no attempt can
        // inherit an earlier one's artifact even inside the same second.
        assert_ne!(
            a.sentinel, other_attempt.sentinel,
            "a retry's sentinel must differ from the first attempt's"
        );
        assert_ne!(
            a.dir_label, other_attempt.dir_label,
            "a retry must not reuse an earlier attempt's workdir"
        );

        // The readback accepts exactly what the mint produced — one definition, both ends.
        let dir = std::env::temp_dir().join(format!("maxplayer-sn-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("probe.txt"), &a.sentinel).expect("write");
        assert!(probe_sentinel_present(&dir, &a.sentinel));
        assert!(
            !probe_sentinel_present(&dir, &b.sentinel),
            "a DIFFERENT probe's sentinel must not be satisfied by this artifact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // TOOTH (#254, review finding R1) — a harness must not be able to pass its probe by ECHOING ITS
    // OWN CWD. The workdir path is the one string a harness always has without doing any work, so if
    // the sentinel lives in that path, an error trace or a log header that mentions the cwd satisfies
    // the probe and a dead harness is restored to the roster as "serving".
    // ⇒ A discriminator must not appear in the environment it discriminates.
    // Bite: key the workdir off the sentinel again (`job_workdir(home, &sentinel)`) and the first
    // assertion below still holds, but the second fails — a file containing nothing but the cwd path
    // passes a probe the harness did no work for.
    #[test]
    fn a_harness_cannot_pass_its_probe_by_echoing_its_own_workdir_path() {
        let probe = mint_probe_identity(3, 0, 1_785_400_000);

        // ① The mint keeps them disjoint: no workdir named after the label can contain the sentinel.
        assert!(
            !probe.dir_label.contains(&probe.sentinel),
            "the workdir label must never carry the sentinel: label={} sentinel={}",
            probe.dir_label,
            probe.sentinel
        );

        // ② And the readback refuses a path echo even when the path DOES carry the sentinel — the
        // belt to ①'s braces, so a future caller reintroducing the leak cannot revive a dead harness.
        let leaky = std::env::temp_dir()
            .join(format!("maxplayer-leak-{}-{}", std::process::id(), probe.sentinel));
        std::fs::create_dir_all(&leaky).expect("leaky dir");
        std::fs::write(
            leaky.join("trace.log"),
            format!("error: could not write to {}\n", leaky.display()),
        )
        .expect("write trace");
        assert!(
            !probe_sentinel_present(&leaky, &probe.sentinel),
            "a file containing only the cwd path must NOT pass — the harness did no task work"
        );

        // ③ Positive control: the SAME leaky workdir passes once the harness actually writes it.
        std::fs::write(leaky.join("probe.txt"), &probe.sentinel).expect("write sentinel");
        assert!(
            probe_sentinel_present(&leaky, &probe.sentinel),
            "real work in the same workdir must still pass, or ② is just breaking the predicate"
        );

        let _ = std::fs::remove_dir_all(&leaky);
    }

    // TOOTH (#254) — only failures ATTRIBUTABLE to the harness narrow the roster. Attribution, not
    // severity, is the test: an execution failure caused by a remote, our own signer, or our own
    // policy refusal says nothing about whether the harness works.
    // Bite: map every `ExecError` to a drop and a node whose git remote is briefly unreachable, or
    // whose delivery oid we ourselves declined to type, takes its own harness out of service — an
    // outage the node inflicts on itself, and one that looks exactly like a real harness failure.
    #[test]
    fn only_harness_attributable_failures_narrow_the_roster() {
        // OUR refusal is never the harness's fault.
        assert_eq!(
            harness_fault_for(&ExecError::Policy("un-typeable delivery oid".into())),
            None,
            "a policy WE refused must never drop the harness"
        );

        // A missing build feature is structural and NAMED — no probe can supply it.
        assert_eq!(
            harness_fault_for(&ExecError::AcpRequired),
            Some(ExecutionFailure::Harness(Fault::Incapable(
                MissingCapability::AcpFeature
            )))
        );

        // An untyped agent failure is deliberately UNPROVEN: a timeout and a provider that will
        // never resolve arrive here identically, so the probe decides rather than this classifier.
        assert_eq!(
            harness_fault_for(&ExecError::Agent("turn ended non-terminal".into())),
            Some(ExecutionFailure::Harness(Fault::Unproven))
        );

        // The deadline-derived response timer is already attributed before this seam. It reaches
        // the roster as a typed non-striking failure, never as message text to re-parse.
        assert_eq!(
            harness_fault_for(&ExecError::DeadlineExceeded),
            Some(ExecutionFailure::DeadlineExceeded)
        );
        assert!(
            ExecError::DeadlineExceeded
                .to_string()
                .contains("job deadline reached"),
            "the operator line must name the job clock, not an ACP request timeout"
        );

        // A config barrier is structural too, but its remedy is DERIVED — reporting "rebuild" for a
        // harness whose provider was never selected would send the operator after the wrong thing.
        let config = harness_fault_for(&ExecError::Config("GOOSE_PROVIDER is unset".into()))
            .expect("a config barrier implicates the harness");
        match config {
            ExecutionFailure::Harness(Fault::Incapable(capability)) => {
                let remedy = capability.remedy();
                assert!(remedy.contains("GOOSE_PROVIDER"), "{remedy}");
                assert!(
                    !remedy.contains("--features acp"),
                    "a configuration barrier must not be reported as a rebuild: {remedy}"
                );
            }
            other => panic!("a config barrier is structural, got {other:?}"),
        }
    }

    // TOOTH (the seam my other teeth do not look at) — the harness request survives the trip from
    // WIRE EVENT to STORED ROW.
    //
    // Every other tooth here either builds the `Offer` row by hand or reads one back, so all of
    // them stay green if this mapping silently drops the field — invariant 2 would then be built,
    // green, and dead the moment execution happened after a restart. This one starts from an
    // offer draft, parses it the way the claim path does, and asserts the row carries the request.
    // Bite (measured): replace `requested_agent` in `offer_row` with `None` — before this tooth
    // existed the whole suite stayed green; with it, this test and only this test goes red.
    #[test]
    fn the_harness_request_survives_the_wire_to_row_mapping() {
        let asked = gateway::OfferDraft::new("do a task", "text/plain", 5, NOW + 600, "a".repeat(64))
            .requesting_agent(Some("codex"))
            .to_event_draft();
        let parsed = parse_offer(&asked).expect("parse offer");
        let row = offer_row("job-1", "buyer-1", &parsed);
        assert_eq!(
            row.requested_agent.as_deref(),
            Some("codex"),
            "the request must reach the row — everything downstream reads the ROW, not the event"
        );
        // The rest of the mapping is asserted alongside it, so a field dropped here is caught too.
        assert_eq!(row.amount_sats, 5);
        assert_eq!(row.unit, "sat");
        assert_eq!(row.task, "do a task");
        assert_eq!(row.deadline_unix, (NOW + 600) as i64);
        assert!(row.targeted);
        assert_eq!(row.output.as_deref(), Some("text/plain"));

        // An offer that asked for nothing stores nothing — absence is carried, not invented.
        let plain = gateway::OfferDraft::new("do a task", "text/plain", 5, NOW + 600, "a".repeat(64))
            .to_event_draft();
        let parsed = parse_offer(&plain).expect("parse offer");
        assert_eq!(offer_row("job-2", "buyer-1", &parsed).requested_agent, None);
    }

    // TOOTH (#686) — the buyer's DECLARED OUTPUT TYPE survives the whole trip: WIRE EVENT → stored
    // row → (restart) → the agent's prompt.
    //
    // This is the seam the issue is about. The `output` tag is mandatory on ingest, so every offer
    // carries one, but it used to stop at the parsed offer: the hired agent was never told what form
    // the buyer asked for. The store is reopened between the write and the read because execution can
    // be a RESTART away from the claim — an unpersisted field would be gone for that job permanently,
    // exactly as the store's `requested_agent` comment says.
    //
    // Bites (each measured, one at a time — every one turns THIS test red and nothing else in the
    // file): (1) `output: None` in `offer_row`; (2) drop `output` from `record_offer`'s INSERT;
    // (3) pass `None` for the declared output in `job_prompt`.
    #[test]
    fn the_declared_output_type_survives_wire_to_row_to_prompt_across_a_restart() {
        let job = "c".repeat(64);
        let asked =
            gateway::OfferDraft::new("do a task", "application/json", 5, NOW + 600, "a".repeat(64))
                .to_event_draft();
        let parsed = parse_offer(&asked).expect("parse offer");
        let row = offer_row(&job, "buyer-1", &parsed);
        assert_eq!(
            row.output.as_deref(),
            Some("application/json"),
            "the declared output type must reach the row — the prompt is composed from the ROW"
        );

        let root = temp_dir("declared-output-restart");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        let db = root.join("seller.sqlite");
        {
            let store = SellerStore::open(&db).expect("open store");
            store.record_offer(&row, 1).expect("record offer");
        }
        // …the process dies here. A fresh store handle is all the resumed node has.
        let store = SellerStore::open(&db).expect("reopen store");
        let resumed = store.offer_row(&job).expect("offer row").expect("offer survives");

        // The exact call the execute path makes, over the exact row a resumed job reads.
        let prompt = job_prompt(&resumed, "https://relay.example/git/abc.git", 2_000_000_000, None);
        assert!(
            prompt.contains("application/json"),
            "the buyer's declared output type must reach the hired agent: {prompt}"
        );
        assert!(
            prompt.contains("DECLARED OUTPUT TYPE:"),
            "stated as the buyer's declared output type: {prompt}"
        );
        // A VALUE, not fixed prose: a different declared type produces a different prompt, and never
        // the other job's type.
        let mut other = resumed.clone();
        other.output = Some("text/plain".to_owned());
        let other_prompt = job_prompt(&other, "https://relay.example/git/abc.git", 2_000_000_000, None);
        assert!(other_prompt.contains("text/plain"), "{other_prompt}");
        assert!(!other_prompt.contains("application/json"), "{other_prompt}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (charter invariant 2, RESTART form — the strong one) — a job requesting harness X is
    // dispatched to X even when the process that claimed it is gone. The request is journaled with
    // the offer facts, so the resumed execute path reads it from the STORE; the registry below
    // deliberately PREFERS claude, so a regression that dispatches the preferred harness (or that
    // re-reads live config) runs claude and goes red.
    #[test]
    fn a_resumed_job_still_dispatches_to_the_harness_it_requested() {
        let job = "a".repeat(64);
        let buyer = "b".repeat(64);
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &job,
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let root = temp_dir("restart-dispatch");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        let db = root.join("seller.sqlite");

        // Claim time: the offer asks for codex, and its facts are journaled with the claim.
        {
            let store = SellerStore::open(&db).expect("open store");
            store
                .record_offer(
                    &Offer {
                        offer_id: job.clone(),
                        buyer_pubkey: buyer.clone(),
                        amount_sats: 21,
                        unit: "sat".to_owned(),
                        task: "build a widget".to_owned(),
                        deadline_unix: 2_000_000_000,
                        targeted: true,
                        requested_agent: Some("codex".to_owned()),
                        output: Some("text/plain".to_owned()),
                    },
                    1,
                )
                .expect("record offer");
            let draft = claim_draft(&job, &buyer, &seller, &creq, &["codex".to_owned()], &Default::default());
            store
                .claim_and_enqueue(&job, &job, &creq, &draft, 1, 9_999_999_999, 1)
                .expect("claim");
        }

        // …the process dies here. A fresh store handle is all the resumed node has.
        let store = SellerStore::open(&db).expect("reopen store");
        let resumed = store.offer_row(&job).expect("offer row").expect("offer survives");
        assert_eq!(resumed.requested_agent.as_deref(), Some("codex"));

        let registry = AgentRegistry::new(vec![
            crate::seller_agents::RegisteredAgent {
                name: Some("claude".to_owned()),
                argv: vec!["claude-agent-acp".to_owned()],
            },
            crate::seller_agents::RegisteredAgent {
                name: Some("codex".to_owned()),
                argv: vec!["codex-acp".to_owned()],
            },
        ]);
        let dispatched = registry
            .dispatch(resumed.requested_agent.as_deref())
            .expect("the requested harness is available");
        assert_eq!(dispatched.name.as_deref(), Some("codex"));
        assert_eq!(dispatched.argv, vec!["codex-acp"], "the RUN command is codex's, not the preferred harness's");

        // And the journal names what ran it.
        store
            .record_award(&"w".repeat(64), &job, &buyer, 4242)
            .expect("award");
        store
            .assign_agent(&job, dispatched.name.as_deref().expect("label"))
            .expect("journal the harness");
        assert_eq!(store.job_agent(&job).expect("job agent"), Some("codex".to_owned()));

        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (#146 / #117 refusal taxonomy) — a cross-version offer is a DISTINCT refusal, not the
    // generic "unparseable" bucket. Build a well-formed offer, then swap ONLY its `v` tag so the sole
    // parse failure is version skew; the node's on_offer routes that to the unsupported-version skip.
    #[test]
    fn unsupported_version_offer_is_a_distinct_parse_refusal() {
        let offer = gateway::OfferDraft::new("do a task", "text/plain", 5, NOW + 600, "a".repeat(64));
        let mut draft = offer.to_event_draft();
        for tag in &mut draft.tags {
            if tag.0.first().map(String::as_str) == Some("v") {
                tag.0 = vec!["v".to_owned(), "99".to_owned()];
            }
        }
        let skew = parse_offer(&draft).expect_err("version skew must not parse");
        assert!(
            matches!(&skew, OfferParseError::UnsupportedVersion(v) if v == "99"),
            "version skew must parse as a distinct UnsupportedVersion, not generic unparseable"
        );

        // The ROUTING is the thing under test: pinning `parse_offer`'s enum alone let a revert that
        // collapsed on_offer's version arm into the generic bucket pass green. Assert on the refusal
        // on_offer actually emits, and that a genuinely malformed offer emits a DIFFERENT one.
        let mut malformed = draft.clone();
        malformed.tags.clear();
        let broken = parse_offer(&malformed).expect_err("a tagless offer must not parse");

        let skew_reason = offer_parse_refusal(&skew);
        let broken_reason = offer_parse_refusal(&broken);
        assert!(
            skew_reason.contains("unsupported maxplayer protocol version") && skew_reason.contains("99"),
            "the version-skew refusal must say so and name the version, got {skew_reason:?}"
        );
        assert!(
            broken_reason.contains("unparseable"),
            "a malformed offer stays in the generic bucket, got {broken_reason:?}"
        );
        assert_ne!(
            skew_reason, broken_reason,
            "collapsing version skew into the generic unparseable bucket is the #146 regression"
        );
    }

    // TOOTH (#171 layer 2 / #172) — the offer REQ carries the un-pinned open-pool filter IFF the
    // seller opted in, and BOTH filters carry the `#t=maxplayer` namespace guard. The node subscribed
    // targeted-only unconditionally, so a `claim_open_pool = true` seller ran a claim gate over
    // offers its subscription could never deliver. Bite: drop the `claim_open_pool` branch and the
    // two-filter assertions go red; drop the hashtag and the guard assertions go red.
    #[test]
    fn open_pool_filter_rides_the_offer_req_iff_opted_in() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key();
        let now = nostr_sdk::Timestamp::from(NOW);

        let targeted_only = offer_subscription_filters(seller, false, 1200, None, now);
        assert_eq!(
            targeted_only.len(),
            1,
            "a targeted-only seller subscribes exactly the pinned filter"
        );
        assert_eq!(
            targeted_only[0].generic_tags.get(&nostr_sdk::SingleLetterTag::lowercase(
                nostr_sdk::Alphabet::P
            )),
            Some(&[seller.to_hex()].into_iter().collect()),
            "the targeted filter must stay pinned to this seller"
        );

        let open_pool = offer_subscription_filters(seller, true, 1200, None, now);
        assert_eq!(
            open_pool.len(),
            2,
            "an open-pool seller must ALSO subscribe the un-pinned filter — without it the \
             claim_open_pool gate governs offers that never arrive"
        );
        assert!(
            open_pool[1]
                .generic_tags
                .get(&nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::P))
                .is_none(),
            "the open-pool filter is un-pinned by definition"
        );

        // The namespace guard rides BOTH filters: a foreign event squatting the offer kind is never
        // even delivered.
        let hashtag = nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::T);
        for (index, filter) in open_pool.iter().enumerate() {
            assert_eq!(
                filter.generic_tags.get(&hashtag),
                Some(&[crate::gateway::MAXPLAYER_TAG.to_owned()].into_iter().collect()),
                "offer filter {index} must carry the #t=maxplayer namespace guard"
            );
        }

        // `offer_backfill_secs = 0` is live-only: `since(now)` + `limit(0)` requests zero stored
        // offers. A window asks for a bounded stored burst instead.
        let live_only = offer_subscription_filters(seller, true, 0, None, now);
        assert_eq!(live_only[1].limit, Some(0), "live-only requests no stored offers");
        assert_eq!(live_only[1].since, Some(now));
        let windowed = offer_subscription_filters(seller, true, 1200, None, now);
        assert_eq!(windowed[1].limit, Some(OFFER_BACKFILL_LIMIT));
        assert_eq!(windowed[1].since, Some(nostr_sdk::Timestamp::from(NOW - 1200)));

        // On a post-stall resubscribe BOTH filters carry the overlap cursor — only the stall gap is
        // missing, and the classify-level deadline refusal is the staleness guard.
        let overlap = nostr_sdk::Timestamp::from(NOW - 60);
        let resubscribed = offer_subscription_filters(seller, true, 1200, Some(overlap), now);
        for filter in &resubscribed {
            assert_eq!(filter.since, Some(overlap));
        }
    }

    // TOOTH (#171 layer 1, THE fix) — an in-process reconnect re-authenticates and the receive path
    // comes BACK, against a fixture that enforces NIP-42 before it will serve a REQ.
    //
    // The fixture parity matters: the previous watchdog teeth ran against a LocalRelay that served
    // reads unauthenticated, so the auth step was decorative and the ordering bug shipped green. Here
    // `RelayBuilderNip42Mode::Both` refuses a REQ from an unauthenticated session, so an event can
    // only arrive if auth genuinely completed on the new socket.
    //
    // The assertion is DELIVERY, not a return code: a live socket happily coexists with dead
    // subscriptions (that is exactly what wedged the field nodes — heartbeating, deaf), so a tooth
    // that only checked `Ok(..)` would be the same false green.
    //
    // BITE: swap the two lines in `reconnect_and_authenticate` so `relay.notifications()` is taken
    // before `client.disconnect()`, and this goes red — the auth wait reads our own Shutdown and
    // returns "relay shutdown before NIP-42 authentication".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_reauthenticates_and_delivery_resumes_in_process() {
        use nostr_relay_builder::prelude::{
            LocalRelay, RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode,
        };
        use nostr_sdk::prelude::{Client, EventBuilder, Keys, RelayOptions, RelayUrl};

        let wait = std::time::Duration::from_secs(10);

        // Auth-enforcing fixture: it will not serve a REQ (nor accept an EVENT) until the session
        // has completed NIP-42.
        let relay_fixture = LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42 {
            mode: RelayBuilderNip42Mode::Both,
        }));
        relay_fixture.run().await.expect("fixture relay run");
        let relay_url = relay_fixture.url().await.to_string();

        let seller = Keys::generate();
        let client = Client::new(seller.clone());
        client.automatic_authentication(true);
        client
            .pool()
            .add_relay(&relay_url, RelayOptions::default().reconnect(true))
            .await
            .expect("add relay");
        let relay = client
            .relays()
            .await
            .get(&RelayUrl::parse(&relay_url).expect("relay url"))
            .cloned()
            .expect("relay handle");

        // The beacon is published by a SEPARATE client, which is both the faithful shape (the node's
        // receive path carries events from OTHERS — offers, awards, payment wraps) and a hard
        // requirement: `RelayPool::send_event_to` saves every published event into the publishing
        // client's own database (pool/mod.rs:767), and the inbound handler drops any event already
        // present there without notifying (relay/inner.rs:1215-1218). A client therefore cannot
        // observe its own event coming back — using this client to publish would test nothing.
        let author = Keys::generate();
        let author_pubkey = author.public_key();
        let publisher = Client::new(author.clone());
        publisher.automatic_authentication(true);
        publisher
            .pool()
            .add_relay(&relay_url, RelayOptions::default())
            .await
            .expect("add relay (publisher)");
        let publisher_relay = publisher
            .relays()
            .await
            .get(&RelayUrl::parse(&relay_url).expect("relay url"))
            .cloned()
            .expect("publisher relay handle");
        let mut publisher_notifications = publisher_relay.notifications();
        publisher.connect().await;
        publisher.wait_for_connection(wait).await;
        // Writes are auth-gated too, and this fixture challenges on the first REQ — so probe once to
        // get the publisher authenticated before it tries to publish anything.
        publisher
            .subscribe(Filter::new().kind(Kind::TextNote).limit(0), None)
            .await
            .expect("publisher probe subscribe");
        relay_auth::wait_for_nip42_auth(&mut publisher_notifications, wait)
            .await
            .expect("publisher auth");

        let beacon_filter = Filter::new().kind(Kind::TextNote).author(author_pubkey);
        let beacon = |content: &str| {
            EventBuilder::new(Kind::TextNote, content)
                .sign_with_keys(&author)
                .expect("sign beacon")
        };
        // Await one specific event on a pool receiver. Returns false on timeout — the failure this
        // whole tooth exists to catch is silence.
        async fn arrives(
            notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
            id: nostr_sdk::EventId,
            wait: std::time::Duration,
        ) -> bool {
            tokio::time::timeout(wait, async {
                loop {
                    match notifications.recv().await {
                        Ok(RelayPoolNotification::Event { event, .. }) if event.id == id => {
                            return true
                        }
                        Ok(_) => continue,
                        Err(_) => return false,
                    }
                }
            })
            .await
            .unwrap_or(false)
        }
        // EOSE is the relay confirming our REQ is registered. Waiting for it makes the test
        // deterministic: publishing before the subscription lands would race, and a race here would
        // read as the very silence the tooth is meant to detect.
        async fn subscription_live(
            notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
            wait: std::time::Duration,
        ) -> bool {
            tokio::time::timeout(wait, async {
                loop {
                    match notifications.recv().await {
                        Ok(RelayPoolNotification::Message {
                            message: nostr_sdk::RelayMessage::EndOfStoredEvents(_),
                            ..
                        }) => return true,
                        Ok(_) => continue,
                        Err(_) => return false,
                    }
                }
            })
            .await
            .unwrap_or(false)
        }

        // Boot the way the node does: receiver before connect, then subscribe. This fixture
        // challenges lazily (on the first REQ) where the deployed relay challenges on connect;
        // either way auto-auth answers it and `Authenticated` is what the node waits for.
        let mut boot_notifications = relay.notifications();
        client.connect().await;
        client.wait_for_connection(wait).await;
        client
            .subscribe(beacon_filter.clone(), None)
            .await
            .expect("boot subscribe");
        assert_eq!(
            relay_auth::wait_for_nip42_auth(&mut boot_notifications, wait)
                .await
                .expect("boot auth"),
            AuthWait::Authenticated,
            "the fixture must actually enforce NIP-42 — if it never challenges, this whole tooth \
             is decorative (which is how the ordering bug shipped green)"
        );

        // Baseline: delivery works BEFORE the reconnect, so a post-reconnect silence is the code's
        // fault and not the harness's. Re-subscribe post-auth exactly as the recovery path does —
        // the boot REQ was refused pre-auth — and wait for the relay to confirm it.
        let mut notifications = client.notifications();
        client.unsubscribe_all().await;
        client
            .subscribe(beacon_filter.clone(), None)
            .await
            .expect("post-auth subscribe");
        assert!(
            subscription_live(&mut notifications, wait).await,
            "harness check: the relay must confirm (EOSE) the post-auth subscription"
        );
        let before = beacon("pre-reconnect baseline");
        publisher
            .send_event(&before)
            .await
            .expect("publish baseline");
        assert!(
            arrives(&mut notifications, before.id, wait).await,
            "harness check: the subscription must deliver before we induce the reconnect"
        );

        // THE PRODUCTION PATH under test: an in-process reconnect, no process restart.
        let outcome = reconnect_and_authenticate(&client, &relay)
            .await
            .expect("in-process reconnect must re-authenticate — this is #171");
        assert_eq!(
            outcome,
            AuthWait::Authenticated,
            "the reconnect must complete NIP-42 on the NEW socket, not report a shutdown it caused"
        );

        // What the recovery path does next: replace the stale subscriptions AFTER auth.
        let mut post = client.notifications();
        client.unsubscribe_all().await;
        client
            .subscribe(beacon_filter, None)
            .await
            .expect("post-reconnect subscribe");
        assert!(
            subscription_live(&mut post, wait).await,
            "the relay must serve the post-reconnect REQ — on this fixture that is only possible \
             on an authenticated session"
        );

        let after = beacon("post-reconnect liveness beacon");
        publisher.send_event(&after).await.expect("publish beacon");
        assert!(
            arrives(&mut post, after.id, wait).await,
            "the receive path must be ALIVE after an in-process reconnect — a recovery that \
             returns Ok while nothing is delivered is the silent wedge this fixes"
        );

        client.disconnect().await;
        publisher.disconnect().await;
    }

    // TOOTH (#171 TRIGGER) — the liveness probe asserts the property the watchdog actually needs:
    // "the relay is serving MY REQs on THIS authenticated session". Both halves, because a probe that
    // can only ever answer one way is exactly the bug being fixed — the own-heartbeat round-trip it
    // replaced could NEVER succeed (nostr-sdk saves published events into the client's own database
    // and then swallows the relay's echo of them), so every node declared a stall every
    // `stall_threshold` forever, healthy or not, and drove a recovery that could not succeed either.
    //
    // BITE (positive half): break the probe — wrong sub id in the EOSE match, or drop the `limit(0)`
    // REQ — and the authenticated case goes red.
    // BITE (negative half): make the probe return true on timeout, or accept any EOSE regardless of
    // session, and the unauthenticated case goes red. That half is what pins "on THIS session":
    // against this fixture an unauthenticated REQ is answered with CLOSED and never an EOSE.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn liveness_probe_answers_only_on_an_authenticated_session() {
        use nostr_relay_builder::prelude::{
            LocalRelay, RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode,
        };
        use nostr_sdk::prelude::{Client, Keys, RelayOptions};

        let wait = std::time::Duration::from_secs(10);
        let relay_fixture = LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42 {
            mode: RelayBuilderNip42Mode::Both,
        }));
        relay_fixture.run().await.expect("fixture relay run");
        let relay_url = relay_fixture.url().await.to_string();

        // POSITIVE: an authenticated session. Auto-auth answers the fixture's challenge (raised on the
        // probe's own REQ), the relay serves it, and the EOSE comes back.
        let seller = Keys::generate();
        let authed = Client::new(seller.clone());
        authed.automatic_authentication(true);
        authed
            .pool()
            .add_relay(&relay_url, RelayOptions::default())
            .await
            .expect("add relay (authed)");
        authed.connect().await;
        authed.wait_for_connection(wait).await;
        assert!(
            probe_relay_serves_our_reqs(&authed, seller.public_key(), wait).await,
            "the probe must be answerable on a healthy authenticated session — otherwise the \
             watchdog is back to a signal that can never arrive"
        );

        // NEGATIVE: same relay, same probe, but auto-auth OFF so the session never authenticates. The
        // fixture answers the REQ with CLOSED instead of serving it, so no EOSE ever arrives and the
        // probe must report the loss of liveness rather than assuming it.
        let stranger = Keys::generate();
        let unauthed = Client::new(stranger.clone());
        unauthed.automatic_authentication(false);
        unauthed
            .pool()
            .add_relay(&relay_url, RelayOptions::default())
            .await
            .expect("add relay (unauthed)");
        unauthed.connect().await;
        unauthed.wait_for_connection(wait).await;
        assert!(
            !probe_relay_serves_our_reqs(
                &unauthed,
                stranger.public_key(),
                std::time::Duration::from_secs(2),
            )
            .await,
            "a session the relay refuses to serve is NOT alive — reporting it alive is how the \
             watchdog would go blind in the other direction"
        );

        authed.disconnect().await;
        unauthed.disconnect().await;
    }

    /// Counts REQs the relay is asked to serve for kind-1059, so a test can assert the backfill
    /// actually reached the wire rather than inferring it from a log line.
    #[derive(Debug)]
    struct CountWrapQueries(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl nostr_relay_builder::prelude::QueryPolicy for CountWrapQueries {
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
                    .is_some_and(|kinds| kinds.contains(&Kind::GiftWrap))
                {
                    self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                nostr_relay_builder::prelude::PolicyResult::Accept
            })
        }
    }

    // TOOTH (wrap backfill, UNCONDITIONALITY) — the backfill fetch must reach the relay even when the
    // store has nothing pending, because the empty case is exactly the one that goes quiet.
    //
    // The obvious future optimisation — "skip the fetch when nothing is outstanding" — would silence
    // precisely the healthy idle seats an operator least suspects, putting external supervision back
    // on absence-reasoning (a parked process satisfies pid-presence; see #173). The cursor teeth below
    // guard the log line's CONTENT; this one guards that it happens at all.
    //
    // Asserted at the wire, not in the log: the fixture counts kind-1059 REQs, so a skip-when-empty
    // guard cannot pass by keeping the eprintln and dropping the fetch. It also drives the REAL boot
    // path (`SellerNodeRunner::boot`), so the assertion covers the deployable shape rather than a
    // hand-built runner.
    //
    // BITE: add `if self.node.store().oldest_unsettled_delivery_unix().ok().flatten().is_none() {
    // return; }` at the top of run_wrap_backfill → rc=101 here (verified).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrap_backfill_fetches_even_with_nothing_pending() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let wrap_queries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let relay = LocalRelay::new(
            RelayBuilder::default()
                .query_policy(CountWrapQueries(std::sync::Arc::clone(&wrap_queries))),
        );
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        let root = temp_dir("backfill-empty");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = crate::home::bootstrap(&root).expect("bootstrap");
        home.config.relay_url = relay_url;

        let runner = SellerNodeRunner::boot(home).await.expect("boot node");
        // Baseline AFTER boot: boot's own subscriptions include the live 1059 REQ, so only the delta
        // across the backfill call is evidence.
        let before = wrap_queries.load(std::sync::atomic::Ordering::SeqCst);

        // A pristine home: no deliveries, no receipts, nothing outstanding whatsoever.
        assert_eq!(
            runner.node.store().oldest_unsettled_delivery_unix().expect("unsettled"),
            None,
            "fixture check: the store must be empty for this to be the nothing-pending case"
        );

        runner.run_wrap_backfill().await;

        assert!(
            wrap_queries.load(std::sync::atomic::Ordering::SeqCst) > before,
            "the backfill must re-ask the relay for stored kind-1059(s) even with nothing pending — \
             skipping the fetch when the store looks idle silences the only periodic signal a healthy \
             seat emits"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Counts REQs the relay is asked to serve for [`JOB_OFFER_KIND`], so a test can assert the offer
    /// backfill actually reached the wire rather than inferring it from a log line — the offers analog
    /// of [`CountWrapQueries`].
    #[derive(Debug)]
    struct CountOfferQueries(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl nostr_relay_builder::prelude::QueryPolicy for CountOfferQueries {
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
                    .is_some_and(|kinds| kinds.contains(&Kind::Custom(JOB_OFFER_KIND)))
                {
                    self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                nostr_relay_builder::prelude::PolicyResult::Accept
            })
        }
    }

    // TOOTH (offer backfill, UNCONDITIONALITY — #560) — the offer-backfill fetch must reach the relay
    // even with nothing pending, the same idle case that goes quiet for wraps. Asserted at the WIRE
    // (the fixture counts JOB_OFFER_KIND REQs), so a future "skip the fetch when idle" optimisation
    // cannot pass by keeping the log line and dropping the fetch. The offers analog of
    // `wrap_backfill_fetches_even_with_nothing_pending`; it drives the REAL boot path.
    //
    // BITE: empty `run_offer_backfill`'s body (or gate its fetch on something pending) → the delta
    // assertion goes red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offer_backfill_fetches_even_with_nothing_pending() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let offer_queries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let relay = LocalRelay::new(
            RelayBuilder::default()
                .query_policy(CountOfferQueries(std::sync::Arc::clone(&offer_queries))),
        );
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        // A real (targeted) seller seat: boot registers the live offer REQ, so only the delta across
        // the backfill call is evidence.
        let (runner, root) =
            boot_capacity_skip_seller("offer-backfill-empty", &relay_url, false, 0, Some(0)).await;
        let before = offer_queries.load(std::sync::atomic::Ordering::SeqCst);

        // `on_offer` is only ever driven under a LocalSet in this suite; `run_offer_backfill` contains
        // that path, so keep the same discipline even though nothing is pending here.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                runner.run_offer_backfill().await;
            })
            .await;

        assert!(
            offer_queries.load(std::sync::atomic::Ordering::SeqCst) > before,
            "the offer backfill must re-ask the relay for stored kind-{}(s) even with nothing pending \
             — skipping the fetch when idle silences a periodic recovery leg and its positive signal",
            JOB_OFFER_KIND
        );
        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── #450: capacity-skip backfill — a whole-path LocalRelay proof ─────────────────────────────
    //
    // The unit tests above pin the pieces (a SlotsBusy skip arms the flag; the classify gate order;
    // the filter shapes). These drive the WHOLE path end to end against a real in-process relay: a
    // 1-slot seller claims one offer, capacity-skips a second, and — once the parked claim lapses and
    // frees the slot — the drain tick's `sweep_lapsed_claims` + `reconsider_capacity_skips` pair
    // (run.rs loop arm) re-runs the offer backfill, the relay re-delivers the stored offers, and the
    // previously skipped offer is claimed WITHOUT a restart. The re-delivered lapsed offer must NOT
    // be re-claimed — its `released` claim row dedups it — the money-safety property the fix leans on.
    //
    // The slot is freed by a claim LAPSE (`claim_award_timeout_secs = 0` ⇒ the next sweep reclaims
    // it), needing no award/execute cycle, so the whole drive is deterministic with no wall-clock
    // waits. Freeing via a losing award (which would also exercise #456's loser-release) needs a
    // published-and-confirmed claim id (`match_award` returns `Release` only for `Some(our_claim)`),
    // i.e. the outbox publisher running — a heavier harness left as a separate lane.

    /// A 1-slot seller wired to `relay_url`. `claim_award_timeout_secs` sets the parked-claim lapse:
    /// `Some(0)` ⇒ a parked claim counts as lapsed on the very next sweep regardless of wall-clock, so
    /// a test frees the slot on demand (the #450 capacity-skip drive); a LARGE value makes the lapse
    /// unreachable in-test, pinning the AWARD-visibility path as the ONLY thing that can free the slot
    /// (the loser-release pin, which must NOT ride the lapse). `offer_backfill_secs` bounds only the
    /// open-pool re-delivery; the targeted re-subscribe is unbounded regardless.
    async fn boot_capacity_skip_seller(
        label: &str,
        relay_url: &str,
        claim_open_pool: bool,
        offer_backfill_secs: u64,
        claim_award_timeout_secs: Option<u64>,
    ) -> (SellerNodeRunner, std::path::PathBuf) {
        let root = temp_dir(label);
        let _ = std::fs::remove_dir_all(&root);
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = relay_url.to_string();
        let mut seller = seller_cfg(1, claim_open_pool);
        seller.claim_award_timeout_secs = claim_award_timeout_secs;
        seller.offer_backfill_secs = offer_backfill_secs;
        home.config.seller = Some(seller);
        let runner = SellerNodeRunner::boot(home)
            .await
            .expect("boot the capacity-skip seller against the fixture relay");
        (runner, root)
    }

    /// Post one offer as `buyer` (targeted to `to_seller` when `Some`, open-pool otherwise) and return
    /// its event id — which is the `job_id` the node keys the recorded offer and claim on.
    async fn post_offer(
        publisher: &Client,
        buyer: &Keys,
        task: &str,
        to_seller: Option<&str>,
        amount_sats: u64,
        deadline_unix: u64,
    ) -> String {
        // `task` MUST differ between the two offers: a nostr event id is the hash of its content, so
        // two byte-identical offers from one buyer in the same second collapse to ONE event id — the
        // relay would dedup them and the "second offer" would never exist to be capacity-skipped.
        let draft = match to_seller {
            Some(seller_hex) => {
                crate::gateway::OfferDraft::new(task, "", amount_sats, deadline_unix, seller_hex)
            }
            None => crate::gateway::OfferDraft::untargeted(task, "", amount_sats, deadline_unix),
        }
        .to_event_draft();
        let event = crate::gateway::nostr::event_builder(&draft)
            .expect("offer event builder")
            .sign_with_keys(buyer)
            .expect("sign offer");
        let id = event.id.to_hex();
        publisher.send_event(&event).await.expect("post offer to relay");
        id
    }

    /// Feed JOB_OFFER events off the seller's live notification stream into `on_offer` — exactly what
    /// the run loop's select arm does — until `done` holds or the deadline passes. Returns whether
    /// `done` held.
    async fn pump_offers_until(
        runner: &SellerNodeRunner,
        notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
        deadline: std::time::Instant,
        mut done: impl FnMut(&SellerNodeRunner) -> bool,
    ) -> bool {
        while !done(runner) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if let Ok(Ok(RelayPoolNotification::Event { event, .. })) = tokio::time::timeout(
                remaining.min(Duration::from_millis(150)),
                notifications.recv(),
            )
            .await
            {
                if event.kind.as_u16() == JOB_OFFER_KIND {
                    runner.on_offer(&event).await;
                }
            }
        }
        done(runner)
    }

    /// Feed JOB_AWARD events off the seller's live notification stream into `on_award` — exactly what
    /// the run loop's select arm does — until `done` holds or the deadline passes. Mirrors
    /// [`pump_offers_until`]; takes an `Arc` runner because `on_award` binds `self: &Arc<Self>` (its
    /// execute branch spawns the job onto the LocalSet). Returns whether `done` held.
    async fn pump_awards_until(
        runner: &Arc<SellerNodeRunner>,
        notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
        deadline: std::time::Instant,
        mut done: impl FnMut(&SellerNodeRunner) -> bool,
    ) -> bool {
        while !done(runner) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if let Ok(Ok(RelayPoolNotification::Event { event, .. })) = tokio::time::timeout(
                remaining.min(Duration::from_millis(150)),
                notifications.recv(),
            )
            .await
            {
                if event.kind.as_u16() == JOB_AWARD_KIND {
                    runner.on_award(&event).await;
                }
            }
        }
        done(runner)
    }

    /// #541 — a settled offer another seat won is never claimed, and the fail-open control still claims
    /// an unsettled one. The receipt is BUYER-authored (a co-signed kind-3400) for an offer awarded to
    /// ANOTHER seller, so THIS node never locally collected it: `store.has_receipt` is false and a
    /// vacuous local gate would claim. The relay-derived, buyer-bound cache must make it terminal.
    ///
    /// RED ON REVERT: drop the `terminal_offers.settled_by` guard at the top of `claim_offer` and the
    /// settled offer is recorded and reserves the slot, failing the first assertion block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_settled_offer_won_by_another_seat_is_not_claimed() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        // A 1-slot OPEN-POOL seller — the pool that claims an untargeted offer it can lose.
        let (runner, root) =
            boot_capacity_skip_seller("settled-not-claimed", &relay_url, true, 0, Some(0)).await;

        let buyer = Keys::generate();
        let deadline_unix = now_unix() as u64 + 3_600;

        // An untargeted offer the open-pool seller WOULD otherwise claim (over the floor, future
        // deadline, default harness) — so a skip can ONLY be the settlement gate, never a stray refusal.
        let offer_draft =
            crate::gateway::OfferDraft::untargeted("settled job", "", 100, deadline_unix)
                .to_event_draft();
        let offer_event = crate::gateway::nostr::event_builder(&offer_draft)
            .expect("offer builder")
            .sign_with_keys(&buyer)
            .expect("sign offer");
        let settled_id = offer_event.id.to_hex();

        // The BUYER publishes the co-signed kind-3400 settling it; the award went to ANOTHER seller
        // (the seller p-tag), so this node never collected the receipt.
        let other_seller = Keys::generate().public_key().to_hex();
        let receipt = crate::gateway::receipt_draft(
            &settled_id,
            "result-id",
            &buyer.public_key().to_hex(),
            &other_seller,
            "https://mint.invalid",
            100,
            "job-hash",
            "seller-sig",
            "buyer-sig",
            None,
            None,
            &[],
        );
        let receipt_event = crate::gateway::nostr::event_builder(&receipt)
            .expect("receipt builder")
            .sign_with_keys(&buyer) // the 3400 is BUYER-authored (authorize_pay.rs)
            .expect("sign receipt");

        // A second, UNSETTLED untargeted offer — the fail-open control that must still be claimed.
        let fresh_draft =
            crate::gateway::OfferDraft::untargeted("fresh job", "", 100, deadline_unix)
                .to_event_draft();
        let fresh_event = crate::gateway::nostr::event_builder(&fresh_draft)
            .expect("fresh offer builder")
            .sign_with_keys(&buyer)
            .expect("sign fresh offer");
        let fresh_id = fresh_event.id.to_hex();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Precondition: this node has NOT locally collected the receipt (another seat won it),
                // so a `store.has_receipt` gate would be vacuous — the whole point of relay-deriving.
                assert!(
                    !runner.node.store().has_receipt(&settled_id).expect("has_receipt read"),
                    "precondition: the settlement was collected by another seat, not us"
                );

                // The co-signed receipt arrives and marks the offer terminal.
                runner.on_receipt(&receipt_event).await;
                // The settled offer re-appears (backfill / redelivery) — it must NOT be claimed.
                runner.on_offer(&offer_event).await;
                assert!(
                    runner.node.store().offer_facts(&settled_id).expect("offer_facts").is_none(),
                    "a settled offer must not be recorded"
                );
                assert!(
                    runner.node.store().claim_row_state(&settled_id).expect("claim row").is_none(),
                    "a settled offer must park no claim"
                );
                assert_eq!(runner.slots.available(), 1, "no slot is reserved for a settled offer");

                // Fail-open control: an UNSETTLED offer is still claimed — proving the seller is willing
                // and able, so the skip above was the settlement gate, not a blanket refusal.
                runner.on_offer(&fresh_event).await;
                assert!(
                    runner.node.store().offer_facts(&fresh_id).expect("fresh offer_facts").is_some(),
                    "an unsettled offer is still claimed (fail-open on the unknown)"
                );
                assert_eq!(runner.slots.available(), 0, "the unsettled claim takes the slot");
            })
            .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- #814 late losing claims (award/acceptance to another seller) ----------------------------

    /// A buyer-authored AWARD naming `claim_id` for `offer_id`. `author` is separate from `buyer` on
    /// purpose: the forgery leg signs the very same event with a stranger's key, so the two cases
    /// differ ONLY by signer.
    fn award_event(
        offer_id: &str,
        claim_id: &str,
        buyer: &Keys,
        winner_seller_hex: &str,
        author: &Keys,
    ) -> nostr_sdk::Event {
        let draft = crate::gateway::award_draft(
            offer_id,
            claim_id,
            &buyer.public_key().to_hex(),
            winner_seller_hex,
        );
        crate::gateway::nostr::event_builder(&draft)
            .expect("award builder")
            .sign_with_keys(author)
            .expect("sign award")
    }

    /// The kind-3406 twin of [`award_event`] — the delivery ACCEPT, #814's backup signal.
    fn accept_event(
        offer_id: &str,
        claim_id: &str,
        buyer: &Keys,
        winner_seller_hex: &str,
        author: &Keys,
    ) -> nostr_sdk::Event {
        let draft = crate::gateway::accept_draft(
            offer_id,
            claim_id,
            &buyer.public_key().to_hex(),
            winner_seller_hex,
        );
        crate::gateway::nostr::event_builder(&draft)
            .expect("accept builder")
            .sign_with_keys(author)
            .expect("sign accept")
    }

    /// Boot a 1-slot OPEN-POOL seller whose claims lapse instantly, drive it to the exact #814 state —
    /// one offer claimed and holding the only slot, a SECOND offer recorded but capacity-skipped — and
    /// return `(runner, root, buyer, skipped_offer_id)`.
    ///
    /// OPEN-POOL is not a test convenience, it is the precondition: `award_filter` scopes a TARGETED
    /// seller's REQ to its own pubkey and an award p-tags only the winner, so a targeted seller never
    /// receives another seat's award and #814 cannot occur there at all.
    async fn boot_at_capacity_with_a_skipped_offer(
        label: &str,
        relay_url: &str,
    ) -> (Arc<SellerNodeRunner>, std::path::PathBuf, Keys, String) {
        let (runner, root) = boot_capacity_skip_seller(label, relay_url, true, 0, Some(0)).await;
        let runner = Arc::new(runner);
        let buyer = Keys::generate();
        let deadline_unix = now_unix() as u64 + 3_600;

        // Two untargeted offers an open-pool seller would otherwise BOTH claim, so any later skip can
        // only be the suppression gate. The tasks differ because an event id is a hash of its content.
        let mut ids = Vec::new();
        for task in ["#814 busy job", "#814 skipped job"] {
            let draft = crate::gateway::OfferDraft::untargeted(task, "", 100, deadline_unix)
                .to_event_draft();
            let event = crate::gateway::nostr::event_builder(&draft)
                .expect("offer builder")
                .sign_with_keys(&buyer)
                .expect("sign offer");
            ids.push(event.id.to_hex());
            runner.on_offer(&event).await;
        }
        let (claimed_id, skipped_id) = (ids[0].clone(), ids[1].clone());

        assert_eq!(runner.slots.available(), 0, "the first offer must take the only slot");
        assert_eq!(
            runner.node.store().claim_row_state(&claimed_id).expect("claim row"),
            Some("claimed".to_owned()),
            "precondition: the first offer is claimed"
        );
        assert!(
            runner.node.store().offer_facts(&skipped_id).expect("offer facts").is_some(),
            "precondition: the second offer is RECORDED (record_offer runs before the slot reserve)"
        );
        assert!(
            runner.node.store().claim_row_state(&skipped_id).expect("skip row").is_none(),
            "precondition: the second offer is capacity-SKIPPED — recorded with no claim, which is \
             exactly the state #814 re-drives into a losing claim"
        );
        assert!(
            runner.capacity_skip_pending.load(std::sync::atomic::Ordering::Relaxed),
            "precondition: the SlotsBusy skip armed the reconsider flag"
        );
        (runner, root, buyer, skipped_id)
    }

    /// Free the slot the way the drain tick does, then run the one-shot capacity reconsider.
    async fn free_the_slot_and_reconsider(runner: &Arc<SellerNodeRunner>) {
        runner.sweep_lapsed_claims();
        assert_eq!(runner.slots.available(), 1, "the lapsed claim frees the slot");
        runner.reconsider_capacity_skips().await;
    }

    /// #814 REGRESSION 1 — THE REPRO. An authentic buyer award to ANOTHER seller arrives while this
    /// seller is at capacity; freeing capacity must not produce a claim.
    ///
    /// Non-vacuous by construction: after the sweep, the capacity-skipped offer is the ONLY row
    /// `offers_awaiting_claim` can return (the other offer holds a `released` claim row and is
    /// excluded), so the reconsider loop provably reaches it. The free slot afterwards is the proof it
    /// was REACHED AND REFUSED rather than never visited.
    ///
    /// RED ON REVERT: replace the `suppress_taken_elsewhere` call in `on_award`'s `job_creq == None`
    /// arm with the old `opline!("… no claim of ours"); return;` and the reconsider claims the offer —
    /// `claim_row_state` becomes `Some("claimed")` and the slot drops to 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_authentic_award_to_another_seller_is_not_claimed_when_capacity_frees() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        let (runner, root, buyer, skipped_id) =
            boot_at_capacity_with_a_skipped_offer("814-award-repro", &relay_url).await;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // The buyer awards ANOTHER seller's claim for the offer we recorded but never claimed.
                let other_seller = Keys::generate().public_key().to_hex();
                let award =
                    award_event(&skipped_id, "another-seats-claim", &buyer, &other_seller, &buyer);
                runner.on_award(&award).await;

                // The durable leg: the decision is on disk, so a restart cannot forget it.
                assert!(
                    runner.node.store().job_award_time(&skipped_id).expect("award time").is_some(),
                    "an authentic award for a recorded offer must be PERSISTED, not discarded"
                );
                // …and it created no job, because we hold no claim to bind.
                assert!(
                    runner.node.store().job_state(&skipped_id).expect("job state").is_none(),
                    "recording another seat's award must never create a job of ours"
                );

                free_the_slot_and_reconsider(&runner).await;

                assert!(
                    runner.node.store().claim_row_state(&skipped_id).expect("claim row").is_none(),
                    "#814: the capacity reconsider must NOT publish a losing claim for an offer \
                     already awarded to another seller"
                );
                assert_eq!(
                    runner.slots.available(),
                    1,
                    "no slot is burned on a race that is already over"
                );

                // FAIL-OPEN CONTROL, same runner and same slot: a fresh offer with no decision against
                // it is still claimed — so the refusal above was the suppression gate, not a node that
                // had simply stopped claiming.
                let fresh_draft = crate::gateway::OfferDraft::untargeted(
                    "#814 fresh job",
                    "",
                    100,
                    now_unix() as u64 + 3_600,
                )
                .to_event_draft();
                let fresh = crate::gateway::nostr::event_builder(&fresh_draft)
                    .expect("fresh builder")
                    .sign_with_keys(&buyer)
                    .expect("sign fresh");
                runner.on_offer(&fresh).await;
                assert_eq!(
                    runner.node.store().claim_row_state(&fresh.id.to_hex()).expect("fresh row"),
                    Some("claimed".to_owned()),
                    "an undecided offer is still claimed (fail-open on the unknown)"
                );
            })
            .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #814 REGRESSION 2 — an authentic buyer ACCEPTANCE gives the same backup behaviour. This is the
    /// signal that carries when the award itself never reached us.
    ///
    /// RED ON REVERT: restore `on_accept`'s `job_creq == None` arm to its old
    /// `opline!("… no claim of ours"); return;` and the reconsider claims the offer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_authentic_acceptance_is_the_backup_suppression() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        let (runner, root, buyer, skipped_id) =
            boot_at_capacity_with_a_skipped_offer("814-accept-backup", &relay_url).await;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // No award ever arrives — only the buyer's acceptance of another seat's delivery.
                let other_seller = Keys::generate().public_key().to_hex();
                let accept =
                    accept_event(&skipped_id, "another-seats-claim", &buyer, &other_seller, &buyer);
                runner.on_accept(&accept).await;

                free_the_slot_and_reconsider(&runner).await;

                assert!(
                    runner.node.store().claim_row_state(&skipped_id).expect("claim row").is_none(),
                    "an acceptance is buyer-authenticated evidence the offer is decided; the \
                     reconsider must not claim it"
                );
                assert_eq!(runner.slots.available(), 1, "no slot is burned");
            })
            .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #814 REGRESSION 3 — BOTH forgery legs the issue names. A decision from a NON-BUYER, and a
    /// decision bound to the WRONG OFFER, must each leave the job claimable.
    ///
    /// This is the anti-grief property: if either leg failed, anyone on an open relay could silence a
    /// seller by publishing awards for offers it recorded. The buyer is read from OUR OWN recorded
    /// offer, never from the event, which is what keeps the check non-circular.
    ///
    /// RED ON REVERT: delete the `if author != buyer` gate at the top of `suppress_taken_elsewhere`
    /// and the stranger's award suppresses the offer — the reconsider stops claiming and the final
    /// assertion fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forged_or_misbound_decision_never_suppresses() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        let (runner, root, buyer, skipped_id) =
            boot_at_capacity_with_a_skipped_offer("814-forged", &relay_url).await;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let other_seller = Keys::generate().public_key().to_hex();

                // LEG 1 — a stranger signs an otherwise well-formed award for OUR offer.
                let forger = Keys::generate();
                let forged =
                    award_event(&skipped_id, "any-claim", &buyer, &other_seller, &forger);
                runner.on_award(&forged).await;
                assert!(
                    runner.node.store().job_award_time(&skipped_id).expect("award time").is_none(),
                    "a forged award must not even be PERSISTED — the author gate runs before the write"
                );

                // LEG 2 — the REAL buyer, but the award roots on a different offer. It can only ever
                // speak about the offer it names, and that one is not recorded here.
                let stranger_offer_id = "f".repeat(64);
                let misbound =
                    award_event(&stranger_offer_id, "any-claim", &buyer, &other_seller, &buyer);
                runner.on_award(&misbound).await;
                assert!(
                    runner.node.store().job_award_time(&skipped_id).expect("award time").is_none(),
                    "an award bound to another offer must not touch this one"
                );

                // The offer therefore stays ours to claim, and the reconsider claims it.
                free_the_slot_and_reconsider(&runner).await;
                assert_eq!(
                    runner.node.store().claim_row_state(&skipped_id).expect("claim row"),
                    Some("claimed".to_owned()),
                    "neither a forged nor a misbound decision may take an offer off this seller"
                );
            })
            .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #814 REGRESSION 4 — the RECEIPT stays terminal, including out-of-order against an award.
    ///
    /// Both orders asserted, because `combine` must be order-independent: an award landing after a
    /// receipt must not downgrade a permanent suppression to one that expires at the offer deadline.
    /// The probe time is far past every deadline used here, so only a PERMANENT suppression survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_receipt_stays_terminal_against_an_award_in_either_order() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        let (runner, root) =
            boot_capacity_skip_seller("814-receipt-order", &relay_url, true, 0, Some(0)).await;
        let runner = Arc::new(runner);

        let buyer = Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let other_seller = Keys::generate().public_key().to_hex();
        let deadline_unix = now_unix() as u64 + 3_600;
        let far_future = deadline_unix + 86_400;

        let receipt_for = |offer_id: &str| {
            let draft = crate::gateway::receipt_draft(
                offer_id,
                "result-id",
                &buyer_hex,
                &other_seller,
                "https://mint.invalid",
                100,
                "job-hash",
                "seller-sig",
                "buyer-sig",
                None,
                None,
                &[],
            );
            crate::gateway::nostr::event_builder(&draft)
                .expect("receipt builder")
                .sign_with_keys(&buyer)
                .expect("sign receipt")
        };

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // ORDER A — award first, then the receipt UPGRADES it to permanent.
                let a = "a".repeat(64);
                runner.terminal_offers.record_taken_elsewhere(&a, &buyer_hex, deadline_unix);
                assert!(
                    runner.terminal_offers.suppressed_by(&a, &buyer_hex, far_future).is_none(),
                    "control: an award-only suppression HAS expired by the probe time"
                );
                runner.on_receipt(&receipt_for(&a)).await;
                assert_eq!(
                    runner.terminal_offers.suppressed_by(&a, &buyer_hex, far_future),
                    Some(Suppression::Settled),
                    "a receipt after an award upgrades the offer to terminal FOREVER"
                );

                // ORDER B — receipt first; a later award must not weaken it.
                let b = "b".repeat(64);
                runner.on_receipt(&receipt_for(&b)).await;
                runner.terminal_offers.record_taken_elsewhere(&b, &buyer_hex, deadline_unix);
                assert_eq!(
                    runner.terminal_offers.suppressed_by(&b, &buyer_hex, far_future),
                    Some(Suppression::Settled),
                    "an award arriving after a receipt must NOT downgrade it to an expiring \
                     suppression — #541 terminality is not weakened by #814"
                );
            })
            .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⛔ THE #563 FOIL — the money-adjacent invariant. Suppression must NEVER apply to a job we hold
    /// a CLAIM for, because suppressing our own live award strands it: compute spent, nothing paid.
    ///
    /// The discriminator is the CLAIM row, not the award row. That distinction is load-bearing
    /// precisely because #814 makes the suppression path itself write an award row — keying the
    /// invariant on "we hold an award" would have made the whole fix inert.
    ///
    /// Here the seller genuinely claims the offer and the buyer then awards a DIFFERENT seat. The
    /// handler must take the existing `match_award` → `Release` path (the claim row moves to
    /// `released`) and must NOT record a suppression, so the claim's own lifecycle stays in charge.
    ///
    /// RED ON REVERT: move the `suppress_taken_elsewhere` call out of the `job_creq == None` arm so it
    /// runs unconditionally, and this test goes red on the suppression assertion — the exact
    /// over-suppression that would strand a real award.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_we_hold_a_claim_for_is_never_suppressed() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        // A LONG lapse timeout: the parked claim must still be ours when the award lands.
        let (runner, root) =
            boot_capacity_skip_seller("814-563-foil", &relay_url, true, 0, Some(3_600)).await;
        let runner = Arc::new(runner);

        let buyer = Keys::generate();
        let deadline_unix = now_unix() as u64 + 3_600;
        let draft = crate::gateway::OfferDraft::untargeted("#814 foil job", "", 100, deadline_unix)
            .to_event_draft();
        let offer = crate::gateway::nostr::event_builder(&draft)
            .expect("offer builder")
            .sign_with_keys(&buyer)
            .expect("sign offer");
        let job_id = offer.id.to_hex();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                runner.on_offer(&offer).await;
                assert_eq!(
                    runner.node.store().claim_row_state(&job_id).expect("claim row"),
                    Some("claimed".to_owned()),
                    "precondition: WE HOLD A CLAIM for this job — the complement of #814's case"
                );

                // The buyer picks another seat. This is the #563 shape: an award rooting an offer we
                // are live on.
                let other_seller = Keys::generate().public_key().to_hex();
                let award = award_event(&job_id, "another-seats-claim", &buyer, &other_seller, &buyer);
                runner.on_award(&award).await;

                assert!(
                    runner
                        .terminal_offers
                        .suppressed_by(&job_id, &buyer.public_key().to_hex(), now_unix() as u64)
                        .is_none(),
                    "⛔ a job we hold a claim for must NEVER be suppressed — that is how a real award \
                     gets stranded (#563 foil)"
                );
                assert_eq!(
                    runner.node.store().claim_row_state(&job_id).expect("claim row"),
                    Some("released".to_owned()),
                    "the existing match_award → Release path still owns this case, untouched"
                );
            })
            .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #814 REQUIREMENT 5 — the suppression survives a RESTART, with no relay in the loop.
    ///
    /// This is the leg the in-memory cache cannot provide alone, and the exposure is measured, not
    /// hypothetical: the #560 periodic offer-backfill re-feeds every stored offer through `on_offer`
    /// each tick, so a bounce that forgets the decision re-drives the offer straight back into a
    /// losing claim. A relay redelivery would usually re-derive it, but "relay deafness manufactures
    /// absence" (#560/#563) — recovery is not durability.
    ///
    /// The second runner opens the SAME home directory with no subscription and no relay traffic, so
    /// anything it knows came off disk.
    ///
    /// RED ON REVERT: delete the `rehydrate_suppressions()` call in `serve` — or the `record_award`
    /// write in `suppress_taken_elsewhere` — and the restarted node claims the offer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_suppression_survives_a_restart_off_the_store_alone() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        let (runner, root, buyer, skipped_id) =
            boot_at_capacity_with_a_skipped_offer("814-restart", &relay_url).await;
        let buyer_hex = buyer.public_key().to_hex();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let other_seller = Keys::generate().public_key().to_hex();
                let award =
                    award_event(&skipped_id, "another-seats-claim", &buyer, &other_seller, &buyer);
                runner.on_award(&award).await;
                // Free the slot but do NOT reconsider — the pre-restart node stops here.
                runner.sweep_lapsed_claims();
            })
            .await;
        drop(runner);

        // ---- restart: a brand-new runner over the SAME store ----------------------------------
        let mut home = crate::home::bootstrap(&root).expect("re-bootstrap the same home");
        home.config.relay_url = relay_url.clone();
        let mut seller = seller_cfg(1, true);
        seller.claim_award_timeout_secs = Some(0);
        seller.offer_backfill_secs = 0;
        home.config.seller = Some(seller);
        let restarted = Arc::new(
            SellerNodeRunner::boot(home).await.expect("boot the restarted seller"),
        );

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Cold cache proves the assertion below is about DISK, not leftover memory.
                assert!(
                    restarted
                        .terminal_offers
                        .suppressed_by(&skipped_id, &buyer_hex, now_unix() as u64)
                        .is_none(),
                    "a fresh process starts with an empty in-memory cache"
                );

                restarted.rehydrate_suppressions();

                assert!(
                    restarted
                        .terminal_offers
                        .suppressed_by(&skipped_id, &buyer_hex, now_unix() as u64)
                        .is_some(),
                    "the decision must be restored from the store, with no relay redelivery"
                );

                // And the behaviour that matters: the reconsider still refuses to claim it.
                restarted
                    .capacity_skip_pending
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                restarted.reconsider_capacity_skips().await;
                assert!(
                    restarted.node.store().claim_row_state(&skipped_id).expect("claim row").is_none(),
                    "#814 must not come back after a restart"
                );
            })
            .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Feed JOB_RECEIPT events off the seller's live notification stream into `on_receipt` — exactly
    /// what the run loop's select arm does — until `done` holds or the deadline passes. Mirrors
    /// [`pump_offers_until`]; the receipt path is `&self` (no execution spawn), so no `Arc` is needed.
    async fn pump_receipts_until(
        runner: &SellerNodeRunner,
        notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
        deadline: std::time::Instant,
        mut done: impl FnMut(&SellerNodeRunner) -> bool,
    ) -> bool {
        while !done(runner) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if let Ok(Ok(RelayPoolNotification::Event { event, .. })) = tokio::time::timeout(
                remaining.min(Duration::from_millis(150)),
                notifications.recv(),
            )
            .await
            {
                if event.kind.as_u16() == JOB_RECEIPT_KIND {
                    runner.on_receipt(&event).await;
                }
            }
        }
        done(runner)
    }

    /// #541 — the settlement-receipt SUB actually feeds the terminal cache, and a fresh boot rebuilds
    /// it from the relay's STORED receipts (the relay-verify-on-restart). This exercises the one novel,
    /// silent-failure-prone bit: the boot backfill WINDOW. A receipt published BEFORE the seller boots
    /// must be re-fetched by the sub. RED ON REVERT: break the boot `since` in `subscribe_receipts`
    /// (e.g. `since(now)` live-only) and the past receipt never backfills, so the cache stays empty and
    /// the `settled_by` wait times out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_receipt_sub_backfills_the_terminal_cache_on_boot() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        // A buyer publishes an offer and its co-signed settlement receipt to the relay BEFORE any
        // seller boots — the "already settled when we arrive" case.
        let buyer = Keys::generate();
        let publisher = Client::new(buyer.clone());
        publisher.add_relay(&relay_url).await.expect("add relay");
        publisher.connect().await;
        publisher.wait_for_connection(Duration::from_secs(5)).await;
        let buyer_hex = buyer.public_key().to_hex();
        let deadline_unix = now_unix() as u64 + 3_600;

        let offer_draft =
            crate::gateway::OfferDraft::untargeted("settled-by-relay", "", 100, deadline_unix)
                .to_event_draft();
        let offer_event = crate::gateway::nostr::event_builder(&offer_draft)
            .expect("offer builder")
            .sign_with_keys(&buyer)
            .expect("sign offer");
        let settled_id = offer_event.id.to_hex();

        let other_seller = Keys::generate().public_key().to_hex();
        let receipt = crate::gateway::receipt_draft(
            &settled_id,
            "result-id",
            &buyer_hex,
            &other_seller,
            "https://mint.invalid",
            100,
            "job-hash",
            "seller-sig",
            "buyer-sig",
            None,
            None,
            &[],
        );
        let receipt_event = crate::gateway::nostr::event_builder(&receipt)
            .expect("receipt builder")
            .sign_with_keys(&buyer)
            .expect("sign receipt");
        publisher.send_event(&receipt_event).await.expect("publish receipt to relay");

        // Boot an open-pool seller WITH a backfill window; its receipt sub must re-fetch the stored
        // 3400 into the terminal cache.
        let (runner, root) =
            boot_capacity_skip_seller("receipt-backfill", &relay_url, true, 3_600, Some(0)).await;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut notifications = runner.client.notifications();
                runner.subscribe_receipts(None).await.expect("subscribe receipts");
                assert!(
                    pump_receipts_until(
                        &runner,
                        &mut notifications,
                        std::time::Instant::now() + Duration::from_secs(5),
                        |r| {
                            r.terminal_offers
                                .suppressed_by(&settled_id, &buyer_hex, now_unix() as u64)
                                .is_some()
                        },
                    )
                    .await,
                    "the receipt sub must backfill the past 3400 into the terminal cache"
                );
                // With the cache rebuilt from the relay, the settled offer is not claimed.
                runner.on_offer(&offer_event).await;
                assert!(
                    runner.node.store().offer_facts(&settled_id).expect("offer_facts").is_none(),
                    "a relay-learned settled offer must not be claimed"
                );
            })
            .await;
        let _ = std::fs::remove_dir_all(&root);
        publisher.disconnect().await;
    }

    /// The full capacity-skip → lapse → reconsider → backfill → re-claim drive, shared by the targeted
    /// and open-pool cases. `open_pool` shapes both the seller's subscription and whether the offers
    /// are targeted; the mechanism under test is identical either way.
    async fn drive_capacity_skip_backfill(label: &str, open_pool: bool, backfill_secs: u64) {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        let (runner, root) =
            boot_capacity_skip_seller(label, &relay_url, open_pool, backfill_secs, Some(0)).await;
        let seller_hex = runner.seller_pubkey();
        let to_seller = if open_pool { None } else { Some(seller_hex.clone()) };

        // A separate publisher: a node cannot be delivered its OWN events, so offers must come from a
        // different key.
        let buyer = Keys::generate();
        let publisher = Client::new(buyer.clone());
        publisher.add_relay(&relay_url).await.expect("publisher add relay");
        publisher.connect().await;
        publisher.wait_for_connection(Duration::from_secs(5)).await;

        let mut notifications = runner.client.notifications();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                runner
                    .subscribe_offers(None, open_pool)
                    .await
                    .expect("establish the offer subscription");

                let deadline_unix = now_unix() as u64 + 3_600;
                let target = to_seller.as_deref();
                let id_a =
                    post_offer(&publisher, &buyer, "capacity-skip offer A", target, 100, deadline_unix)
                        .await;
                let id_b =
                    post_offer(&publisher, &buyer, "capacity-skip offer B", target, 100, deadline_unix)
                        .await;

                // ROUND 1 — a 1-slot seller claims one offer and capacity-skips the other.
                assert!(
                    pump_offers_until(
                        &runner,
                        &mut notifications,
                        std::time::Instant::now() + Duration::from_secs(5),
                        |r| r.node.store().offer_facts(&id_a).ok().flatten().is_some()
                            && r.node.store().offer_facts(&id_b).ok().flatten().is_some(),
                    )
                    .await,
                    "both offers must reach on_offer"
                );

                let claimed_a =
                    runner.node.store().claim_row_state(&id_a).expect("claim a").is_some();
                let claimed_b =
                    runner.node.store().claim_row_state(&id_b).expect("claim b").is_some();
                assert!(
                    claimed_a ^ claimed_b,
                    "a 1-slot seller must claim exactly one of the two offers (a={claimed_a} b={claimed_b})"
                );
                assert_eq!(runner.slots.available(), 0, "the single slot is full after the claim");
                assert!(
                    runner
                        .capacity_skip_pending
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "the SlotsBusy skip must arm the reconsider flag"
                );
                let (claimed_id, skipped_id) = if claimed_a {
                    (id_a.clone(), id_b.clone())
                } else {
                    (id_b.clone(), id_a.clone())
                };

                // Reconsidering while the slot is still full is a no-op that PRESERVES the pending flag
                // — else the skip is forgotten and never revisited.
                runner.reconsider_capacity_skips().await;
                assert_eq!(runner.slots.available(), 0, "reconsider must not free a slot on its own");
                assert!(
                    runner
                        .capacity_skip_pending
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "reconsider while full must keep the skip pending"
                );

                // Free the slot exactly as the drain tick does: the parked claim lapses and its row
                // moves to `released` — still present, so a re-delivery dedups instead of re-claiming.
                runner.sweep_lapsed_claims();
                assert_eq!(runner.slots.available(), 1, "the lapsed claim frees the slot");
                assert_eq!(
                    runner.node.store().claim_row_state(&claimed_id).expect("released row"),
                    Some("released".to_string()),
                    "the lapsed claim's row must remain, as `released`"
                );

                // #450 — reconsider re-drives the recorded capacity-skipped offer STRAIGHT FROM THE
                // STORE (no relay round-trip, so no dependence on a re-delivery the pool's seen-cache
                // would swallow). It is a ONE-SHOT: the first fire that finds a free slot consumes the
                // pending flag (`swap(false)`), so it is called exactly ONCE here — never in the poll
                // loop below.
                runner.reconsider_capacity_skips().await;
                assert!(
                    !runner
                        .capacity_skip_pending
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "a reconsider that fires clears the pending flag"
                );
                // The re-drive's claim is synchronous — `claim_offer` awaits nothing and
                // `claim_and_enqueue` commits the `claimed` row before `reconsider_capacity_skips`
                // returns — so the row is normally observable the instant the await returns. Poll it to
                // a bounded deadline anyway (#580): under the loaded money-path suite this test shares a
                // multi-thread runtime with ~840 others, and the bounded poll mirrors the sibling
                // helper's `pump_*_until` shape so a scheduler beat cannot flake the read. It still
                // FAILS if the state never reaches `claimed`, so it cannot mask a genuine non-delivery.
                let claimed_deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    let state = runner
                        .node
                        .store()
                        .claim_row_state(&skipped_id)
                        .expect("skipped claim state");
                    if state.as_deref() == Some("claimed") {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < claimed_deadline,
                        "the capacity-skipped offer must be claimed once a slot frees — no restart, no \
                         relay round-trip (last observed claim state: {state:?})"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert_eq!(
                    runner.slots.available(),
                    0,
                    "claiming the re-driven offer refills the slot"
                );
                assert_eq!(
                    runner.node.store().claim_row_state(&claimed_id).expect("still released"),
                    Some("released".to_string()),
                    "the lapsed offer is NOT re-claimed (it has a released row; only never-claimed offers are re-driven)"
                );
            })
            .await;

        runner.client.disconnect().await;
        publisher.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #450 — the targeted case. A targeted seller's re-subscribe is unbounded, so the backfill window
    /// is immaterial; a capacity-skipped offer is revisited the moment a slot frees.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconsider_backfills_a_capacity_skipped_targeted_offer_when_a_slot_frees() {
        drive_capacity_skip_backfill("cap-skip-targeted", false, 0).await;
    }

    /// #450 + #519 — the OPEN-POOL case, run LIVE-ONLY (`offer_backfill_secs = 0`). The re-drive
    /// reads the store, not the relay, so it has NO backfill dependence: an open-pool capacity-skip is
    /// revisited even on a seller that keeps nothing for stored open-pool re-delivery — the exact case
    /// a relay re-subscribe could never reach (its untargeted replay is `since(now).limit(0)`), and
    /// what closes #519 in full.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconsider_backfills_a_capacity_skipped_open_pool_offer_when_a_slot_frees() {
        drive_capacity_skip_backfill("cap-skip-open-pool", true, 0).await;
    }

    /// An OPEN-POOL offer signed by `buyer` with an explicit wire `created_at` — the field #604's
    /// admission age gate reads (a live/backfilled event carries its real authored-at second), so a
    /// test must be able to author an offer "in the past". `task` differs per offer: an event id is a
    /// content hash, so identical content collapses to ONE id.
    fn open_pool_offer(
        buyer: &Keys,
        task: &str,
        amount: u64,
        deadline_unix: u64,
        created_at: u64,
    ) -> nostr_sdk::Event {
        let draft =
            crate::gateway::OfferDraft::untargeted(task, "", amount, deadline_unix).to_event_draft();
        crate::gateway::nostr::event_builder(&draft)
            .expect("offer event builder")
            .custom_created_at(nostr_sdk::Timestamp::from(created_at))
            .sign_with_keys(buyer)
            .expect("sign offer")
    }

    /// #604 REGRESSION (Rocky's 2-prong bar) — the periodic offer-backfill must not re-admit a
    /// long-aged, never-awarded historical, and the fix must NOT weaken the live claim-lapse capacity
    /// guard. Drives the REAL admission path (`on_offer`, which `run_offer_backfill` calls per fetched
    /// event) with offers whose wire `created_at` is authored in the past — the knob the age gate reads.
    ///
    /// - PRONG 1 (re-admit stopped): an offer authored longer ago than the admit horizon, with a
    ///   FAR-FUTURE deadline (so the pre-existing `deadline_unix` gate cannot catch it), is REFUSED —
    ///   never claimed, and it does not consume the freed slot.
    /// - PRONG 2 (capacity guard intact): a FRESH eligible offer STILL claims that freed slot — proving
    ///   the gate left admission working, backfill was not disabled, and claim-lapse still frees slots.
    ///
    /// RED ON THE UNFIXED TIP: with no age gate `classify_offer` returns `Claim` for the aged offer, so
    /// it is claimed and takes the single slot — prong 1's assertions fail. GREEN once the gate lands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_refuses_aged_historical_but_still_admits_a_fresh_offer() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        // 1-slot open-pool seller with immediate claim-lapse (`Some(0)`) so a parked claim frees its
        // slot the instant the sweep runs — the fixture the capacity-guard prong needs.
        let (runner, root) =
            boot_capacity_skip_seller("604-aged-historical", &relay_url, true, 0, Some(0)).await;
        let buyer = Keys::generate();
        let now = now_unix() as u64;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // FREED SLOT — a fresh offer is claimed, then lapses unawarded, returning its slot to
                // the pool. This is the "lapse an unawarded claim" that the aged historical would re-fill.
                let occupier = open_pool_offer(&buyer, "604 occupier", 100, now + 3_600, now);
                runner.on_offer(&occupier).await;
                assert_eq!(runner.slots.available(), 0, "the occupier claims the single slot");
                runner.sweep_lapsed_claims();
                assert_eq!(runner.slots.available(), 1, "the lapsed occupier frees the slot");

                // PRONG 1 — the aged historical (authored before the admit horizon, deadline far in the
                // future) is REFUSED at admission: no claim row, and the freed slot is untouched.
                let aged = open_pool_offer(
                    &buyer,
                    "604 aged historical",
                    100,
                    now + 3_600,
                    now - (OFFER_BACKFILL_WINDOW_SECS + 600),
                );
                runner.on_offer(&aged).await;
                assert_eq!(
                    runner.node.store().claim_row_state(&aged.id.to_hex()).expect("aged claim state"),
                    None,
                    "prong 1: the aged historical is REFUSED — never claimed (backfill re-admit stopped)"
                );
                assert_eq!(
                    runner.slots.available(),
                    1,
                    "prong 1: the aged historical must NOT consume the freed slot"
                );

                // PRONG 2 — a FRESH eligible offer STILL claims the freed slot: the age gate left the
                // capacity path intact (backfill not disabled, claim-lapse still frees slots).
                let fresh = open_pool_offer(&buyer, "604 fresh eligible", 100, now + 3_600, now);
                runner.on_offer(&fresh).await;
                assert_eq!(
                    runner
                        .node
                        .store()
                        .claim_row_state(&fresh.id.to_hex())
                        .expect("fresh claim state")
                        .as_deref(),
                    Some("claimed"),
                    "prong 2: a fresh eligible offer still claims the freed slot"
                );
                assert_eq!(runner.slots.available(), 0, "prong 2: the fresh claim refills the slot");
            })
            .await;

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #456 / #514 END-TO-END PIN — a losing open-pool claimant frees its reserved slot the MOMENT it
    /// sees the award, over the wire, with no lapse involved.
    ///
    /// #450's tests reach slot-release only through the LAPSE sweep (`claim_award_timeout_secs = 0`),
    /// so they can never exercise the award-visibility release #514 shipped. This drives the real
    /// path: TWO one-slot open-pool sellers both claim ONE untargeted offer (each reserving its single
    /// slot), the buyer awards seller A's claim, and seller B — the loser — receives that award
    /// because its open-pool award REQ is UNSCOPED (`award_filter`, #514) and releases its slot + marks
    /// its claim `released` SYNCHRONOUSLY, i.e. without waiting out the 120s claim-award timeout. The
    /// lapse is pinned unreachable here (`claim_award_timeout_secs = Some(3600)`, and no sweep is ever
    /// called), so the AWARD is the ONLY thing that can free B's slot.
    ///
    /// `publish_award` parameterises the non-vacuity foil: with it FALSE the identical harness runs
    /// minus the award and B's slot must STAY reserved (the committed test passes TRUE). Two red-proofs
    /// are recorded on the PR: (a) re-scoping `award_filter` to the pubkey for open-pool ⇒ the loser
    /// never receives the award ⇒ this fails; (b) `publish_award = false` ⇒ nothing frees the slot
    /// in-window ⇒ this fails. Both prove the release is caused by the AWARD arriving — not by a slot
    /// that was never reserved, nor a claim row that defaulted to `released`.
    async fn drive_loser_release_on_award(publish_award: bool) {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        // Two DISTINCT-key one-slot open-pool sellers (distinct labels ⇒ distinct homes ⇒ distinct
        // keys). The large lapse timeout pins the AWARD path as the only slot-release path in-test.
        let (winner, winner_root) =
            boot_capacity_skip_seller("loser-release-winner", &relay_url, true, 0, Some(3600)).await;
        let (loser_runner, loser_root) =
            boot_capacity_skip_seller("loser-release-loser", &relay_url, true, 0, Some(3600)).await;
        // `on_award` binds `self: &Arc<Self>`, so the loser is driven through an Arc.
        let loser = Arc::new(loser_runner);
        let winner_hex = winner.seller_pubkey();

        // A separate buyer/publisher: a node is never delivered its OWN events, so the offer AND the
        // award come from a third key — and that key IS the offer's buyer, the sole authorized awarder
        // (`on_award` checks the award author == the recorded offer buyer).
        let buyer = Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let publisher = Client::new(buyer.clone());
        publisher.add_relay(&relay_url).await.expect("publisher add relay");
        publisher.connect().await;
        publisher.wait_for_connection(Duration::from_secs(5)).await;

        let mut winner_notifs = winner.client.notifications();
        let mut loser_notifs = loser.client.notifications();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                winner
                    .subscribe_offers(None, true)
                    .await
                    .expect("winner offer subscription");
                // The loser needs BOTH the offer REQ (to claim) and the award REQ (to see the award).
                loser
                    .subscribe_all(None)
                    .await
                    .expect("loser offer + award subscriptions");

                let deadline_unix = now_unix() as u64 + 3_600;
                let job_id = post_offer(
                    &publisher,
                    &buyer,
                    "loser-release open-pool offer",
                    None,
                    100,
                    deadline_unix,
                )
                .await;

                // Both sellers record + claim the one untargeted offer, each reserving its lone slot.
                assert!(
                    pump_offers_until(
                        &winner,
                        &mut winner_notifs,
                        std::time::Instant::now() + Duration::from_secs(5),
                        |r| r.node.store().claim_row_state(&job_id).ok().flatten().is_some(),
                    )
                    .await,
                    "the winner must claim the open-pool offer"
                );
                assert!(
                    pump_offers_until(
                        &loser,
                        &mut loser_notifs,
                        std::time::Instant::now() + Duration::from_secs(5),
                        |r| r.node.store().claim_row_state(&job_id).ok().flatten().is_some(),
                    )
                    .await,
                    "the loser must claim the open-pool offer"
                );
                assert_eq!(winner.slots.available(), 0, "the winner's single slot is reserved");
                assert_eq!(loser.slots.available(), 0, "the loser's single slot is reserved");

                // Publish both claims so each has a real id on the wire; the award names the winner's.
                winner.drain().await;
                loser.drain().await;
                let winner_claim_id = match winner.node.store().outbox_row(&format!("claim:{job_id}"))
                {
                    Ok(Some((_, _, Some(published)))) => published,
                    other => panic!("winner claim must be published with an id; got {other:?}"),
                };
                let loser_claim_id = match loser.node.store().outbox_row(&format!("claim:{job_id}")) {
                    Ok(Some((_, _, Some(published)))) => published,
                    other => panic!("loser claim must be published with an id; got {other:?}"),
                };
                assert_ne!(
                    winner_claim_id, loser_claim_id,
                    "two distinct sellers publish two distinct claim events"
                );

                // The buyer AWARDS the winner's claim: e-tags offer-root + winner-claim, p-tags buyer +
                // winner, signed by the buyer (the offer's author). The award p-tags ONLY the winner —
                // the loser sees it solely because its open-pool award REQ is unscoped (#514).
                if publish_award {
                    let award = crate::gateway::award_draft(
                        &job_id,
                        &winner_claim_id,
                        &buyer_hex,
                        &winner_hex,
                    );
                    let event = crate::gateway::nostr::event_builder(&award)
                        .expect("award event builder")
                        .sign_with_keys(&buyer)
                        .expect("sign award as the buyer");
                    publisher.send_event(&event).await.expect("post award to relay");
                }

                // Drive the LOSER's award ingestion off its own notification stream. With the fix the
                // loser receives the award naming another claim → `match_award` → `Release` → slot +
                // durable claim released, SYNCHRONOUSLY (no 120s lapse; none is even reachable here).
                let released = pump_awards_until(
                    &loser,
                    &mut loser_notifs,
                    std::time::Instant::now() + Duration::from_secs(5),
                    |r| {
                        r.slots.available() == 1
                            && r.node
                                .store()
                                .claim_row_state(&job_id)
                                .ok()
                                .flatten()
                                .as_deref()
                                == Some("released")
                    },
                )
                .await;

                assert!(
                    released,
                    "the losing open-pool claimant must free its slot on the AWARD, promptly and \
                     without a lapse (available={}, claim_state={:?})",
                    loser.slots.available(),
                    loser.node.store().claim_row_state(&job_id)
                );
                // The slot is back AND the claim row is `released` — never `awarded`. `release_claim`
                // only moves `claimed`→`released`; the execute path (`record_award`) would have moved
                // it to `awarded`. So `released` positively proves the loser took the Release branch
                // and NEVER executed — a free slot alone could not (a failed execute frees one too).
                assert_eq!(loser.slots.available(), 1, "the loser's reserved slot is released");
                assert_eq!(
                    loser.node.store().claim_row_state(&job_id).expect("loser claim row"),
                    Some("released".to_string()),
                    "the loser's claim row is `released` (Release path), never `awarded` (execute)"
                );
            })
            .await;

        winner.client.disconnect().await;
        loser.client.disconnect().await;
        publisher.disconnect().await;
        let _ = std::fs::remove_dir_all(&winner_root);
        let _ = std::fs::remove_dir_all(&loser_root);
    }

    /// #456 / #514 — the committed GREEN pin: award published, `award_filter` intact. The two RED
    /// foils (award-filter re-scoped; award withheld) are RUN and recorded on the PR, not in CI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_losing_open_pool_claimant_releases_its_slot_when_it_sees_the_award() {
        drive_loser_release_on_award(true).await;
    }

    /// Pump the shared AWARD/ACCEPT stream into `on_accept`. The twin of [`pump_awards_until`]: both
    /// kinds ride ONE REQ, and dispatching only the kind under test keeps the ACCEPT path the sole
    /// cause of whatever the predicate observes.
    async fn pump_accepts_until(
        runner: &Arc<SellerNodeRunner>,
        notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
        deadline: std::time::Instant,
        mut done: impl FnMut(&SellerNodeRunner) -> bool,
    ) -> bool {
        while !done(runner) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if let Ok(Ok(RelayPoolNotification::Event { event, .. })) = tokio::time::timeout(
                remaining.min(Duration::from_millis(150)),
                notifications.recv(),
            )
            .await
            {
                if event.kind.as_u16() == JOB_ACCEPT_KIND {
                    runner.on_accept(&event).await;
                }
            }
        }
        done(runner)
    }

    /// #626 — an ACCEPT that names ANOTHER seat's claim must not bind local state.
    ///
    /// The field shape, reproduced: two one-slot open-pool sellers claim ONE untargeted offer, the
    /// buyer accepts the WINNER's claim, and the loser receives that ACCEPT because its open-pool
    /// award REQ is unscoped (#456 — both kinds ride that one REQ). No award is published at all, so
    /// both seats reach `on_accept` with `job_award_time == None`: the arm that WRITES. Binding there
    /// on claim EXISTENCE alone gives the loser a phantom `awarded` job row, which `jobs_in_flight`
    /// counts and the heartbeat then publishes as `accepting=n` — a seat stranded out of the market
    /// by another seat's win, holding capacity for work it never had.
    ///
    /// The WINNER leg is the anti-vacuity control and it is load-bearing: the same ACCEPT, on the
    /// seat whose claim it names, MUST still bind. Without it a handler that refused every accept
    /// would satisfy the loser assertions completely.
    ///
    /// RED ON REVERT: drop the `match_award` identity match in `on_accept` and the loser binds — its
    /// claim row reads `awarded` and `jobs_in_flight` reads 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_accept_naming_another_seats_claim_never_binds_the_loser() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        let (winner_runner, winner_root) =
            boot_capacity_skip_seller("accept-identity-winner", &relay_url, true, 0, Some(3600))
                .await;
        let (loser_runner, loser_root) =
            boot_capacity_skip_seller("accept-identity-loser", &relay_url, true, 0, Some(3600))
                .await;
        // `on_accept` binds `self: &Arc<Self>`, so both seats are driven through an Arc.
        let winner = Arc::new(winner_runner);
        let loser = Arc::new(loser_runner);
        let winner_hex = winner.seller_pubkey();

        // The buyer is a third key: a node is never delivered its own events, and the accept's author
        // must be the recorded offer's buyer or `on_accept` refuses it before the identity match.
        let buyer = Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let publisher = Client::new(buyer.clone());
        publisher.add_relay(&relay_url).await.expect("publisher add relay");
        publisher.connect().await;
        publisher.wait_for_connection(Duration::from_secs(5)).await;

        let mut winner_notifs = winner.client.notifications();
        let mut loser_notifs = loser.client.notifications();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Both seats need the offer REQ (to claim) and the award/accept REQ (to see it).
                winner.subscribe_all(None).await.expect("winner subscriptions");
                loser.subscribe_all(None).await.expect("loser subscriptions");

                let deadline_unix = now_unix() as u64 + 3_600;
                let job_id = post_offer(
                    &publisher,
                    &buyer,
                    "accept-identity open-pool offer",
                    None,
                    100,
                    deadline_unix,
                )
                .await;

                for (label, seat, notifs) in [
                    ("winner", &winner, &mut winner_notifs),
                    ("loser", &loser, &mut loser_notifs),
                ] {
                    assert!(
                        pump_offers_until(
                            seat,
                            notifs,
                            std::time::Instant::now() + Duration::from_secs(5),
                            |r| r.node.store().claim_row_state(&job_id).ok().flatten().is_some(),
                        )
                        .await,
                        "the {label} must claim the open-pool offer"
                    );
                }

                // Publish both claims so each has a real id on the wire; the accept names the
                // winner's. Without this the loser's own id is unreadable and it would refuse to
                // bind for the WRONG reason (fail-closed), which would not test the identity match.
                winner.drain().await;
                loser.drain().await;
                let published = |seat: &Arc<SellerNodeRunner>, label: &str| match seat
                    .node
                    .store()
                    .outbox_row(&format!("claim:{job_id}"))
                {
                    Ok(Some((_, _, Some(id)))) => id,
                    other => panic!("{label} claim must be published with an id; got {other:?}"),
                };
                let winner_claim_id = published(&winner, "winner");
                let loser_claim_id = published(&loser, "loser");
                assert_ne!(
                    winner_claim_id, loser_claim_id,
                    "two distinct sellers publish two distinct claim events"
                );

                // The buyer ACCEPTS the winner's claim. No AWARD is ever published, so both seats
                // reach the `job_award_time == None` arm — the one that writes.
                let accept = crate::gateway::accept_draft(
                    &job_id,
                    &winner_claim_id,
                    &buyer_hex,
                    &winner_hex,
                );
                let event = crate::gateway::nostr::event_builder(&accept)
                    .expect("accept event builder")
                    .sign_with_keys(&buyer)
                    .expect("sign accept as the buyer");
                publisher.send_event(&event).await.expect("post accept to relay");

                // Both seats leave `claimed` on this ACCEPT — the loser by releasing, the winner by
                // binding — so this predicate terminates on a fixed AND an unfixed tree. It waits for
                // the ACCEPT to be HANDLED rather than asserting on a state that already holds before
                // it arrives, which would pass without the event ever being processed.
                let left_claimed = |r: &SellerNodeRunner| {
                    r.node.store().claim_row_state(&job_id).ok().flatten().as_deref()
                        != Some("claimed")
                };
                let loser_acted = pump_accepts_until(
                    &loser,
                    &mut loser_notifs,
                    std::time::Instant::now() + Duration::from_secs(5),
                    left_claimed,
                )
                .await;
                assert!(
                    loser_acted,
                    "the loser must handle the ACCEPT (claim_state={:?})",
                    loser.node.store().claim_row_state(&job_id)
                );

                // THE DEFECT. A foreign ACCEPT must create no local state.
                assert_eq!(
                    loser.node.store().jobs_in_flight().expect("loser in-flight"),
                    0,
                    "an ACCEPT naming another seat's claim must not bind: the loser holds no \
                     in-flight job (claim_state={:?})",
                    loser.node.store().claim_row_state(&job_id)
                );
                assert_eq!(
                    loser.node.store().claim_row_state(&job_id).expect("loser claim row"),
                    Some("released".to_string()),
                    "the loser's claim is `released` (it lost the race), never `awarded` (bound \
                     from an accept that names someone else)"
                );
                assert_eq!(
                    loser.slots.available(),
                    1,
                    "the loser's reserved slot returns, so the seat keeps advertising capacity"
                );

                // ANTI-VACUITY: the same ACCEPT, on the seat it names, still binds.
                let winner_bound = pump_accepts_until(
                    &winner,
                    &mut winner_notifs,
                    std::time::Instant::now() + Duration::from_secs(5),
                    left_claimed,
                )
                .await;
                assert!(
                    winner_bound,
                    "the winner must handle the ACCEPT (claim_state={:?})",
                    winner.node.store().claim_row_state(&job_id)
                );
                assert_eq!(
                    winner.node.store().claim_row_state(&job_id).expect("winner claim row"),
                    Some("awarded".to_string()),
                    "the ACCEPT names the winner's claim, so the winner binds from it (#143 \
                     across-restart re-bind) — the identity match must not refuse everything"
                );
                assert!(
                    winner.node.store().job_award_time(&job_id).expect("winner award time").is_some(),
                    "the winner's bind records an award row"
                );
            })
            .await;

        winner.client.disconnect().await;
        loser.client.disconnect().await;
        publisher.disconnect().await;
        let _ = std::fs::remove_dir_all(&winner_root);
        let _ = std::fs::remove_dir_all(&loser_root);
    }

    // TOOTH (offer backfill, DEAF-SUB RECOVERY — #560, the acceptance bar) — an offer a silently-deaf
    // live subscription NEVER delivered is recovered by the periodic offer backfill, WITHOUT a restart.
    // This is the offers analog of the wrap backfill's whole reason to exist.
    //
    // The deaf leg is modelled faithfully: the seat boots (its live offer REQ is registered) but the
    // run loop is NOT driven, so NOTHING pumps the offer notification stream into `on_offer` — exactly
    // the field's silently-deaf leg, where a posted offer never reaches `on_offer`. `run_offer_backfill`
    // is then the SOLE path that can claim it, and it recovers because `fetch_events` returns the
    // stored offer off the call itself, past the pool's per-connection seen-cache (a plain re-subscribe
    // would re-REQ but the cache swallows the replay — the trap the fix exists to avoid).
    //
    // The BEFORE block pins the deaf premise (unclaimed with no backfill); the AFTER block is the
    // recovery. RED ON REVERT: empty `run_offer_backfill`'s body (or delete its on_offer/drain loop)
    // → the offer stays unclaimed and the AFTER block goes red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offer_backfill_recovers_an_offer_the_deaf_live_sub_never_delivered() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, Keys};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        // A targeted 1-slot seller. `offer_backfill_secs = 0` proves recovery does not depend on a
        // configured backfill window — even a live-only seat's deaf leg must recover.
        let (runner, root) =
            boot_capacity_skip_seller("offer-backfill-deaf", &relay_url, false, 0, Some(0)).await;
        let seller_hex = runner.seller_pubkey();

        // A separate publisher: a node is never delivered its OWN events.
        let buyer = Keys::generate();
        let publisher = Client::new(buyer.clone());
        publisher.add_relay(&relay_url).await.expect("publisher add relay");
        publisher.connect().await;
        publisher.wait_for_connection(Duration::from_secs(5)).await;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let deadline_unix = now_unix() as u64 + 3_600;
                let job_id = post_offer(
                    &publisher,
                    &buyer,
                    "deaf-sub recovery offer",
                    Some(&seller_hex),
                    100,
                    deadline_unix,
                )
                .await;

                // The deaf premise: nothing drove the live sub into `on_offer`, so the offer is
                // unclaimed. `post_offer` awaited the relay's OK, so it IS stored and fetchable.
                assert!(
                    runner.node.store().offer_facts(&job_id).expect("offer_facts").is_none(),
                    "deaf premise: with no backfill the offer is never claimed (the live sub \
                     delivered nothing to on_offer)"
                );
                assert_eq!(runner.slots.available(), 1, "no slot is taken before the backfill");

                // THE FIX — the periodic offer backfill re-asks the relay via `fetch_events` (past the
                // seen-cache) and feeds the returned offer through `on_offer`, which claims it.
                runner.run_offer_backfill().await;

                assert!(
                    runner.node.store().offer_facts(&job_id).expect("offer_facts").is_some(),
                    "the offer backfill must recover an offer the deaf live sub never delivered"
                );
                assert_eq!(
                    runner.node.store().claim_row_state(&job_id).expect("claim row").as_deref(),
                    Some("claimed"),
                    "the recovered offer is claimed"
                );
                assert_eq!(runner.slots.available(), 0, "the recovered claim takes the slot");
            })
            .await;

        runner.client.disconnect().await;
        publisher.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── #563: the relay-derive belt — a live-deadline residual settled ELSEWHERE ─────────────────
    //
    // `resume_action` RunAgents a slot-occupying row with no delivery/receipt, no pushed commit, and a
    // live deadline. Right for a genuine mid-flight award (THE FOIL), WRONG for a row already settled
    // elsewhere (a buyer receipt) or delivered by a pre-#552 binary (our own result on the relay). The
    // belt refines it: SKIP only on the POSITIVE presence of a settlement event the relay actually
    // returned; ABSENCE (incl. relay-deafness, #560) always runs the agent. The two positive tests
    // exercise BOTH spine arms (our result; a buyer receipt with another seat); the foil is the spine's
    // proof that absence never strands.

    /// Boot a 1-slot seller AND return its seller Keys, so a test can publish an event AUTHORED BY the
    /// seller (its own kind-3403 result) that `fetch_events` reads back past the per-connection
    /// seen-cache. Mirrors [`boot_capacity_skip_seller`]; kept separate so the common helper stays
    /// keyless. `offer_backfill_secs = 0` — the belt depends on no configured backfill window.
    async fn boot_seller_with_keys(
        label: &str,
        relay_url: &str,
    ) -> (SellerNodeRunner, Keys, std::path::PathBuf) {
        let root = temp_dir(label);
        let _ = std::fs::remove_dir_all(&root);
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = relay_url.to_string();
        let mut seller = seller_cfg(1, false);
        seller.claim_award_timeout_secs = Some(0);
        seller.offer_backfill_secs = 0;
        home.config.seller = Some(seller);
        let secret = crate::home::read_secret_key_hex(&home).expect("read seller secret");
        let keys = Keys::parse(&secret).expect("parse seller keys");
        let runner = SellerNodeRunner::boot(home)
            .await
            .expect("boot the settled-elsewhere seller against the fixture relay");
        (runner, keys, root)
    }

    /// Seed the store with the exact residual `resume_action` RunAgents: a recorded offer (live
    /// deadline) + a parked claim + an award ⇒ a jobs row in `awarded`, no delivery/receipt/pushed, no
    /// settled marker. The row #563's belt refines.
    fn seed_residual_awarded_job(
        runner: &SellerNodeRunner,
        job_id: &str,
        buyer_hex: &str,
        deadline_unix: i64,
        now: i64,
    ) {
        let store = runner.node.store();
        store
            .record_offer(
                &crate::seller_node::store::Offer {
                    offer_id: job_id.to_owned(),
                    buyer_pubkey: buyer_hex.to_owned(),
                    amount_sats: 100,
                    unit: "sat".to_owned(),
                    task: "residual".to_owned(),
                    deadline_unix,
                    targeted: true,
                    requested_agent: None,
                    output: Some("text/plain".to_owned()),
                },
                now,
            )
            .expect("record offer");
        let draft = claim_draft(job_id, buyer_hex, &"s".repeat(64), "creq", &[], &Default::default());
        store
            .claim_and_enqueue(job_id, job_id, "creq", &draft, now, now + 3_600, now)
            .expect("claim");
        store
            .record_award(&"a".repeat(64), job_id, buyer_hex, now)
            .expect("award");
    }

    /// Compose the resume decision exactly as `execute_job` does on resume: read every durable marker
    /// (incl. the #563 settled-elsewhere marker) and feed them to `resume_action`.
    fn compose_resume_action(runner: &SellerNodeRunner, job_id: &str, now: i64) -> ResumeAction {
        let store = runner.node.store();
        let state = store.job_state(job_id).expect("job_state").expect("job row present");
        let has_delivery = store.has_delivery(job_id).expect("has_delivery");
        let has_receipt = store.has_receipt(job_id).expect("has_receipt");
        let settled = store.has_settled_elsewhere(job_id).expect("has_settled_elsewhere");
        let pushed = store.pushed_commit(job_id).expect("pushed_commit");
        let deadline = store.offer_row(job_id).expect("offer_row").map(|o| o.deadline_unix);
        resume_action(state, has_delivery, has_receipt, settled, pushed, deadline, now)
    }

    // POSITIVE (spine arm 1) — OUR OWN result is on the relay (delivered by a pre-#552 binary, so no
    // local delivery marker). The belt fetches it, marks the row settled-elsewhere, and the resume
    // composes to SkipTerminal instead of re-emitting a duplicate result inside the live window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn belt_skips_a_live_residual_when_our_result_is_on_the_relay() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        let (runner, seller_keys, root) = boot_seller_with_keys("belt-our-result", &relay_url).await;
        let buyer = Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        // A valid 64-hex offer id (== job_id); the published result roots THIS id, so `#e` matches.
        let job_id = "1".repeat(64);

        // A separate publisher SENDS the pre-signed result; the AUTHOR is the seller (signed with its
        // keys), which is what the belt's result-branch author-check requires.
        let publisher = Client::new(Keys::generate());
        publisher.add_relay(&relay_url).await.expect("add relay");
        publisher.connect().await;
        publisher.wait_for_connection(Duration::from_secs(5)).await;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let now = now_unix();
                let deadline = now + 3_600; // LIVE
                seed_residual_awarded_job(&runner, &job_id, &buyer_hex, deadline, now);
                assert!(
                    !runner.node.store().has_settled_elsewhere(&job_id).expect("pre"),
                    "unmarked before the derive"
                );
                assert_eq!(
                    compose_resume_action(&runner, &job_id, now),
                    ResumeAction::RunAgent,
                    "PRECONDITION: with no relay evidence this residual would RunAgent (non-vacuous)"
                );

                let result = crate::gateway::result_draft(
                    &job_id, &buyer_hex, "output-ref", 100, "job-hash", "seller-sig", "", None, &[],
                );
                let result_event = crate::gateway::nostr::event_builder(&result)
                    .expect("result builder")
                    .sign_with_keys(&seller_keys) // AUTHORED BY THE SELLER (our own result)
                    .expect("sign result");
                publisher.send_event(&result_event).await.expect("publish our result");

                let settled = runner
                    .settled_elsewhere_on_relay(&job_id, Some(&buyer_hex), now)
                    .await;
                assert!(settled, "our own result on the relay ⇒ settled-elsewhere POSITIVE");
                assert!(
                    runner.node.store().has_settled_elsewhere(&job_id).expect("post"),
                    "the marker is armed AFTER the positive read (arm-after-the-event)"
                );
                assert_eq!(
                    compose_resume_action(&runner, &job_id, now),
                    ResumeAction::SkipTerminal,
                    "the belt reclassifies the live residual as terminal — no re-run, no duplicate 3403"
                );
            })
            .await;

        runner.client.disconnect().await;
        publisher.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // POSITIVE (spine arm 2) — a buyer RECEIPT for the offer, co-signed with ANOTHER seat. The offer is
    // terminal (settled elsewhere) even though we hold an awarded row and never delivered; the belt
    // skips rather than burning compute on finished work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn belt_skips_a_live_residual_when_a_buyer_receipt_settled_elsewhere() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        let (runner, root) = boot_capacity_skip_seller("belt-buyer-receipt", &relay_url, true, 0, Some(0)).await;
        let buyer = Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "2".repeat(64);

        let publisher = Client::new(buyer.clone());
        publisher.add_relay(&relay_url).await.expect("add relay");
        publisher.connect().await;
        publisher.wait_for_connection(Duration::from_secs(5)).await;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let now = now_unix();
                let deadline = now + 3_600; // LIVE
                seed_residual_awarded_job(&runner, &job_id, &buyer_hex, deadline, now);

                // A co-signed 3400 receipt settling THIS offer with ANOTHER seat, authored by the buyer
                // (the #541 buyer-binding — a forged receipt with a foreign author would never count).
                let other_seller = Keys::generate().public_key().to_hex();
                let receipt = crate::gateway::receipt_draft(
                    &job_id, "result-id", &buyer_hex, &other_seller, "https://mint.invalid", 100,
                    "job-hash", "seller-sig", "buyer-sig", None, None, &[],
                );
                let receipt_event = crate::gateway::nostr::event_builder(&receipt)
                    .expect("receipt builder")
                    .sign_with_keys(&buyer)
                    .expect("sign receipt");
                publisher.send_event(&receipt_event).await.expect("publish buyer receipt");

                let settled = runner
                    .settled_elsewhere_on_relay(&job_id, Some(&buyer_hex), now)
                    .await;
                assert!(settled, "a buyer receipt for the offer ⇒ settled-elsewhere POSITIVE (another seat)");
                assert!(
                    runner.node.store().has_settled_elsewhere(&job_id).expect("post"),
                    "marker armed after the positive read"
                );
                assert_eq!(
                    compose_resume_action(&runner, &job_id, now),
                    ResumeAction::SkipTerminal,
                    "settled elsewhere ⇒ terminal, never re-driven"
                );
            })
            .await;

        runner.client.disconnect().await;
        publisher.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // THE FOIL (the spine's proof) — the SAME live residual, but the relay holds NO settlement for THIS
    // job (a decoy receipt for a DIFFERENT offer proves the relay answers AND that the `#e` match is
    // EXACT — the #562 hazard). Absence — a relay-deaf empty return manufactures it (#560) — must NEVER
    // strand a real award: the derive returns false, no marker is written, and the resume runs the
    // agent. RunAgent is the SAFE branch here (the receipt gate holds the money line; the lapse check
    // bounds wasted compute).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn belt_runs_the_agent_when_the_relay_holds_no_settlement_the_foil() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        let (runner, root) = boot_capacity_skip_seller("belt-foil", &relay_url, true, 0, Some(0)).await;
        let buyer = Keys::generate();
        let buyer_hex = buyer.public_key().to_hex();
        let job_id = "3".repeat(64);

        let publisher = Client::new(buyer.clone());
        publisher.add_relay(&relay_url).await.expect("add relay");
        publisher.connect().await;
        publisher.wait_for_connection(Duration::from_secs(5)).await;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let now = now_unix();
                let deadline = now + 3_600; // LIVE
                seed_residual_awarded_job(&runner, &job_id, &buyer_hex, deadline, now);

                // A DECOY receipt for a DIFFERENT offer id: the relay IS live and answering, but nothing
                // settles OUR job. An exact `#e` match must exclude it (a substring/prefix match would
                // not — the #562 hazard).
                let decoy_offer = "9".repeat(64);
                let other_seller = Keys::generate().public_key().to_hex();
                let decoy = crate::gateway::receipt_draft(
                    &decoy_offer, "result-id", &buyer_hex, &other_seller, "https://mint.invalid", 100,
                    "job-hash", "seller-sig", "buyer-sig", None, None, &[],
                );
                let decoy_event = crate::gateway::nostr::event_builder(&decoy)
                    .expect("decoy builder")
                    .sign_with_keys(&buyer)
                    .expect("sign decoy");
                publisher.send_event(&decoy_event).await.expect("publish decoy receipt");

                let settled = runner
                    .settled_elsewhere_on_relay(&job_id, Some(&buyer_hex), now)
                    .await;
                assert!(
                    !settled,
                    "no settlement for THIS job ⇒ ABSENCE, never a skip (the FOIL must not strand)"
                );
                assert!(
                    !runner.node.store().has_settled_elsewhere(&job_id).expect("no mark"),
                    "absence never pre-marks — the row stays re-checkable"
                );
                assert_eq!(
                    compose_resume_action(&runner, &job_id, now),
                    ResumeAction::RunAgent,
                    "absence ⇒ RunAgent (over-skipping a live award strands it — worse than a bounded replay)"
                );
            })
            .await;

        runner.client.disconnect().await;
        publisher.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (wrap backfill) — the cursor keeps an OLDER delivered-but-unpaid job's payment window in
    // range, and a read failure ABORTS rather than silently becoming a full-history rescan.
    //
    // The clamp is the whole point: a receipt collected for a NEWER job must not advance the cursor
    // past an OLDER unsettled delivery, or that job's payment wrap falls out of every future backfill
    // and the payment is stranded forever — which is the failure this backfill exists to recover.
    //
    // BITE: drop the `.min(oldest - margin)` clamp and `cursor_stays_behind_an_unsettled_delivery`
    // goes red; make the error arm fall back to 0 and the abort assertion goes red.
    #[test]
    fn wrap_backfill_cursor_clamps_to_the_oldest_unsettled_delivery_and_fails_closed() {
        use super::super::store::StoreError;

        // Nothing collected, nothing unsettled ⇒ 0 is legitimate (first boot), not an error.
        assert_eq!(resolve_backfill_since(Ok(None), Ok(None)).expect("fresh"), 0);

        // A receipt at t=10_000 with NO unsettled delivery ⇒ cursor is the receipt.
        assert_eq!(
            resolve_backfill_since(Ok(Some(10_000)), Ok(None)).expect("settled"),
            10_000
        );

        // cursor_stays_behind_an_unsettled_delivery: a NEWER receipt must not step over an OLDER
        // delivered-but-unpaid job — the cursor clamps to that job's delivery minus the skew margin.
        let cursor = resolve_backfill_since(Ok(Some(10_000)), Ok(Some(6_000))).expect("clamped");
        assert_eq!(cursor, (6_000 - WRAP_BACKFILL_MARGIN_SECS) as u64);
        assert!(
            cursor < 6_000,
            "the cursor must sit BEFORE the unsettled delivery or its wrap is never re-fetched"
        );

        // Fail-closed: a store READ ERROR aborts the cycle. Falling back to 0 would turn a transient
        // failure into a full-history rescan.
        assert!(
            resolve_backfill_since(Err(StoreError("boom".into())), Ok(None)).is_err(),
            "a cursor read failure must abort, never default to since=0"
        );
        assert!(
            resolve_backfill_since(Ok(Some(1)), Err(StoreError("boom".into()))).is_err(),
            "an unsettled-delivery read failure must abort too"
        );
    }

    // TOOTH (offer backfill, CURSOR — #560) — the periodic offer-backfill `since` cursor is a bounded
    // lookback: never shorter than the window floor, widened to the configured backfill, and clamped
    // at the epoch so a near-epoch clock cannot underflow. Offers carry no store cursor to clamp to
    // (unlike wraps); the classify deadline gate is their staleness backstop, so a purely time-bounded
    // window is the whole design — this pins that it stays BOUNDED and never narrows below the floor.
    //
    // BITE: drop the `.max(offer_backfill_secs)` and the widen case goes red; drop `saturating_` and
    // the near-epoch case panics on debug overflow.
    #[test]
    fn offer_backfill_since_is_a_bounded_lookback_widened_to_the_configured_window() {
        // Floor: a 0 configured backfill still looks back the full window (a live-only seat's deaf leg
        // must still recover — the window is not the config).
        assert_eq!(
            resolve_offer_backfill_since(100_000, 0),
            nostr_sdk::Timestamp::from(100_000 - OFFER_BACKFILL_WINDOW_SECS)
        );
        // A configured backfill NARROWER than the floor does not shrink the window.
        assert_eq!(
            resolve_offer_backfill_since(100_000, OFFER_BACKFILL_WINDOW_SECS - 1),
            nostr_sdk::Timestamp::from(100_000 - OFFER_BACKFILL_WINDOW_SECS)
        );
        // A configured backfill WIDER than the floor widens the recovery window to match, so periodic
        // recovery is never narrower than the boot backfill the operator asked for.
        let wide = OFFER_BACKFILL_WINDOW_SECS + 5_000;
        assert_eq!(
            resolve_offer_backfill_since(100_000, wide),
            nostr_sdk::Timestamp::from(100_000 - wide)
        );
        // Clamp: a `now` inside the window saturates to the epoch, never underflows.
        assert_eq!(
            resolve_offer_backfill_since(10, 0),
            nostr_sdk::Timestamp::from(0u64)
        );
    }

    // TOOTH (wrap backfill, store) — the two cursor readers answer over real rows: a delivered job
    // with no receipt is "unsettled" and pins the cursor; collecting its receipt releases it.
    #[test]
    fn unsettled_delivery_pins_the_cursor_until_its_receipt_lands() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let job = "a".repeat(64);
        let buyer = "b".repeat(64);
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, 4242);

        assert_eq!(store.last_receipt_unix().expect("receipts"), None);
        assert_eq!(
            store.oldest_unsettled_delivery_unix().expect("unsettled"),
            None,
            "nothing delivered yet"
        );

        let draft = claim_draft(&job, &buyer, &seller, &creq, &[], &Default::default());
        let delivered_at = 6_000;
        assert!(store
            .deliver_and_enqueue(
                &job,
                &"c".repeat(40),
                &draft,
                delivered_at,
                delivered_at + RESULT_PUBLISH_WINDOW_SECS,
                delivered_at
            )
            .expect("deliver"));
        assert_eq!(
            store.oldest_unsettled_delivery_unix().expect("unsettled"),
            Some(delivered_at),
            "a delivered job with no receipt is unsettled and must pin the backfill cursor"
        );

        // The payment lands ⇒ no longer unsettled, and the receipt time becomes the cursor.
        store
            .collect_receipt(&"d".repeat(64), &job, 21, 10_000)
            .expect("collect");
        assert_eq!(store.last_receipt_unix().expect("receipts"), Some(10_000));
        assert_eq!(
            store.oldest_unsettled_delivery_unix().expect("unsettled"),
            None,
            "a settled delivery must stop pinning the cursor"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (#171 layer 3) — recovery attempts back off instead of re-dialing at a flat interval,
    // and the backoff is capped so one bounded recovery still fits inside a heartbeat interval.
    #[test]
    fn recovery_backoff_grows_and_stays_capped() {
        assert_eq!(recovery_backoff(1), RECOVERY_BACKOFF);
        assert!(
            recovery_backoff(2) > recovery_backoff(1),
            "a flat retry interval is what hammered shared relay infrastructure"
        );
        for attempt in 1..=64 {
            assert!(
                recovery_backoff(attempt) <= RECOVERY_BACKOFF_MAX,
                "attempt {attempt} exceeded the cap"
            );
        }
    }

    // TOOTH (buyer-facing feedback) — a TARGETED under-rate refusal surfaces a 3404 to the buyer;
    // open-pool under-rate, an at/above-rate offer, and a lapsed skip never do.
    #[test]
    fn under_rate_feedback_only_for_targeted_under_rate_rate_gate() {
        // Targeted-to-self + under-rate + RateGate ⇒ publish (buyer learns why).
        assert!(should_publish_under_rate_feedback(SkipReason::RateGate, true, 1, 5));
        // Open-pool (not targeted-to-self) under-rate ⇒ log-only (spam guard).
        assert!(!should_publish_under_rate_feedback(SkipReason::RateGate, false, 1, 5));
        // Targeted but at/above rate ⇒ no refusal feedback.
        assert!(!should_publish_under_rate_feedback(SkipReason::RateGate, true, 5, 5));
        // A lapsed skip never emits under-rate feedback, even if targeted + under-rate.
        assert!(!should_publish_under_rate_feedback(SkipReason::Lapsed, true, 1, 5));
    }

    // TOOTH (#582, bounded) — the fed-offer set is FIFO-bounded so a long-running seller never leaks
    // memory: a new offer past the cap evicts the OLDEST id (the one most likely already aged past the
    // backfill lookback), and an evicted id fails open — it may surface ONE more feedback if re-ingested,
    // exactly the pre-#582 duplicate this bounds, never worse.
    #[test]
    fn fed_under_rate_offers_fifo_bounds_the_set() {
        let mut f = FedUnderRateOffersInner::new(2); // cap = 2
        f.record("o1");
        f.record("o2");
        assert_eq!(f.len(), 2);
        f.record("o3"); // evicts o1 (oldest first-seen)
        assert_eq!(f.len(), 2, "the set never exceeds its cap");
        assert!(!f.contains("o1"), "the oldest id aged out ⇒ can re-emit once (fail-open)");
        assert!(f.contains("o2") && f.contains("o3"), "the two newest ids stay suppressed");
    }

    // TOOTH (#582, idempotent) — re-recording the same offer (the backfill re-ingesting it every tick)
    // is a no-op: the id is tracked once, not once per pass, so the set grows only with DISTINCT
    // under-rate offers.
    #[test]
    fn fed_under_rate_offers_record_is_idempotent() {
        let mut f = FedUnderRateOffersInner::new(4);
        f.record("offerX");
        f.record("offerX");
        f.record("offerX");
        assert_eq!(f.len(), 1, "one id however many re-ingests");
        assert!(f.contains("offerX"));
    }

    /// A targeted (non-open-pool) seller wired to `relay_url` with a controllable `rate_sats`, so a
    /// test can post an offer BELOW the floor (`amount < rate_sats`). [`boot_capacity_skip_seller`]
    /// pins rate_sats=1, leaving no room for an under-rate offer; this one does.
    async fn boot_seller_with_rate(
        label: &str,
        relay_url: &str,
        rate_sats: u64,
    ) -> (SellerNodeRunner, std::path::PathBuf) {
        let root = temp_dir(label);
        let _ = std::fs::remove_dir_all(&root);
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = relay_url.to_string();
        home.config.seller = Some(seller_cfg(rate_sats, false));
        let runner = SellerNodeRunner::boot(home)
            .await
            .expect("boot the under-rate seller against the fixture relay");
        (runner, root)
    }

    /// A targeted under-rate offer (`amount` below the seller's floor) signed by `buyer`, addressed to
    /// `seller_hex`. `task` MUST differ between offers meant to be distinct: an event id is the hash of
    /// its content, so two byte-identical offers collapse to ONE id (the trap `post_offer` documents).
    fn under_rate_offer(buyer: &Keys, seller_hex: &str, task: &str, amount: u64) -> nostr_sdk::Event {
        let deadline_unix = now_unix() as u64 + 3_600;
        let draft = crate::gateway::OfferDraft::new(task, "", amount, deadline_unix, seller_hex)
            .to_event_draft();
        crate::gateway::nostr::event_builder(&draft)
            .expect("offer event builder")
            .sign_with_keys(buyer)
            .expect("sign offer")
    }

    /// A fresh third-party client connected to `relay_url`, used to READ the feedback the seller
    /// publishes. Separate from the seller because a node is not re-served its own events on a live
    /// sub; a stored fetch from another key is the clean read.
    async fn connect_observer(relay_url: &str) -> Client {
        let observer = Client::new(Keys::generate());
        observer.add_relay(relay_url).await.expect("observer add relay");
        observer.connect().await;
        observer.wait_for_connection(Duration::from_secs(5)).await;
        observer
    }

    /// Count the `BelowRate` feedback events (kind-3404) the seller published for `offer`. `on_offer`
    /// awaits the publish (`send_event_to` resolves after the relay's OK), so by the time this runs the
    /// relay holds every feedback the drive can produce and a single fetch is deterministic.
    async fn count_under_rate_feedbacks(observer: &Client, offer: &nostr_sdk::Event) -> usize {
        let filter = Filter::new()
            .kind(Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND))
            .event(offer.id);
        observer
            .fetch_events(filter, Duration::from_secs(5))
            .await
            .expect("fetch under-rate feedbacks")
            .len()
    }

    // TOOTH (#582, whole-path RED-PROVE) — the #560 offer-backfill re-feeds every stored offer through
    // `on_offer` each tick, so a targeted under-rate offer sitting in the lookback would re-emit its
    // `BelowRate` buyer-feedback on EVERY pass (~12×/window in prod). The first-sight dedup collapses
    // that to ONE feedback per offer per boot. This drives the real path: a LocalRelay holds the 3404s
    // the seller actually publishes, and the SAME offer is fed twice (a live sighting + a backfill
    // re-ingest a tick later).
    //
    // RED ON REVERT: drop the `!self.fed_under_rate_offers.contains(&offer_id) &&` first-sight guard at
    // the under-rate emit in `on_offer` and the re-ingest re-emits, so the relay holds 2 distinct
    // feedbacks and the `== 1` assertion fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn under_rate_feedback_emitted_once_across_backfill_re_ingest() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        // Rate floor 100 sat ⇒ a 10-sat TARGETED offer is under-rate (the RateGate skip that earns
        // buyer feedback; open-pool under-rate stays log-only).
        let (runner, root) = boot_seller_with_rate("under-rate-dedup", &relay_url, 100).await;
        let seller_hex = runner.seller_pubkey();
        let buyer = Keys::generate();
        let offer = under_rate_offer(&buyer, &seller_hex, "under-rate re-ingest", 10);
        let observer = connect_observer(&relay_url).await;

        // A live sighting emits the feedback and records the offer as fed. `contains` becoming true
        // proves the record-AFTER-successful-emit path fired (the send returned Ok ⇒ relay stored it).
        runner.on_offer(&offer).await;
        assert!(
            runner.fed_under_rate_offers.contains(&offer.id.to_hex()),
            "a successful under-rate emit must record the offer as fed (record-after-success)"
        );

        // Advance past the 1-second created_at granularity before the re-ingest. In prod the backfill
        // re-ingests ~300s later, so its feedback carries a LATER created_at and is a DISTINCT wire
        // event the buyer really receives; making the two would-be events distinct here means the ONLY
        // thing that can collapse them to one is the #582 dedup gate, never the relay's own id-dedup.
        tokio::time::sleep(Duration::from_millis(1_200)).await;

        // The backfill re-ingesting the SAME stored offer must NOT re-emit.
        runner.on_offer(&offer).await;

        assert_eq!(
            count_under_rate_feedbacks(&observer, &offer).await,
            1,
            "a re-ingested under-rate offer must surface the BelowRate feedback exactly once"
        );

        observer.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (#582, non-vacuity) — the dedup is per offer id, NOT a global one-shot: two DISTINCT
    // targeted under-rate offers each still surface their OWN `BelowRate` feedback. Guards against a
    // dedup that suppresses the second offer because ANY under-rate feedback was already sent.
    //
    // RED ON REVERT: swap the per-offer_id set for a single "already fed anyone" bool and the second
    // offer is suppressed — its feedback count drops to 0 and its assertion fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn distinct_under_rate_offers_each_get_their_own_feedback() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();

        let (runner, root) = boot_seller_with_rate("under-rate-per-offer", &relay_url, 100).await;
        let seller_hex = runner.seller_pubkey();
        let buyer = Keys::generate();
        // Distinct tasks ⇒ distinct offer ids ⇒ distinct feedback events (no created_at gap needed).
        let offer_a = under_rate_offer(&buyer, &seller_hex, "under-rate A", 10);
        let offer_b = under_rate_offer(&buyer, &seller_hex, "under-rate B", 20);
        assert_ne!(offer_a.id, offer_b.id, "the two offers must be distinct events");
        let observer = connect_observer(&relay_url).await;

        runner.on_offer(&offer_a).await;
        runner.on_offer(&offer_b).await;

        assert_eq!(
            count_under_rate_feedbacks(&observer, &offer_a).await,
            1,
            "offer A gets its own feedback"
        );
        assert_eq!(
            count_under_rate_feedbacks(&observer, &offer_b).await,
            1,
            "offer B ALSO gets its own feedback — the dedup is per offer id, not a global one-shot"
        );
        assert_eq!(
            runner.fed_under_rate_offers.len(),
            2,
            "both distinct offers are recorded as fed"
        );

        observer.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (idempotency, live-caught) — the execute guard keys on job_state: a job already DELIVERED
    // or PAID is not re-execute-eligible, so a DUPLICATE award (a second award_id for the same job —
    // seen live in the smoke) does no second agent run (no wasted operator compute) and never clobbers
    // the terminal state. Bite: were should_resume_execution to admit Delivered/Paid, execute_job
    // would re-run the agent — the assertions here go red (and so does the resume-selection tooth).
    #[test]
    fn delivered_or_paid_job_is_not_re_executed() {
        use crate::seller_node::store::{Collected, JobState};
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let job = "a".repeat(64);
        let buyer = "b".repeat(64);
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, 4242);
        let draft = claim_draft(&job, &buyer, &seller, &creq, &[], &Default::default());

        // Deliver ⇒ state Delivered ⇒ NOT re-execute-eligible (the guard early-returns).
        assert!(store
            .deliver_and_enqueue(&job, &"c".repeat(40), &draft, 5000, 5000 + RESULT_PUBLISH_WINDOW_SECS, 5000)
            .expect("deliver"));
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Delivered));
        assert!(
            !should_resume_execution(store.job_state(&job).expect("s").expect("s")),
            "a delivered job is not re-execute-eligible"
        );

        // Pay ⇒ state Paid ⇒ likewise not re-execute-eligible (terminal never clobbered).
        assert_eq!(
            store.collect_receipt(&"e".repeat(64), &job, 21, 6000).expect("collect"),
            Collected::New
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Paid));
        assert!(
            !should_resume_execution(store.job_state(&job).expect("s").expect("s")),
            "a paid job must not re-execute"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (award dedup, LEG 1 — the retry shape, post-terminal). TWO DISTINCT award events for ONE
    // job id. That is the shape a buyer retry produces, and it is NOT the shape a relay redelivery
    // produces: the two take DIFFERENT dedup keys, so exercising one says nothing about the other.
    // `awards.award_id` is the PK, so a RE-SEEN award id is absorbed at the record layer
    // (`Awarded::Duplicate`, no job row touched). A FRESH award id for a job we already hold is
    // `Awarded::New` — it reaches the spawn, takes an execution slot, and the only thing between it and
    // a second agent run is the job-STATE guard at the top of `execute_job`. So the contract worth
    // asserting is the COMPOSITION (record decision AND state guard), never either half alone:
    // asserting the predicate by itself leaves the record layer's answer unstated, and that answer is
    // what decides whether the guard is load-bearing at all.
    #[test]
    fn a_fresh_award_id_for_a_delivered_job_is_recorded_but_never_re_executed() {
        use crate::seller_node::store::{Awarded, JobState};
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let job = "a".repeat(64);
        let buyer = "b".repeat(64);
        // The helper journals award event A (`w`×64) — that is execution #1's award.
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, 4242);
        let draft = claim_draft(&job, &buyer, &seller, &creq, &[], &Default::default());

        // Execution #1 finished and published ⇒ Delivered.
        assert!(
            store
                .deliver_and_enqueue(
                    &job,
                    &"c".repeat(40),
                    &draft,
                    5000,
                    5000 + RESULT_PUBLISH_WINDOW_SECS,
                    5000
                )
                .expect("deliver")
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Delivered));

        // REDELIVERY shape: award A seen a second time. The event-id key absorbs it; execute is never
        // reached, so the state guard is not even consulted.
        assert_eq!(
            store.record_award(&"w".repeat(64), &job, &buyer, 6000).expect("re-see A"),
            Awarded::Duplicate,
            "a re-seen award id must be absorbed by the award-record key"
        );

        // RETRY shape: a DIFFERENT award event for the SAME job. The record layer does NOT catch this
        // one — new row, and it re-attempts job creation.
        assert_eq!(
            store.record_award(&"d".repeat(64), &job, &buyer, 6001).expect("fresh B"),
            Awarded::New,
            "a fresh award id is NOT deduped at the record layer — the retry shape reaches the spawn"
        );

        // The job row survives that re-attempt, so the state guard is what refuses the second run.
        assert_eq!(
            store.job_state(&job).expect("state"),
            Some(JobState::Delivered),
            "a second award must never clobber a terminal job state back to awarded"
        );
        assert!(
            !should_resume_execution(store.job_state(&job).expect("s").expect("s")),
            "a delivered job must not re-execute when a SECOND award event arrives"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ⚠⚠ LEG 2 (#279) — THIS TEST PASSING IS NOT GOOD NEWS. It pins a known defect in executable form.
    //
    // Same retry shape as the test above (two distinct award ids, one job id), but arriving MID-FLIGHT
    // while the job is still `awarded`/`executing` rather than terminal. NEITHER dedup layer catches
    // it: the award record keys on EVENT ID (a fresh id ⇒ `Awarded::New`) and the execute guard keys on
    // JOB STATE (`awarded`/`executing` ⇒ proceed). Nothing sits between them — there is no per-job
    // in-flight lock — so a second `execute_job` is admitted for a job that is already running: a
    // second agent process and a second delivery attempt.
    //
    // Note what it does to capacity, because it is worse than double-booking a slot: the first award
    // already REMOVED this job's parked permit (`take_for_execution` is a map `remove`), so the second
    // award's `take_for_execution` returns `None` and the second execution runs holding NO permit at
    // all. It does not consume a slot — it BYPASSES slot accounting, so the node exceeds its
    // configured concurrency without that showing up in `available()`.
    //
    // It is NOT a payment defect. Double payment is blocked further down the path (reservation PK, a
    // single job-keyed watcher, outbox dedup); that path was verified by the maxplayer lead, not here, and
    // this comment should not be read as this test having checked it. The costs are operator compute,
    // slot accounting, and a second push that can move the branch tip off the journaled commit — which
    // the buyer then refuses on tip-match, i.e. it fails toward NOT paying a seller who did the work.
    //
    // 🛑 IF THIS TEST GOES RED, THE FIX LANDED — do not "repair" it. Invert the final assertion to the
    // guarded behaviour and close #279. It is written as a PASSING tripwire rather than an
    // `#[ignore]`d failing test precisely so it cannot rot in silence: an ignored test reports nothing
    // whether the hole is open or closed.
    #[test]
    fn mid_flight_second_award_is_admitted_for_execution_no_per_job_in_flight_lock() {
        use crate::seller_node::store::{Awarded, JobState};
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let buyer = "b".repeat(64);

        // Both mid-flight states, because the guard admits both and either one means a second run.
        for (label, drive_to_executing, expected) in [
            ("awarded", false, JobState::Awarded),
            ("executing", true, JobState::Executing),
        ] {
            let job = "a".repeat(64);
            let (store, root) = store_with_awarded_job(&creq, &job, &buyer, 4242);
            if drive_to_executing {
                store.mark_executing(&job, 4300).expect("mark executing");
            }
            assert_eq!(
                store.job_state(&job).expect("state"),
                Some(expected),
                "{label}: precondition — the job is mid-flight, not terminal"
            );

            // A DIFFERENT award event for the SAME job, landing while execution #1 is in flight.
            assert_eq!(
                store.record_award(&"d".repeat(64), &job, &buyer, 4400).expect("fresh award"),
                Awarded::New,
                "{label}: the award record keys on event id, so a fresh id is not deduped"
            );
            assert!(
                should_resume_execution(store.job_state(&job).expect("s").expect("s")),
                "{label}: MID-FLIGHT SECOND EXECUTION IS ADMITTED — if THIS line is what failed, the \
                 per-job in-flight lock has landed: invert this assertion and close #279"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    // TOOTH (buyer-facing feedback) — an execution failure produces a buyer-addressed feedback-kind
    // `status=error` carrying the (path-free) reason AND the §10 `reason_code=execution_failed` tag,
    // so the buyer learns the job failed and can CLASS it without parsing prose (the silence the live
    // smoke's first attempt exposed).
    #[test]
    fn execution_failure_feedback_is_a_buyer_addressed_error() {
        let draft = error_draft(
            "offer1",
            "buyerpk",
            &"s".repeat(64),
            ReasonCode::ExecutionFailed,
            EXEC_FAILURE_FEEDBACK,
        );
        assert_eq!(draft.kind, crate::kinds::JOB_FEEDBACK_KIND);
        assert_eq!(draft.content, EXEC_FAILURE_FEEDBACK);
        let has = |name: &str, val: &str| {
            draft.tags.iter().any(|tag| {
                tag.0.first().map(String::as_str) == Some(name)
                    && tag.0.get(1).map(String::as_str) == Some(val)
            })
        };
        assert!(has("status", "error"), "an execution failure is the error class");
        assert!(has("reason_code", "execution_failed"), "carries the authoritative §10 code");
        assert!(has("p", "buyerpk"), "addressed to the buyer");
        assert!(has("e", "offer1"), "references the offer");
    }

    // TOOTH (#374 §10) — every emitting site rides the authoritative `reason_code` tag; a reader keys
    // on it, not on parsing content. The coarse `status` stays `error` for now (the buyer claim-list
    // view keys on it — re-classing refusals to `status=refusal` is a deliberate view change left as a
    // follow-up, not smuggled in here). Bite: drop the reason_code tag from `error_draft` and the code
    // assertions go red.
    #[test]
    fn feedback_rides_the_authoritative_reason_code_tag() {
        let tag = |code: ReasonCode, name: &str| {
            let draft = error_draft("offer1", "buyerpk", &"s".repeat(64), code, "why");
            draft
                .tags
                .iter()
                .find(|t| t.0.first().map(String::as_str) == Some(name))
                .and_then(|t| t.0.get(1).cloned())
        };
        assert_eq!(tag(ReasonCode::BelowRate, "reason_code").as_deref(), Some("below_rate"));
        assert_eq!(tag(ReasonCode::NoSentinel, "reason_code").as_deref(), Some("no_sentinel"));
        assert_eq!(
            tag(ReasonCode::DeliveryFailed, "reason_code").as_deref(),
            Some("delivery_failed")
        );
        assert_eq!(
            tag(ReasonCode::ExecutionFailed, "reason_code").as_deref(),
            Some("execution_failed")
        );
        assert_eq!(
            tag(ReasonCode::CapabilityMissing, "reason_code").as_deref(),
            Some("capability_missing")
        );
        // Coarse status is unchanged (historical `error`) for every code — the tag is the discriminator.
        assert_eq!(tag(ReasonCode::BelowRate, "status").as_deref(), Some("error"));
        assert_eq!(tag(ReasonCode::NoSentinel, "status").as_deref(), Some("error"));
        // #821 keeps the existing convention deliberately: the §10 refusal re-class is a separate view
        // change, so `capability_missing` rides `status=error` like every other code. This is also what
        // discharges #821's buyer-side item without a code change — the claim-list view keys on
        // `status`, so a code that stays `error` is already handled there.
        assert_eq!(
            tag(ReasonCode::CapabilityMissing, "status").as_deref(),
            Some("error")
        );
    }

    // ── Execute-body delivery contract (invariants 2 & 8), no network ────────────────────────────

    use crate::seller_node::store::{Offer, SellerStore};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("maxplayer-run-{label}-{}-{id}", std::process::id()))
    }

    // #591 A3 — the REAL provisioning seam `execute_job` calls (`provision_delivery_workdir`), NOT the
    // pure planner: a served contribution's STORED pin routes the delivery workdir into the base-clone
    // path (`init_contribution_workdir`), while a from-scratch job (no pin) takes the empty-workdir
    // default. This is the wiring the isolated planner test could not prove — before the fix
    // `execute_job` provisioned Empty unconditionally, so the pin never reached a clone. The
    // `base_oid → HEAD == base_oid` value is the reused `init_contribution_workdir`'s contract
    // (`seller_git::checkout_base_branch_from_oid_creates_fork_tip`); driven OFFLINE (no networked/paid
    // e2e), the contribution route is proven by its base-locator refusal — the https-only allowlist
    // that the empty path never touches — so no network base is fetched here.
    #[tokio::test]
    async fn clone_at_base_oid_not_empty_workdir() {
        let root = temp_dir("provision");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        let home = crate::home::bootstrap(&root).expect("bootstrap home");
        let store = SellerStore::open(root.join("seller.sqlite")).expect("open store");
        let identity = DeliveryAgentIdentity::for_seller(&"aa".repeat(32));

        // A served contribution records its pin at claim. Its buyer-controlled base is a LOCAL path
        // here — refused by the https-only allowlist — so provision fails deterministically INSIDE the
        // base-clone path, offline. `init_empty` never consults a locator, so this refusal proves the
        // stored pin was READ and ROUTED to the contribution clone, not the empty default.
        let pin = crate::seller_node::store::ContributionPin {
            owner_pubkey: "bb".repeat(32),
            clone_url: "/not/a/remote/base.git".to_owned(),
            base_branch: "main".to_owned(),
            base_oid: "a".repeat(40),
        };
        store
            .record_contribution_pin("contrib-job", &pin, 1)
            .expect("record pin");
        let err = provision_delivery_workdir(
            &store,
            &home,
            "contrib-job",
            root.join("wd-contrib"),
            identity.clone(),
        )
        .await
        .expect_err("a served contribution must enter the base-clone path, not the empty default");
        assert!(
            matches!(
                err,
                DeliveryWorkdirError::Git(seller_git::SellerGitError::Transport(_))
            ),
            "served contribution routed to init_contribution_workdir (locator allowlist), got {err:?}"
        );

        // A from-scratch job has no pin: provision takes the empty-workdir default and SUCCEEDS — there
        // is no base to clone. The result is an initialized, empty delivery repo (the inverse route).
        let scratch = root.join("wd-scratch");
        provision_delivery_workdir(&store, &home, "scratch-job", scratch.clone(), identity)
            .await
            .expect("a from-scratch job provisions an empty workdir");
        assert!(
            scratch.join(".git").exists(),
            "empty delivery workdir is an initialized git repo"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // #613 TOOTH — the seller EMIT wiring the execute/finalize deliver tail calls. A served
    // contribution (recorded pin) produces a contribution result envelope that PARSES as a
    // contribution result AND whose seller-signed tuple verifies over the buyer's OWN reconstruction
    // (tuple rebuilt from the echo target/base + the fork facts, exactly as `authorize_pay`); a
    // from-scratch job (no pin) produces NONE, so the standard result rides unchanged. Before the fix
    // the deliver tail emitted the from-scratch shape for BOTH, so the buyer refused a correctly
    // delivered fork ("...requires a contribution result..."). Signs through the REAL signer actor
    // (the seller key never leaves it) — the same path production uses.
    #[tokio::test]
    async fn contribution_deliver_emits_result_envelope_pin_only() {
        let root = temp_dir("contrib-envelope");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        let home = crate::home::bootstrap(&root).expect("bootstrap home");
        let store = SellerStore::open(root.join("seller.sqlite")).expect("open store");
        let signer = crate::seller_node::signer::spawn(&home).expect("spawn signer");
        let seller_hex = signer.public_key_via_actor().await.expect("seller pubkey");

        let fork_repo = "https://relay.maxplayer.test/git/seller/fork.git";
        let branch = crate::contribution::ForkRef::unique_branch("contrib-job");
        let commit = "d".repeat(40);

        // From-scratch (no pin): no contribution envelope — the standard result rides unchanged.
        assert!(
            contribution_result_envelope_tags(
                &store, &signer, "scratch-job", &seller_hex, fork_repo, &branch, &commit,
            )
            .await
            .expect("no pin is not an error")
            .is_none(),
            "a from-scratch job produces no contribution envelope"
        );

        // A served contribution records its pin; the envelope tags parse as a contribution result
        // echoing the pinned target/base.
        let pin = crate::seller_node::store::ContributionPin {
            owner_pubkey: "bb".repeat(32),
            clone_url: "https://relay.maxplayer.test/git/owner/target.git".to_owned(),
            base_branch: "main".to_owned(),
            base_oid: "a".repeat(40),
        };
        store
            .record_contribution_pin("contrib-job", &pin, 1)
            .expect("record pin");

        let extra = contribution_result_envelope_tags(
            &store, &signer, "contrib-job", &seller_hex, fork_repo, &branch, &commit,
        )
        .await
        .expect("build envelope")
        .expect("a served contribution emits a contribution envelope");

        let (echo, sig) = crate::contribution::parse_contribution_result_echo(&extra)
            .expect("parse ok")
            .expect("is a contribution result");
        assert_eq!(echo.target.owner_pubkey(), pin.owner_pubkey);
        assert_eq!(echo.target.clone_url(), pin.clone_url);
        assert_eq!(echo.base.branch(), pin.base_branch);
        assert_eq!(echo.base.oid(), pin.base_oid);

        // The buyer reconstructs the tuple from the echo target/base + the fork facts on the result
        // (authorize_pay), and the seller sig verifies over it.
        let buyer_tuple = crate::contribution::AuthorshipTuple {
            job_id: "contrib-job".to_owned(),
            seller_pubkey: seller_hex.clone(),
            target: crate::contribution::TargetRepoPin::new(
                echo.target.owner_pubkey(),
                echo.target.clone_url(),
            )
            .unwrap(),
            base_oid: echo.base.oid().to_owned(),
            fork: crate::contribution::ForkRef::new(fork_repo, &branch).unwrap(),
            commit_oid: commit.clone(),
        };
        let seller_key = nostr_sdk::PublicKey::parse(&seller_hex).expect("seller pubkey parse");
        crate::contribution::verify_tuple_sig(&buyer_tuple, &sig, &seller_key)
            .expect("seller-signed tuple verifies over the buyer's reconstruction");

        let _ = std::fs::remove_dir_all(&root);
    }

    // A store with a full offer → claim(creq) → award already journaled, so the execute-body readers
    // (offer_row / job_creq / job_award_time) have real rows to answer from.
    fn store_with_awarded_job(
        creq: &str,
        job: &str,
        buyer: &str,
        award_time: i64,
    ) -> (SellerStore, std::path::PathBuf) {
        let root = temp_dir("store");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        let store = SellerStore::open(root.join("seller.sqlite")).expect("open store");
        store
            .record_offer(
                &Offer {
                    offer_id: job.to_owned(),
                    buyer_pubkey: buyer.to_owned(),
                    amount_sats: 21,
                    unit: "sat".to_owned(),
                    task: "build a widget".to_owned(),
                    deadline_unix: 2_000_000_000,
                    targeted: true,
                    requested_agent: None,
                    output: Some("text/plain".to_owned()),
                },
                1,
            )
            .expect("record offer");
        let draft = claim_draft(job, buyer, &"s".repeat(64), creq, &[], &Default::default());
        store
            .claim_and_enqueue(job, job, creq, &draft, 1, 9_999_999_999, 1)
            .expect("claim");
        store
            .record_award(&"w".repeat(64), job, buyer, award_time)
            .expect("award");
        (store, root)
    }

    /// A store carrying exactly the stranded shape a #626 phantom bind leaves behind: offer → claim →
    /// award journaled, and the offer's own deadline already PASSED at `now`.
    fn store_with_lapsed_awarded_job(
        job: &str,
        buyer: &str,
        now: i64,
    ) -> (SellerStore, std::path::PathBuf) {
        let root = temp_dir("lapsed-awarded");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        let store = SellerStore::open(root.join("seller.sqlite")).expect("open store");
        store
            .record_offer(
                &Offer {
                    offer_id: job.to_owned(),
                    buyer_pubkey: buyer.to_owned(),
                    amount_sats: 21,
                    unit: "sat".to_owned(),
                    task: "an open-pool job another seat won".to_owned(),
                    deadline_unix: now - 1,
                    targeted: false,
                    requested_agent: None,
                    output: Some("text/plain".to_owned()),
                },
                1,
            )
            .expect("record offer");
        let draft = claim_draft(job, buyer, &"s".repeat(64), "creqL", &[], &Default::default());
        store
            .claim_and_enqueue(job, job, "creqL", &draft, 1, 9_999_999_999, 1)
            .expect("claim");
        store
            .record_award(&"w".repeat(64), job, buyer, 2)
            .expect("award");
        (store, root)
    }

    /// CHARACTERIZATION — NOT a #626 red-prove. This passes with AND without the identity match,
    /// because the path it exercises shipped in #552.
    ///
    /// It pins the self-heal that already-stranded seats depend on: a slot-occupying `awarded` row
    /// with no delivery, no receipt, no pushed commit and a PASSED offer deadline classifies as
    /// `SkipLapsed`, and once failed it stops counting toward `jobs_in_flight` — which is what puts
    /// the seat back to `accepting=y`. #626 closes the source of such rows; this guards the path that
    /// clears the ones already written, so a later change cannot delete the healing silently.
    #[test]
    fn characterization_a_lapsed_awarded_row_lapses_and_stops_counting_once_failed() {
        let job = "j".repeat(64);
        let buyer = "b".repeat(64);
        let now = 2_000_i64;
        let (store, root) = store_with_lapsed_awarded_job(&job, &buyer, now);

        // The stranded shape, asserted rather than assumed: the seat reports itself busy.
        assert_eq!(
            store.jobs_in_flight().expect("in flight"),
            1,
            "precondition: the awarded row occupies a slot, so the heartbeat publishes accepting=n"
        );
        let state = store.job_state(&job).expect("job_state").expect("job row present");
        assert!(!store.has_delivery(&job).expect("has_delivery"), "never delivered");
        assert!(!store.has_receipt(&job).expect("has_receipt"), "never paid");
        assert_eq!(store.pushed_commit(&job).expect("pushed_commit"), None, "never pushed");
        let deadline = store
            .offer_row(&job)
            .expect("offer_row")
            .expect("offer row present")
            .deadline_unix;
        assert!(deadline <= now, "precondition: the offer's own deadline has passed");

        assert_eq!(
            resume_action(state, false, false, false, None, Some(deadline), now),
            ResumeAction::SkipLapsed,
            "a restart must FAIL this row, not re-drive it — the award can no longer be paid"
        );

        // What the SkipLapsed arm does, and the property the seat is judged on.
        store.fail_job(&job, now).expect("fail the stale award");
        assert_eq!(
            store.jobs_in_flight().expect("in flight"),
            0,
            "the failed row no longer occupies a slot, so the seat advertises capacity again"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (#552) — RESTART-SCOPED terminal-shape resume SELECTION over REAL persisted rows. The pure
    // `resume_action` unit test drives SYNTHETIC markers; this proves the STORE getters return the
    // right markers for rows that survived a process restart (drop + reopen on the SAME sqlite home),
    // and that they COMPOSE to the correct action — the exact composition `execute_job` performs on
    // resume (job_state + has_delivery + has_receipt + pushed_commit + offer deadline → resume_action).
    //
    // Trigger scoping (a quiet window is a SILENT false-pass, #552/37716): every row is seeded
    // settled/lapsed/pushed BEFORE the drop and asserted AFTER the reopen, so the assertion window
    // provably contains a restart-after-settled — it cannot decay into an always-green check.
    //
    // Bites (red on revert): drop the deadline-lapse check and the two lapsed rows become RunAgent —
    // re-driving a dead award, re-running the agent and re-emitting a delivery (the #552 harm); drop
    // the `pushed_commit` column or its migration and `pushed_live` cannot read its commit back after
    // reopen, so the FinalizeFromPushed corner fails; break offer-row persistence and every deadline
    // reads None (all lapsed rows would wrongly resume).
    #[test]
    fn restart_resume_selection_over_persisted_rows() {
        let now = 1_000_000i64; // the reopen "now"
        let live = now + 3_600; // deadline still in the future
        let lapsed = now - 1; // deadline already passed
        let buyer = "b".repeat(64);
        let seller = "s".repeat(64);
        let creq = "creqZ";

        // One job id (== offer id, the production invariant) per terminal shape.
        let mid_flight = "1".repeat(64); // awarded, live, no marker    → RunAgent (THE FOIL)
        let lapsed_bare = "2".repeat(64); // awarded, LAPSED, no marker  → SkipLapsed (dominant #552 case)
        let pushed_live = "3".repeat(64); // awarded, live, pushed       → FinalizeFromPushed
        let pushed_lapsed = "4".repeat(64); // awarded, LAPSED, pushed   → SkipLapsed (lapse BEFORE finalize)
        let delivered = "5".repeat(64); // delivered                     → SkipTerminal
        let settled_live = "6".repeat(64); // awarded, live, settled_elsewhere marker → SkipTerminal (#563)

        let root = temp_dir("resume-restart");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        let db = root.join("seller.sqlite");

        // ---- Pre-restart: seed every shape into ONE store, then DROP it (process "crash").
        {
            let store = SellerStore::open(&db).expect("open");
            let seed = |job: &str, deadline: i64, award: &str| {
                store
                    .record_offer(
                        &Offer {
                            offer_id: job.to_owned(),
                            buyer_pubkey: buyer.clone(),
                            amount_sats: 21,
                            unit: "sat".to_owned(),
                            task: "t".to_owned(),
                            deadline_unix: deadline,
                            targeted: true,
                            requested_agent: None,
                            output: Some("text/plain".to_owned()),
                        },
                        1,
                    )
                    .expect("record offer");
                // A per-job draft (distinct event id) — claim_and_enqueue dedups on `claim:{job}`.
                let draft = claim_draft(job, &buyer, &seller, creq, &[], &Default::default());
                store
                    .claim_and_enqueue(job, job, creq, &draft, 1, 9_999_999_999, 1)
                    .expect("claim");
                store.record_award(award, job, &buyer, 2).expect("award");
            };
            seed(&mid_flight, live, &format!("{}1", "w".repeat(63)));
            seed(&lapsed_bare, lapsed, &format!("{}2", "w".repeat(63)));
            seed(&pushed_live, live, &format!("{}3", "w".repeat(63)));
            store.mark_pushed(&pushed_live, "commitL", 3).expect("mark pushed (live)");
            seed(&pushed_lapsed, lapsed, &format!("{}4", "w".repeat(63)));
            store.mark_pushed(&pushed_lapsed, "commitX", 3).expect("mark pushed (lapsed)");
            seed(&delivered, live, &format!("{}5", "w".repeat(63)));
            let ddraft = claim_draft(&delivered, &buyer, &seller, creq, &[], &Default::default());
            store
                .deliver_and_enqueue(&delivered, &"c".repeat(40), &ddraft, 4, 4 + RESULT_PUBLISH_WINDOW_SECS, 4)
                .expect("deliver");
            // #563: a live-deadline row a PRIOR resume relay-derived as settled elsewhere. The durable
            // marker must survive the restart and short-circuit the next resume to SkipTerminal WITHOUT
            // re-querying the relay (exactly what execute_job's `settled_marked` branch does at boot).
            seed(&settled_live, live, &format!("{}6", "w".repeat(63)));
            store.mark_settled_elsewhere(&settled_live, 3).expect("mark settled elsewhere");
            // store drops here — the sqlite home persists on disk.
        }

        // ---- Restart: reopen the SAME home. Every marker must come back from durable storage.
        let store = SellerStore::open(&db).expect("reopen");

        // Compose exactly as execute_job does on resume: read the durable markers, then decide. The
        // settled_elsewhere marker is read from the store like the others (its relay-derive already
        // ran on a PRIOR resume); a set marker short-circuits without any relay round-trip.
        let action = |job: &str| {
            let state = store.job_state(job).expect("job_state").expect("job row present");
            let has_delivery = store.has_delivery(job).expect("has_delivery");
            let has_receipt = store.has_receipt(job).expect("has_receipt");
            let settled_elsewhere = store.has_settled_elsewhere(job).expect("has_settled_elsewhere");
            let pushed = store.pushed_commit(job).expect("pushed_commit");
            let deadline = store.offer_row(job).expect("offer_row").map(|o| o.deadline_unix);
            resume_action(state, has_delivery, has_receipt, settled_elsewhere, pushed, deadline, now)
        };

        assert_eq!(
            action(&mid_flight),
            ResumeAction::RunAgent,
            "live + no marker ⇒ resume + run (THE FOIL — a real award must not stall)"
        );
        assert_eq!(
            action(&lapsed_bare),
            ResumeAction::SkipLapsed,
            "lapsed stale award ⇒ fail, never re-drive (#552 durable primary)"
        );
        assert_eq!(
            action(&pushed_live),
            ResumeAction::FinalizeFromPushed("commitL".into()),
            "live + pushed ⇒ finalize from the durable commit (it survived the restart)"
        );
        assert_eq!(
            action(&pushed_lapsed),
            ResumeAction::SkipLapsed,
            "pushed BUT lapsed ⇒ fail — lapse BEFORE finalize (never emit a result past the deadline)"
        );
        assert_eq!(
            action(&delivered),
            ResumeAction::SkipTerminal,
            "delivered stays terminal across the restart"
        );
        assert_eq!(
            action(&settled_live),
            ResumeAction::SkipTerminal,
            "#563: a live-deadline row marked settled_elsewhere stays terminal across the restart \
             (durable marker short-circuits — never re-driven, never re-queried)"
        );

        // The coarse pre-filter (what the resume loop iterates) admits every slot-occupier and the
        // delivered row (jobs.state IN awarded/executing/delivered); resume_action is the refinement.
        let resumable: std::collections::BTreeSet<String> = store
            .resumable_jobs()
            .expect("resumable")
            .into_iter()
            .map(|(job, _state)| job)
            .collect();
        for job in [&mid_flight, &lapsed_bare, &pushed_live, &pushed_lapsed, &delivered, &settled_live] {
            assert!(resumable.contains(job), "resumable set must contain {job}");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (invariant 8 / audit N-4), NODE-level: the delivery cosignature the execute body signs
    // binds the hash of the STORED claim-time creq read from the store — never a rebuild from live
    // config. Author a creq under one accepted-mint set, journal it, then build the preimage the exec
    // body builds (from `store.job_creq`): its creq_hash equals the STORED creq's hash and differs
    // from the hash a drifted mint set would produce. Bite: if the exec body sourced the creq from
    // live config instead of the store, the bound hash would be the drifted one and this goes red.
    #[test]
    fn delivery_preimage_binds_stored_creq_not_drifted_config() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let buyer = "b".repeat(64);
        let job = "a".repeat(64);
        let mints_claim = vec!["https://testnut.cashudevkit.org".to_owned()];
        let creq_a =
            gateway::creq::build_seller_creq(&job, 21, "sat", &mints_claim, &seller).expect("creq A");

        let (store, root) = store_with_awarded_job(&creq_a, &job, &buyer, 4242);
        let stored = store.job_creq(&job).expect("read").expect("present");
        assert_eq!(stored, creq_a, "the stored creq is the claim-time creq");

        let preimage = delivery_receipt_preimage(
            &job,
            "build a widget",
            21,
            &buyer,
            &seller,
            &"c".repeat(40),
            "fork",
            &stored,
        );
        assert_eq!(
            preimage.creq_hash,
            Some(gateway::creq_hash_hex(&creq_a)),
            "delivery signs the STORED creq's hash"
        );

        // Config drifts to a different accepted-mint set after the claim: its creq hashes differently
        // and the delivery must NOT bind it.
        let mints_drifted = vec![
            "https://testnut.cashudevkit.org".to_owned(),
            "https://mint.example.invalid".to_owned(),
        ];
        let creq_b =
            gateway::creq::build_seller_creq(&job, 21, "sat", &mints_drifted, &seller).expect("creq B");
        assert_ne!(
            preimage.creq_hash,
            Some(gateway::creq_hash_hex(&creq_b)),
            "a config-drifted creq hashes differently; the delivery must not sign it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (invariant 2), NODE-level: a re-created delivery commit is deterministic (identical tree
    // + the STORED award-time author date ⇒ identical oid), and the durable delivery journal adopts
    // the existing tip instead of double-publishing. Bite: if the snapshot used wall-clock now()
    // instead of the journaled date, the two commits differ and the equality assert goes red; if
    // `deliver_and_enqueue` did not dedup, the second call returns true and a SECOND result outbox
    // row appears — the count assert goes red.
    #[test]
    fn resume_redelivery_is_deterministic_and_never_double_publishes() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let buyer = "b".repeat(64);
        let job = "a".repeat(64);
        let author_date = 4242_i64;
        let branch = "maxplayer/aaaaaaaa";
        let identity = DeliveryAgentIdentity::for_seller(&seller);

        // Two independent workdirs with byte-identical trees, each snapshotted at the SAME journaled
        // author date — the exact "crashed, re-created the commit on resume" shape.
        let make_commit = |label: &str| -> String {
            let wd = temp_dir(label);
            let _ = std::fs::remove_dir_all(&wd);
            seller_git::init_empty_delivery_workdir(&wd, &identity).expect("init workdir");
            std::fs::write(wd.join("deliverable.txt"), b"the widget\n").expect("write file");
            // Same job hash on both passes: the sentinel is seeded from it, so a deterministic
            // manifest is part of what keeps the re-created delivery oid identical across a resume.
            let commit = seller_git::snapshot_delivery_at(
                &wd,
                &identity,
                None,
                branch,
                "maxplayer delivery: build a widget",
                author_date,
                &"c".repeat(64),
            )
            .expect("snapshot");
            let _ = std::fs::remove_dir_all(&wd);
            commit
        };
        let commit_first = make_commit("wd1");
        let commit_resume = make_commit("wd2");
        assert_eq!(
            commit_first, commit_resume,
            "identical tree + stored author date ⇒ identical delivery oid (deterministic re-push)"
        );

        // The durable delivery journal: first delivery lands, a resumed re-delivery is a dedup no-op.
        let creq = gateway::creq::build_seller_creq(
            &job,
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, author_date);
        let draft = claim_draft(&job, &buyer, &seller, &creq, &[], &Default::default());
        let now = 5000;
        assert!(
            store
                .deliver_and_enqueue(&job, &commit_first, &draft, now, now + RESULT_PUBLISH_WINDOW_SECS, now)
                .expect("deliver"),
            "first delivery journals + enqueues the result"
        );
        assert!(
            !store
                .deliver_and_enqueue(&job, &commit_resume, &draft, now, now + RESULT_PUBLISH_WINDOW_SECS, now)
                .expect("re-deliver"),
            "resume adopts the existing tip: a second delivery re-enqueues nothing"
        );
        let result_rows = store
            .pending_outbox(now)
            .expect("pending")
            .into_iter()
            .filter(|item| item.dedup_key == format!("result:{job}"))
            .count();
        assert_eq!(result_rows, 1, "exactly one result event enqueued across the resume");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Pay arm money-safety (invariant 3), no mint ─────────────────────────────────────────────

    // TOOTH (invariant 3 / finding S) — the redeem classification never forges a receipt from a
    // pending-receive breadcrumb; the ONLY positive proof of our prior collection is a COMPLETED
    // receipt, read fail-closed. Covers the replay-of-collected case (already-spent + has_receipt=true
    // ⇒ no-op) and the crash-between-import-and-receipt-row case (already-spent + has_receipt=false ⇒
    // refuse, never a forged receipt).
    #[test]
    fn redeem_classification_finalizes_and_never_forges_from_a_breadcrumb() {
        // A clean successful receive finalizes exactly its amount; has_receipt is never consulted.
        assert!(matches!(
            classify_redeem_outcome(Ok(21), || panic!("has_receipt must not be read on success")),
            RedeemDecision::Finalize(21)
        ));
        // Already-spent + a COMPLETED receipt ⇒ idempotent no-op (legit backfill/restart re-see).
        assert!(matches!(
            classify_redeem_outcome(Err("Token already spent".into()), || Ok(true)),
            RedeemDecision::IdempotentNoOp
        ));
        // Already-spent + NO receipt (crash-between, or a replay/theft — indistinguishable) ⇒ refuse.
        assert!(matches!(
            classify_redeem_outcome(Err("Token already spent".into()), || Ok(false)),
            RedeemDecision::Refuse(_)
        ));
        // has_receipt READ ERROR ⇒ refuse, FAIL CLOSED (never read unreadable as "no receipt ⇒ safe").
        assert!(matches!(
            classify_redeem_outcome(Err("already redeemed".into()), || Err("corrupt".into())),
            RedeemDecision::Refuse(_)
        ));
        // A non-already-spent receive error refuses without consulting has_receipt.
        assert!(matches!(
            classify_redeem_outcome(Err("mint offline".into()), || panic!("must not read has_receipt")),
            RedeemDecision::Refuse(_)
        ));
    }

    // TOOTH (#150 relay-stall watchdog) — the stall threshold is interval*missed with each factor
    // clamped ≥1 (never 0, so the watchdog can never trip on the first tick), and staleness trips
    // only AT/after the threshold.
    #[test]
    fn watchdog_stall_math_clamps_and_trips_only_at_threshold() {
        assert_eq!(stall_threshold_secs(300, 3), 900);
        assert_eq!(stall_threshold_secs(0, 0), 1, "each factor clamped ≥1 so the product is never 0");
        assert!(!subscription_stalled(899, 900), "below threshold ⇒ live");
        assert!(subscription_stalled(900, 900), "at threshold ⇒ stalled");
        assert!(subscription_stalled(901, 900));
    }

    // #509 red-prove helper: build a `send_event_to`-shaped `Output` with the given per-relay verdict.
    fn publish_output(accepted: bool, rejected: bool) -> Output<()> {
        use std::collections::{HashMap, HashSet};
        let url = RelayUrl::parse("wss://relay.example").expect("relay url");
        let mut success = HashSet::new();
        let mut failed = HashMap::new();
        if accepted {
            success.insert(url.clone());
        }
        if rejected {
            failed.insert(url, "blocked: not accepting this kind".to_string());
        }
        Output {
            val: (),
            success,
            failed,
        }
    }

    // TOOTH (#509) — the seat's heartbeat health is CONFIRMED, not inferred. `send_event_to` returns
    // `Ok(Output)` even when the sole relay REJECTS the write (`OK: false` ⇒ relay lands in
    // `output.failed`, NOT a top-level `Err`). The pre-fix `Ok(_) => true` read that rejection as
    // success. `publish_confirmed` must treat it — and an empty `success` — as health-RED.
    //
    // RED ON REVERT: give `publish_confirmed` the old semantics (`|_| true`) and the two `!`
    // assertions below fail — a relay rejection / silent drop reads as a healthy publish again.
    #[test]
    fn heartbeat_publish_is_confirmed_only_on_a_relay_ok() {
        // Relay acknowledged and none rejected ⇒ confirmed.
        assert!(
            publish_confirmed(&publish_output(true, false)),
            "an accepted publish is confirmed"
        );
        // Relay `OK: false` — the #509 fingerprint: connection up, event rejected.
        assert!(
            !publish_confirmed(&publish_output(false, true)),
            "an OK-false (relay in `failed`) is NOT confirmed — the #509 defect"
        );
        // Nothing acknowledged at all (silent drop): empty `success`, empty `failed`.
        assert!(
            !publish_confirmed(&publish_output(false, false)),
            "an empty `success` set is NOT confirmed"
        );
    }

    // TOOTH (#509) — relay-observed liveness AND-gates the watchdog clock on BOTH legs. The read
    // probe answering (`probe_ok`) while the heartbeat WRITE is rejected (`publish_ok == false`) is
    // the exact 2691s blind spot: the seat is dark on the relay yet used to refresh the clock on the
    // read leg alone and never trip. `relay_liveness_confirmed` must return false there, so the clock
    // ages and the RELAY-STALL branch fires.
    //
    // RED ON REVERT: make `relay_liveness_confirmed` return `probe_ok` (the pre-fix wiring) and the
    // read-alive/publish-dead assertion fails — the watchdog goes blind to publish-death again.
    #[test]
    fn watchdog_clock_refreshes_only_when_both_legs_confirm() {
        assert!(
            relay_liveness_confirmed(true, true),
            "read + publish both confirmed ⇒ liveness confirmed"
        );
        assert!(
            !relay_liveness_confirmed(true, false),
            "read alive but publish dead ⇒ NOT confirmed (the #509 blind spot)"
        );
        assert!(
            !relay_liveness_confirmed(false, true),
            "publish landed but reads dead ⇒ NOT confirmed"
        );
        assert!(!relay_liveness_confirmed(false, false));
    }

    // TOOTH (invariant 3, security) — a payment settles a job ONLY when the authenticated seal sender
    // is the bound offer buyer; a third party can never pay-once and close someone else's job.
    #[test]
    fn seal_sender_must_be_the_bound_offer_buyer() {
        assert!(seal_sender_is_bound_buyer("buyerpk", "buyerpk"));
        assert!(!seal_sender_is_bound_buyer("attackerpk", "buyerpk"));
    }

    // TOOTH (invariant 3 / Fix Q) — the redeem guard settles only at a mint the STORED claim-time creq
    // advertised; a realized mint outside that set is refused, so a config change across the trade can
    // neither strand this payment nor introduce a settling mint.
    #[test]
    fn realized_mint_must_be_in_the_stored_creq() {
        use std::str::FromStr as _;
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let advertised = "https://testnut.cashudevkit.org";
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &[advertised.to_owned()],
            &seller,
        )
        .expect("creq");
        let request = gateway::creq::parse_creq(&creq).expect("parse");
        let advertised_mint = cashu::MintUrl::from_str(advertised).expect("mint");
        let foreign_mint = cashu::MintUrl::from_str("https://mint.example.invalid").expect("mint");
        assert!(request.mints.contains(&advertised_mint), "the advertised mint settles");
        assert!(
            !request.mints.contains(&foreign_mint),
            "a mint outside the stored creq is refused"
        );
    }

    // TOOTH (invariant 3) — the receipt row dedups a replayed payment on the wrap id: a job is marked
    // paid at most once, so a re-delivered gift-wrap never double-credits.
    #[test]
    fn receipt_collect_dedups_a_replayed_payment() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let job = "a".repeat(64);
        let buyer = "b".repeat(64);
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, 4242);
        let wrap_id = "e".repeat(64);
        assert!(!store.has_receipt(&job).expect("read"), "not paid before collect");
        assert_eq!(
            store.collect_receipt(&wrap_id, &job, 21, 5000).expect("collect"),
            crate::seller_node::store::Collected::New
        );
        assert_eq!(
            store.collect_receipt(&wrap_id, &job, 21, 5001).expect("replay"),
            crate::seller_node::store::Collected::Duplicate,
            "a replayed wrap id never credits the job twice"
        );
        assert!(store.has_receipt(&job).expect("read"), "paid after the first collect");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Resume execution across a process restart (invariant 4, fallback form) ───────────────────

    // TOOTH (invariant 4) — the resume selection re-drives only jobs left mid-flight (awarded /
    // executing); a delivered job is left for the pay path and terminal jobs never re-run.
    #[test]
    fn resume_selects_awarded_or_executing_not_delivered_or_terminal() {
        use crate::seller_node::store::JobState;
        assert!(should_resume_execution(JobState::Awarded));
        assert!(should_resume_execution(JobState::Executing));
        assert!(!should_resume_execution(JobState::Delivered));
        assert!(!should_resume_execution(JobState::Paid));
        assert!(!should_resume_execution(JobState::Failed));
    }

    // TOOTH (invariant 4) — boot with a journaled awarded-but-undelivered job: it is resume-eligible
    // (the field-test promise — nous's Mac kills processes mid-job), and the re-execution's delivery
    // lands EXACTLY ONCE (deliver_and_enqueue is idempotent on the job), so a resumed job never
    // double-publishes.
    #[test]
    fn boot_resume_re_drives_awarded_undelivered_job_delivery_lands_once() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let job = "a".repeat(64);
        let buyer = "b".repeat(64);
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, 4242);

        // Awarded + undelivered ⇒ the boot resume pass selects it.
        let resumable = store.resumable_jobs().expect("resumable");
        assert!(
            resumable
                .iter()
                .any(|(id, state)| id == &job && should_resume_execution(*state)),
            "the awarded, undelivered job is resume-eligible: {resumable:?}"
        );
        assert_eq!(
            store.job_state(&job).expect("state"),
            Some(crate::seller_node::store::JobState::Awarded)
        );

        // Re-execution delivers exactly once: deliver_and_enqueue is idempotent on the job.
        let draft = claim_draft(&job, &buyer, &seller, &creq, &[], &Default::default());
        let now = 5000;
        assert!(
            store
                .deliver_and_enqueue(&job, &"c".repeat(40), &draft, now, now + RESULT_PUBLISH_WINDOW_SECS, now)
                .expect("deliver"),
            "first (resumed) delivery lands"
        );
        assert!(
            !store
                .deliver_and_enqueue(&job, &"c".repeat(40), &draft, now, now + RESULT_PUBLISH_WINDOW_SECS, now)
                .expect("re-deliver"),
            "a resumed re-execution delivers at most once"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- #189 / #190 recovery teeth ------------------------------------------------------------
    //
    // These drive the REAL paths against [`p_gate_relay_fixture`], which answers a `#p`-gated REQ
    // from an unauthenticated session with the permanent-class `restricted:` prefix exactly as
    // maxplayer-relay does. The nostr-relay-builder fixture used above cannot express this: it says
    // `auth-required:`, which nostr-sdk keeps and restores by itself, so every ordering would pass.

    use crate::seller_node::p_gate_relay_fixture::{PGateRelay, PublishedEvent, ReqRecord, Verdict};

    /// Generous enough that a slow box never flakes, short enough that a real failure fails fast.
    const FIXTURE_WAIT: Duration = Duration::from_secs(15);

    /// A throwaway home per test. Unique per test name AND process so a parallel run never collides
    /// on the exclusive home lock.
    fn throwaway_root(label: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("maxplayer-recoveryfix-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// Boot a real runner against the fixture relay.
    async fn boot_against(
        root: &std::path::Path,
        fixture: &PGateRelay,
        claim_open_pool: bool,
    ) -> SellerNodeRunner {
        let mut home = crate::home::bootstrap(root).expect("bootstrap home");
        home.config.relay_url = fixture.url();
        home.config.seller = Some(seller_cfg(1, claim_open_pool));
        SellerNodeRunner::boot(home)
            .await
            .expect("boot the node against the fixture relay")
    }

    /// The relay handle the recovery path takes.
    async fn relay_handle(runner: &SellerNodeRunner) -> nostr_sdk::prelude::Relay {
        runner
            .client
            .relays()
            .await
            .get(&RelayUrl::parse(&runner.relay_url).expect("relay url"))
            .cloned()
            .expect("relay handle")
    }

    /// Every REQ that reached the relay before that session had completed NIP-42, on a filter the
    /// relay p-gates. This set being non-empty IS #189.
    fn p_gated_before_auth(reqs: &[ReqRecord]) -> Vec<&ReqRecord> {
        reqs.iter()
            .filter(|record| record.p_pinned && !record.authenticated)
            .collect()
    }

    /// Every REQ the relay refused with the permanent-class prefix — each one a subscription
    /// nostr-sdk has deleted from its registry and will never restore.
    fn permanently_removed(reqs: &[ReqRecord]) -> Vec<&ReqRecord> {
        reqs.iter()
            .filter(|record| {
                matches!(&record.verdict, Verdict::Closed(reason) if reason.starts_with("restricted:"))
            })
            .collect()
    }

    /// TOOTH #189 (a) — THE ORDERING. A recovery whose AUTH lands well after the socket does must
    /// still put every REQ on the wire AFTER NIP-42, leaving all four subscriptions live and nothing
    /// permanently removed.
    ///
    /// The fixture withholds its challenge for 400ms, so the pre-auth window is wide and the outcome
    /// is decided by ordering rather than luck.
    ///
    /// RED ON REVERT: move `clear_subscription_registrations` back to AFTER
    /// `reconnect_and_authenticate` in `reconnect_and_resubscribe` and this goes red — the SDK's
    /// `post_connection` resubscribe (`relay/inner.rs:748-752`) puts all three registered REQs on the
    /// new socket immediately, the fixture refuses the p-gated ones `restricted:`, and both
    /// assertions below fire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recovery_puts_no_p_gated_req_on_the_wire_before_nip42_completes() {
        let fixture = PGateRelay::start(Duration::from_millis(400)).await;
        let root = throwaway_root("order");
        let runner = boot_against(&root, &fixture, false).await;
        let relay = relay_handle(&runner).await;

        runner.subscribe_all(None).await.expect("boot subscribe");
        assert!(
            fixture
                .wait_until(FIXTURE_WAIT, |reqs| {
                    [OFFER_SUB_ID, AWARD_SUB_ID, WRAP_SUB_ID]
                        .iter()
                        .all(|id| reqs.iter().any(|r| r.subscription_id == *id))
                })
                .await,
            "harness check: the boot subscriptions must reach the relay before we induce a recovery"
        );

        runner
            .reconnect_and_resubscribe(&relay, nostr_sdk::Timestamp::from(0))
            .await
            .expect("recovery must succeed against a relay that authenticates");

        assert!(
            fixture
                .wait_until(FIXTURE_WAIT, |reqs| {
                    [OFFER_SUB_ID, AWARD_SUB_ID, WRAP_SUB_ID]
                        .iter()
                        .all(|id| reqs.iter().filter(|r| r.subscription_id == *id).count() >= 2)
                })
                .await,
            "the recovery must re-issue every REQ"
        );

        // The fourth subscription: the liveness probe, which only exists on a session the relay is
        // actually serving. Asserting it here is what makes "all four end live" true rather than
        // three-plus-an-assumption.
        assert!(
            probe_relay_serves_our_reqs(&runner.client, runner.seller_pubkey, FIXTURE_WAIT).await,
            "the liveness probe must answer on the recovered session"
        );

        let reqs = fixture.reqs().await;
        assert!(
            p_gated_before_auth(&reqs).is_empty(),
            "a p-gated REQ reached the relay before NIP-42 completed — that is #189: {:?}",
            p_gated_before_auth(&reqs)
        );
        assert!(
            permanently_removed(&reqs).is_empty(),
            "the relay permanently removed a subscription (`restricted:`), so the money leg is dead \
             until the next backfill: {:?}",
            permanently_removed(&reqs)
        );
        for id in [
            OFFER_SUB_ID,
            AWARD_SUB_ID,
            WRAP_SUB_ID,
            LIVENESS_PROBE_SUB_ID,
        ] {
            let last = fixture
                .reqs_for(id)
                .await
                .pop()
                .unwrap_or_else(|| panic!("no REQ recorded for {id}"));
            assert_eq!(
                last.verdict,
                Verdict::Eose,
                "{id} must end the recovery LIVE (served), not closed"
            );
        }

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// TOOTH #189 (c) — the money leg survives REPEATED recoveries, not just the first. A one-shot
    /// ordering fix that degrades after a few cycles would still pin settlement to the 300s backfill
    /// on the reconnect-heavy hosts where this was found.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wraps_subscription_survives_ten_consecutive_reconnects() {
        let fixture = PGateRelay::start(Duration::from_millis(120)).await;
        let root = throwaway_root("tenreconnects");
        let runner = boot_against(&root, &fixture, false).await;
        let relay = relay_handle(&runner).await;
        runner.subscribe_all(None).await.expect("boot subscribe");
        // The boot REQ must have LANDED before the first recovery clears the registrations, or the
        // clear races it and the first cycle measures a REQ that was never sent.
        assert!(
            fixture
                .wait_until(FIXTURE_WAIT, |reqs| reqs
                    .iter()
                    .any(|r| r.subscription_id == WRAP_SUB_ID))
                .await,
            "harness check: the boot kind-1059 REQ must reach the relay first"
        );

        for cycle in 1..=10 {
            // Counted against the previous cycle rather than the cycle index: the SDK's background
            // reconnect can issue a wrap REQ of its own at any point, and an absolute count would
            // read that as this cycle's.
            let before = fixture.reqs_for(WRAP_SUB_ID).await.len();
            runner
                .reconnect_and_resubscribe(&relay, nostr_sdk::Timestamp::from(0))
                .await
                .unwrap_or_else(|error| panic!("recovery {cycle} failed: {error}"));
            assert!(
                fixture
                    .wait_until(FIXTURE_WAIT, |reqs| {
                        reqs.iter()
                            .filter(|r| r.subscription_id == WRAP_SUB_ID)
                            .count()
                            > before
                    })
                    .await,
                "recovery {cycle} did not re-issue the kind-1059 REQ"
            );
            let wraps = fixture.reqs_for(WRAP_SUB_ID).await;
            let last = wraps.last().expect("a wrap REQ exists");
            assert_eq!(
                last.verdict,
                Verdict::Eose,
                "the kind-1059 money leg was refused on recovery {cycle}: {last:?}"
            );
            assert!(
                last.authenticated,
                "recovery {cycle} sent the kind-1059 REQ on an unauthenticated session"
            );
        }

        // Deliberately NOT asserting "zero pre-auth p-gated REQs across all ten cycles". That would
        // contradict what the fix claims: the SDK's own background reconnect resubscribes before
        // AUTH (`relay/inner.rs:748-752`) and has no hook, and a recovery whose reconnect fails
        // re-registers on purpose so the SDK can still rescue us — both put a pre-auth REQ on the
        // wire by design, which is exactly why the retry belt exists. The per-cycle assertions above
        // are the real claim: after every recovery the money leg ends up SERVED on an AUTHENTICATED
        // session. The blanket form flaked here under full-suite parallelism, and it deserved to.
        // The single controlled recovery in the ordering tooth is where the zero-leak claim belongs.

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// TOOTH #189 (b) — THE TAXONOMY MUST NOT SOFTEN. A genuine gate violation — a REQ for somebody
    /// else's `#p` — is still refused `restricted:`, still deleted by the SDK, and stays deleted.
    ///
    /// The belt cannot reach it by construction, and both halves of that are asserted: the id is not
    /// one of ours, and `subscription_pins_only_our_pubkey` refuses it even if it were. Collapse the
    /// belt's own-`#p` guard into a bare `restricted:` check and this goes red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_genuine_wrong_p_restricted_stays_removed() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("wrongp");
        let runner = boot_against(&root, &fixture, false).await;
        let relay = relay_handle(&runner).await;

        let stranger = Keys::generate().public_key();
        let foreign_id = "someone-elses-gift-wraps";
        runner
            .client
            .subscribe_with_id(
                nostr_sdk::SubscriptionId::new(foreign_id),
                Filter::new().kind(Kind::GiftWrap).pubkey(stranger),
                None,
            )
            .await
            .expect("send the offending REQ");

        assert!(
            fixture
                .wait_until(FIXTURE_WAIT, |reqs| reqs
                    .iter()
                    .any(|r| r.subscription_id == foreign_id))
                .await,
            "harness check: the offending REQ must reach the relay"
        );
        let refusal = fixture
            .reqs_for(foreign_id)
            .await
            .pop()
            .expect("the offending REQ was recorded");
        assert!(
            matches!(&refusal.verdict, Verdict::Closed(reason) if reason.starts_with("restricted:")),
            "a wrong-#p REQ must still be refused `restricted:`, authenticated or not: {refusal:?}"
        );
        assert!(
            refusal.authenticated,
            "harness check: this refusal must come from an AUTHENTICATED session, otherwise it \
             proves nothing about a genuine violation"
        );

        // The SDK deleted it, and nothing in the client puts it back. Waited for rather than read
        // once: the relay recording the REQ and the SDK processing the CLOSED are different sides of
        // the socket, so a bare read races the removal under load — which is a flaky test, not a
        // finding.
        let removed = tokio::time::timeout(FIXTURE_WAIT, async {
            loop {
                if !relay
                    .subscriptions()
                    .await
                    .keys()
                    .any(|id| id.to_string() == foreign_id)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .is_ok();
        assert!(
            removed,
            "`restricted:` must remain permanent-class: the subscription stays removed"
        );
        assert!(
            !is_our_subscription(foreign_id),
            "the belt only ever considers our own subscription ids"
        );
        assert!(
            !subscription_pins_only_our_pubkey(foreign_id, false),
            "and even then only ids whose every filter pins #p to our OWN pubkey"
        );
        assert_eq!(
            fixture.reqs_for(foreign_id).await.len(),
            1,
            "the offending REQ must never be retried"
        );

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// TOOTH — an unknown-id `CLOSED` is INERT beyond its log line. Escalating one cost a reconnect
    /// per cycle on a socket that was never broken, and that reconnect is what re-closed the money
    /// leg. Pinned here so a refactor cannot quietly restore the escalation.
    ///
    /// Two things make this tooth bite rather than decorate. The watchdog is ENABLED, so a forced
    /// recovery has a tick to run on — with it off, "no reconnect happened" would be true even with
    /// the escalation restored. And the window is long enough for a reconnect to COMPLETE against
    /// this fixture (~6s), because a window shorter than that reads a recovery still in progress as
    /// a recovery that never happened. A first draft of this tooth waited 4s and passed under
    /// revert; the wait below returns early when the socket count moves, so the red path is fast and
    /// only the green path pays the full window.
    ///
    /// RED ON REVERT: drop the `!is_our_subscription` early return so the unknown id falls through
    /// to `forced_recovery`, and the socket-count assertion fires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unknown_id_closed_costs_no_reconnect_and_no_resubscribe() {
        use_fast_backfill_tick();
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("unknownid");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = fixture.url();
        home.config.seller = Some(seller_cfg(1, false));
        home.config.seller_heartbeat.enabled = true;
        home.config.seller_heartbeat.interval_secs = 1;
        let runner = SellerNodeRunner::boot(home).await.expect("boot runner");
        // `run()` is NOT `Send` under the `acp` feature — the runner holds an `AcpDriver`
        // whose std mpsc `Receiver` is `!Sync` — so `tokio::spawn` fails to COMPILE on the
        // seller's real feature combo (`acp` + `wallet`), while compiling fine on the
        // workspace default. A `LocalSet` keeps the loop on this thread, which is also the
        // truer shape: the node runs its loop as one task, not spread across a pool.
        let local = tokio::task::LocalSet::new();
        let loop_handle = local.spawn_local(async move { runner.run().await });
        local
            .run_until(async {

                assert!(
                    fixture
                        .wait_until(FIXTURE_WAIT, |reqs| reqs
                            .iter()
                            .any(|r| r.subscription_id == WRAP_SUB_ID))
                        .await,
                    "harness check: the seat must be up before we close something it never registered"
                );
                // The watchdog must be demonstrably live, or "no reconnect" is just a dead loop.
                assert!(
                    fixture
                        .wait_until(FIXTURE_WAIT, |reqs| reqs
                            .iter()
                            .any(|r| r.subscription_id == LIVENESS_PROBE_SUB_ID))
                        .await,
                    "harness check: the heartbeat watchdog must be ticking, otherwise a forced recovery \
                     could not have fired even if one had been requested"
                );
                let connections_before = fixture.connections();

                let stranger_id = "some-subscription-we-never-registered";
                fixture
                    .close_now(
                        stranger_id,
                        "restricted: p-gated events require #p matching your pubkey",
                    )
                    .await;

                let escalated = tokio::time::timeout(Duration::from_secs(20), async {
                    loop {
                        if fixture.connections() != connections_before {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await
                .is_ok();
                assert!(
                    !escalated,
                    "a CLOSED for an id we never registered forced a reconnect — that is a reconnect per \
                     cycle on a socket that was never broken"
                );
                assert!(
                    fixture.reqs_for(stranger_id).await.is_empty(),
                    "we must never REQ a subscription id that was never ours"
                );
                // Still alive and still watching: inert about the close, not inert about liveness.
                let probes_before = fixture.reqs_for(LIVENESS_PROBE_SUB_ID).await.len();
                assert!(
                    fixture
                        .wait_until(FIXTURE_WAIT, |reqs| reqs
                            .iter()
                            .filter(|r| r.subscription_id == LIVENESS_PROBE_SUB_ID)
                            .count()
                            > probes_before)
                        .await,
                    "the node must keep probing after an unknown-id CLOSED"
                );

            })
            .await;
        loop_handle.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every kind-30340 seat announcement that reached the relay, in arrival order.
    fn seat_announcements(events: &[PublishedEvent]) -> Vec<&PublishedEvent> {
        events
            .iter()
            .filter(|event| event.kind == u64::from(crate::heartbeat::SELLER_HEARTBEAT_KIND))
            .collect()
    }

    /// TOOTH #747 — a seat that leaves the selling role RETRACTS its announcement on the way out.
    ///
    /// Why this needs a wire test rather than a unit test of the draft: kind-30340 is addressable,
    /// so the seat's LAST published announcement is its permanent public answer. A builder that
    /// produces `accepting=n` proves nothing unless that beat actually reaches the relay after the
    /// loop has decided to stop — which is a property of the exit path, not of the draft. The
    /// assertion is therefore on the last announcement the relay received, after a real run loop was
    /// asked to shut down and returned.
    ///
    /// The shutdown is driven through the handle rather than a real SIGTERM because a signal would
    /// hit the whole test binary. `sell.rs` wires that same handle to SIGTERM/SIGINT via
    /// `shutdown::spawn_os_signal_listener`; this covers everything downstream of the request.
    ///
    /// ⛔ WHAT THIS DOES NOT PROVE — and no test could: that the directory is now truthful. This
    /// path runs only on a graceful exit. SIGKILL, a panic that skips unwinding, an OOM kill and a
    /// power cut publish nothing at all, and leave the seat's last `accepting=y` standing exactly as
    /// the issue describes. Consumer-side recency filtering stays the only cover for those.
    ///
    /// RED ON REVERT: drop the `self.publish_retraction().await` from `run_loop` (or the
    /// `shutdown::next_request` arm from the select, which strands the loop so the join times out)
    /// and this goes red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_seat_leaving_the_selling_role_retracts_its_announcement() {
        use_fast_backfill_tick();
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("retract-on-leave");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = fixture.url();
        home.config.seller = Some(seller_cfg(1, false));
        home.config.seller_heartbeat.enabled = true;
        home.config.seller_heartbeat.interval_secs = 1;
        let runner = SellerNodeRunner::boot(home).await.expect("boot runner");
        // Taken BEFORE `run`, which consumes the runner — the same order `sell.rs` uses.
        let shutdown = runner.shutdown_handle();

        let local = tokio::task::LocalSet::new();
        let loop_handle = local.spawn_local(async move { runner.run().await });
        let joined = local
            .run_until(async {
                // Harness check: the seat must first be OPEN on the wire. Without this the terminal
                // `accepting=n` would be asserted against a seat that never advertised itself as
                // available, and the tooth would pass on a node that simply never published.
                assert!(
                    fixture
                        .wait_until_published(FIXTURE_WAIT, |events| seat_announcements(events)
                            .iter()
                            .any(|beat| beat.tag_value("accepting") == Some("y")))
                        .await,
                    "harness check: the seat must advertise itself as open before it retracts"
                );

                assert!(
                    shutdown.request("test-requested stop"),
                    "the loop must accept a shutdown request"
                );
                tokio::time::timeout(FIXTURE_WAIT, loop_handle).await
            })
            .await;

        // The loop returns rather than being killed — the exit path is what publishes the terminal
        // beat, so a loop that never returns cannot have published one.
        let outcome = joined
            .expect("the run loop must RETURN on a shutdown request, not have to be killed")
            .expect("the loop task must not panic");
        assert!(outcome.is_ok(), "a requested shutdown is a clean exit: {outcome:?}");

        let events = fixture.events().await;
        let beats = seat_announcements(&events);
        let terminal = beats
            .last()
            .expect("the seat published at least one announcement");
        assert_eq!(
            terminal.tag_value("accepting"),
            Some("n"),
            "the LAST announcement a departing seat leaves standing must retract it — it is \
             addressable, so no later event will ever correct it"
        );
        // It must land at the SAME address, or it sits beside the stale announcement instead of
        // replacing it, and the directory goes on reading the old one.
        assert_eq!(
            terminal.tag_value("d"),
            Some(crate::heartbeat::SELLER_HEARTBEAT_D)
        );
        assert_eq!(terminal.tag_value("v"), Some(crate::gateway::PROTOCOL_VERSION));
        assert_eq!(
            terminal.tag_value("t"),
            Some(crate::gateway::MAXPLAYER_TAG),
            "the terminal beat is an ordinary §4.2 announcement, not a special-cased event"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Budget for one challenge-roll to re-authenticate the live socket and for the fix to re-issue
    /// the long-lived legs on that completed auth. Generous; the REVERT path never re-issues, so it
    /// spends the whole budget then trips — a slow but unambiguous red.
    const RECHALLENGE_WAIT: Duration = Duration::from_secs(30);

    /// The long-lived subscriptions the daemon must re-issue on a completed auth: the offer, award,
    /// and kind-1059 legs (what [`subscribe_all`] carries). The liveness probe is transient — it is
    /// re-issued every heartbeat and self-heals — so it is not in this set.
    const LONG_LIVED_SUBS: [&str; 3] = [OFFER_SUB_ID, AWARD_SUB_ID, WRAP_SUB_ID];

    /// How many REQs for `id` the relay served (`EOSE`) on an AUTHENTICATED session — the count that
    /// must grow after each challenge-roll for the leg to be genuinely restored (not merely re-sent
    /// onto a stale generation and refused).
    fn served_authed(reqs: &[ReqRecord], id: &str) -> usize {
        reqs.iter()
            .filter(|r| r.subscription_id == id && r.authenticated && r.verdict == Verdict::Eose)
            .count()
    }

    /// TOOTH #429 — a live-socket NIP-42 RE-CHALLENGE must not leave the money leg deaf: the daemon
    /// re-issues the long-lived subscriptions on every COMPLETED auth, re-armed per challenge-roll.
    ///
    /// FIELD MECHANISM (deployed relay + nostr-sdk 0.44.1, three independent source reads): the relay
    /// refuses an unauthenticated `#p`-self REQ with `auth-required:` — retryable, and NEVER
    /// `restricted:`, so the #189 belt cannot even fire for the money leg — and closes the auth-scoped
    /// subs when auth lapses on a LIVE socket. nostr-sdk's only live-socket repair is a one-shot
    /// post-auth `resubscribe()` (`relay/inner.rs:941`) gated on `closed==true`; it races the CLOSED
    /// that sets that flag and, field-observed, loses — offers/awards/kind-1059 stay
    /// registered-but-deaf, no reconnect and no restart, until the next completed auth (which the
    /// periodic backfill triggers incidentally). Every missed kind-1059 is an unredeemed payment.
    ///
    /// WHY THIS TOOTH MODELS THE ROLL WITHOUT THE `auth-required:` CLOSE FRAME. The faithful frame
    /// makes the red-prove VACUOUS against a REAL nostr-sdk: an `auth-required:` CLOSE marks the sub
    /// `closed==true` (MarkAsClosed — kept in the registry), and the re-challenge's post-auth
    /// `resubscribe()` then re-sends it and the relay serves it, so the SDK SELF-HEALS and a
    /// fix-removed run still passes (empirically confirmed). The field failure is the resubscribe
    /// RACING that CLOSE and losing (it fires while `closed==false`, then the CLOSE lands with no
    /// further resubscribe) — inherently non-deterministic, and a race-based test would be a flake in
    /// a module we are actively de-flaking. So this tooth WITHHOLDS the CLOSE and lets the STALE
    /// generation alone deafen the leg: with the sub `closed==false`, the SDK's `closed==true`-gated
    /// resubscribe correctly SKIPS it, so the ONLY thing that re-serves it is the daemon's
    /// unconditional re-issue-on-auth. The induction differs from the field (closed==false-skip vs
    /// closed==true race-loss) but the OUTCOME under test is identical — the SDK does not restore on
    /// the re-auth, only the daemon does — and `subscribe_all` re-sends unconditionally
    /// (`pool/mod.rs:603`) regardless of closed-state, so the fix behaves identically either way. That
    /// the fix ALSO restores the real `closed==true` state is confirmed separately by
    /// [`resubscribe_on_auth_restores_an_auth_required_closed_leg`].
    ///
    /// [`PGateRelay::roll_challenge`] with an EMPTY close-set: bump the relay's auth generation and
    /// re-issue an `AUTH` challenge so the client re-authenticates IN PLACE (no reconnect). The boot
    /// subs are now on the stale generation; only a REQ re-issued after the socket catches up serves
    /// on the authenticated (current-generation) session. Test double for the observed contract, not a
    /// claim about relay internals — the deployed relay has no generation concept (see [`decide`]).
    ///
    /// Two consecutive rolls prove the re-issue is re-armed per challenge-roll, not once-per-process.
    /// WITH the fix, both cycles restore every long-lived leg SERVED on an AUTHENTICATED session.
    ///
    /// RED ON REVERT: delete the resubscribe-on-auth block from the `Authenticated` arm. The socket
    /// still re-authenticates, but nothing re-issues the stale legs, so no fresh served+authed REQ
    /// ever appears after the roll and the cycle-1 wait times out — the leg stays deaf on the stale
    /// generation exactly as the field's leg stayed deaf on its closed one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn long_lived_subs_resubscribe_on_auth_after_a_challenge_roll() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("rechallenge");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = fixture.url();
        home.config.seller = Some(seller_cfg(1, false));
        // The heartbeat/watchdog is deliberately OFF so no stall-recovery reconnect can re-serve the
        // legs on its own and mask the fix — matching the field, where the leg went deaf with NO
        // reconnect. The resubscribe-on-auth fix lives in the `Authenticated` arm, not the heartbeat,
        // so it fires regardless; OFF leaves it the ONLY restorer, which is what makes the revert red.
        home.config.seller_heartbeat.enabled = false;
        let runner = SellerNodeRunner::boot(home).await.expect("boot runner");
        // `run()` is NOT `Send` under the `acp` feature (the runner holds an `AcpDriver` with a
        // `!Sync` std mpsc `Receiver`), so keep the loop on this thread with a `LocalSet`, exactly as
        // the sibling loop teeth do.
        let local = tokio::task::LocalSet::new();
        let loop_handle = local.spawn_local(async move { runner.run().await });
        local
            .run_until(async {
                // Boot: every long-lived leg is served on an authenticated session — the baseline a
                // challenge-roll knocks down and every completed auth must restore.
                for id in LONG_LIVED_SUBS {
                    assert!(
                        fixture
                            .wait_until(FIXTURE_WAIT, |reqs| served_authed(reqs, id) >= 1)
                            .await,
                        "harness check: {id} must boot SERVED on an AUTHENTICATED session"
                    );
                }

                for cycle in 1..=2 {
                    let snapshot = fixture.reqs().await;
                    let before: Vec<usize> = LONG_LIVED_SUBS
                        .iter()
                        .map(|id| served_authed(&snapshot, id))
                        .collect();

                    // The live-socket re-challenge, WITHOUT the `auth-required:` CLOSE (see the tooth
                    // doc for why): the generation rolls and the client re-authenticates in place,
                    // leaving the boot legs on the now-stale generation.
                    fixture.roll_challenge(&[]).await;

                    // Every long-lived leg must be RE-ISSUED and SERVED on the re-authenticated
                    // session — a fresh served+authed REQ beyond the pre-roll count. WITHOUT the fix
                    // nothing re-issues them, so this times out on cycle 1 (the leg stays deaf).
                    for (idx, id) in LONG_LIVED_SUBS.iter().enumerate() {
                        let target = before[idx];
                        assert!(
                            fixture
                                .wait_until(RECHALLENGE_WAIT, |reqs| served_authed(reqs, id)
                                    > target)
                                .await,
                            "cycle {cycle}: {id} was not re-issued+served on the re-authenticated \
                             session after a challenge roll — the completed-auth resubscribe did not \
                             restore it"
                        );
                    }
                }
            })
            .await;
        loop_handle.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// CONFIRMATION (not a red-prove) — the resubscribe-on-auth fix operates correctly against the
    /// REAL `closed==true` field state, restoring an `auth-required:`-CLOSED long-lived leg on the
    /// re-auth. Kept as SEPARATE evidence that the fix runs on the frame the field actually showed
    /// (re-issuing offers+awards+kind-1059 on the completed auth, no loop, no error), so the no-CLOSE
    /// red-prove above is not the only thing tying the fix to the field.
    ///
    /// This deliberately CANNOT be the non-vacuous prover: against a real nostr-sdk a `closed==true`
    /// sub is ALSO re-sent by the SDK's own post-auth `resubscribe()`, so removing the fix does NOT
    /// turn this red (the SDK self-heals it) — which is precisely why the deterministic red-prove
    /// withholds the CLOSE. Its job here is fidelity to the observed frame, not regression-guarding;
    /// [`long_lived_subs_resubscribe_on_auth_after_a_challenge_roll`] does the guarding.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resubscribe_on_auth_restores_an_auth_required_closed_leg() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("authrequiredclose");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = fixture.url();
        home.config.seller = Some(seller_cfg(1, false));
        home.config.seller_heartbeat.enabled = false;
        let runner = SellerNodeRunner::boot(home).await.expect("boot runner");
        let local = tokio::task::LocalSet::new();
        let loop_handle = local.spawn_local(async move { runner.run().await });
        local
            .run_until(async {
                for id in LONG_LIVED_SUBS {
                    assert!(
                        fixture
                            .wait_until(FIXTURE_WAIT, |reqs| served_authed(reqs, id) >= 1)
                            .await,
                        "harness check: {id} must boot SERVED on an AUTHENTICATED session"
                    );
                }
                let snapshot = fixture.reqs().await;
                let before: Vec<usize> = LONG_LIVED_SUBS
                    .iter()
                    .map(|id| served_authed(&snapshot, id))
                    .collect();

                // The field's ACTUAL frame: each long-lived leg CLOSED `auth-required:` on the live
                // socket (marked `closed==true`), then an in-place re-auth.
                fixture.roll_challenge(&LONG_LIVED_SUBS).await;

                for (idx, id) in LONG_LIVED_SUBS.iter().enumerate() {
                    let target = before[idx];
                    assert!(
                        fixture
                            .wait_until(RECHALLENGE_WAIT, |reqs| served_authed(reqs, id) > target)
                            .await,
                        "{id} was not restored SERVED+AUTHED after an auth-required close + re-auth"
                    );
                }
            })
            .await;
        loop_handle.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The unknown-id line is field-facing: the relay owner reads it to separate our own transient
    /// `fetch_events` REQ from a relay-side auth-TTL sweep, so both ages have to actually be in it.
    #[test]
    fn the_unknown_close_diagnostic_carries_both_ages_and_the_auth_state() {
        let line = unknown_close_diagnostic("deadbeef", 7, 301, true);
        assert!(line.starts_with("seller node RELAY-CLOSED UNKNOWN-ID:"));
        for expected in [
            "id=deadbeef",
            "last_backfill=7s ago",
            "last_nip42_auth=301s ago",
            "authed=true",
            "no recovery forced",
            WRAP_SUB_ID,
        ] {
            assert!(
                line.contains(expected),
                "the unknown-id diagnostic must carry {expected:?}, or the relay owner cannot tell \
                 the two hypotheses apart: {line}"
            );
        }
    }

    /// One owned tick, for the loop teeth below. Both #190 loop teeth set the SAME value, so running
    /// them in parallel cannot make them disagree.
    const TEST_BACKFILL_SECS: &str = "1";

    /// Drive the backfill tick fast enough to observe. This is the documented test-only seam; no
    /// production path sets it.
    fn use_fast_backfill_tick() {
        unsafe { std::env::set_var(WRAP_BACKFILL_INTERVAL_ENV, TEST_BACKFILL_SECS) };
    }

    /// Offer REQs that carried the un-pinned open-pool filter — i.e. the grouped shape, armed.
    fn grouped_offer_reqs(reqs: &[ReqRecord]) -> Vec<&ReqRecord> {
        reqs.iter()
            .filter(|record| record.subscription_id == OFFER_SUB_ID && record.has_unpinned_filter)
            .collect()
    }

    /// TOOTH #190 (a) + (b) — THE OWNED RE-ARM. Drop the open-pool half on a seat that is perfectly
    /// healthy and never reconnects; the open-pool half must come back on its own within one owned
    /// tick, and the targeted half must never be disturbed while that happens.
    ///
    /// This is the only proof Fix 2 has. The reported stuck specimen was withdrawn — every seat seen
    /// degraded in the field was flapping on the #189 sawtooth — so the quiet-seat case is reasoned,
    /// not observed, and this tooth is what stands in for the observation.
    ///
    /// RED ON REVERT: delete the `open_pool` block from the `wrap_backfill_tick` arm (the hookup) and
    /// this goes red — nothing else re-arms without a recovery, and no recovery ever happens here.
    /// A state-machine-only test would stay green under that revert, which is why this drives the
    /// real loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_pool_rearms_on_an_owned_tick_without_any_reconnect() {
        use_fast_backfill_tick();
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("rearm");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = fixture.url();
        home.config.seller = Some(seller_cfg(1, true));
        // The watchdog is off so nothing but the CLOSED under test can move the node. A recovery
        // would re-arm the open-pool half for the wrong reason and the tooth would prove nothing.
        home.config.seller_heartbeat.enabled = false;
        let runner = SellerNodeRunner::boot(home).await.expect("boot runner");
        // `run()` is NOT `Send` under the `acp` feature — the runner holds an `AcpDriver`
        // whose std mpsc `Receiver` is `!Sync` — so `tokio::spawn` fails to COMPILE on the
        // seller's real feature combo (`acp` + `wallet`), while compiling fine on the
        // workspace default. A `LocalSet` keeps the loop on this thread, which is also the
        // truer shape: the node runs its loop as one task, not spread across a pool.
        let local = tokio::task::LocalSet::new();
        let loop_handle = local.spawn_local(async move { runner.run().await });
        local
            .run_until(async {

                assert!(
                    fixture
                        .wait_until(FIXTURE_WAIT, |reqs| !grouped_offer_reqs(reqs).is_empty())
                        .await,
                    "harness check: the seat must boot with the open-pool half ARMED, or there is nothing \
                     to degrade"
                );
                let connections_before = fixture.connections();
                let grouped_before = grouped_offer_reqs(&fixture.reqs().await).len();

                // The degrade, exactly as the field sees it: an unsolicited CLOSED on a healthy socket.
                fixture
                    .close_now(
                        OFFER_SUB_ID,
                        "restricted: p-gated events require #p matching your pubkey",
                    )
                    .await;

                assert!(
                    fixture
                        .wait_until(FIXTURE_WAIT, |reqs| grouped_offer_reqs(reqs).len()
                            > grouped_before)
                        .await,
                    "the open-pool half was never re-armed: a healthy seat that degrades has no recovery to \
                     wait for, which is #190"
                );

                // Not an observation but the test's PREMISE, and the reason it proves anything: with the
                // watchdog off there is no recovery path in this process at all, so the re-arm above cannot
                // have come from one. `open_pool_degraded = false` in the recovery-success arm — the only
                // re-arm before this fix — is unreachable here.
                assert_eq!(
                    fixture.connections(),
                    connections_before,
                    "harness check: nothing may reconnect in this test, or the re-arm could be the old \
                     recovery path in disguise"
                );

                // (b) The targeted half is never disturbed: every offer REQ ever sent, degraded or grouped,
                // carries the `#p == self` filter. A degrade that dropped it would stop targeted claiming.
                let offers = fixture.reqs_for(OFFER_SUB_ID).await;
                assert!(offers.len() >= 3, "expected boot + degrade + re-arm REQs");
                for req in &offers {
                    assert!(
                        req.p_pinned,
                        "an offer REQ went out without the targeted #p filter: {req:?}"
                    );
                    assert_eq!(
                        req.verdict,
                        Verdict::Eose,
                        "the relay refused an offer REQ it should have served: {req:?}"
                    );
                    // Both shapes ride ONE subscription: grouped is targeted + un-pinned, degraded is
                    // targeted alone. A third shape would mean the two filters had been split across
                    // subscriptions, which delivers stored offers but never live ones.
                    let expected = if req.has_unpinned_filter { 2 } else { 1 };
                    assert_eq!(
                        req.filter_count, expected,
                        "an offer REQ carried an unexpected filter count: {req:?}"
                    );
                }

            })
            .await;
        loop_handle.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// TOOTH #190 (c) — a relay that keeps refusing the open-pool half must cost a REQ per BACKOFF,
    /// never a REQ per tick. With a 1s owned tick and refusals armed, the doubling schedule (attempt,
    /// skip 1, skip 2, skip 4, …) has to hold the attempt count far below the tick count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn repeated_open_pool_rejection_backs_off_and_never_hot_loops() {
        use_fast_backfill_tick();
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("backoff");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        home.config.relay_url = fixture.url();
        home.config.seller = Some(seller_cfg(1, true));
        home.config.seller_heartbeat.enabled = false;
        let runner = SellerNodeRunner::boot(home).await.expect("boot runner");
        // `run()` is NOT `Send` under the `acp` feature — the runner holds an `AcpDriver`
        // whose std mpsc `Receiver` is `!Sync` — so `tokio::spawn` fails to COMPILE on the
        // seller's real feature combo (`acp` + `wallet`), while compiling fine on the
        // workspace default. A `LocalSet` keeps the loop on this thread, which is also the
        // truer shape: the node runs its loop as one task, not spread across a pool.
        let local = tokio::task::LocalSet::new();
        let loop_handle = local.spawn_local(async move { runner.run().await });
        local
            .run_until(async {

                assert!(
                    fixture
                        .wait_until(FIXTURE_WAIT, |reqs| !grouped_offer_reqs(reqs).is_empty())
                        .await,
                    "harness check: the seat must boot with the open-pool half armed"
                );
                // Every grouped REQ from here on is refused; the targeted-only re-subscribe is still served.
                fixture
                    .refuse_unpinned(
                        OFFER_SUB_ID,
                        12,
                        "restricted: p-gated events require #p matching your pubkey",
                    )
                    .await;
                let grouped_before = grouped_offer_reqs(&fixture.reqs().await).len();

                fixture
                    .close_now(
                        OFFER_SUB_ID,
                        "restricted: p-gated events require #p matching your pubkey",
                    )
                    .await;

                // Twelve owned ticks. Un-backed-off, that is twelve attempts; the schedule allows at most
                // four (t+0, +2, +5, +10).
                tokio::time::sleep(Duration::from_secs(12)).await;
                let attempts = grouped_offer_reqs(&fixture.reqs().await).len() - grouped_before;
                assert!(
                    attempts >= 1,
                    "the re-arm must still be attempted — backoff is not abandonment"
                );
                assert!(
                    attempts <= 5,
                    "the open-pool re-arm hot-looped: {attempts} attempts over ~12 owned ticks, which is a \
                     REQ per tick against a relay that has refused every one"
                );

                // The targeted half kept working throughout — a backing-off re-arm must not starve claiming.
                let served_targeted = fixture
                    .reqs_for(OFFER_SUB_ID)
                    .await
                    .into_iter()
                    .filter(|req| !req.has_unpinned_filter && req.verdict == Verdict::Eose)
                    .count();
                assert!(
                    served_targeted >= 1,
                    "the targeted-only offer subscription must stay live across the backoff"
                );

            })
            .await;
        loop_handle.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The backoff arithmetic itself: doubling, capped, and never zero after a refusal — a zero
    /// cooldown at any rejection count would be the hot loop the loop tooth above forbids.
    #[test]
    fn open_pool_rearm_backoff_doubles_and_stays_capped() {
        assert_eq!(
            open_pool_rearm_cooldown_ticks(0),
            0,
            "the first attempt after a degrade is not delayed"
        );
        let schedule: Vec<u32> = (1..=8).map(open_pool_rearm_cooldown_ticks).collect();
        assert_eq!(schedule, vec![1, 2, 4, 8, 12, 12, 12, 12]);
        for rejections in 1..=64 {
            assert!(
                open_pool_rearm_cooldown_ticks(rejections) >= 1,
                "a refused re-arm must always cost at least one skipped tick"
            );
            assert!(
                open_pool_rearm_cooldown_ticks(rejections) <= 12,
                "the backoff must stay capped so a re-arm is never abandoned"
            );
        }
    }

    /// The degrade state machine, including the case a timer-less design would park on: an attempt
    /// that draws no verdict at all. Silence must advance the backoff, never wait forever.
    #[test]
    fn a_rearm_attempt_with_no_verdict_is_treated_as_a_refusal() {
        let mut state = OpenPoolDegrade::new();
        assert_eq!(state.on_tick(), RearmStep::Attempt, "first tick attempts");
        assert!(state.attempt_pending);

        // No EOSE, no CLOSED — the relay simply said nothing.
        assert_eq!(
            state.on_tick(),
            RearmStep::Wait,
            "a pending attempt is not re-sent on top of itself"
        );
        assert!(
            !state.attempt_pending,
            "silence must resolve the attempt rather than leave it pending forever"
        );
        assert_eq!(state.rejections, 1);
        assert_eq!(state.cooldown_ticks, 1);

        assert_eq!(state.on_tick(), RearmStep::Wait, "cooling down");
        assert_eq!(state.on_tick(), RearmStep::Attempt, "then attempting again");
    }

    // ---- #773 pre-advertise probe report -------------------------------------------------------
    //
    // Operator lines for the prove-before-advertise probe are composed HERE, not printed, so they
    // can be asserted on injected verdicts with no relay, no home lock, and no spawn. The call site
    // in `boot_advertising_only_proven` emits them BEFORE the gate — including the partial-failure
    // path that used to boot, narrow the roster, and advertise in silence.

    /// Nothing proved: every failing harness is named, then `0/m`.
    ///
    /// RED ON REVERT: drop the FAILED-line push from `pre_advertise_probe_lines` and this set
    /// returns only the serving count.
    #[test]
    fn the_pre_advertise_probe_names_every_failure_when_nothing_proved() {
        let lines = pre_advertise_probe_lines(&[
            HarnessProbeVerdict {
                index: 0,
                name: Some("claude".to_owned()),
                result: Err(("spawn failed".to_owned(), Fault::Unproven)),
            },
            HarnessProbeVerdict {
                index: 1,
                name: Some("codex".to_owned()),
                result: Err(("no artifact".to_owned(), Fault::Unproven)),
            },
        ]);
        assert_eq!(
            lines,
            vec![
                "seller node pre-advertise probe FAILED claude: spawn failed".to_owned(),
                "seller node pre-advertise probe FAILED codex: no artifact".to_owned(),
                "seller node pre-advertise probe: serving 0/2 configured harness(es)".to_owned(),
            ],
            "an all-failed probe must name every drop and report serving 0/m"
        );
    }

    /// THE #773 CASE. At least one harness proves AND at least one fails: the seat will boot and
    /// advertise a narrowed roster, so the log must still name every drop. The FAILED loop used to
    /// live inside the all-failed gate, and this mixed set produced no operator line at all.
    ///
    /// RED ON REVERT: gate the FAILED-line push on `proven_serving_indices(verdicts).is_empty()` —
    /// the pre-fix shape — and this mixed set returns only the serving line.
    #[test]
    fn the_pre_advertise_probe_names_every_failure_when_some_still_prove() {
        let lines = pre_advertise_probe_lines(&[
            HarnessProbeVerdict {
                index: 0,
                name: Some("claude".to_owned()),
                result: Ok(None),
            },
            HarnessProbeVerdict {
                index: 1,
                name: Some("codex".to_owned()),
                result: Err(("launcher missing".to_owned(), Fault::Unproven)),
            },
        ]);
        assert_eq!(
            lines,
            vec![
                "seller node pre-advertise probe FAILED codex: launcher missing".to_owned(),
                "seller node pre-advertise probe: serving 1/2 configured harness(es)".to_owned(),
            ],
            "a partial failure must name the drop AND the surviving fraction"
        );
    }

    /// Every harness proved: no FAILED line, just the serving count. Without this, a reporter that
    /// always emitted a FAILED line (or never emitted the count) would still pass the failure tests.
    ///
    /// RED ON REVERT: drop the serving-count line from `pre_advertise_probe_lines` and an all-proved
    /// set returns empty.
    #[test]
    fn the_pre_advertise_probe_reports_full_service_when_every_harness_proved() {
        let lines = pre_advertise_probe_lines(&[
            HarnessProbeVerdict {
                index: 0,
                name: Some("claude".to_owned()),
                result: Ok(None),
            },
            HarnessProbeVerdict {
                index: 1,
                name: Some("codex".to_owned()),
                result: Ok(Some("codex-current".to_owned())),
            },
        ]);
        assert_eq!(
            lines,
            vec!["seller node pre-advertise probe: serving 2/2 configured harness(es)".to_owned()],
            "an all-proved probe must not invent a FAILED line, and must report serving m/m"
        );
    }

    /// A verdict whose name is `None` renders as `<unlabelled>` — the same fallback the old
    /// in-branch print used. A named-only assertion would let that fallback rot.
    ///
    /// RED ON REVERT: render `None` names as empty (or skip them) instead of `<unlabelled>` and this
    /// needle disappears.
    #[test]
    fn the_pre_advertise_probe_renders_a_nameless_verdict_as_unlabelled() {
        let lines = pre_advertise_probe_lines(&[HarnessProbeVerdict {
            index: 0,
            name: None,
            result: Err(("timed out".to_owned(), Fault::Unproven)),
        }]);
        assert_eq!(
            lines,
            vec![
                "seller node pre-advertise probe FAILED <unlabelled>: timed out".to_owned(),
                "seller node pre-advertise probe: serving 0/1 configured harness(es)".to_owned(),
            ],
            "a nameless failure must render as <unlabelled>, not as an empty label"
        );
    }

    // ---- #357 prove-before-advertise -----------------------------------------------------------
    //
    // These drive `boot_advertising_only_proven` against the fixture relay and assert what reached
    // the wire BY KIND (kind-0 identity, kind-30340 seat announcement). Safe: the bogus case fails
    // at spawn with ENOENT (no process created), and the healthy case injects a passing verdict so
    // no agent is ever spawned.

    /// A seat whose every configured harness FAILS its pre-advertise probe must publish nothing: no
    /// kind-0 identity and no kind-30340 announcement. The `[sandbox]` launcher is unresolvable, so
    /// the probe's spawn fails ENOENT before any agent runs.
    ///
    /// RED ON REVERT: delete the `is_empty` gate block in `boot_advertising_only_proven` (publish
    /// unconditionally) and the relay sees ≥1 kind-0 — the #357 bug, advertise-then-fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_seat_whose_harness_fails_its_probe_advertises_nothing() {
        use crate::home::SandboxConfig;
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("noprobe");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        // Persist seller + fixture relay + the bogus launcher. The launcher resolves to nothing, so
        // every probe spawn fails ENOENT (no child — `AcpDriver::spawn` is a synchronous
        // `std::process::Command::spawn` that errors first). Persisting the FIXTURE relay is
        // load-bearing for the red-prove: reverting the gate makes the disco publish run, and the
        // `reload_config` inside it must read the fixture url, never a real relay.
        crate::home::save_config(&mut home, |config| {
            config.seller = Some(seller_cfg(1, false));
            config.relay_url = fixture.url();
            config.sandbox = Some(SandboxConfig {
                mode: crate::home::SandboxMode::Launcher,
                launcher: vec!["definitely-not-a-real-binary-xyz".to_owned()],
                ..Default::default()
            });
        })
        .expect("persist config");

        let verdicts = probe_configured_harnesses(&home)
            .await
            .expect("probe returns verdicts");
        assert!(
            verdicts.iter().all(|verdict| verdict.result.is_err()),
            "harness check: a bogus launcher must fail every probe"
        );

        let outcome = boot_advertising_only_proven(home, verdicts).await;

        // The load-bearing property: a seat whose harness failed its probe put NOTHING on the wire.
        assert_eq!(
            fixture.event_kind_count(0).await,
            0,
            "a dead seat must NOT publish its kind-0 identity"
        );
        assert_eq!(
            fixture.event_kind_count(30340).await,
            0,
            "a dead seat must NOT publish a kind-30340 announcement"
        );
        assert_eq!(
            fixture.event_kind_count(31990).await,
            0,
            "kind-31990 is not part of the protocol (#645)"
        );
        // …and it refused to boot rather than lingering while advertising nothing (fail loud).
        match outcome {
            Err(NodeError::NoProvenHarness(_)) => {}
            Err(other) => panic!("expected NoProvenHarness, got: {other}"),
            Ok(_) => panic!("no prover must refuse to boot"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A seat with a PROVEN harness advertises and serves — and, since #645, its start publishes
    /// ZERO kind-31990 events.
    ///
    /// The kind-0 count is the POSITIVE CONTROL and it is load-bearing: `31990 == 0` is also what a
    /// seat that published nothing at all would report, and a boot that silently failed to reach the
    /// relay would pass a bare zero-check while proving nothing. Asserting kind-0 ≥ 1 in the same
    /// transcript establishes that the discoverability publish RAN and its events reached the wire,
    /// so the zero next to it is a real absence rather than an empty transcript.
    ///
    /// RED ON REVERT: restore the `publish_nip89_announce_async` call in
    /// `publish_seller_discoverability_async` and the 31990 count goes to 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn seller_start_publishes_kind0_and_zero_kind_31990() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("proven");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        // The discoverability publish reloads config from disk, so the seller + fixture relay must be
        // PERSISTED, not merely set in memory.
        crate::home::save_config(&mut home, |config| {
            config.seller = Some(seller_cfg(1, false));
            config.relay_url = fixture.url();
        })
        .expect("persist config");

        // Index 0 — the single `claude` preset — proved out. Injected: no agent spawn.
        let verdicts = vec![HarnessProbeVerdict {
            index: 0,
            name: Some("claude".to_owned()),
            result: Ok(None),
        }];

        let runner = boot_advertising_only_proven(home, verdicts)
            .await
            .expect("a proven seat must boot");

        // Positive control first: the publish path ran and its event reached this relay.
        let kind0 = fixture.event_kind_count(0).await;
        assert!(
            kind0 >= 1,
            "a proven seat MUST publish its kind-0 identity (positive control for the zero below)"
        );
        // The #645 property: nothing on the retired kind, in a transcript proven non-empty.
        assert_eq!(
            fixture.event_kind_count(31990).await,
            0,
            "seller start must publish ZERO kind-31990 events; the transcript holds {kind0} kind-0"
        );
        assert!(
            runner.agents.advertisement().serving,
            "the live roster must serve the proven harness"
        );

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Point ⑤ of #784: the FIRST advertisement carries the MEASURED capability set.
    ///
    /// `boot_advertising_only_proven` probes the environment and records the result into the roster
    /// BEFORE it hands back the runner, so the roster the very first heartbeat and the first claim read
    /// from already reflects the seat's real capability — never an empty set that fills in later.
    ///
    /// Deterministic regardless of which toolchains this host carries: it asserts the roster equals a
    /// FRESH probe of the same pass-through environment taken moments after, so it holds whether the set
    /// is empty or full. Delete the `record_capabilities` call in `boot_advertising_only_proven` and the
    /// roster stays empty while a fresh probe finds this host's tools — reddening this test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_first_advertisement_carries_the_measured_capability_set() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("cap-wired");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        crate::home::save_config(&mut home, |config| {
            config.seller = Some(seller_cfg(1, false));
            config.relay_url = fixture.url();
        })
        .expect("persist config");

        let verdicts = vec![HarnessProbeVerdict {
            index: 0,
            name: Some("claude".to_owned()),
            result: Ok(None),
        }];
        let runner = boot_advertising_only_proven(home, verdicts)
            .await
            .expect("a proven seat must boot");

        // Ground truth: a fresh probe of the SAME pass-through environment, taken now. The host does not
        // change between boot and here, so the roster must equal this exactly.
        let probe_dir = crate::seller_exec::ProbeWorkdir::create(runner.node.home())
            .expect("probe workdir");
        let expected = crate::capability::probe_seat_capabilities(
            &crate::seller_exec::SandboxPolicy::passthrough(),
            probe_dir.path(),
        )
        .expect("a pass-through probe is measurable");
        drop(probe_dir);

        assert_eq!(
            runner.agents.advertisement().capabilities,
            expected,
            "the first heartbeat's roster must carry exactly the probed set — wired at boot, not later"
        );
        // The claim emitter reads the SAME snapshot, so both wire events agree.
        assert_eq!(
            runner
                .agents
                .advertisement()
                .capability(&runner.node.home().config.seat)
                .capabilities,
            expected,
            "the claim capability must carry the same probed set as the heartbeat"
        );

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // The model the PROVING TURN's `session/new` reported must reach the roster before the runner
    // serves, on both wire surfaces. Without it a production roster's `models` stays empty, no
    // `harness_model` is emitted at all, and #866's model filter rejects every real seat — a seat
    // that works perfectly and is unreachable to exactly the buyers who named the model it reports.
    //
    // The model arrives here the only way it legitimately can: carried out of the probe on the `Ok`
    // arm of the verdict. It is NOT read from config, because config would destroy the value's one
    // property — machine-sourced provenance — and advertise an operator-typed id instead. Neither
    // source promises what will execute; the advertised value is a self-report either way.
    #[tokio::test]
    async fn the_proving_turns_model_reaches_both_wire_surfaces_before_serving() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("model-wired");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        crate::home::save_config(&mut home, |config| {
            // A NAMED roster entry, deliberately: a model tag is keyed by the harness it belongs to,
            // so the unlabelled `--agent-argv` hatch drops an observed model as unattributable. The
            // wiring this test is about is only observable on a named entry.
            let mut seller = seller_cfg(1, false);
            seller.agents = vec!["claude".to_owned()];
            config.seller = Some(seller);
            // A preset argv that resolves in any environment. The harness is never RUN here — the
            // proving turn is supplied as a verdict — so the argv only has to exist; requiring the
            // real ACP adapter would make this a test of the CI image rather than of the wiring.
            config.agents.insert(
                "claude".to_owned(),
                crate::home::AgentPresetConfig {
                    argv: vec!["true".to_owned()],
                },
            );
            config.relay_url = fixture.url();
        })
        .expect("persist config");

        let verdicts = vec![HarnessProbeVerdict {
            index: 0,
            name: Some("claude".to_owned()),
            result: Ok(Some("claude-opus-5".to_owned())),
        }];
        let runner = boot_advertising_only_proven(home, verdicts)
            .await
            .expect("a proven seat must boot");

        // The roster pairs the model with the entry's advertised NAME.
        assert_eq!(
            runner
                .agents
                .advertisement()
                .models
                .iter()
                .map(|entry| (entry.harness.as_str(), entry.model.as_str()))
                .collect::<Vec<_>>(),
            vec![("claude", "claude-opus-5")],
            "the first heartbeat must carry the model the proving turn reported, paired to its \
             roster entry — an empty `models` here is the defect this test exists for"
        );
        // The claim emitter reads the SAME snapshot and canonicalises the name to a wire FAMILY, so
        // a buyer filtering on a claim and a buyer filtering on a heartbeat cannot get different
        // answers about this seat. Asserted in the wire vocabulary, which is what a filter matches:
        // the roster says `claude`, the wire says `claude-code`, and that difference is deliberate.
        assert_eq!(
            runner
                .agents
                .advertisement()
                .capability(&runner.node.home().config.seat)
                .models
                .iter()
                .map(|entry| (entry.family.as_str(), entry.model.as_str()))
                .collect::<Vec<_>>(),
            vec![("claude-code", "claude-opus-5")],
            "the claim capability must carry the same model as the heartbeat, in the wire family \
             vocabulary #866's filter matches on"
        );

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // The operator-declared half of #784. `harness_variant` and `hardware` are the two fields no
    // probe can answer — a fork name and a machine description are facts about the operator, and
    // nothing the daemon runs measures them — so they come from `[seat]` in config.toml. Without
    // that key they are emit helpers with no source, and a seat can never state either one.
    //
    // Asserted on the event the fixture relay actually RECEIVED, not on a capability built here: a
    // test that reassembles the beat itself proves its own arithmetic and would stay green with the
    // production emit path reading nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_operators_declared_colour_reaches_the_beat_and_never_the_claim() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("seat-colour");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        crate::home::save_config(&mut home, |config| {
            config.seller = Some(seller_cfg(1, false));
            config.seat = crate::home::SeatConfig {
                harness_variant: Some("my-fork".to_owned()),
                hardware: Some("mac studio, 64GB".to_owned()),
            };
            config.relay_url = fixture.url();
        })
        .expect("persist config");

        let verdicts = vec![HarnessProbeVerdict {
            index: 0,
            name: Some("claude".to_owned()),
            result: Ok(None),
        }];
        let runner = boot_advertising_only_proven(home, verdicts)
            .await
            .expect("a proven seat must boot");

        assert!(
            runner.publish_heartbeat().await,
            "harness check: the relay must CONFIRM the beat, or the tag assertions below would be \
             read off an event that never landed"
        );

        let events = fixture.events().await;
        let beats = seat_announcements(&events);
        let beat = beats.last().expect("the seat published an announcement");
        assert_eq!(
            beat.tag_value(crate::heartbeat::HARNESS_VARIANT_TAG),
            Some("my-fork"),
            "the beat must carry what the operator declared — an absent tag here is the defect \
             this test exists for"
        );
        assert_eq!(
            beat.tag_value(crate::heartbeat::HARDWARE_TAG),
            Some("mac studio, 64GB")
        );

        // The other half of the contract, and the reason passing the config to the CLAIM's
        // capability is not a leak: display fields are separated at EMIT. `claim_draft` asks for
        // `filterable_tags` alone, so a claim built from this very capability carries neither field
        // — asserted on that exact tag set, which is the one a claim publishes.
        let filterable = runner
            .agents
            .advertisement()
            .capability(&runner.node.home().config.seat)
            .filterable_tags();
        assert!(
            !filterable.iter().any(|tag| {
                tag.first() == Some(crate::heartbeat::HARNESS_VARIANT_TAG)
                    || tag.first() == Some(crate::heartbeat::HARDWARE_TAG)
            }),
            "a display-only field on a claim would be weight no award decision reads: {filterable:?}"
        );
        assert!(
            !filterable.is_empty(),
            "POSITIVE CONTROL: the claim states SOMETHING, or the absence above is only an empty \
             tag list and proves nothing about the split"
        );

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `/bin/sh` agent that speaks just enough ACP to be probed: it answers `initialize` and
    /// `session/new`, writes the sentinel, and ends the turn.
    ///
    /// It reads the workdir out of the `session/new` PARAMS rather than using its own cwd, because
    /// that is where a real ACP agent reads it from — the driver carries `cwd` in the protocol and
    /// does not set the child process's directory. (Distinct from the capability probe, which has no
    /// protocol channel to carry it and therefore must set the child's cwd.) A stub that wrote to its
    /// own directory would be testing a mechanism the ACP path does not use.
    ///
    /// `model` is the model id the session start reports, emitted in the LEGACY
    /// `models.currentModelId` shape. `None` omits the block entirely, which is what a harness
    /// exposing no model looks like on the wire.
    ///
    /// ⚠ Coverage bound: this stub speaks the legacy shape only. The Session Config Options shape
    /// current Claude adapters use (#896) is covered by unit tests over
    /// `driver::acp_driver::session_model_from_result`, NOT by this end-to-end probe.
    ///
    /// `write_sentinel` is the artifact leg. False makes a stub that answers every message
    /// correctly and does no work — the shape a quota-exhausted harness has (#254).
    #[cfg(feature = "acp")]
    fn write_probe_stub(
        dir: &std::path::Path,
        label: &str,
        model: Option<&str>,
        write_sentinel: bool,
    ) -> std::path::PathBuf {
        let models = match model {
            Some(id) => format!(r#","models":{{"currentModelId":"{id}"}}"#),
            None => String::new(),
        };
        // One `sed` over the RAW request line, and never `grep`: `grep` on this box is `ugrep`, and
        // a stub that depends on which grep is first on PATH fails for a reason having nothing to do
        // with the code under test.
        //
        // The sentinel is read out of the JSON-escaped prompt, so it cannot be anchored to the start
        // of a line. The prompt puts `\n\n` before it, which survives into the wire text as literal
        // backslash-n — and `n` is a sentinel-legal character, so any tokenizer leaves `n` glued to
        // the front of the value. Matching the PREFIX wherever it sits is the shape that holds.
        let artifact = if write_sentinel {
            "sentinel=$(printf '%s' \"$turn\" | \
             sed -n 's/.*\\(maxplayer-probe-[0-9][0-9-]*[0-9]\\).*/\\1/p')\n\
             printf '%s\\n' \"$sentinel\" > \"$cwd/probe.txt\"\n"
        } else {
            ""
        };
        let script = dir.join(format!("stub-acp-{label}.sh"));
        let body = format!(
            "#!/bin/sh\n\
             read -r _init\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":2}}}}'\n\
             read -r new\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"sessionId\":\"stub\"{models}}}}}'\n\
             read -r turn\n\
             cwd=$(printf '%s' \"$new\" | sed 's/.*\"cwd\": *\"\\([^\"]*\\)\".*/\\1/')\n\
             {artifact}\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"stopReason\":\"end_turn\"}}}}'\n"
        );
        std::fs::write(&script, body).expect("write stub agent");
        script
    }

    /// The argv that runs one stub under a pass-through policy — the same `argv` shape a configured
    /// harness preset supplies.
    #[cfg(feature = "acp")]
    fn stub_argv(script: &std::path::Path) -> Vec<String> {
        vec![
            "/bin/sh".to_owned(),
            script.to_string_lossy().into_owned(),
        ]
    }

    // The model a seat advertises must come from the harness's OWN `session/new` result, and this
    // asserts it across the real ACP driver rather than from an injected verdict: a stub agent
    // reports a model id in the legacy `models.currentModelId` shape, the driver folds it into the
    // run's usage, and the probe carries it out on the proven arm.
    //
    // THREE legs, because the first alone proves almost nothing:
    //   · a session start that REPORTS a model puts that model on the verdict;
    //   · one that reports NONE leaves it absent — absence stays absence, never a fabricated default;
    //   · one that answers every message perfectly and writes NO artifact is NOT proven at all, and
    //     therefore carries no model however confidently it named one.
    //
    // The third leg is also the control on this test's own vehicle. `run_agent_job` opens its event
    // log INSIDE the probe workdir, and `probe_sentinel_present` scans every file there — so if the
    // log ever recorded the prompt verbatim, the sentinel would be present without the harness
    // having done anything, and legs one and two would pass on a probe that cannot fail.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_probe_carries_the_model_its_session_start_reported() {
        let dir = std::env::temp_dir().join(format!("maxplayer-probe-acp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("stub dir");
        let identity = DeliveryAgentIdentity::for_seller(&"aa".repeat(32));
        let sandbox = SandboxPolicy::passthrough();

        let run = |script: std::path::PathBuf, tag: &'static str| {
            let identity = identity.clone();
            let sandbox = sandbox.clone();
            let dir = dir.clone();
            async move {
                let probe = mint_probe_identity(0, 0, now_unix() as u64);
                let workdir = dir.join(format!("wd-{tag}"));
                let outcome = run_harness_probe_once(
                    &stub_argv(&script),
                    &sandbox,
                    &identity,
                    &workdir,
                    &probe.sentinel,
                )
                .await;
                let _ = std::fs::remove_dir_all(&workdir);
                outcome
            }
        };

        let reported = run(
            write_probe_stub(&dir, "with-model", Some("claude-opus-5"), true),
            "with-model",
        )
        .await;
        assert!(
            matches!(&reported, ProbeAttempt::Proven { model } if model.as_deref() == Some("claude-opus-5")),
            "the model the session start reported must reach the verdict: {reported:?}"
        );

        let silent = run(write_probe_stub(&dir, "no-model", None, true), "no-model").await;
        assert!(
            matches!(&silent, ProbeAttempt::Proven { model: None }),
            "a harness that reports no model must state none — a default here would advertise a \
             model no buyer could hold this seat to: {silent:?}"
        );

        let idle = run(
            write_probe_stub(&dir, "no-artifact", Some("claude-opus-5"), false),
            "no-artifact",
        )
        .await;
        assert!(
            matches!(&idle, ProbeAttempt::CompletedNoArtifact { .. }),
            "CONTROL: a turn that completed and produced nothing is not proven, whatever model it \
             named. A pass here would mean the sentinel is reachable without the harness writing \
             it — and the two legs above would be asserting nothing: {idle:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // The whole chain Rocky asked for, with nothing injected: a configured harness preset whose argv
    // is the ACP stub → the real boot probe → the verdict → the roster → the tags on the event the
    // relay received. No `SeatCapability` is constructed by this test and no verdict is handed to the
    // boot; the only thing supplied is an agent that speaks the protocol.
    //
    // It is what separates "the pieces are each tested" from "the pieces are connected". Every seam
    // here has a unit test already, and the seat still advertised no model at all.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_seat_advertises_the_model_its_acp_handshake_reported() {
        let fixture = PGateRelay::start(Duration::from_millis(0)).await;
        let root = throwaway_root("acp-model-e2e");
        let mut home = crate::home::bootstrap(&root).expect("bootstrap home");
        let script = write_probe_stub(&root, "boot", Some("claude-opus-5"), true);
        crate::home::save_config(&mut home, |config| {
            let mut seller = seller_cfg(1, false);
            seller.agents = vec!["claude".to_owned()];
            config.seller = Some(seller);
            config.agents.insert(
                "claude".to_owned(),
                crate::home::AgentPresetConfig {
                    argv: stub_argv(&script),
                },
            );
            config.relay_url = fixture.url();
        })
        .expect("persist config");

        // The REAL boot probe: it runs the harness, reads its handshake, and decides on the artifact.
        let verdicts = probe_configured_harnesses(&home)
            .await
            .expect("the boot probe must run");
        assert_eq!(
            verdicts.len(),
            1,
            "harness check: exactly the configured preset was probed: {verdicts:?}"
        );
        assert_eq!(
            verdicts[0].result.as_ref().map(Option::as_deref),
            Ok(Some("claude-opus-5")),
            "the boot probe must carry the handshake's model — everything below reads from this"
        );

        let runner = boot_advertising_only_proven(home, verdicts)
            .await
            .expect("a proven seat must boot");
        assert!(
            runner.publish_heartbeat().await,
            "harness check: the relay must CONFIRM the beat"
        );

        let events = fixture.events().await;
        let beat = seat_announcements(&events)
            .last()
            .copied()
            .expect("the seat published an announcement");
        assert!(
            beat.tags.contains(&vec![
                crate::heartbeat::HARNESS_MODEL_TAG.to_owned(),
                "claude-code".to_owned(),
                "claude-opus-5".to_owned(),
            ]),
            "the beat must pair the reported model with its wire FAMILY — the vocabulary #866's \
             filter matches on, not the roster's preset name: {:?}",
            beat.tags
        );

        runner.client.disconnect().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- #472: prove-before-advertise probe diagnostic — two failure shapes, retry only the flaky
    // one. The policy is pure (`probe_step`), so it is proven here with scripted outcomes and no agent
    // spawn — the same seam that lets `boot_advertising_only_proven` take verdicts as a parameter.

    #[test]
    fn probe_step_retries_the_flaky_shape_up_to_the_cap_then_fails_closed() {
        // Completed-but-empty turns retry while turns remain...
        assert!(matches!(
            probe_step(0, 3, ProbeAttempt::CompletedNoArtifact { agent_message: None }),
            ProbeStep::Retry
        ));
        assert!(matches!(
            probe_step(1, 3, ProbeAttempt::CompletedNoArtifact { agent_message: None }),
            ProbeStep::Retry
        ));
        // ...and the FINAL empty turn fails closed with the flaky remedy, never another retry.
        let ProbeStep::Done(Err((reason, fault))) =
            probe_step(2, 3, ProbeAttempt::CompletedNoArtifact { agent_message: None })
        else {
            panic!("the last flaky attempt must be Done(Err), not a retry");
        };
        assert_eq!(fault, Fault::Unproven);
        // The RETRY POLICY above is unchanged and is what this test is for. The WORDING assertions
        // that used to sit here required the verdict to say "FLAKY", to prescribe "retry later /
        // more reliable model", and to avoid mentioning the launcher at all. That premise was wrong:
        // on 2026-08-21 this exact shape came from a containment fault, so the verdict must no longer
        // name a cause — and must not steer the operator away from the launcher either.
        assert!(
            reason.contains("does NOT establish the cause"),
            "the verdict must state what a completed turn cannot tell us: {reason}"
        );
        assert!(
            !reason.contains("FLAKY") && !reason.contains("not a containment fault"),
            "the verdict must not assert a cause it cannot know: {reason}"
        );
        assert!(
            reason.contains("sandbox egress"),
            "with no agent message, the verdict must leave containment ON the list: {reason}"
        );
    }

    #[test]
    fn probe_step_never_retries_the_unrunnable_shape() {
        // A launcher/exec failure is structural: Done on EVERY attempt index, turns remaining or not.
        for attempt in 0..3 {
            let outcome = ProbeAttempt::Unrunnable {
                reason: launcher_unrunnable_reason(&ExecError::Agent("spawn refused".to_owned())),
                fault: Fault::Unproven,
            };
            let ProbeStep::Done(Err((reason, _))) = probe_step(attempt, 3, outcome) else {
                panic!("an unrunnable turn must never retry (attempt {attempt})");
            };
            assert!(
                reason.contains("fix the launcher") && reason.contains("containment"),
                "the unrunnable verdict must name the launcher/containment remedy: {reason}"
            );
            assert!(
                !reason.contains("retry later") && !reason.contains("more reliable model"),
                "the unrunnable verdict must NOT prescribe a model retry: {reason}"
            );
        }
    }

    #[test]
    fn is_auth_class_matches_only_the_acp_auth_message() {
        // TRUE for the ACP "Authentication required" JSON-RPC error carried inside Agent(msg)...
        assert!(is_auth_class(&ExecError::Agent(
            "ACP request 3 failed: {\"code\":-32000,\"message\":\"Authentication required\"}"
                .to_owned()
        )));
        // ...case-insensitively — the agent may render the message in any case.
        assert!(is_auth_class(&ExecError::Agent(
            "acp request 1 failed: authentication REQUIRED".to_owned()
        )));
        // FALSE for a non-auth agent error, and specifically for a bare `-32000` carrying some other
        // message: that code is the generic JSON-RPC server-error, not proof of an auth fault.
        assert!(!is_auth_class(&ExecError::Agent(
            "ACP request 2 failed: {\"code\":-32000,\"message\":\"internal error\"}".to_owned()
        )));
        assert!(!is_auth_class(&ExecError::Agent("spawn refused".to_owned())));
        // FALSE for every non-Agent shape — the auth signal only ever arrives as Agent text.
        assert!(!is_auth_class(&ExecError::AcpRequired));
        assert!(!is_auth_class(&ExecError::Config("GOOSE_PROVIDER unset".to_owned())));
        assert!(!is_auth_class(&ExecError::Policy("un-typeable oid".to_owned())));
    }

    #[test]
    fn an_auth_class_unrunnable_turn_gets_a_sign_in_remedy_not_a_sandbox_one() {
        // The exact shape run_agent_job returns when the agent CLI is not logged in: the ACP auth
        // JSON-RPC error, carried inside Agent(msg). It lands in the Unrunnable shape (the turn
        // never ran), but its remedy must point at SIGNING IN, never at the launcher/sandbox that
        // was not at fault (#555).
        let auth_error = ExecError::Agent(
            "ACP request 3 failed: {\"code\":-32000,\"message\":\"Authentication required\"}"
                .to_owned(),
        );
        let reason = unrunnable_reason(&auth_error);
        // (a) it must prescribe signing in / `/login`...
        assert!(
            reason.contains("/login") && reason.to_ascii_lowercase().contains("sign in"),
            "an auth-class probe failure must prescribe signing in: {reason}"
        );
        // (b) ...and must NOT send the operator to the containment config that was never at fault.
        assert!(
            !reason.contains("sandbox") && !reason.contains("launcher config"),
            "an auth-class remedy must not point at the sandbox/launcher config: {reason}"
        );
        // Red-prove the branch: the OLD path (launcher_unrunnable_reason, the shape's remedy before
        // the #555 split) DOES send the operator to the launcher/sandbox config for this SAME auth
        // error. So this test passes only because `unrunnable_reason` routes on the class — revert
        // that routing and both asserts above go red.
        let old = launcher_unrunnable_reason(&auth_error);
        assert!(
            old.contains("launcher/sandbox") && !old.contains("/login"),
            "the pre-#555 path must still yield the sandbox/launcher remedy (red-prove anchor): {old}"
        );
    }

    #[test]
    fn probe_step_stops_on_a_proven_turn() {
        // A proven turn ends the loop immediately — the gate's healthy direction is unchanged.
        assert!(matches!(
            probe_step(0, 3, ProbeAttempt::Proven { model: None }),
            ProbeStep::Done(Ok(None))
        ));
    }

    #[test]
    fn the_completed_turn_reason_never_rules_containment_out() {
        // This test's PREMISE changed, and that is the point. It used to require the completed-turn
        // reason to say "a FLAKY harness/model, not a containment fault" and to avoid pointing at the
        // launcher at all. On 2026-08-21 a contained cursor seat produced exactly this shape and the
        // fault WAS containment — its model host would not resolve — so the old string argued against
        // the correct hypothesis for a full day.
        let completed = flaky_harness_reason(3, None);
        let launcher = launcher_unrunnable_reason(&ExecError::Agent("boom".to_owned()));
        assert!(
            !completed.contains("not a containment fault"),
            "the completed-turn reason must not deny containment; it cannot know: {completed}"
        );
        assert!(
            !completed.contains("FLAKY harness/model"),
            "the completed-turn reason must not assert a cause it cannot know: {completed}"
        );
        assert!(
            completed.contains("does NOT establish the cause"),
            "it must say what it does not know: {completed}"
        );
        // The UNRUNNABLE shape still names its remedy — that one really is structural, and this half
        // of the split is unchanged.
        assert!(launcher.contains("fix the launcher") && launcher.contains("containment"));
    }

    #[test]
    fn the_completed_turn_reason_carries_the_agents_own_words() {
        // FIXTURE, measured 2026-08-21 on a contained cursor seat: the turn completed, produced no
        // artifact, and the ONLY thing naming the cause was the agent's own message. It was already in
        // the ACP stream on the first run; the seller's sink discarded it (`&mut |_| {}`), so the
        // operator got a guess instead. Acceptance is that this text REACHES the operator reason.
        const MEASURED: &str =
            "Error: RetriableError: [unavailable] getaddrinfo EAI_AGAIN agentn.global.api5.cursor.sh";
        let reason = flaky_harness_reason(3, Some(MEASURED));
        assert!(
            reason.contains("getaddrinfo EAI_AGAIN agentn.global.api5.cursor.sh"),
            "the agent's own words must reach the operator verbatim: {reason}"
        );
        // And the no-message case must be rendered in words, never as empty quotes — "the agent said
        // nothing" is itself a finding, and it must not look like a missing field.
        let silent = flaky_harness_reason(3, None);
        assert!(
            silent.contains("NO message") && !silent.contains("\"\""),
            "a silent turn must be stated, not printed as empty quotes: {silent}"
        );
        // A message that is only whitespace is silence, not a quote.
        assert_eq!(
            flaky_harness_reason(3, Some("   \n  ")),
            silent,
            "whitespace-only must be treated as no message at all"
        );
    }

    #[test]
    fn a_long_agent_message_is_truncated_on_a_char_boundary() {
        // A vendor error can carry any UTF-8. Truncating by BYTES would panic mid-codepoint, so the
        // quote counts chars — this is the red-prove for that choice: every char here is 4 bytes.
        let long = "🙂".repeat(AGENT_MESSAGE_QUOTE_CHARS + 50);
        let quoted = quoted_agent_message(Some(&long)).expect("a non-empty message must quote");
        assert!(quoted.ends_with("[…]"), "an over-length message must be elided: {quoted}");
        assert_eq!(
            quoted.chars().filter(|c| *c == '🙂').count(),
            AGENT_MESSAGE_QUOTE_CHARS,
            "exactly the cap must survive, counted in chars"
        );
    }

    #[test]
    fn a_flaky_harness_that_recovers_on_a_later_turn_is_proven() {
        // Drive the exact loop `probe_one_harness` drives, over a scripted [flaky, flaky, proven]
        // sequence: the first two retry, the third proves — so the harness ends PROVEN, not grounded.
        // This is the money-relevant direction: a merely flaky model still gets to advertise.
        let script = [
            ProbeAttempt::CompletedNoArtifact { agent_message: None },
            ProbeAttempt::CompletedNoArtifact { agent_message: None },
            ProbeAttempt::Proven { model: None },
        ];
        let mut verdict = None;
        for (attempt, outcome) in script.into_iter().enumerate() {
            match probe_step(attempt, 3, outcome) {
                ProbeStep::Done(result) => {
                    verdict = Some(result);
                    break;
                }
                ProbeStep::Retry => continue,
            }
        }
        assert!(
            matches!(verdict, Some(Ok(None))),
            "a harness that delivers on its third turn must be proven, not grounded: {verdict:?}"
        );
    }

    #[test]
    fn an_unrunnable_turn_stops_the_loop_even_after_a_flaky_retry() {
        // [flaky, unrunnable, proven]: the flaky retries, the unrunnable STOPS at attempt 1 — the
        // third element is never consumed, so a launcher fault surfacing on a retry still fails fast,
        // and the RECORDED fault is the launcher's, not the flaky Unproven.
        let script = [
            ProbeAttempt::CompletedNoArtifact { agent_message: None },
            ProbeAttempt::Unrunnable {
                reason: launcher_unrunnable_reason(&ExecError::AcpRequired),
                fault: Fault::Incapable(MissingCapability::AcpFeature),
            },
            ProbeAttempt::Proven { model: None },
        ];
        let mut consumed = 0;
        let mut verdict = None;
        for (attempt, outcome) in script.into_iter().enumerate() {
            consumed += 1;
            match probe_step(attempt, 3, outcome) {
                ProbeStep::Done(result) => {
                    verdict = Some(result);
                    break;
                }
                ProbeStep::Retry => continue,
            }
        }
        assert_eq!(
            consumed, 2,
            "the loop must stop at the unrunnable turn, not run the third"
        );
        assert!(
            matches!(
                verdict,
                Some(Err((_, Fault::Incapable(MissingCapability::AcpFeature))))
            ),
            "the unrunnable fault must be the recorded verdict: {verdict:?}"
        );
    }
}

/// The supervision #301 adds around a restore self-probe: an outer wall-clock ceiling for a
/// live-but-endless-stream hang, and a Drop guard that releases the in-flight mark when the probe task
/// PANICS (its `JoinHandle` is dropped in production, so the panic is otherwise silent). Both paths
/// used to leave the harness marked `probing` for the life of the process — stuck `Dropped`, never
/// re-probed. Each test drives the extracted [`supervise_harness_probe`] directly with an injected
/// probe body, then proves the harness is claimable again afterwards (the mark WAS released) and was
/// never restored to service (a died probe proves nothing).
#[cfg(test)]
mod supervised_probe_tests {
    use super::{supervise_harness_probe, ProbeOutcome};
    use crate::seller_agents::{AgentRegistry, RegisteredAgent};
    use crate::seller_roster::{Fault, LiveRoster};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A one-harness roster already dropped (strike 1) with its due probe CLAIMED, i.e. marked
    /// `probing` — the exact state `start_due_harness_probes` hands to the spawned task. Returns the
    /// roster and a `due` instant past the probe window.
    fn claimed_probe(now: Instant) -> (Arc<LiveRoster>, Instant) {
        let roster = Arc::new(LiveRoster::new(AgentRegistry::new(vec![RegisteredAgent {
            name: Some("claude".to_owned()),
            argv: vec!["claude-acp".to_owned()],
        }])));
        roster.fault(0, Fault::Unproven, now);
        let due = now + Duration::from_secs(24 * 60 * 60);
        assert_eq!(
            roster.claim_due_probes(due),
            vec![0],
            "the probe is due and gets claimed, marking the harness probing"
        );
        assert!(
            roster.claim_due_probes(due).is_empty(),
            "the in-flight mark must suppress a second claim while the probe runs"
        );
        (roster, due)
    }

    #[tokio::test]
    async fn a_panicking_probe_releases_the_in_flight_mark_so_the_harness_is_re_probed() {
        // A poisoned-mutex `.expect`, in the wild: the probe task panics. In production its JoinHandle
        // is dropped and the panic vanishes; here we spawn+join so the test proceeds only once the task
        // (and its Drop guard) has fully unwound.
        let now = Instant::now();
        let (roster, due) = claimed_probe(now);

        let handle = tokio::spawn(supervise_harness_probe(Arc::clone(&roster), 0, async {
            panic!("probe task panicked (a poisoned-mutex .expect, say)")
        }));
        let joined = handle.await;
        assert!(joined.is_err(), "the injected probe must have panicked");
        assert!(joined.unwrap_err().is_panic(), "and it must be a panic, not a cancellation");

        // WITHOUT the Drop guard `probing` stays true forever and this is empty — the harness is stuck
        // Dropped and never re-probed. WITH it, the mark is released and the window re-armed, so far
        // enough out the harness is claimable again.
        let long_after = due + Duration::from_secs(24 * 60 * 60);
        assert_eq!(
            roster.claim_due_probes(long_after),
            vec![0],
            "a panicking probe must release the in-flight mark so the harness is probed again (#301)"
        );
        assert!(
            !roster.serves(None),
            "a probe that died proved nothing — it must NEVER restore the dead harness to service"
        );
    }

    /// The advertised model for the one harness in `claimed_probe`'s roster, as the wire would carry
    /// it. `None` means the seat states no model for it.
    fn advertised_model(roster: &LiveRoster) -> Option<String> {
        roster
            .advertisement()
            .models
            .into_iter()
            .find(|entry| entry.harness == "claude")
            .map(|entry| entry.model)
    }

    // A restore self-probe must publish the model IT observed, not the one standing from before the
    // harness was dropped.
    //
    // The stale value is not hypothetical and neither half of the drop clears it: `fault` leaves
    // `model` untouched and `restore` clears only availability. So a harness that faults, gets
    // re-pointed at a different default while it is out of service, and is then proven by a self-probe
    // comes back advertising what it ran BEFORE the fault — a filterable claim, on both the beat and
    // the claim, that #866's model filter will match and award against.
    //
    // BOTH LEGS, because a replacement test alone cannot see the worse direction: a harness that has
    // stopped reporting a model must come back UNSTATED, not merely un-updated. The `Option` in the
    // setter exists for exactly that, and writing `None` through is what makes absence reachable.
    #[tokio::test]
    async fn a_restored_harness_advertises_the_model_its_probe_observed() {
        let now = Instant::now();

        // LEG 1 — REPLACEMENT. The pre-fault model must not survive the restore.
        let (roster, _due) = claimed_probe(now);
        roster.record_model(0, Some("claude-opus-4".to_owned()));
        assert_eq!(
            advertised_model(&roster),
            None,
            "harness check: a DROPPED harness advertises nothing at all, so the assertion after the \
             restore below is about the restore and not about a model that was already visible"
        );

        let outcome = supervise_harness_probe(
            Arc::clone(&roster),
            0,
            std::future::ready(Ok(Some("claude-opus-5".to_owned()))),
        )
        .await;
        assert!(matches!(outcome, ProbeOutcome::Restored), "{outcome:?}");
        assert_eq!(
            advertised_model(&roster).as_deref(),
            Some("claude-opus-5"),
            "the restored harness must advertise the model its PROBE observed — `claude-opus-4` here \
             is the pre-fault value, and advertising it is a claim this seat can no longer keep"
        );

        // LEG 2 — CLEARING. A harness that reports no model comes back stating none.
        let (roster, _due) = claimed_probe(now);
        roster.record_model(0, Some("claude-opus-4".to_owned()));
        let outcome =
            supervise_harness_probe(Arc::clone(&roster), 0, std::future::ready(Ok(None))).await;
        assert!(matches!(outcome, ProbeOutcome::Restored), "{outcome:?}");
        assert_eq!(
            advertised_model(&roster),
            None,
            "a harness that has stopped reporting a model must come back UNSTATED — keeping the last \
             value it ever gave outlives the truth, and absent is the only honest spelling"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_probe_hits_the_wall_clock_ceiling_and_the_harness_is_re_probed() {
        // A probe the 120s ACP IDLE timeout can never bound: it never resolves (in the wild it holds
        // its stream alive with periodic updates, resetting the idle timer each time). Under paused
        // time `tokio::time::timeout` auto-advances and fires the outer WALL ceiling.
        let now = Instant::now();
        let (roster, due) = claimed_probe(now);

        let outcome = supervise_harness_probe(
            Arc::clone(&roster),
            0,
            std::future::pending::<Result<Option<String>, (String, Fault)>>(),
        )
        .await;
        assert!(
            matches!(outcome, ProbeOutcome::WallTimeout { .. }),
            "the wall-clock ceiling must fire on a probe the idle timeout cannot bound"
        );

        // WITHOUT the outer timeout this await never returns (the test hangs). WITH it the harness is
        // faulted, released, and re-armed — claimable again, and never restored.
        let long_after = due + Duration::from_secs(24 * 60 * 60);
        assert_eq!(
            roster.claim_due_probes(long_after),
            vec![0],
            "after the wall timeout the harness must be re-probed, not stuck probing forever (#301)"
        );
        assert!(
            !roster.serves(None),
            "a hung probe must never restore the dead harness to service"
        );
    }
}
