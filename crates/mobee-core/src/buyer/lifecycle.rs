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
use crate::job_lifecycle::{AwardClaimOutcome, JobLifecycleError, JobView};

use super::reservations::{Converted, JobDisposition, ReservationState, ReserveRefused};
use super::store::{BuyerStore, StoreError};

/// Hard filters an awardable claim must pass (issue #126). Grounded in the wire the offer/claim
/// actually carry: the offer's signed `amount_sats` is the fixed price, the seller's claim `creq`
/// carries the payable terms + accepted mints, and the claim's `mobee_agent` tag carries the
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
                && claim_is_payable(&view.job_id, claim.creq.as_deref(), filters)
        })
        .map(|claim| claim.claim_id.clone())
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
        }
    }
}

impl std::error::Error for NamedAwardRefused {}

/// Verify a specifically-named claim is awardable under the hard filters — the manual-award
/// counterpart of [`select_awardable_claim`], so `max_sats` and mint/price compatibility are applied
/// on the manual path rather than ignored. Pure: relay truth + filters in, verdict out.
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

/// Failure of [`award_with_reservation`].
#[derive(Debug)]
pub enum AwardError {
    /// The reservation was refused — NOTHING was published and (by the store's zero-write
    /// guarantee on refusal) no reservation row was written.
    Reserve(ReserveRefused),
    /// The award publish failed after the reservation was taken; the reservation was released
    /// (no award reached the relay), so its funds are not stranded.
    Publish(JobLifecycleError),
    /// Releasing the reservation after a publish failure itself failed.
    Store(StoreError),
}

impl std::fmt::Display for AwardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reserve(refused) => write!(formatter, "{refused}"),
            Self::Publish(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for AwardError {}

/// Reserve `amount` for `job_id` FIRST, then publish the award. A refused reservation returns
/// [`AwardError::Reserve`] without ever calling `publish` — so an award the buyer cannot afford
/// never reaches the relay and (by [`BuyerStore::reserve`]) leaves no row. A publish that fails
/// after the reservation releases it (no award went out) so the funds return to `available`.
///
/// `balance`/`total_cap`/`spent` are the honest snapshots the caller supplies (live wallet
/// balance, budget cap, budget spent total) — the same two-ceiling inputs [`BuyerStore::reserve`]
/// guards against.
pub async fn award_with_reservation<F, Fut>(
    store: &BuyerStore,
    job_id: &str,
    amount: u64,
    balance: u64,
    total_cap: u64,
    spent: u64,
    now_unix: i64,
    publish: F,
) -> Result<AwardClaimOutcome, AwardError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<AwardClaimOutcome, JobLifecycleError>>,
{
    // Reserve before any publish: a refusal publishes NOTHING (and writes no row).
    store
        .reserve(job_id, amount, balance, total_cap, spent, now_unix)
        .map_err(AwardError::Reserve)?;

    match publish().await {
        Ok(outcome) => {
            // Record the PUBLISHED award. This is the one seam both the manual and the auto award
            // path go through, so recording here covers both by construction — recording at the two
            // call sites instead would let one drift silently.
            //
            // A store failure here does NOT fail the award: the 3405 is already public and the
            // reservation is already held, so reporting failure would be a lie that also releases
            // funds the buyer has genuinely committed. The window is narrow (the `reserve` write
            // above went through the same store moments earlier), but it is not nothing, and the
            // consequence is specific: without this row the delivery watcher cannot see the job, so
            // it will not auto-settle. Say exactly that, unconditionally — an operator reading the
            // log must not have to infer why one job stopped settling itself.
            if let Err(error) = store.record_award(
                job_id,
                &outcome.claim_id,
                &outcome.award_event_id,
                &outcome.seller_pubkey,
                amount,
                now_unix,
            ) {
                eprintln!(
                    "buyer: award for {job_id} published ({}) but recording it failed ({error}); \
                     this job will NOT be auto-settled on delivery — collect it manually",
                    outcome.award_event_id
                );
            }
            Ok(outcome)
        }
        Err(error) => {
            // No award reached the relay — reclaim the reservation rather than strand the funds.
            store
                .release(job_id, now_unix)
                .map_err(AwardError::Store)?;
            Err(AwardError::Publish(error))
        }
    }
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
    /// No payment attempt has left funds for this job (no journal, or only `Intent`/`Locked`).
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

/// The idempotent re-arm decision (#126/#127 invariant A: never award twice). A job is already
/// awarded iff a buyer AWARD (3405) is on the relay OR its reservation is already `Spent` (collect
/// paid it). A `Reserved` row with NO relay award is the crash window between reserve and publish —
/// the award never went out, so it must be re-attempted (republished); [`award_with_reservation`]'s
/// reserve is idempotent for the same amount, so the re-attempt reserves once and publishes.
///
/// Checking BOTH the relay AND the local ledger is load-bearing: the relay catches a 3405 that
/// published before a crash (local ledger alone would miss it → double award); the `Spent` check
/// catches an already-paid job.
pub fn plan_rearm(award_on_relay: bool, reservation: Option<ReservationState>) -> RearmAction {
    if award_on_relay {
        return RearmAction::Skip;
    }
    if matches!(reservation, Some(ReservationState::Spent)) {
        return RearmAction::Skip;
    }
    RearmAction::Attempt
}

/// What the auto-award driver does when its relay read comes back carrying no offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingOfferAction {
    /// The offer's own deadline has passed — park on positive knowledge.
    ParkExpired,
    /// The read cannot tell absence from a failed read — read again.
    Retry,
    /// The bound on consecutive unverifiable reads is exhausted and the offer was never seen —
    /// park, stating only that the read never succeeded.
    ParkUnverified,
}

/// The decision behind a missing offer, kept separate from the read so the rule is testable
/// without a relay.
///
/// A relay read that returns nothing is NOT evidence of absence. `fetch_events` yields an empty
/// set both for "no such event" and for "the timeout expired first", and the two are
/// indistinguishable at the call site — so an empty offer read is CANNOT-VERIFY, never ABSENT.
/// (Issue #253: rendering it as absence parked valid trades, four of them provably so, because a
/// relay answering near the read budget loses the race often enough to matter.) This is the same
/// posture [`plan_rearm`]'s caller already takes on the award check, where a relay error is
/// treated as "unknown" rather than "no award".
///
/// `deadline_unix` is the offer's deadline when it is known — from the post that created this
/// intent, or from any earlier successful read. Knowing it is what makes retrying safe: the
/// deadline bounds the retry, so a job cannot spin forever. Only when the deadline is unknown
/// does the consecutive-read bound apply, and giving up then parks with a reason that claims no
/// more than we can support.
pub fn plan_missing_offer(
    deadline_unix: Option<u64>,
    now_unix: u64,
    consecutive_unverified_reads: u32,
    max_unverified_reads: u32,
) -> MissingOfferAction {
    if let Some(deadline) = deadline_unix {
        if now_unix > deadline {
            return MissingOfferAction::ParkExpired;
        }
        return MissingOfferAction::Retry;
    }
    if consecutive_unverified_reads < max_unverified_reads {
        return MissingOfferAction::Retry;
    }
    MissingOfferAction::ParkUnverified
}

/// Classify a reserved job for [`BuyerStore::reconcile`] from its payment progress + relay
/// liveness. The payment journal is authoritative over relay liveness: a `Closed` payment is
/// `Paid` regardless of whether the claim still looks live, and an ambiguous payment is KEPT
/// (`Payable`) even if the claim looks dead — the funds may have moved, so only the phase-3 saga
/// may resolve it. A job with no payment is `Dead` only when it is no longer payable on the relay.
pub fn classify_disposition(payment: PaymentProgress, claim_payable: bool) -> JobDisposition {
    match payment {
        PaymentProgress::Closed => JobDisposition::Paid,
        PaymentProgress::Uncertain => JobDisposition::Payable,
        PaymentProgress::None if claim_payable => JobDisposition::Payable,
        PaymentProgress::None => JobDisposition::Dead,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetGate;
    use crate::gateway::creq::build_seller_creq;
    use crate::home::{self, DEFAULT_MINT_URL};
    use crate::job_lifecycle::{ClaimView, OfferView};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-buyer-lifecycle-{label}-{}-{id}.sqlite",
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
            requested_agent: None,
        }
    }

    fn claim(job_id: &str, live: bool, creq_amount: u64, mints: &[String]) -> ClaimView {
        let creq = build_seller_creq(job_id, creq_amount, "sat", mints, SELLER_HEX).expect("creq");
        ClaimView {
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
        }
    }

    fn filters<'a>(offer_amount: u64, max_sats: u64) -> AwardFilters<'a> {
        AwardFilters {
            offer_amount_sats: offer_amount,
            max_sats,
            buyer_mint: DEFAULT_MINT_URL,
            allow_real_mints: false,
            requested_agent: None,
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

    // AWARD-REFUSED tooth: when the reservation is refused, `publish` is NEVER called and NO
    // reservation row is written. Red-on-revert: reserving AFTER the publish would fire the
    // publish closure here (the flag flips), failing the "publish must not run" assertion.
    #[tokio::test(flavor = "current_thread")]
    async fn award_refused_publishes_nothing_and_writes_no_row() {
        let (store, path) = fresh_store("award-refused");
        let job_a = "a".repeat(64);
        let job_b = "b".repeat(64);
        // Reserve the whole balance against job_a so job_b cannot fit.
        store.reserve(&job_a, 100, 100, u64::MAX, 0, 1).expect("first reserve");

        let published = AtomicBool::new(false);
        let error = award_with_reservation(&store, &job_b, 40, 100, u64::MAX, 0, 2, || {
            published.store(true, Ordering::SeqCst);
            async { unreachable!("publish must not run when the reservation is refused") }
        })
        .await
        .expect_err("over-available award must refuse");

        assert!(matches!(error, AwardError::Reserve(ReserveRefused::InsufficientAvailable { .. })));
        assert!(!published.load(Ordering::SeqCst), "a refused reservation must publish NOTHING");
        assert!(store.reservation(&job_b).expect("read").is_none(), "refused award writes NO row");
        assert_eq!(store.reserved_in_flight().expect("r"), 100, "only job_a's reserve stands");
        let _ = std::fs::remove_file(&path);
    }

    // A publish failure after a successful reservation RELEASES it (no award reached the relay),
    // so the funds return to available rather than stranding against a job with no live award.
    #[tokio::test(flavor = "current_thread")]
    async fn award_publish_failure_releases_the_reservation() {
        let (store, path) = fresh_store("award-publish-fail");
        let job = "a".repeat(64);
        let error = award_with_reservation(&store, &job, 40, 100, u64::MAX, 0, 1, || async {
            Err(JobLifecycleError::Relay("relay down".into()))
        })
        .await
        .expect_err("publish failed");
        assert!(matches!(error, AwardError::Publish(_)));
        assert_eq!(store.reserved_in_flight().expect("r"), 0, "publish failure reclaimed the reserve");
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::super::reservations::ReservationState::Released)
        );
        // Nothing was published, so nothing may be recorded as awarded — otherwise the delivery
        // watcher would sweep a job that has no award on the relay.
        assert!(
            store.award_record(&job).expect("read").is_none(),
            "a failed publish must record no award"
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

        let outcome = award_with_reservation(&store, &job, 40, 100, u64::MAX, 0, 7, || {
            let (job, claim, award_event) = (job.clone(), claim.clone(), award_event.clone());
            async move {
                Ok(AwardClaimOutcome {
                    award_event_id: award_event,
                    job_id: job,
                    claim_id: claim,
                    seller_pubkey: SELLER_HEX.to_owned(),
                    quoted_mints: Vec::new(),
                })
            }
        })
        .await
        .expect("award");
        assert_eq!(outcome.award_event_id, award_event);

        let recorded = store.award_record(&job).expect("read").expect("award recorded");
        assert_eq!(recorded.award_event_id, award_event);
        assert_eq!(recorded.claim_id, claim);
        assert_eq!(recorded.seller_pubkey, SELLER_HEX);
        assert_eq!(recorded.amount_sats, 40, "the RESERVED amount, not a seller-quoted one");
        assert_eq!(recorded.awarded_at_unix, 7);

        // And the job is now the delivery watcher's work — awarded, reservation still held.
        assert_eq!(
            store.awarded_unsettled_job_ids().expect("awarded"),
            vec![job],
            "a published award must put the job in the watcher's work set"
        );
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
        store.reserve(&job, 40, 100, u64::MAX, 0, 1).expect("reserve");
        assert_eq!(store.available(100, u64::MAX, 0).expect("avail"), 60);

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
        assert_eq!(store.available(100, u64::MAX, 0).expect("avail"), 60, "available NOT over-stated");

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
    // UNDER-stated (the amount is counted in BOTH terms — never in neither) — never over-stated. On
    // restart, reconcile with a `Paid` disposition converges the dangling reservation to `spent`,
    // and `available` returns to the correct post-settle value. Uses the REAL durable store + the
    // REAL durable BudgetGate (spent.jsonl), not a model.
    #[test]
    fn crash_between_pay_and_flip_never_overstates_available_and_reconcile_converges() {
        let root = std::env::temp_dir().join(format!(
            "mobee-buyer-lifecycle-crash-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        home.config.total_budget_sats = 1000;
        home.config.per_job_budget_sats = 100;
        let db = root.join("buyer.sqlite");
        let job = "a".repeat(64);

        let cap = home.config.total_budget_sats; // 1000
        let starting_balance = 100u64;
        let amount = 40u64;

        // True post-settle available if the flip HAD happened: min(balance-amount, cap-amount).
        let true_available_after = std::cmp::min(starting_balance - amount, cap - amount);

        {
            let store = BuyerStore::open(&db).expect("open");
            store
                .reserve(&job, amount, starting_balance, cap, 0, 1)
                .expect("reserve");

            // PAY: budget append (durable) + melt. The melt drops the live wallet balance by
            // `amount`; we model that post-melt balance below. Crucially we DO NOT flip here.
            let mut gate = BudgetGate::from_home(&home).expect("gate");
            gate.authorize_and_commit(amount).expect("budget append");
            assert_eq!(gate.spent(), amount, "budget spend is durable pre-flip");
            let melted_balance = starting_balance - amount;

            // WINDOW (crash before the flip): the reservation is still `reserved`, budget spent is
            // `amount`, the wallet has melted. available must be conservative — counted in BOTH the
            // wallet ceiling (balance already dropped, reserved still holds it) and the budget
            // ceiling (spent rose, reserved still holds it) — hence UNDER-stated, never over.
            let windowed = store.available(melted_balance, cap, amount).expect("windowed avail");
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

        // CONVERGED: available now equals the true post-settle value (reserved cleared, spent held
        // by the budget ledger, wallet already melted). Neither over- nor under-stated.
        assert_eq!(
            store.available(melted_balance, cap, reloaded.spent()).expect("avail"),
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
        store.reserve(&job, 30, 100, u64::MAX, 0, 1).expect("reserve");
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
        store.reserve(&job, 100, 100, u64::MAX, 0, 1).expect("reserve");
        assert_eq!(store.available(100, u64::MAX, 0).expect("avail"), 0, "all funds committed");

        let mut dispositions = super::super::reservations::Dispositions::new();
        dispositions.insert(job.clone(), classify_disposition(PaymentProgress::None, false));
        let report = store.reconcile(&dispositions, 2).expect("reconcile");
        assert_eq!(report.released, vec![job.clone()]);
        assert_eq!(store.available(100, u64::MAX, 0).expect("avail"), 100, "dead job's funds reclaimed");
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

    // An empty offer read is CANNOT-VERIFY, never ABSENT (#253). Enumerated failure modes: a
    // never-seen offer must retry up to the bound and then park WITHOUT claiming absence; a known
    // deadline must retry until it actually passes, then park on that positive knowledge.
    #[test]
    fn plan_missing_offer_never_treats_an_empty_read_as_absence() {
        const MAX: u32 = 5;

        // Deadline unknown: retry up to the bound. The FIRST empty read must not park — that is
        // exactly the read that killed four valid trades.
        assert_eq!(plan_missing_offer(None, 1_000, 0, MAX), MissingOfferAction::Retry);
        assert_eq!(plan_missing_offer(None, 1_000, MAX - 1, MAX), MissingOfferAction::Retry);
        // Bound exhausted, offer never seen ⇒ park, but as unverified, not as absent.
        assert_eq!(plan_missing_offer(None, 1_000, MAX, MAX), MissingOfferAction::ParkUnverified);
        assert_eq!(plan_missing_offer(None, 1_000, MAX + 1, MAX), MissingOfferAction::ParkUnverified);

        // Deadline known and in the future: retry no matter how many reads have failed — the
        // deadline is the bound, so consecutive failures never cut a live offer short.
        assert_eq!(plan_missing_offer(Some(2_000), 1_000, 0, MAX), MissingOfferAction::Retry);
        assert_eq!(plan_missing_offer(Some(2_000), 1_000, 99, MAX), MissingOfferAction::Retry);
        // Exactly at the deadline is still inside it (the read loop uses `now > deadline`).
        assert_eq!(plan_missing_offer(Some(2_000), 2_000, 99, MAX), MissingOfferAction::Retry);
        // Past the deadline ⇒ park on positive knowledge, whatever the read count.
        assert_eq!(plan_missing_offer(Some(2_000), 2_001, 0, MAX), MissingOfferAction::ParkExpired);

        // A bound of zero still cannot park a job whose deadline is known and unexpired.
        assert_eq!(plan_missing_offer(Some(2_000), 1_000, 0, 0), MissingOfferAction::Retry);
        // …but with no deadline it gives up at once, which is the only case that may.
        assert_eq!(plan_missing_offer(None, 1_000, 0, 0), MissingOfferAction::ParkUnverified);
    }

    // ── CONCURRENCY test helpers ────────────────────────────────────────────────────────────────

    /// A home with the given budget cap, on a scratch dir. `per_job` is set equal to `cap` so the
    /// per-job ceiling never masks the total/wallet ceilings these tests exercise.
    fn conc_home(label: &str, cap: u64) -> (crate::home::MobeeHome, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "mobee-buyer-lifecycle-conc-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        home.config.total_budget_sats = cap;
        home.config.per_job_budget_sats = cap;
        (home, root)
    }

    /// A stand-in published-award outcome for the `award_with_reservation` publish closure (these
    /// tests never touch a relay — the money accounting is what is under test, not the wire).
    fn fake_award_outcome(job_id: &str) -> AwardClaimOutcome {
        AwardClaimOutcome {
            award_event_id: "e".repeat(64),
            job_id: job_id.to_owned(),
            claim_id: "c".repeat(64),
            seller_pubkey: SELLER_HEX.to_owned(),
            quoted_mints: Vec::new(),
        }
    }

    // ★ N-AGENT NO-OVERSPEND TOOTH. The buyer daemon serves N MCP agents that all draw the SAME
    // wallet + budget (gudnuf's product decision: one wallet, one budget, N equal agents, no
    // per-agent caps). This is the assembled-money invariant at that scale: no matter how the awards
    // interleave, the funds committed across every agent can never exceed what the buyer actually
    // has — the smaller of the live wallet balance and the budget cap.
    //
    // It composes the SAME seam the daemon's `award` RPC uses — a balance/spent snapshot then
    // `award_with_reservation` — serialized behind the SAME kind of async money lock the daemon
    // holds (`BuyerContext::money_lock`), over the REAL durable store + REAL budget ledger. Five
    // agents each try to reserve 30 against a shared 100: at most three fit (90), the other two must
    // get a clean insufficient-available refusal, and the total reserved must land at exactly 90.
    //
    // The reserved-accumulation race itself is closed one layer down by the store's `BEGIN
    // IMMEDIATE` (store `tooth2`); this proves the assembled award path — snapshot + reserve seam +
    // budget ledger — enforces the two-ceiling cap under many concurrent agents, and that idempotent
    // re-awards never inflate the committed total.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn n_equal_agents_cannot_overspend_the_shared_budget() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let cap = 100u64;
        let balance = 100u64; // modeled wallet ecash; no settle in this test, so it never moves
        let amount = 30u64; // min(cap, balance) = 100 ⇒ exactly 3 fit (90), 2 must be refused
        let agents = 5usize;

        let (home, root) = conc_home("n-agents", cap);
        let home = Arc::new(home);
        let store = Arc::new(BuyerStore::open(root.join("buyer.sqlite")).expect("store"));
        let money_lock = Arc::new(Mutex::new(()));

        let mut set = tokio::task::JoinSet::new();
        for agent in 0..agents {
            let (home, store, money_lock) = (home.clone(), store.clone(), money_lock.clone());
            // A distinct 64-hex job id per agent.
            let job = format!("{agent:064x}");
            set.spawn(async move {
                // The daemon's money lock, held across the snapshot AND the reserve — exactly as
                // `award` composes it, so no agent's snapshot races another's commit.
                let _guard = money_lock.lock().await;
                let gate = BudgetGate::from_home(&home).expect("gate");
                let (total_cap, spent) = (gate.total_cap(), gate.spent());
                let job_out = job.clone();
                award_with_reservation(&store, &job, amount, balance, total_cap, spent, 1, || async {
                    Ok(fake_award_outcome(&job_out))
                })
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
                .is_ok()
            });
        }

        let mut wins = 0usize;
        let mut refusals = 0usize;
        while let Some(result) = set.join_next().await {
            if result.expect("join") {
                wins += 1;
            } else {
                refusals += 1;
            }
        }

        assert_eq!(wins, 3, "exactly three 30-sat awards fit under a shared 100");
        assert_eq!(refusals, 2, "the other two agents must get a clean refusal, not an overspend");

        let reserved = store.reserved_in_flight().expect("reserved");
        assert_eq!(reserved, 90, "total committed is exactly the three winners' 90");
        assert!(
            reserved <= cap.min(balance),
            "committed {reserved} must never exceed min(cap, balance) = {}",
            cap.min(balance)
        );

        // Idempotency under concurrency: a winning agent re-awarding its own job (a client retry)
        // must not commit a second time — the reserved total stays put.
        let winner = format!("{:064x}", 0);
        let winner_out = winner.clone();
        let _ = award_with_reservation(&store, &winner, amount, balance, cap, 0, 2, || async {
            Ok(fake_award_outcome(&winner_out))
        })
        .await
        .expect("idempotent re-award");
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
        let cap = 1_000u64; // budget cap deliberately NOT the binding ceiling — the wallet is
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
                .reserve(&job_x, paid, start_balance, cap, 0, 1)
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
                let (home, job_y) = (home.clone(), job_y.clone());
                let (snapshot_taken, settle_done) = (snapshot_taken.clone(), settle_done.clone());
                tokio::spawn(async move {
                    // Read the balance snapshot BEFORE the settle melts it…
                    let stale_balance = balance.load(Ordering::SeqCst);
                    snapshot_taken.notify_one();
                    // …then reserve AFTER the settle's flip has cleared job_x from `reserved`.
                    settle_done.notified().await;
                    let spent = BudgetGate::from_home(&home).expect("gate").spent();
                    let job_out = job_y.clone();
                    award_with_reservation(
                        &store,
                        &job_y,
                        award,
                        stale_balance,
                        cap,
                        spent,
                        4,
                        || async { Ok(fake_award_outcome(&job_out)) },
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
                .reserve(&job_x, paid, start_balance, cap, 0, 1)
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
                let (home, job_y) = (home.clone(), job_y.clone());
                tokio::spawn(async move {
                    // The daemon's `award` holds money_lock across snapshot + reserve.
                    let _guard = money_lock.lock().await;
                    let snap = balance.load(Ordering::SeqCst);
                    let spent = BudgetGate::from_home(&home).expect("gate").spent();
                    let job_out = job_y.clone();
                    award_with_reservation(&store, &job_y, award, snap, cap, spent, 4, || async {
                        Ok(fake_award_outcome(&job_out))
                    })
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
}
