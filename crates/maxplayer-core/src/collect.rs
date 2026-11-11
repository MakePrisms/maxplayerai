//! Buyer single-call collect: accept-if-needed, verify delivery integrity, auto-pay, materialize.
//!
//! The buyer's post-award receive flow is one atomic call. A buyer with just an awarded job and a
//! delivered result calls `collect(job_id)` ONCE — no separate accept_claim step. `collect_async`
//! composes the sealed [`authorize_pay`](crate::authorize_pay) money path — spend gate, budget,
//! single-redeem, and mint-compat refusal all live there and are inherited here by construction —
//! with a read-only checkout of the paid delivery's tree from the buyer store. It adds NO new money
//! authority:
//!
//!   1. accept-if-needed: if no accept-bind exists yet, run the accept step ITSELF
//!      ([`accept_for_collect_async`](crate::job_lifecycle::accept_for_collect_async)) — fetch the
//!      seller's delivered result from the relay and record the co-signed pay-bind (seller / result
//!      / commit / repo / branch / job-hash / creq_hash, all accept-time money gates). This only
//!      moves WHERE the bind is created; the explicit `accept_claim` primitive stays available for
//!      buyers who want to bind separately, but collect no longer REQUIRES it.
//!   2/3. verify integrity + pay: the buyer's tip-match commitment is the oid it accepted
//!        (`bind.commit_oid`); the machine tip-match (fetch the delivered branch, compare its tip to
//!        that oid) runs inside `authorize_pay` BEFORE any spend, so a delivered-oid ≠ bound-oid
//!        mismatch — and the pre-pay seller co-signature check — refuse with ZERO spend and NO
//!        materialize;
//!   4. materialize: only after the pay above succeeds (or idempotently reconciles) are the
//!      delivered files checked out.
//!
//! Idempotent by attempt id: re-collecting an already-paid job loads the existing bind, reconciles
//! the payment without a second spend, and re-materializes the files.
//!
//! # Two modes
//!
//! Step 2/3 above is the ONE place `payment=none` (§1.1) diverges. `collect_async` branches on
//! `bind.payment_mode` — the mode the buyer-signed offer and the seller-signed claim AGREED on,
//! frozen at accept — and a free bind runs [`collect_free`] instead of the money path: the same
//! tip-match and the same §19 execution-sentinel check, then materialize and record, with no
//! `authorize_pay`, no [`BudgetGate`] append, no wallet open and no mint contact. The paid path is
//! untouched.
//!
//! That branch is not an optimisation, it is a REQUIREMENT of shipping a free post surface:
//! `authorize_pay_async` refuses a free bind at its entry, so a buyer able to post a free job
//! without this branch could post one it could never collect. The branch and the post-surface
//! `payment` parameter therefore land in one commit.

use std::path::{Path, PathBuf};

use crate::authorize_pay::{self, AuthorizePayError, AuthorizePayOutcome};
use crate::budget::BudgetGate;
use crate::delivery::{CommitOid, DeliveryVerifier, GitDelivery};
use crate::delivery_git::PayPathDeliveryVerifier;
use crate::home::{self, MaxplayerHome};
use crate::job_lifecycle::{self, AcceptedBind, JobLifecycleError};

/// Inputs for [`collect_async`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectRequest {
    pub accept_review_verdict: Option<String>,
    /// Offer event id (hex). A prior accept_claim bind is used when present; otherwise collect
    /// accepts the delivered claim itself.
    pub job_id: String,
    /// Optional output folder NAME (no path separators) under `<home>/results`. `None` ⇒ the job id.
    pub out: Option<String>,
}

/// What a collect SETTLED — the one place the two payment modes differ in the outcome.
///
/// A variant rather than an `Option<AuthorizePayOutcome>`: `Free` is a positive statement that this
/// trade had no payment leg, and it is named `Free` (never `None`) so no call site has to read
/// `CollectPayment::None` next to an `Option::None` and keep the two apart — the same reason
/// [`crate::gateway::PaymentMode::is_free`] exists.
#[derive(Clone, Debug)]
pub enum CollectPayment {
    /// `payment=sat`: the sealed money path ran and returned this outcome.
    Sat(AuthorizePayOutcome),
    /// `payment=none`: there was nothing to pay. No `authorize_pay`, no [`BudgetGate`] append, no
    /// wallet open, no mint contact — the delivery was still tip-matched + sentinel-checked before
    /// its files were materialized.
    Free,
}

impl CollectPayment {
    /// What the seller received: the paid amount, or `0` for a free trade.
    pub fn amount_sats(&self) -> u64 {
        match self {
            Self::Sat(outcome) => outcome.amount_sats,
            Self::Free => 0,
        }
    }

    /// Whether this collect settled a FREE trade.
    pub fn is_free(&self) -> bool {
        matches!(self, Self::Free)
    }

    /// The pay outcome when one exists; `None` for a free trade.
    pub fn paid(&self) -> Option<&AuthorizePayOutcome> {
        match self {
            Self::Sat(outcome) => Some(outcome),
            Self::Free => None,
        }
    }
}

/// Successful collect outcome: what settled plus the materialized delivery.
#[derive(Clone, Debug)]
pub struct CollectOutcome {
    pub pay: CollectPayment,
    pub commit_oid: String,
    /// Absolute path the delivery files were checked out to.
    pub path: String,
    /// Sorted relative file list written under `path`.
    pub files: Vec<String>,
    /// Seller-claimed harness that RAN the paid result, from the accept-bind (#261). An
    /// attribution captured off the delivered result's exec-metadata — never the requested
    /// harness written upfront. `None` when the result carried no metadata.
    pub agent_used: Option<String>,
    /// Model the harness self-reported for the run; same trust class as `agent_used`.
    pub model_used: Option<String>,
}

#[derive(Debug)]
pub enum CollectError {
    Input(String),
    Lifecycle(JobLifecycleError),
    Pay(AuthorizePayError),
    /// A FREE collect's delivery verification refused — the tip-match or the §19 execution
    /// sentinel. The paid path's equivalent refusals arrive as [`CollectError::Pay`] because they
    /// run inside `authorize_pay`; a free job never enters that function, so its verification
    /// refusals need a class of their own rather than a borrowed pay error that names no payment.
    Integrity(String),
    Materialize(String),
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "collect: {message}"),
            Self::Lifecycle(error) => write!(formatter, "collect: {error}"),
            Self::Pay(error) => write!(formatter, "collect: {error}"),
            Self::Integrity(message) => write!(formatter, "collect integrity: {message}"),
            Self::Materialize(message) => write!(formatter, "collect materialize: {message}"),
        }
    }
}

impl std::error::Error for CollectError {}

/// Single-call buyer collect: verify integrity + pay + materialize. See the module docs.
///
/// On an integrity mismatch (the delivered branch does not tip at the accepted `commit_oid`), the
/// composed [`authorize_pay`](crate::authorize_pay::authorize_pay_async) refuses BEFORE the budget
/// gate — so this returns [`CollectError::Pay`] with ZERO spend and no files are materialized.
///
/// `gate` is OPTIONAL, and the `Option` is the free lane's contract stated in the type: a free
/// collect runs no payment leg, so it needs no budget gate, so a caller must not be made to open
/// the durable spend ledger in order to reach one. `None` is what a free caller passes. The paid
/// arm below refuses LOUDLY on `None` rather than substituting an in-memory gate, so a future edit
/// that routed a spend onto the free path fails red instead of quietly charging an empty ledger.
pub async fn collect_async(
    home: &MaxplayerHome,
    gate: Option<&mut BudgetGate>,
    request: CollectRequest,
) -> Result<CollectOutcome, CollectError> {
    crate::review::validate_accept_review_verdict(request.accept_review_verdict.as_deref())
        .map_err(|e| CollectError::Lifecycle(job_lifecycle::JobLifecycleError::Input(e)))?;
    // 1. Load the buyer's accept-bind — the delivered commit + pay terms it recorded at accept.
    // Never caller input, so collect always settles the accepted, tip-matched delivery. When no bind
    // exists yet, run the accept step ITSELF (fetch the delivered result, record the co-signed
    // pay-bind through the SAME accept path) so collect is a true one-call. Re-collect loads the
    // existing bind and never re-accepts.
    let bind = match job_lifecycle::load_accepted_bind(home, &request.job_id)
        .map_err(CollectError::Lifecycle)?
    {
        Some(bind) => bind,
        None => job_lifecycle::accept_for_collect_async(
            home,
            &request.job_id,
            request.accept_review_verdict,
        )
        .await
        .map_err(CollectError::Lifecycle)?,
    };

    // Resolve + validate the destination BEFORE spending, so a bad `out` name never pays then fails.
    let dest = results_dest(home, &request.job_id, request.out.as_deref())
        .map_err(CollectError::Input)?;
    check_dest(&dest).map_err(CollectError::Input)?;

    // 2/3. THE MODE BRANCH. `bind.payment_mode` is the agreement of the buyer-signed OFFER and the
    // seller-signed CLAIM, decided and frozen at accept by `verify_free_accept` — never re-derived
    // here, never inferred from `amount_sats == 0`, and never caller input. A free trade has no
    // payment leg to run, so it takes the branch below; every priced trade goes through the sealed
    // money path exactly as before.
    //
    // ⛔ ORDERING (Bob, 2026-09-04): this branch and the post-surface `payment` parameter are ONE
    // commit. `authorize_pay_async` refuses a free bind at its entry (§2.5), so a shipped surface
    // that could post a free job WITHOUT this branch would produce jobs that are postable and
    // uncollectable. Nothing on this branch may reintroduce that window.
    if bind.payment_mode.is_free() {
        job_lifecycle::validate_private_bind(home, &bind).map_err(CollectError::Lifecycle)?;
        let files = match bind.inline_answer.as_deref() {
            // An inline delivery has nothing to fetch: the answer is on the bind.
            Some(answer) if bind_is_inline(&bind) => {
                let files = materialize_inline_delivery(answer, &dest)
                    .map_err(CollectError::Materialize)?;
                // §7.0 — the record is the artifact, never the silence. `collect_free` writes it
                // on the git arm, and this arm does not go through `collect_free`, so it writes
                // its own. Without it a completed free inline trade leaves no buyer-side trace
                // that it ever happened.
                write_free_collect_record(home, &bind, &dest, &files)?;
                files
            }
            _ => collect_free(home, &bind, &dest)?,
        };
        return Ok(CollectOutcome {
            pay: CollectPayment::Free,
            commit_oid: bind.commit_oid,
            path: dest.display().to_string(),
            files,
            agent_used: bind.agent_used,
            model_used: bind.model_used,
        });
    }

    // The PAID path, unchanged. The tip-match commitment is the oid the buyer accepted; the machine
    // tip-match runs inside authorize_pay before any spend. Idempotent by attempt id: a re-collect
    // reconciles without a second spend.
    let pay_request = job_lifecycle::authorize_request_from_bind(
        &bind,
        bind.amount_sats,
        bind.commit_oid.clone(),
    )
    .map_err(CollectError::Lifecycle)?;
    // A priced collect REQUIRES the durable gate. Refuse rather than fabricate one: an in-memory
    // substitute would read `spent = 0` and append nowhere, which is a spend with no ledger.
    let gate = gate.ok_or_else(|| {
        CollectError::Input(
            "a priced collect requires the durable budget gate; none was supplied".to_owned(),
        )
    })?;
    let pay = authorize_pay::authorize_pay_async(home, gate, pay_request)
        .await
        .map_err(CollectError::Pay)?;

    // 4. Materialize the paid delivery's files (read-only checkout from the buyer store). Reached
    // only after the pay above succeeded or reconciled — never on an integrity refusal.
    let files = match bind.inline_answer.as_deref() {
        // Reached only after the pay above succeeded, exactly like the git arm below. The answer
        // was bound at accept under the digest `authorize_pay` just verified the co-signature over,
        // so writing it here materializes the artifact that was paid for.
        Some(answer) if bind_is_inline(&bind) => {
            materialize_inline_delivery(answer, &dest).map_err(CollectError::Materialize)?
        }
        _ => {
            let store = delivery_store_path(home);
            let store_ref = PayPathDeliveryVerifier::store_ref_for(&bind.commit_oid);
            materialize_delivery(&store, &store_ref, &bind.commit_oid, &dest)
                .map_err(CollectError::Materialize)?
        }
    };

    Ok(CollectOutcome {
        pay: CollectPayment::Sat(pay),
        commit_oid: bind.commit_oid,
        path: dest.display().to_string(),
        files,
        // Attribution recorded at accept from the delivered result's seller-claimed
        // exec-metadata; the settle path persists it onto the awards row (#261).
        agent_used: bind.agent_used,
        model_used: bind.model_used,
    })
}

/// Session jobs (§8.4): settle ONE turn ≥ 2 — accept its result if needed (per-turn bind at
/// `jobs/<followup_id>.json`, ACCEPT with `reply`), refuse a git turn whose commit does not have the
/// previous git turn's commit as its single parent (before any spend), then pay (NUT-18 payload id
/// = the follow-up id) or settle free, and materialize into `results/<job_id>-t<n>`.
/// `earlier` lists the follow-up ids of turns 2..n-1, oldest first.
pub async fn collect_turn_async(
    home: &MaxplayerHome,
    gate: Option<&mut BudgetGate>,
    job_id: &str,
    followup_id: &str,
    turn: u32,
    earlier: &[String],
) -> Result<CollectOutcome, CollectError> {
    let bind = job_lifecycle::accept_turn_async(home, job_id, followup_id)
        .await
        .map_err(CollectError::Lifecycle)?;
    let dest = results_dest(home, &crate::session::collect_dir_name(job_id, turn), None)
        .map_err(CollectError::Input)?;
    check_dest(&dest).map_err(CollectError::Input)?;
    if !bind_is_inline(&bind)
        && let Some(parent) = job_lifecycle::turn_parent_commit(home, job_id, earlier)
            .map_err(CollectError::Lifecycle)?
    {
        verify_turn_parent_in_store(home, &bind, &parent)?;
    }
    if bind.payment_mode.is_free() {
        job_lifecycle::validate_private_bind(home, &bind).map_err(CollectError::Lifecycle)?;
        let files = match bind.inline_answer.as_deref() {
            Some(answer) if bind_is_inline(&bind) => {
                let files = materialize_inline_delivery(answer, &dest)
                    .map_err(CollectError::Materialize)?;
                write_free_collect_record(home, &bind, &dest, &files)?;
                files
            }
            _ => collect_free(home, &bind, &dest)?,
        };
        return Ok(CollectOutcome {
            pay: CollectPayment::Free,
            commit_oid: bind.commit_oid,
            path: dest.display().to_string(),
            files,
            agent_used: bind.agent_used,
            model_used: bind.model_used,
        });
    }
    let pay_request =
        job_lifecycle::authorize_request_from_bind(&bind, bind.amount_sats, bind.commit_oid.clone())
            .map_err(CollectError::Lifecycle)?;
    let gate = gate.ok_or_else(|| {
        CollectError::Input("a priced turn collect requires the durable budget gate".to_owned())
    })?;
    let pay = authorize_pay::authorize_pay_async(home, gate, pay_request)
        .await
        .map_err(CollectError::Pay)?;
    let files = match bind.inline_answer.as_deref() {
        Some(answer) if bind_is_inline(&bind) => {
            materialize_inline_delivery(answer, &dest).map_err(CollectError::Materialize)?
        }
        _ => materialize_delivery(
            &delivery_store_path(home),
            &PayPathDeliveryVerifier::store_ref_for(&bind.commit_oid),
            &bind.commit_oid,
            &dest,
        )
        .map_err(CollectError::Materialize)?,
    };
    Ok(CollectOutcome {
        pay: CollectPayment::Sat(pay),
        commit_oid: bind.commit_oid,
        path: dest.display().to_string(),
        files,
        agent_used: bind.agent_used,
        model_used: bind.model_used,
    })
}

/// Fetch + tip-match the turn's delivery into the buyer store, then require its single parent to
/// be `parent` (failure test 10). Runs before any payment; a refusal pays and writes nothing.
fn verify_turn_parent_in_store(
    home: &MaxplayerHome,
    bind: &AcceptedBind,
    parent: &str,
) -> Result<(), CollectError> {
    let commit_oid = CommitOid::parse(bind.commit_oid.clone())
        .map_err(|error| CollectError::Integrity(format!("accepted commit_oid: {error}")))?;
    let delivery = GitDelivery::new(bind.repo.clone(), bind.branch.clone(), commit_oid)
        .map_err(|error| CollectError::Integrity(format!("accepted delivery: {error}")))?;
    let secret_hex = home::read_secret_key_hex(home)
        .map_err(|error| CollectError::Integrity(format!("buyer key: {error}")))?;
    let store = delivery_store_path(home);
    let mut verifier = PayPathDeliveryVerifier::new(store.clone(), Some(secret_hex));
    let verified = verifier
        .verify(&delivery)
        .map_err(|error| CollectError::Integrity(format!("delivery verify refused: {error}")))?;
    if verified.commit_oid().as_str() != bind.commit_oid {
        return Err(CollectError::Integrity("turn delivery tip does not match its bind".into()));
    }
    let parents = commit_parents(&store, &bind.commit_oid).map_err(CollectError::Integrity)?;
    crate::session::verify_turn_parent(&parents, parent)
        .map_err(|why| CollectError::Integrity(format!("turn {}: {why}", bind.commit_oid)))
}

/// The parent oids of `commit_oid` in the buyer store.
pub fn commit_parents(store: &Path, commit_oid: &str) -> Result<Vec<String>, String> {
    let repo = git2::Repository::open_bare(store)
        .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
    let oid = git2::Oid::from_str(commit_oid).map_err(|error| error.to_string())?;
    let commit = repo.find_commit(oid).map_err(|error| error.to_string())?;
    Ok(commit.parent_ids().map(|id| id.to_string()).collect())
}

/// Blocking wrapper over [`collect_async`] for the CLI (builds a current-thread runtime).
pub fn collect_blocking(
    home: &MaxplayerHome,
    gate: Option<&mut BudgetGate>,
    request: CollectRequest,
) -> Result<CollectOutcome, CollectError> {
    crate::runtime_guard::refuse_nested_block_on("collect_blocking")
        .map_err(CollectError::Input)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| CollectError::Input(format!("collect runtime: {error}")))?;
    runtime.block_on(collect_async(home, gate, request))
}

/// The FREE collect path (`payment=none`): verify the delivery exactly as hard as the paid path
/// does, materialize it, and record the collect. No payment leg exists, so none is run.
///
/// What this deliberately does NOT do — the list is the point of the function:
///
///   * no [`authorize_pay`](crate::authorize_pay) (it refuses a free bind at its entry, §2.5),
///   * no [`BudgetGate`] append or read — a free collect cannot move the spend total,
///   * no wallet open, no mint contact, no `plan_payment`. A buyer holding zero bitcoin, with no
///     wallet and no mint configured, must reach the delivered files, and every one of those calls
///     is a place a wallet-less buyer would have been refused.
///
/// What it DOES do, both of them borrowed from the paid path so a free delivery is never
/// materialized on weaker evidence than a paid one:
///
///   1. **The tip-match** — the same fetch + compare `authorize_pay` runs through
///      [`crate::payment::verify_pay_path_delivery`]: the delivered branch must tip at the
///      `commit_oid` the buyer accepted. The comparison is against the BIND, never the seller's
///      echo. This is also what fetches the delivery into the buyer store, so it is what makes the
///      materialize below possible at all.
///   2. **The §19 execution sentinel** (from-scratch jobs) — the delivered tree must carry THIS
///      job's sentinel, evidence the buyer reads itself rather than seller testimony. A refusal is
///      journalled through the same `sentinel-refusals` artifact the pay path writes, so §17
///      reputation sees a free job's refusal identically to a priced one's.
///
/// The receipt co-signature tooth is NOT run here, and cannot be: that tooth verifies the seller's
/// signature over the payment RECEIPT preimage (mint, amount, unit, attempt id), which does not
/// exist for a trade with no payment. The seller's co-signature over the delivered RESULT is
/// verified at accept, which has already run by the time we are here.
fn collect_free(
    home: &MaxplayerHome,
    bind: &AcceptedBind,
    dest: &Path,
) -> Result<Vec<String>, CollectError> {
    let commit_oid = CommitOid::parse(bind.commit_oid.clone())
        .map_err(|error| CollectError::Integrity(format!("accepted commit_oid: {error}")))?;
    let delivery = GitDelivery::new(bind.repo.clone(), bind.branch.clone(), commit_oid)
        .map_err(|error| CollectError::Integrity(format!("accepted delivery: {error}")))?;

    // Buyer secret signs NIP-98 for the relay-git READ, exactly as the pay path's verifier does.
    // This is a delivery fetch, not a money operation: no wallet, no mint, no proofs.
    let secret_hex = home::read_secret_key_hex(home)
        .map_err(|error| CollectError::Integrity(format!("buyer key: {error}")))?;
    let store = delivery_store_path(home);
    let mut verifier = PayPathDeliveryVerifier::new(store.clone(), Some(secret_hex));

    if let (Some(evidence), Some(pin)) = (&bind.private_evidence, &bind.contribution) {
        let base = CommitOid::parse(pin.base_oid.clone())
            .map_err(|e| CollectError::Integrity(e.to_string()))?;
        let policy = crate::private_content::runtime::Policy::for_evidence(home, evidence)
            .map_err(|e| CollectError::Integrity(e.to_string()))?;
        // Private jobs read only their relay job repository here, never the offer's target URL.
        let source = authorize_pay::contribution_base_source(
            Some((evidence, &policy)),
            &pin.target_clone_url,
            &pin.base_branch,
        )
        .map_err(CollectError::Integrity)?;
        verifier
            .verify_contribution(
                &delivery,
                &source,
                &base,
                &authorize_pay::contribution_policy(home),
            )
            .map_err(|e| CollectError::Integrity(e.to_string()))?;
    }

    // 1. THE TIP-MATCH. Same verifier, same allowlist, same fetch, same compare as the paid path —
    // only the payment that would have followed it is absent.
    let verified = verifier
        .verify(&delivery)
        .map_err(|error| CollectError::Integrity(format!("delivery verify refused: {error}")))?;
    if verified.commit_oid().as_str() != bind.commit_oid {
        return Err(CollectError::Integrity(format!(
            "verified delivery commit {} does not match the accepted commit_oid {} (buyer \
             tip-match required; refuse mismatch)",
            verified.commit_oid().as_str(),
            bind.commit_oid
        )));
    }

    // 2. THE §19 SENTINEL. Contribution deliveries are gated by the contribution content path and
    // are not served free yet, so — as on the paid path — the sentinel check is from-scratch only.
    // `bind.contribution == None` iff the offer was from-scratch (accept is fail-closed).
    let store_ref = PayPathDeliveryVerifier::store_ref_for(&bind.commit_oid);
    if bind.contribution.is_none() {
        match authorize_pay::delivery_tree_carries_sentinel(
            verifier.repository(),
            &store_ref,
            &bind.commit_oid,
            &bind.job_hash,
            &bind.job_id,
        ) {
            Ok(true) => {}
            Ok(false) => {
                authorize_pay::journal_sentinel_refusal(
                    home,
                    &bind.job_id,
                    &bind.commit_oid,
                    "missing_or_replayed",
                );
                return Err(CollectError::Integrity(format!(
                    "delivery {} carries no execution sentinel bound to job {} (§19); refusing \
                     no_sentinel and materializing nothing",
                    bind.commit_oid, bind.job_id
                )));
            }
            Err(reason) => {
                authorize_pay::journal_sentinel_refusal(
                    home,
                    &bind.job_id,
                    &bind.commit_oid,
                    "verify_error",
                );
                return Err(CollectError::Integrity(format!(
                    "delivery {} sentinel could not be verified ({reason}); fail-closed, \
                     materializing nothing",
                    bind.commit_oid
                )));
            }
        }
    }

    // 3. Materialize — the SAME read-only checkout the paid path performs, reached only after both
    // verifications above passed.
    let files = materialize_delivery(&store, &store_ref, &bind.commit_oid, dest)
        .map_err(CollectError::Materialize)?;

    // 4. Record the collect. A free job produces no payment-journal entry (there is no payment to
    // journal), so without this a completed free trade would leave the buyer no durable local
    // artifact at all — and §7.0 is that the record is the artifact, never the silence. Written
    // ONLY on the free path: the paid path's durable record is its payment journal, and adding a
    // second one there would change paid behaviour.
    write_free_collect_record(home, bind, dest, &files)?;
    Ok(files)
}

/// Where a FREE collect's durable buyer-side record lives: `<home>/collects/<job_id>.json`.
pub fn free_collect_record_path(home: &MaxplayerHome, job_id: &str) -> PathBuf {
    home.root.join("collects").join(format!("{job_id}.json"))
}

/// Write the free collect record, `payment = "none"` — the buyer-side fact that this job completed
/// with no payment leg. WRITE-ONCE: a re-collect re-materializes the files (idempotent) and leaves
/// the first record's `collected_at` standing, so the record dates the collect rather than the last
/// re-read of it.
fn write_free_collect_record(
    home: &MaxplayerHome,
    bind: &AcceptedBind,
    dest: &Path,
    files: &[String],
) -> Result<(), CollectError> {
    // A session turn's record is keyed by its follow-up, so it never collides with turn 1's.
    let path = free_collect_record_path(home, bind.followup_id.as_deref().unwrap_or(&bind.job_id));
    if path.exists() {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| CollectError::Materialize("collect record has no parent dir".to_owned()))?;
    std::fs::create_dir_all(dir)
        .map_err(|error| CollectError::Materialize(format!("collect record dir: {error}")))?;
    let record = serde_json::json!({
        "job_id": bind.job_id,
        "claim_id": bind.claim_id,
        "result_id": bind.result_id,
        "seller_pubkey": bind.seller_pubkey,
        "commit_oid": bind.commit_oid,
        // The fact this record exists to state. Mirrors the seller store's `deliveries.payment`.
        "payment": crate::gateway::PaymentMode::None.as_wire(),
        "amount_sats": 0,
        "path": dest.display().to_string(),
        "files": files,
        "collected_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0),
    });
    std::fs::write(&path, format!("{record}\n"))
        .map_err(|error| CollectError::Materialize(format!("collect record write: {error}")))
}

/// The buyer store: the local bare repository the pay path retains verified delivery objects in.
/// Mirrors the path [`authorize_pay`](crate::authorize_pay) opens the verifier against.
pub fn delivery_store_path(home: &MaxplayerHome) -> PathBuf {
    home.root.join("store")
}

/// Resolve a delivery output folder under `<home>/results`. `out` is an optional simple folder NAME
/// — path separators / traversal are refused so a caller can never write outside `results`. `None`
/// ⇒ `<home>/results/<job_id>`.
pub fn results_dest(
    home: &MaxplayerHome,
    job_id: &str,
    out: Option<&str>,
) -> Result<PathBuf, String> {
    let results = home.root.join("results");
    match out {
        Some(name) => {
            if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
                return Err(
                    "'out' must be a simple folder name (no path separators or '..')".into(),
                );
            }
            Ok(results.join(name))
        }
        None => Ok(results.join(job_id)),
    }
}

/// True when this bind settles an INLINE delivery (§6.4). A bind written before inline delivery
/// existed carries no kind and is a git delivery — the fail-closed reading, and true of all of them.
pub fn bind_is_inline(bind: &AcceptedBind) -> bool {
    crate::job_lifecycle::bind_is_inline_kind(bind.delivery_kind.as_deref())
}

/// The file an inline answer is materialized as, under `results/<job_id>/`.
pub const INLINE_ANSWER_FILENAME: &str = "answer.txt";

/// Write an inline delivery's answer into `dest`. The inline analogue of [`materialize_delivery`]:
/// there is no store to open, no ref to resolve and no tree to walk — the answer arrived inside the
/// result event and was recorded on the bind at accept, under the digest the seller co-signed.
pub fn materialize_inline_delivery(answer: &str, dest: &Path) -> Result<Vec<String>, String> {
    // Same writer as the git path: a symlink already at `dest` or at `dest/answer.txt` (reused
    // `out`, a folder the operator filled) must not redirect the write (#1290 review).
    ensure_real_dir(dest)?;
    write_under(dest, INLINE_ANSWER_FILENAME, answer.as_bytes())?;
    Ok(vec![INLINE_ANSWER_FILENAME.to_owned()])
}

/// Check out the tree of `commit_oid` from the buyer store (bare repo) into `dest`, writing each
/// blob to its path and returning the sorted relative file list. Fail-closed: any read/write error
/// aborts (never a partial-but-reported materialization). Resolves the retention ref first, then the
/// raw oid as a fallback.
pub fn materialize_delivery(
    store: &Path,
    store_ref: &str,
    commit_oid: &str,
    dest: &Path,
) -> Result<Vec<String>, String> {
    let repo = git2::Repository::open_bare(store)
        .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
    let commit = repo
        .revparse_single(store_ref)
        .or_else(|_| repo.revparse_single(commit_oid))
        .map_err(|error| {
            format!("delivery {commit_oid} not found in buyer store (ref {store_ref}): {error}")
        })?
        .peel_to_commit()
        .map_err(|error| error.to_string())?;
    let tree = commit.tree().map_err(|error| error.to_string())?;

    // REPORT1110 F2: every entry name is seller-authored. Validate the WHOLE tree before the first
    // write (names, duplicates, depth, size, and that every file is in the store), so a delivery
    // that would fail below is refused with nothing on disk and, on the pay path, before payment.
    validate_tree_paths(&repo, &tree)?;
    // A first collect writes into a fresh staging directory and renames it into place, so a failure
    // part-way leaves nothing behind (#1290 fable M1). A re-collect into an existing folder writes in
    // place; each file is still replaced by rename, never written through.
    let staged = stage_for(dest)?;
    let target: &Path = staged.as_deref().unwrap_or(dest);
    let mut files: Vec<String> = Vec::new();
    let mut walk_err: Option<String> = None;
    let walked = tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return git2::TreeWalkResult::Ok;
        }
        // git2 0.21: `name()` is `Result` (non-UTF-8 is an error, as `None` was before).
        let name = match entry.name().ok() {
            Some(name) => name,
            None => {
                walk_err = Some("non-UTF-8 tree entry name".into());
                return git2::TreeWalkResult::Abort;
            }
        };
        let rel = format!("{root}{name}");
        let object = match entry.to_object(&repo) {
            Ok(object) => object,
            Err(error) => {
                walk_err = Some(error.to_string());
                return git2::TreeWalkResult::Abort;
            }
        };
        let Some(blob) = object.as_blob() else {
            return git2::TreeWalkResult::Ok;
        };
        // Re-checked per entry (defence in depth: `validate_tree_paths` already refused the tree).
        if let Err(error) = validate_delivery_path(&rel) {
            walk_err = Some(error);
            return git2::TreeWalkResult::Abort;
        }
        // P1 (rc8 review): agent-instruction and harness-config names are written inert.
        let written = materialized_path(&rel);
        if let Err(error) = write_under(target, &written, blob.content()) {
            walk_err = Some(error);
            return git2::TreeWalkResult::Abort;
        }
        files.push(written);
        git2::TreeWalkResult::Ok
    });
    // git2 >= 0.20: an aborting callback makes `walk` itself return `Err` (GIT_EUSER), so the
    // specific reason recorded in `walk_err` wins over libgit2's generic "callback returned".
    let outcome = match (walked, walk_err) {
        (_, Some(reason)) => Err(format!("failed: {reason}")),
        (Err(error), None) => Err(error.to_string()),
        (Ok(()), None) => Ok(()),
    };
    if let Err(error) = outcome {
        if let Some(staging) = &staged {
            let _ = std::fs::remove_dir_all(staging);
        }
        return Err(error);
    }
    if let Some(staging) = staged {
        std::fs::rename(&staging, dest).map_err(|error| {
            let _ = std::fs::remove_dir_all(&staging);
            format!("move {} into place at {}: {error}", staging.display(), dest.display())
        })?;
    }
    files.sort();
    Ok(files)
}

/// Prefix of the staging directories and temporary files collect creates. Refused as a delivered
/// name, so a seller cannot collide with one.
const TMP_PREFIX: &str = ".maxplayer-tmp-";

fn temp_name(kind: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("{TMP_PREFIX}{kind}-{}-{n}", std::process::id())
}

/// `Some(staging dir)` when `dest` does not exist yet (written there, then renamed to `dest`);
/// `None` when `dest` is an existing real directory (written in place).
fn stage_for(dest: &Path) -> Result<Option<std::path::PathBuf>, String> {
    match std::fs::symlink_metadata(dest) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = dest
                .parent()
                .ok_or_else(|| format!("{} has no parent directory", dest.display()))?;
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
            let staging = parent.join(temp_name("stage"));
            std::fs::create_dir(&staging)
                .map_err(|error| format!("create {}: {error}", staging.display()))?;
            Ok(Some(staging))
        }
        Err(error) => Err(format!("inspect {}: {error}", dest.display())),
        Ok(_) => {
            ensure_real_dir(dest)?;
            Ok(None)
        }
    }
}

/// The results folder must be absent or a real directory. Checked BEFORE payment (#1290 fable
/// L1): materialize refuses a symlinked or non-directory `dest`, and that refusal must not come
/// after the buyer has paid.
pub(crate) fn check_dest(dest: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "results folder {} is a symlink; collect does not write through symlinks. Remove it \
             (or pick another out name) and collect again — nothing was paid",
            dest.display()
        )),
        Ok(meta) if !meta.is_dir() => Err(format!(
            "{} exists and is not a directory — nothing was paid",
            dest.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect {}: {error}", dest.display())),
    }?;
    // #1290 fable r2 R1: the longest path the pre-pay check accepts must still be writable here.
    // Worst case: a staging dir in dest's place, the deepest delivered path, and a temporary file
    // name in place of its last component, under macOS's PATH_MAX (a release target).
    let worst = dest.as_os_str().len() + TEMP_NAME_MAX + 1 + MAX_DELIVERY_PATH_BYTES + TEMP_NAME_MAX + 1;
    if worst > PORTABLE_PATH_MAX {
        return Err(format!(
            "results folder {} is {} bytes long; with the longest delivery path collect accepts \
             ({MAX_DELIVERY_PATH_BYTES} bytes) a write could exceed the {PORTABLE_PATH_MAX}-byte \
             path limit. Use a shorter maxplayer home path — nothing was paid",
            dest.display(),
            dest.as_os_str().len()
        ));
    }
    Ok(())
}

/// `PATH_MAX` on macOS, including the terminating NUL: the smallest of the release targets.
const PORTABLE_PATH_MAX: usize = 1024;
/// Upper bound on a [`temp_name`] (`.maxplayer-tmp-stage-<pid>-<n>` is at most 52 bytes).
const TEMP_NAME_MAX: usize = 64;

/// Suffix materialize appends to a delivered path component an agent harness or editor would load
/// on its own (see [`AGENT_CONTROL_NAMES`]). The bytes are kept; only the name changes, and the
/// delivered commit in the buyer store still holds the original path.
pub const INERT_SUFFIX: &str = ".delivered";

/// P1 (v0.7.3-rc8 security review): file and directory names that agent harnesses and editors load
/// without being asked, as project instructions (`AGENTS.md`, `CLAUDE.md`, rules files), settings
/// and hooks (`.claude/`, `.cursor/`, `.vscode/`), MCP/tool servers (`.mcp.json`,
/// `opencode.json`) or shell code (`.envrc`, run by direnv on `cd`). A delivery is seller-authored
/// data, but a harness gives these names the authority of the buyer's own project rules, and some
/// load them from any directory an agent reads in (Codex/Claude nested `AGENTS.md`/`CLAUDE.md`).
/// Matched per path component, after the same case/Unicode folding [`collision_key`] uses, so
/// `Agents.MD` or `.Claude` on a case-insensitive volume is caught too. Lowercase.
///
/// A deny-list lags new harnesses; extend it when one adds an auto-loaded name. Not covered:
/// names only loaded under a parent (e.g. `.github/instructions/`), and a buyer who opens a
/// results folder as a workspace with tools that read every file.
const AGENT_CONTROL_NAMES: &[&str] = &[
    // Instruction files.
    "agents.md",
    "agent.md",
    "claude.md",
    "claude.local.md",
    "gemini.md",
    "crush.md",
    "copilot-instructions.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    ".roorules",
    ".goosehints",
    ".rules",
    // Tool, MCP and execution config.
    ".mcp.json",
    "opencode.json",
    "opencode.jsonc",
    ".aider.conf.yml",
    ".envrc",
    // Harness and editor directories (settings, hooks, rules, skills, tasks).
    ".agents",
    ".amazonq",
    ".claude",
    ".codex",
    ".continue",
    ".cursor",
    ".gemini",
    ".goose",
    ".junie",
    ".kiro",
    ".opencode",
    ".roo",
    ".vscode",
    ".windsurf",
    ".zed",
];

/// True when `component` is an [`AGENT_CONTROL_NAMES`] entry on any volume the buyer may use.
fn is_agent_control_name(component: &str) -> bool {
    let key = collision_key(component);
    AGENT_CONTROL_NAMES.contains(&key.as_str())
}

/// The path materialize writes a validated delivered path at: every component that is an agent-
/// control name gets [`INERT_SUFFIX`] (`AGENTS.md` → `AGENTS.md.delivered`, `.claude/settings.json`
/// → `.claude.delivered/settings.json`); every other component is unchanged. The pre-pay check,
/// the write and `get_job`'s file list all use this one mapping.
pub fn materialized_path(rel: &str) -> String {
    if !rel.split('/').any(is_agent_control_name) {
        return rel.to_owned();
    }
    rel.split('/')
        .map(|component| {
            if is_agent_control_name(component) {
                format!("{component}{INERT_SUFFIX}")
            } else {
                component.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Refuse a seller-authored delivery path that is not a plain relative path under the results
/// directory: empty, absolute, `.`/`..`/empty components, `.git` (any case), backslashes, colons
/// or control characters. The same rule the reviewer applies to the same trees
/// (`private_content::validate_path`), plus a length bound.
pub fn validate_delivery_path(rel: &str) -> Result<(), String> {
    if rel.len() > MAX_DELIVERY_PATH_BYTES
        || crate::private_content::validate_path(rel).is_err()
        || !rel.split('/').all(portable_component)
    {
        return Err(format!("unsafe delivery path {}; refusing the delivery", shown(rel)));
    }
    Ok(())
}

/// Longest delivered path. [`check_dest`] refuses, before pay, a results folder whose own path
/// leaves no room for it under [`PORTABLE_PATH_MAX`] (#1290 fable M1, r2 R1).
const MAX_DELIVERY_PATH_BYTES: usize = 512;
/// Deepest directory nesting in one delivery.
const MAX_DELIVERY_DEPTH: usize = 64;
/// Most tree entries (files, directories, symlinks, submodules) one delivery may hold. Also bounds
/// the walk itself: a tree is a DAG, so a few shared subtrees can name 2^N paths (#1290 fable M2).
const MAX_DELIVERY_ENTRIES: usize = 100_000;
/// Largest single delivered file.
const MAX_DELIVERY_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// Most bytes one delivery may write to disk.
const MAX_DELIVERY_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// A seller-chosen name as it appears in an error: Debug-escaped and cut to 120 characters, since
/// one entry name can be 64 KiB and the error reaches logs and agent tool results (#1290 fable L5).
fn shown(text: &str) -> String {
    const MAX_CHARS: usize = 120;
    if text.chars().count() <= MAX_CHARS {
        return format!("{text:?}");
    }
    let head: String = text.chars().take(MAX_CHARS).collect();
    format!("{head:?}… ({} bytes)", text.len())
}

/// Longest single path component the buyer's filesystem stores (`NAME_MAX` on Linux and APFS).
/// Refused here, on the pay path too, so a name that could only fail at write time is never paid.
const MAX_COMPONENT_BYTES: usize = 255;

/// A component no filesystem the buyer could run on reads as `.git` or `..` — git's
/// `core.protectNTFS` / `core.protectHFS` set (#1290 review): Win32 strips trailing dots and
/// spaces (`.git.`, `...`), NTFS has the 8.3 short name `GIT~1`, and HFS+ ignores some code points
/// when it compares names (`.gi\u{200C}t`). Not an escape on Linux/APFS, but the `.git` rule above
/// must hold on every volume a buyer might use.
fn portable_component(component: &str) -> bool {
    if component.len() > MAX_COMPONENT_BYTES
        || component.to_ascii_lowercase().starts_with(TMP_PREFIX)
        || component.ends_with('.')
        || component.ends_with(' ')
        || component.eq_ignore_ascii_case("git~1")
    {
        return false;
    }
    if component.chars().any(is_hfs_ignorable) {
        let folded: String = component.chars().filter(|c| !is_hfs_ignorable(*c)).collect();
        if folded.is_empty() || folded == "." || folded == ".." || folded.eq_ignore_ascii_case(".git")
        {
            return false;
        }
    }
    true
}

/// The key two delivered paths collide under on a case-insensitive APFS/HFS+ volume (#1290 fable
/// r2 R1): canonical decomposition (NFC `café` and NFD `café` are one name there), HFS-ignorable
/// code points dropped, and a full upper→lower round trip, which folds `ſ`→`s` and the Kelvin sign
/// →`k` where plain `to_lowercase` does not. Deliberately broad: it may refuse a pair Linux could
/// store, never accept a pair macOS cannot.
fn collision_key(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    // ASCII has no decomposition, no ignorables and a 1:1 case map: the same key, cheaply. Nearly
    // every honest tree takes this path (#1290 fable r3 L2).
    if path.is_ascii() {
        return path.to_ascii_lowercase();
    }
    let stripped: String = path.nfd().filter(|c| !is_hfs_ignorable(*c)).collect();
    // lower→upper→lower: the leading lowercase sends `ẞ` (already uppercase) to `ß`, which then
    // folds with `ß` and `ss` (#1290 fable r3 L1).
    stripped.to_lowercase().to_uppercase().to_lowercase().nfd().collect()
}

/// Code points HFS+ skips when comparing names (git `is_hfs_dotgit`).
fn is_hfs_ignorable(c: char) -> bool {
    matches!(c, '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}')
}

/// Walk `tree` and refuse it unless materialize could write ALL of it: every path safe (see
/// [`validate_delivery_path`]), no entry name containing `/`, no two paths that collide on a
/// case-insensitive volume (which also catches a blob and a subtree of the same name), nesting,
/// entry count, per-file and total size within bounds, and every file present in the store as a
/// blob. Reads object headers only and writes nothing, so the pay path runs it before any spend;
/// the entry cap bounds the walk itself.
pub fn validate_tree_paths(repo: &git2::Repository, tree: &git2::Tree<'_>) -> Result<(), String> {
    let odb = repo
        .odb()
        .map_err(|error| format!("open buyer store objects: {error}"))?;
    let mut failure: Option<String> = None;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entries = 0usize;
    let mut total_bytes = 0u64;
    let walked = tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        let mut refuse = |reason: String| {
            failure = Some(reason);
            git2::TreeWalkResult::Abort
        };
        entries += 1;
        if entries > MAX_DELIVERY_ENTRIES {
            return refuse(format!(
                "delivery has more than {MAX_DELIVERY_ENTRIES} entries; refusing the delivery"
            ));
        }
        let Ok(name) = entry.name() else {
            return refuse("non-UTF-8 tree entry name".into());
        };
        if name.contains('/') {
            return refuse(format!(
                "tree entry name {} contains '/'; refusing the delivery",
                shown(name)
            ));
        }
        if root.matches('/').count() >= MAX_DELIVERY_DEPTH {
            return refuse(format!(
                "delivery is nested deeper than {MAX_DELIVERY_DEPTH} directories; refusing the delivery"
            ));
        }
        let path = format!("{root}{name}");
        if let Err(error) = validate_delivery_path(&path) {
            return refuse(error);
        }
        // P1: materialize writes agent-control names with INERT_SUFFIX, so the written path is
        // the one that must fit the length bounds and must not collide (`AGENTS.md` next to a
        // delivered `AGENTS.md.delivered` would be one file on disk).
        let path = materialized_path(&path);
        if let Err(error) = validate_delivery_path(&path) {
            return refuse(error);
        }
        // Names APFS/HFS+ would store as one file collide: case, Unicode form, ignorables.
        if !seen.insert(collision_key(&path)) {
            return refuse(format!(
                "delivery path {} appears twice (up to letter case or Unicode form); refusing the delivery",
                shown(&path)
            ));
        }
        if entry.kind() == Some(git2::ObjectType::Blob) {
            match odb.read_header(entry.id()) {
                Ok((size, git2::ObjectType::Blob)) => {
                    let size = size as u64;
                    total_bytes = total_bytes.saturating_add(size);
                    if size > MAX_DELIVERY_FILE_BYTES {
                        return refuse(format!(
                            "delivered file {} is {size} bytes, over {MAX_DELIVERY_FILE_BYTES}; \
                             refusing the delivery",
                            shown(&path)
                        ));
                    }
                    if total_bytes > MAX_DELIVERY_TOTAL_BYTES {
                        return refuse(format!(
                            "delivery is over {MAX_DELIVERY_TOTAL_BYTES} bytes; refusing the delivery"
                        ));
                    }
                }
                Ok((_, kind)) => {
                    return refuse(format!(
                        "delivered file {} is a {kind} object, not a file; refusing the delivery",
                        shown(&path)
                    ));
                }
                // libgit2's fetch does not check connectivity, so a seller's server can omit blobs
                // from the pack. Refuse here, before pay, not at write time after it (fable M1).
                Err(_) => {
                    return refuse(format!(
                        "delivered file {} is missing from the buyer store; refusing the delivery",
                        shown(&path)
                    ));
                }
            }
        }
        git2::TreeWalkResult::Ok
    });
    match (walked, failure) {
        (_, Some(reason)) => Err(reason),
        (Err(error), None) => Err(error.to_string()),
        (Ok(()), None) => Ok(()),
    }
}

/// [`validate_tree_paths`] for the delivery `commit_oid` retained in the buyer store: the pre-pay
/// form, so a delivery collect would refuse to materialize is never paid for.
pub(crate) fn validate_delivery_tree_in_store(
    store: &Path,
    store_ref: &str,
    commit_oid: &str,
) -> Result<(), String> {
    let repo = git2::Repository::open_bare(store)
        .map_err(|error| format!("open buyer store {}: {error}", store.display()))?;
    let commit = repo
        .revparse_single(store_ref)
        .or_else(|_| repo.revparse_single(commit_oid))
        .map_err(|error| format!("delivery {commit_oid} not found in buyer store: {error}"))?
        .peel_to_commit()
        .map_err(|error| error.to_string())?;
    let tree = commit.tree().map_err(|error| error.to_string())?;
    validate_tree_paths(&repo, &tree)
}

/// Create `dir` if absent; refuse if it (the final component) is a symlink or not a directory.
fn ensure_real_dir(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(format!("refusing to materialize through symlink {}", dir.display()))
        }
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(format!("{} exists and is not a directory", dir.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(dir)
            .map_err(|error| format!("create {}: {error}", dir.display())),
        Err(error) => Err(format!("inspect {}: {error}", dir.display())),
    }
}

/// Write `content` to `dest/rel` without following a symlink at any component under `dest`:
/// intermediate directories are checked one by one, and a symlink already at the target refuses.
/// The bytes go to a fresh temporary file (`O_CREAT|O_EXCL|O_NOFOLLOW`) renamed over the target,
/// so an existing hard link, FIFO or racing symlink there is replaced, never written through or
/// blocked on (#1290 fable L2). Re-collect stays idempotent. `rel` must already have passed
/// [`validate_delivery_path`].
fn write_under(dest: &Path, rel: &str, content: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut parts: Vec<&str> = rel.split('/').collect();
    let file_name = parts.pop().ok_or_else(|| format!("unsafe delivery path {}", shown(rel)))?;
    let mut dir = dest.to_path_buf();
    for part in parts {
        dir.push(part);
        ensure_real_dir(&dir)?;
    }
    let path = dir.join(file_name);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!("refusing to write through symlink {}", path.display()));
        }
        Ok(meta) if meta.is_dir() => {
            return Err(format!("{} exists and is a directory", path.display()));
        }
        _ => {}
    }
    let temp = dir.join(temp_name("file"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("write {}: {error}", temp.display()))?;
    if let Err(error) = file.write_all(content) {
        drop(file);
        let _ = std::fs::remove_file(&temp);
        return Err(format!("write {}: {error}", path.display()));
    }
    drop(file);
    std::fs::rename(&temp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("write {}: {error}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home;
    use crate::job_lifecycle::AcceptedBind;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Failure test 10 (buyer half, git): a turn commit whose single parent is the previous
    /// turn's commit passes; a replayed turn-1 commit (parent ≠ previous) or a merge is refused.
    #[test]
    fn turn_commit_must_have_the_previous_turn_as_its_single_parent() {
        let root = temp_root("turn-parent");
        let store = root.join("store");
        let repo = git2::Repository::init_bare(&store).unwrap();
        let sig = git2::Signature::now("t", "t@example.invalid").unwrap();
        let tree = repo.find_tree(repo.treebuilder(None).unwrap().write().unwrap()).unwrap();
        // Distinct messages: identical commits would share one oid and hide the replay case.
        let commit = |msg: &str, parents: &[&git2::Commit]| {
            let oid = repo.commit(None, &sig, &sig, msg, &tree, parents).unwrap();
            repo.find_commit(oid).unwrap()
        };
        let t1 = commit("turn 1", &[]);
        let t2 = commit("turn 2", &[&t1]);
        let other = commit("unrelated root", &[]);
        let merge = commit("merge", &[&t1, &other]);
        let parents = |c: &git2::Commit| commit_parents(&store, &c.id().to_string()).unwrap();
        let t1_hex = t1.id().to_string();
        assert!(crate::session::verify_turn_parent(&parents(&t2), &t1_hex).is_ok());
        assert!(crate::session::verify_turn_parent(&parents(&t1), &t1_hex).is_err());
        assert!(crate::session::verify_turn_parent(&parents(&merge), &t1_hex).is_err());
        let replay = commit("replayed turn", &[&other]);
        assert!(crate::session::verify_turn_parent(&parents(&replay), &t1_hex).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp_root(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("maxplayer-collect-{label}-{}-{id}", std::process::id()))
    }

    /// Build a buyer store (bare repo) holding a delivered commit (README.md + src/lib.rs) under its
    /// retention ref, returning the commit hex. Mirrors what the pay path retains post-verify.
    fn seed_store(store: &Path) -> String {
        let repo = git2::Repository::init_bare(store).expect("init store");
        let readme = repo.blob(b"# delivered\n").expect("blob readme");
        let lib = repo.blob(b"pub fn delivered() {}\n").expect("blob lib");
        let mut sub = repo.treebuilder(None).expect("subtree");
        sub.insert("lib.rs", lib, 0o100644).expect("insert lib");
        let sub_oid = sub.write().expect("write subtree");
        let mut top = repo.treebuilder(None).expect("tree");
        top.insert("README.md", readme, 0o100644).expect("insert readme");
        top.insert("src", sub_oid, 0o040000).expect("insert src");
        let tree_oid = top.write().expect("write tree");
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo
            .commit(None, &sig, &sig, "delivery", &tree, &[])
            .expect("commit");
        let commit_hex = commit_oid.to_string();
        repo.reference(
            &PayPathDeliveryVerifier::store_ref_for(&commit_hex),
            commit_oid,
            true,
            "retain",
        )
        .expect("retain ref");
        commit_hex
    }

    // No accept-bind + nothing deliverable on the relay ⇒ the folded accept step refuses fail-closed
    // (NotFound: no delivered claim) BEFORE any pay, and burns zero spend. Points at an unreachable
    // relay so the fetch returns no offer/claims without external dependency.
    #[tokio::test(flavor = "current_thread")]
    async fn collect_without_bind_refuses_fail_closed_when_nothing_delivered() {
        let root = temp_root("no-delivery");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");
        home.config.relay_url = "ws://127.0.0.1:1".to_string();
        let mut gate = BudgetGate::from_home(&home).expect("gate");

        let error = collect_async(
            &home,
            Some(&mut gate),
            CollectRequest {
                accept_review_verdict: None,
                job_id: "a".repeat(64),
                out: None,
            },
        )
        .await
        .expect_err("nothing delivered must refuse");
        assert!(
            matches!(error, CollectError::Lifecycle(_)),
            "unexpected error: {error}"
        );
        assert_eq!(gate.spent(), 0, "a no-delivery refusal must not spend");
        let _ = std::fs::remove_dir_all(&root);
    }

    // `results_dest` maps to <home>/results/<job_id> by default and refuses traversal in `out`.
    #[test]
    fn results_dest_defaults_to_job_and_refuses_traversal() {
        let root = temp_root("dest");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let job = "b".repeat(64);
        assert_eq!(
            results_dest(&home, &job, None).expect("default"),
            home.root.join("results").join(&job)
        );
        assert_eq!(
            results_dest(&home, &job, Some("out-1")).expect("named"),
            home.root.join("results").join("out-1")
        );
        for bad in ["../escape", "a/b", "a\\b", ".."] {
            assert!(results_dest(&home, &job, Some(bad)).is_err(), "must refuse {bad:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // `materialize_delivery` writes the delivered tree to disk and returns the sorted file list;
    // re-running is idempotent (same files, overwritten in place — the re-collect materialize path).
    #[test]
    fn materialize_delivery_writes_files_and_is_idempotent() {
        let root = temp_root("materialize");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let commit_hex = seed_store(&store);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");

        let files = materialize_delivery(&store, &store_ref, &commit_hex, &dest).expect("materialize");
        assert_eq!(files, vec!["README.md".to_string(), "src/lib.rs".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dest.join("README.md")).expect("readme"),
            "# delivered\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("src/lib.rs")).expect("lib"),
            "pub fn delivered() {}\n"
        );

        // Idempotent re-materialize (what re-collect does after a reconciled pay): same result.
        let again = materialize_delivery(&store, &store_ref, &commit_hex, &dest).expect("re-materialize");
        assert_eq!(again, files);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Build a buyer store whose delivered tree holds `files` (slash paths, nested trees built as
    /// needed), each blob's content being its own path. Returns the commit hex.
    fn seed_paths_store(store: &Path, files: &[&str]) -> String {
        fn build(repo: &git2::Repository, files: &[(Vec<&str>, &str)]) -> git2::Oid {
            let mut top = repo.treebuilder(None).expect("tree");
            let mut subdirs: std::collections::BTreeMap<&str, Vec<(Vec<&str>, &str)>> =
                Default::default();
            for (parts, full) in files {
                if parts.len() == 1 {
                    let blob = repo.blob(full.as_bytes()).expect("blob");
                    top.insert(parts[0], blob, 0o100644).expect("insert blob");
                } else {
                    subdirs.entry(parts[0]).or_default().push((parts[1..].to_vec(), full));
                }
            }
            for (name, children) in subdirs {
                let oid = build(repo, &children);
                top.insert(name, oid, 0o040000).expect("insert subtree");
            }
            top.write().expect("write tree")
        }
        let repo = git2::Repository::init_bare(store).expect("init store");
        let split: Vec<(Vec<&str>, &str)> =
            files.iter().map(|path| (path.split('/').collect(), *path)).collect();
        let tree = repo.find_tree(build(&repo, &split)).expect("find tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo.commit(None, &sig, &sig, "delivery", &tree, &[]).expect("commit");
        let commit_hex = commit_oid.to_string();
        repo.reference(&PayPathDeliveryVerifier::store_ref_for(&commit_hex), commit_oid, true, "retain")
            .expect("retain ref");
        commit_hex
    }

    // P1 (v0.7.3-rc8 security review): a delivery carrying agent-instruction or harness-config
    // files must not land under the names a harness auto-loads. Each is written with
    // INERT_SUFFIX, bytes unchanged; ordinary files keep their names.
    #[test]
    fn materialize_delivery_writes_agent_control_files_inert() {
        let root = temp_root("agent-control");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let delivered = [
            "AGENTS.md",
            "CLAUDE.md",
            "docs/Agents.MD",
            ".cursorrules",
            ".mcp.json",
            ".envrc",
            ".claude/settings.json",
            ".vscode/tasks.json",
            ".cursor/rules/x.mdc",
            "README.md",
            "src/agents.rs",
            "my.claude/notes.txt",
        ];
        let commit_hex = seed_paths_store(&store, &delivered);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");

        let files = materialize_delivery(&store, &store_ref, &commit_hex, &dest).expect("materialize");
        let mut expected = vec![
            "AGENTS.md.delivered",
            "CLAUDE.md.delivered",
            "docs/Agents.MD.delivered",
            ".cursorrules.delivered",
            ".mcp.json.delivered",
            ".envrc.delivered",
            ".claude.delivered/settings.json",
            ".vscode.delivered/tasks.json",
            ".cursor.delivered/rules/x.mdc",
            "README.md",
            "src/agents.rs",
            "my.claude/notes.txt",
        ];
        expected.sort();
        assert_eq!(files, expected);
        for (original, written) in delivered.iter().zip(
            delivered.iter().map(|path| materialized_path(path)),
        ) {
            assert_eq!(
                std::fs::read_to_string(dest.join(&written)).expect("written file"),
                *original,
                "{original} must keep its bytes under {written}"
            );
        }
        for loaded in [
            "AGENTS.md",
            "CLAUDE.md",
            "docs/Agents.MD",
            ".cursorrules",
            ".mcp.json",
            ".envrc",
            ".claude",
            ".vscode",
            ".cursor",
        ] {
            assert!(
                std::fs::symlink_metadata(dest.join(loaded)).is_err(),
                "{loaded} must not exist under the results folder"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // P1: the inert name is checked before pay like any other written path. A seller delivering
    // both `AGENTS.md` and `AGENTS.md.delivered` names one file twice: refused, nothing written.
    #[test]
    fn a_delivered_name_that_collides_with_an_inert_rename_is_refused_before_pay() {
        let root = temp_root("inert-collision");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let commit_hex = seed_paths_store(&store, &["AGENTS.md", "agents.md.DELIVERED"]);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let error = validate_delivery_tree_in_store(&store, &store_ref, &commit_hex)
            .expect_err("the pre-pay check must refuse the collision");
        assert!(error.contains("appears twice"), "{error}");
        let dest = root.join("out");
        assert!(materialize_delivery(&store, &store_ref, &commit_hex, &dest).is_err());
        assert!(!dest.exists(), "a refused delivery writes nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn materialized_path_maps_only_agent_control_components() {
        for (delivered, written) in [
            ("README.md", "README.md"),
            ("AGENTS.md", "AGENTS.md.delivered"),
            ("a/b/CLAUDE.md", "a/b/CLAUDE.md.delivered"),
            (".claude/agents/x.md", ".claude.delivered/agents/x.md"),
            (".Claude/CLAUDE.md", ".Claude.delivered/CLAUDE.md.delivered"),
            ("AGENTS.md.delivered", "AGENTS.md.delivered"),
            ("notAGENTS.md", "notAGENTS.md"),
            ("agents", "agents"),
            // HFS+ ignores U+200C when it compares names, so this IS `.claude` there.
            (".cla\u{200C}ude/x", ".cla\u{200C}ude.delivered/x"),
        ] {
            assert_eq!(materialized_path(delivered), written, "{delivered:?}");
        }
    }

    // git2 >= 0.20 makes an aborted `Tree::walk` return `Err` itself, which used to pre-empt the
    // specific reason the callback recorded. A non-UTF-8 entry name must still refuse with THAT
    // reason, not libgit2's generic "callback returned" (worker-fable review of #1281).
    #[cfg(unix)]
    #[test]
    fn materialize_delivery_reports_a_non_utf8_entry_name_specifically() {
        use std::os::unix::ffi::OsStrExt;
        let root = temp_root("non-utf8");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let repo = git2::Repository::init_bare(&store).expect("init store");
        let blob = repo.blob(b"x\n").expect("blob");
        let mut top = repo.treebuilder(None).expect("tree");
        top.insert(std::path::Path::new(std::ffi::OsStr::from_bytes(b"bad\xff")), blob, 0o100644)
            .expect("insert non-utf8 name");
        let tree = repo.find_tree(top.write().expect("write tree")).expect("find tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo.commit(None, &sig, &sig, "delivery", &tree, &[]).expect("commit");
        let commit_hex = commit_oid.to_string();
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        repo.reference(&store_ref, commit_oid, true, "retain").expect("retain ref");

        let error = materialize_delivery(&store, &store_ref, &commit_hex, &root.join("out"))
            .expect_err("a non-UTF-8 entry name must refuse");
        assert_eq!(error, "non-UTF-8 tree entry name");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Store a delivery whose tree is written as a RAW tree object, the way a seller-hosted repo
    /// reaches the buyer store through fetch/indexer: libgit2's treebuilder refuses unsafe names,
    /// but the tree parser and the indexer accept any NUL-free name. Returns the commit hex.
    fn seed_raw_tree_store(store: &Path, names: &[&[u8]]) -> String {
        let repo = git2::Repository::init_bare(store).expect("init store");
        let blob = repo.blob(b"seller-controlled bytes\n").expect("blob");
        let mut raw = Vec::new();
        for name in names {
            raw.extend_from_slice(b"100644 ");
            raw.extend_from_slice(name);
            raw.push(0);
            raw.extend_from_slice(blob.as_bytes());
        }
        let odb = repo.odb().expect("odb");
        let tree_oid = odb.write(git2::ObjectType::Tree, &raw).expect("write raw tree");
        let tree = repo.find_tree(tree_oid).expect("find raw tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo.commit(None, &sig, &sig, "delivery", &tree, &[]).expect("commit");
        let commit_hex = commit_oid.to_string();
        repo.reference(&PayPathDeliveryVerifier::store_ref_for(&commit_hex), commit_oid, true, "retain")
            .expect("retain ref");
        commit_hex
    }

    // Security report REPORT1110 F2: a seller-authored tree entry name is a path, not a file name.
    // `..` components or an absolute name must never write outside `dest`, and the whole delivery
    // must refuse (fail-closed: no file at all, never a partial materialization).
    #[test]
    fn materialize_delivery_refuses_tree_entries_that_escape_dest() {
        let root = temp_root("escape");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let absolute = root.join("ABSOLUTE_WRITE.txt");
        let absolute_name = absolute.to_str().expect("utf8 temp path").to_owned();
        let cases: Vec<(&str, Vec<u8>, PathBuf)> = vec![
            ("dotdot", b"../../ESCAPED.txt".to_vec(), root.join("ESCAPED.txt")),
            ("absolute", absolute_name.clone().into_bytes(), absolute.clone()),
            ("dotgit", b".git/config".to_vec(), root.join("results").join("dotgit").join(".git/config")),
            ("dotgit-upper", b".GIT/config".to_vec(), root.join("results").join("dotgit-upper").join(".GIT/config")),
            // On Unix `\` is an ordinary byte, so an accepted `..\x.txt` would land as one file
            // of that name inside dest: that is where to look for the bytes.
            ("backslash", b"..\\x.txt".to_vec(), root.join("results").join("backslash").join("..\\x.txt")),
        ];
        for (label, bad, escaped) in cases {
            let store = root.join(format!("store-{label}"));
            // `ok.txt` sorts after the bad entry for every case: a walker that writes before it
            // validates would leave a partial tree behind.
            let commit_hex = seed_raw_tree_store(&store, &[&bad, b"ok.txt"]);
            let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
            let dest = root.join("results").join(label);
            let result = materialize_delivery(&store, &store_ref, &commit_hex, &dest);
            assert!(!escaped.exists(), "{label}: wrote outside dest at {}", escaped.display());
            let error = result.expect_err(&format!("{label}: an unsafe entry name must refuse"));
            // A raw entry name holding `/` is now refused as such, before its components are read.
            assert!(error.contains("refusing the delivery"), "{label}: unexpected error {error}");
            assert!(!dest.join("ok.txt").exists(), "{label}: partial materialization");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // F2 hardening: a symlink already sitting in `dest` (e.g. planted by an earlier collect into the
    // same named `out` folder) must not redirect a delivered file's write.
    #[cfg(unix)]
    #[test]
    fn materialize_delivery_does_not_follow_a_symlink_in_dest() {
        let root = temp_root("symlink");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let commit_hex = seed_store(&store);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");
        let outside = root.join("outside");
        std::fs::create_dir_all(&dest).expect("dest");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("README.md"), "victim\n").expect("victim");
        std::os::unix::fs::symlink(outside.join("README.md"), dest.join("README.md")).expect("file link");
        std::os::unix::fs::symlink(&outside, dest.join("src")).expect("dir link");
        let error = materialize_delivery(&store, &store_ref, &commit_hex, &dest)
            .expect_err("a symlink in dest must refuse");
        assert!(error.contains("symlink"), "unexpected error {error}");
        assert_eq!(std::fs::read_to_string(outside.join("README.md")).expect("victim"), "victim\n");
        assert!(!outside.join("lib.rs").exists(), "wrote through a directory symlink");
        let _ = std::fs::remove_dir_all(&root);
    }

    // #1290 review (Medium): every fixture above puts the bad name FIRST, so the per-entry check
    // alone would also pass them. Here a safe name comes first: only the whole-tree pre-pass keeps
    // it off disk, so dropping that pre-pass turns this red.
    #[test]
    fn materialize_delivery_writes_nothing_when_a_later_entry_is_unsafe() {
        let root = temp_root("late-unsafe");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let store = root.join("store");
        let commit_hex = seed_raw_tree_store(&store, &[b"0-ok.txt", b"../../LATE_ESCAPE.txt"]);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");
        let error = materialize_delivery(&store, &store_ref, &commit_hex, &dest)
            .expect_err("a later unsafe entry must refuse the whole tree");
        assert!(error.contains("refusing the delivery"), "unexpected error {error}");
        assert!(!dest.join("0-ok.txt").exists(), "a safe entry was written before the refusal");
        assert!(!dest.exists(), "dest must not be created for a refused tree");
        assert!(!root.join("LATE_ESCAPE.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn materialize_delivery_refuses_a_duplicate_entry_before_writing() {
        let root = temp_root("dup");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let commit_hex = seed_raw_tree_store(&store, &[b"dup.txt", b"dup.txt"]);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");
        let error = materialize_delivery(&store, &store_ref, &commit_hex, &dest)
            .expect_err("a repeated name must refuse");
        assert!(error.contains("appears twice"), "unexpected error {error}");
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    // #1290 review (Medium): the earlier symlink test aborts on README.md before `src/` is ever
    // used. This tree has ONLY `src/lib.rs`, so the intermediate-directory check is what refuses.
    #[cfg(unix)]
    #[test]
    fn materialize_delivery_does_not_follow_a_symlinked_intermediate_directory() {
        let root = temp_root("dir-symlink");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let repo = git2::Repository::init_bare(&store).expect("init store");
        let lib = repo.blob(b"pub fn delivered() {}\n").expect("blob");
        let mut sub = repo.treebuilder(None).expect("subtree");
        sub.insert("lib.rs", lib, 0o100644).expect("insert lib");
        let sub_oid = sub.write().expect("write subtree");
        let mut top = repo.treebuilder(None).expect("tree");
        top.insert("src", sub_oid, 0o040000).expect("insert src");
        let tree = repo.find_tree(top.write().expect("write tree")).expect("tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo.commit(None, &sig, &sig, "delivery", &tree, &[]).expect("commit");
        let commit_hex = commit_oid.to_string();
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        repo.reference(&store_ref, commit_oid, true, "retain").expect("retain ref");
        let dest = root.join("results").join("job");
        let outside = root.join("outside");
        std::fs::create_dir_all(&dest).expect("dest");
        std::fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, dest.join("src")).expect("dir link");
        let error = materialize_delivery(&store, &store_ref, &commit_hex, &dest)
            .expect_err("a symlinked directory in dest must refuse");
        assert!(error.contains("symlink"), "unexpected error {error}");
        assert!(!outside.join("lib.rs").exists(), "wrote through a directory symlink");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delivery_paths_that_alias_dotgit_or_dotdot_on_other_filesystems_are_refused() {
        let long = "a".repeat(256);
        let too_long = "a/".repeat(2100);
        for bad in [
            ".git.", ".git ", "x/.git./config", "GIT~1", "git~1/config", "...", ".. ", "a/b.",
            ".gi\u{200C}t/config", "\u{FEFF}.git", "x/.\u{200D}./y", "\u{200E}", long.as_str(),
            "a:b", "a\u{7}b", too_long.as_str(), ".GiT/x",
        ] {
            assert!(validate_delivery_path(bad).is_err(), "{bad:?} must be refused");
        }
        let longest_ok = "a".repeat(255);
        for good in [
            "file.txt", "src/lib.rs", "v1.2.3/notes.md", "日本語.txt", ".gitignore", ".github/x.yml",
            "git~2", "a\u{200C}b", longest_ok.as_str(),
        ] {
            assert!(validate_delivery_path(good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_inline_delivery_does_not_follow_a_symlink() {
        let root = temp_root("inline-symlink");
        let _ = std::fs::remove_dir_all(&root);
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("victim"), "victim\n").expect("victim");
        // A symlink at dest/answer.txt.
        let dest = root.join("results").join("job");
        std::fs::create_dir_all(&dest).expect("dest");
        std::os::unix::fs::symlink(outside.join("victim"), dest.join(INLINE_ANSWER_FILENAME))
            .expect("file link");
        let error = materialize_inline_delivery("seller text", &dest)
            .expect_err("a symlinked answer.txt must refuse");
        assert!(error.contains("symlink"), "unexpected error {error}");
        assert_eq!(std::fs::read_to_string(outside.join("victim")).expect("victim"), "victim\n");
        // dest itself a symlink.
        let linked = root.join("results").join("linked");
        std::os::unix::fs::symlink(&outside, &linked).expect("dir link");
        let error = materialize_inline_delivery("seller text", &linked)
            .expect_err("a symlinked dest must refuse");
        assert!(error.contains("symlink"), "unexpected error {error}");
        assert!(!outside.join(INLINE_ANSWER_FILENAME).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn raw_tree(repo: &git2::Repository, entries: &[(&str, &[u8], git2::Oid)]) -> git2::Oid {
        let mut raw = Vec::new();
        for (mode, name, oid) in entries {
            raw.extend_from_slice(mode.as_bytes());
            raw.push(b' ');
            raw.extend_from_slice(name);
            raw.push(0);
            raw.extend_from_slice(oid.as_bytes());
        }
        repo.odb().expect("odb").write(git2::ObjectType::Tree, &raw).expect("write raw tree")
    }

    fn retain_tree(repo: &git2::Repository, tree_oid: git2::Oid) -> String {
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let sig = git2::Signature::now("t", "t@e").expect("sig");
        let commit_oid = repo.commit(None, &sig, &sig, "delivery", &tree, &[]).expect("commit");
        let commit_hex = commit_oid.to_string();
        repo.reference(&PayPathDeliveryVerifier::store_ref_for(&commit_hex), commit_oid, true, "retain")
            .expect("retain ref");
        commit_hex
    }

    // #1290 fable M1: anything materialize would refuse, the pre-pay check refuses too, so a paid
    // collect never ends in a refused (partial) write.
    #[test]
    fn prepay_check_and_materialize_agree() {
        let root = temp_root("agree");
        let _ = std::fs::remove_dir_all(&root);
        let check = |label: &str, names: &[&[u8]]| {
            let store = root.join(format!("store-{label}"));
            let commit_hex = seed_raw_tree_store(&store, names);
            let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
            let dest = root.join("results").join(label);
            let prepay = validate_delivery_tree_in_store(&store, &store_ref, &commit_hex);
            let written = materialize_delivery(&store, &store_ref, &commit_hex, &dest);
            assert!(prepay.is_err(), "{label}: the pre-pay check must refuse");
            assert_eq!(prepay.is_ok(), written.is_ok(), "{label}: pay path and materialize disagree");
            assert!(!dest.exists(), "{label}: partial tree");
        };
        check("slash-collision", &[b"0-first.txt", b"a", b"a/b"]);
        check("name-max", &[b"0-first.txt", &[b'x'; 300]]);
        check("case-collision", &[b"0-first.txt", b"README.md", b"readme.md"]);
        check("long-path", &[b"0-first.txt", "a/".repeat(260).as_bytes()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_macos_would_store_as_one_file_collide() {
        for (a, b) in [
            ("caf\u{e9}", "cafe\u{301}"),
            ("\u{17f}rc", "src"),
            ("\u{212a}ey", "key"),
            ("a\u{200c}b", "ab"),
            ("Dir/X.md", "dir/x.md"),
            ("\u{1e9e}", "\u{df}"),
            ("\u{1e9e}x", "ssx"),
        ] {
            assert_eq!(collision_key(a), collision_key(b), "{a:?} vs {b:?}");
        }
        assert_ne!(collision_key("a.txt"), collision_key("b.txt"));
        // A blob `café` (NFC) beside a subtree `café` (NFD) is refused before pay.
        let root = temp_root("nfd");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let repo = git2::Repository::init_bare(&store).expect("init store");
        let blob = repo.blob(b"x\n").expect("blob");
        let sub = raw_tree(&repo, &[("100644", b"f", blob)]);
        let commit = retain_tree(
            &repo,
            raw_tree(&repo, &[("100644", "caf\u{e9}".as_bytes(), blob), ("40000", "cafe\u{301}".as_bytes(), sub)]),
        );
        let error = validate_delivery_tree_in_store(&store, &PayPathDeliveryVerifier::store_ref_for(&commit), &commit)
            .expect_err("NFC/NFD pair must refuse");
        assert!(error.contains("appears twice"), "got: {error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_results_folder_too_long_for_the_longest_delivery_is_refused_before_pay() {
        let root = temp_root("long-dest");
        let ok = root.join("results").join("a".repeat(64));
        assert!(check_dest(&ok).is_ok(), "an ordinary home fits");
        let deep = root.join("x".repeat(200)).join("y".repeat(200)).join("results").join("a".repeat(64));
        let error = check_dest(&deep).expect_err("no room left for the longest delivery");
        assert!(error.contains("nothing was paid"), "got: {error}");
        assert!(validate_delivery_path(&"a".repeat(513)).is_err(), "over the path bound");
        // fable r3 L3: legal components, so only the 512-byte bound can refuse it.
        let at = |tail: usize| format!("{}/{}", vec!["a".repeat(200); 2].join("/"), "b".repeat(tail));
        assert_eq!(at(110).len(), 512);
        validate_delivery_path(&at(110)).expect("512 bytes is the bound");
        assert!(validate_delivery_path(&at(111)).is_err(), "513 bytes must refuse");
        assert!(validate_delivery_path(&"a/".repeat(200)[..399]).is_ok(), "a 399-byte path is fine");
    }

    #[test]
    fn a_tree_naming_a_missing_or_non_blob_object_is_refused_before_pay() {
        let root = temp_root("missing-blob");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let repo = git2::Repository::init_bare(&store).expect("init store");
        let present = repo.blob(b"x\n").expect("blob");
        let absent = git2::Oid::from_bytes(&[0xab; 20]).expect("oid");
        let missing = retain_tree(
            &repo,
            raw_tree(&repo, &[("100644", b"0-first.txt", present), ("100644", b"gone.txt", absent)]),
        );
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&missing);
        let error = validate_delivery_tree_in_store(&store, &store_ref, &missing).expect_err("missing blob");
        assert!(error.contains("missing from the buyer store"), "got: {error}");
        let dest = root.join("results").join("job");
        assert!(materialize_delivery(&store, &store_ref, &missing, &dest).is_err());
        assert!(!dest.exists(), "nothing may be written");
        let leaf = raw_tree(&repo, &[("100644", b"f", present)]);
        let not_blob = retain_tree(&repo, raw_tree(&repo, &[("100644", b"x", leaf)]));
        let error = validate_delivery_tree_in_store(
            &store,
            &PayPathDeliveryVerifier::store_ref_for(&not_blob),
            &not_blob,
        )
        .expect_err("a blob-mode entry naming a tree");
        assert!(error.contains("not a file"), "got: {error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deep_and_exponential_trees_are_refused_quickly() {
        let root = temp_root("bomb");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let repo = git2::Repository::init_bare(&store).expect("init store");
        let blob = repo.blob(b"x\n").expect("blob");
        let mut deep = raw_tree(&repo, &[("100644", b"f", blob)]);
        for _ in 0..70 {
            deep = raw_tree(&repo, &[("40000", b"d", deep)]);
        }
        let deep = retain_tree(&repo, deep);
        let error = validate_delivery_tree_in_store(&store, &PayPathDeliveryVerifier::store_ref_for(&deep), &deep)
            .expect_err("70 levels must refuse");
        assert!(error.contains("nested deeper"), "got: {error}");
        // 23 objects, 2^20 paths: the entry cap stops the walk long before it finishes.
        let mut bomb = raw_tree(&repo, &[("100644", b"f", blob)]);
        for _ in 0..20 {
            bomb = raw_tree(&repo, &[("40000", b"a", bomb), ("40000", b"b", bomb)]);
        }
        let bomb = retain_tree(&repo, bomb);
        let started = std::time::Instant::now();
        let error = validate_delivery_tree_in_store(&store, &PayPathDeliveryVerifier::store_ref_for(&bomb), &bomb)
            .expect_err("a 2^20-path tree must refuse");
        assert!(error.contains("more than"), "got: {error}");
        // The entry-cap message above proves the walk was cut short; the clock only catches a
        // pathological regression, with room for slow unoptimised builds (fable r3 L2).
        assert!(started.elapsed() < std::time::Duration::from_secs(120), "took {:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_delivery_replaces_a_hard_link_in_dest_instead_of_writing_through_it() {
        let root = temp_root("hardlink");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let commit_hex = seed_store(&store);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");
        let outside = root.join("outside");
        std::fs::create_dir_all(&dest).expect("dest");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("victim"), "victim\n").expect("victim");
        std::fs::hard_link(outside.join("victim"), dest.join("README.md")).expect("hard link");
        materialize_delivery(&store, &store_ref, &commit_hex, &dest).expect("materializes");
        assert_eq!(std::fs::read_to_string(outside.join("victim")).expect("victim"), "victim\n");
        assert_eq!(std::fs::read_to_string(dest.join("README.md")).expect("readme"), "# delivered\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_delivery_does_not_block_on_a_fifo_in_dest() {
        use std::os::unix::ffi::OsStrExt as _;
        let root = temp_root("fifo");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let commit_hex = seed_store(&store);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");
        std::fs::create_dir_all(&dest).expect("dest");
        let fifo = dest.join("README.md");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("c path");
        // SAFETY: `mkfifo` reads a NUL-terminated path and a mode, and touches no memory of ours.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
        let (tx, rx) = std::sync::mpsc::channel();
        let (s, r, c, d) = (store.clone(), store_ref.clone(), commit_hex.clone(), dest.clone());
        std::thread::spawn(move || {
            let _ = tx.send(materialize_delivery(&s, &r, &c, &d));
        });
        rx.recv_timeout(std::time::Duration::from_secs(20))
            .expect("materialize must not block on a FIFO")
            .expect("materializes");
        assert_eq!(std::fs::read_to_string(&fifo).expect("readme"), "# delivered\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_first_collect_is_staged_and_leaves_no_temporary_entries() {
        let root = temp_root("staged");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let commit_hex = seed_store(&store);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&commit_hex);
        let dest = root.join("results").join("job");
        materialize_delivery(&store, &store_ref, &commit_hex, &dest).expect("materializes");
        materialize_delivery(&store, &store_ref, &commit_hex, &dest).expect("re-materializes");
        for dir in [root.join("results"), dest.clone(), dest.join("src")] {
            for entry in std::fs::read_dir(&dir).expect("read dir") {
                let name = entry.expect("entry").file_name();
                assert!(!name.to_string_lossy().starts_with(TMP_PREFIX), "left behind {name:?} in {}", dir.display());
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_results_folder_is_refused_before_pay() {
        let root = temp_root("dest-check");
        let _ = std::fs::remove_dir_all(&root);
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::fs::create_dir_all(root.join("results")).expect("results");
        let dest = root.join("results").join("job");
        std::os::unix::fs::symlink(&elsewhere, &dest).expect("link");
        let error = check_dest(&dest).expect_err("a symlinked results folder must refuse");
        assert!(error.contains("symlink") && error.contains("nothing was paid"), "got: {error}");
        assert!(check_dest(&root.join("results").join("absent")).is_ok());
        assert!(check_dest(&elsewhere).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refused_path_is_echoed_truncated_and_temp_names_are_reserved() {
        let long = "a".repeat(70_000);
        let error = validate_delivery_path(&long).expect_err("too long");
        assert!(error.len() < 400, "echoed {} bytes", error.len());
        assert!(error.contains("70000 bytes"), "got: {error}");
        assert!(validate_delivery_path(".maxplayer-tmp-stage-1-1/x").is_err());
        assert!(validate_delivery_path(&"a/".repeat(600)).is_err(), "over the path bound");
    }

    // A bind that pins a commit absent from the store fails closed with a precise reason (never a
    // partial/empty "success"). Guards the materialize half of collect.
    #[test]
    fn materialize_delivery_refuses_missing_commit() {
        let root = temp_root("missing");
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        git2::Repository::init_bare(&store).expect("init store");
        let missing = "a".repeat(40);
        let store_ref = PayPathDeliveryVerifier::store_ref_for(&missing);
        let error = materialize_delivery(&store, &store_ref, &missing, &root.join("out"))
            .expect_err("missing commit must refuse");
        assert!(error.contains("not found in buyer store"), "unexpected: {error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// §6.4 — an inline delivery materializes from the bind alone: no store, no ref, no checkout.
    #[test]
    fn an_inline_delivery_materializes_its_answer_to_a_file() {
        let dir = std::env::temp_dir().join(format!("mxp-inline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = materialize_inline_delivery("Europe/Zagreb", &dir).expect("materializes");
        assert_eq!(files, vec![INLINE_ANSWER_FILENAME.to_owned()]);
        assert_eq!(
            std::fs::read_to_string(dir.join(INLINE_ANSWER_FILENAME)).expect("reads"),
            "Europe/Zagreb"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bind written before inline delivery existed carries no kind. Reading it as a GIT delivery
    /// is the fail-closed direction and is true of every such bind.
    #[test]
    fn a_bind_with_no_delivery_kind_is_read_as_git() {
        let mut bind = bind_for(&"aa".repeat(32), &"bb".repeat(32), &"cc".repeat(20));
        assert!(!bind_is_inline(&bind), "absent kind must not read as inline");
        bind.delivery_kind = Some("inline".to_owned());
        assert!(bind_is_inline(&bind));
        bind.delivery_kind = Some("fork".to_owned());
        assert!(!bind_is_inline(&bind));
    }

    // Helper: a from-scratch accept-bind pinning `commit_oid` (used by refuse-path tests).
    fn bind_for(job_id: &str, seller_hex: &str, commit_oid: &str) -> AcceptedBind {
        AcceptedBind {
            followup_id: None,
            private_evidence: None,
            delivery_kind: None,
            inline_answer: None,
            payment_mode: crate::gateway::PaymentMode::Sat,
            job_id: job_id.to_owned(),
            claim_id: "c".repeat(64),
            result_id: "d".repeat(64),
            seller_pubkey: seller_hex.to_owned(),
            commit_oid: commit_oid.to_owned(),
            repo: "https://example.invalid/repo.git".into(),
            branch: "main".into(),
            job_hash: "e".repeat(64),
            amount_sats: 2,
            accept_event_id: "f".repeat(64),
            accepted_at: 1,
            seller_signature: String::new(),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
        }
    }

    // Belt-and-suspenders money gate: a bind carrying a FORGED seller co-signature (a real schnorr
    // sig by a non-seller key) makes the sealed pay path refuse at the pre-pay cosig tooth — BEFORE
    // any spend and BEFORE the wallet opens. collect must surface that refusal, spend ZERO, and
    // materialize NO files. Red-on-revert: rewiring collect to materialize regardless of the pay
    // outcome would leave files on disk here.
    #[tokio::test(flavor = "current_thread")]
    async fn collect_forged_cosig_blocks_pay_and_materialize_zero_spend() {
        use nostr_sdk::secp256k1::Message;
        use nostr_sdk::Keys;

        let root = temp_root("forged-cosig");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let seller_hex = home::public_key_hex(&home).expect("pubkey");

        // Seed the store so the ONLY thing stopping materialize is the pay refusal (not a missing
        // object) — proving collect gates materialize on the pay outcome.
        let commit_hex = seed_store(&delivery_store_path(&home));

        // A real schnorr signature over the honest receipt-preimage digest, but by an unrelated key.
        // We do not need the exact preimage bytes: any signature not from the seller anchor is
        // refused by the pre-pay cosig tooth.
        let attacker = Keys::generate();
        let forged = attacker
            .sign_schnorr(&Message::from_digest([0x11u8; 32]))
            .to_string();
        let mut bind = bind_for(&"a".repeat(64), &seller_hex, &commit_hex);
        bind.seller_signature = forged;
        write_bind(&home, &bind);

        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let error = collect_async(
            &home,
            Some(&mut gate),
            CollectRequest {
                accept_review_verdict: None,
                job_id: bind.job_id.clone(),
                out: None,
            },
        )
        .await
        .expect_err("forged cosig must block collect");
        assert!(matches!(error, CollectError::Pay(_)), "must be a pay refusal: {error}");
        assert_eq!(gate.spent(), 0, "a pay refusal must burn zero spend");
        assert_eq!(
            BudgetGate::from_home(&home).expect("reload").spent(),
            0,
            "durable spent must stay 0"
        );
        assert!(
            !home.root.join("payment-journal").exists(),
            "no payment journal may be created on a pre-pay refusal"
        );
        assert!(
            !home.root.join("results").join(&bind.job_id).exists(),
            "collect must NOT materialize files when the pay refuses"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // FOLD + red-on-revert: with NO prior accept-bind, collect performs the accept step ITSELF
    // (fetch the delivered result from the relay, record the co-signed pay-bind), then the sealed
    // pay path refuses at the pre-pay cosig tooth because the result carries a FORGED seller
    // signature. Proves the folded one-call path still gates spend + materialize on the money
    // checks: ZERO spend, NO journal, NO files — AND that collect DID create the accept-bind (so
    // reverting the fold, which would refuse with no bind written, turns this red).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_folds_accept_then_refuses_forged_cosig_zero_spend() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::secp256k1::Message;
        use nostr_sdk::prelude::{Client, Keys};

        let root = temp_root("fold-forged");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("home");

        // In-process NIP-01 relay; point the buyer home at it.
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        home.config.relay_url = relay_url.clone();
        // Isolate the existing payment-signature refusal from reviewer availability.
        home.config.review.buyer_delivery = false;

        let buyer =
            Keys::parse(&home::read_secret_key_hex(&home).expect("buyer secret")).expect("buyer keys");
        let buyer_hex = buyer.public_key().to_hex();
        let seller = Keys::generate();
        let seller_hex = seller.public_key().to_hex();
        let attacker = Keys::generate();

        // Seed the buyer store so the ONLY thing stopping materialize is the pay refusal.
        let commit_hex = seed_store(&delivery_store_path(&home));

        let amount = 2u64;
        let task = "do a task";
        let output = "text";
        let deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + 3_600;

        // One publisher client publishes the buyer offer + seller claim/result (signed per role).
        let net = Client::new(Keys::generate());
        net.add_relay(&relay_url).await.expect("add relay");
        net.connect().await;
        net.wait_for_connection(std::time::Duration::from_secs(5)).await;
        let publish = |keys: &Keys, draft: &crate::gateway::EventDraft| {
            let builder = crate::gateway::nostr::event_builder(draft).expect("event builder");
            let event = builder.sign_with_keys(keys).expect("sign");
            let id = event.id;
            let net = net.clone();
            async move {
                net.send_event(&event).await.expect("publish");
                id
            }
        };

        let offer_draft =
            crate::gateway::OfferDraft::new(task, output, amount, deadline, &seller_hex)
                .to_event_draft();
        let offer_id = publish(&buyer, &offer_draft).await.to_hex();

        let creq = crate::gateway::creq::build_seller_creq(
            &offer_id,
            amount,
            "sat",
            &[crate::home::DEFAULT_MINT_URL.to_string()],
            &seller_hex,
        )
        .expect("creq");
        let claim_draft = crate::gateway::claim_draft(&offer_id, &buyer_hex, &seller_hex, crate::gateway::ClaimPayment::Sat(&creq), &[], &Default::default());
        let claim_id = publish(&seller, &claim_draft).await.to_hex();

        // The buyer AWARDS the seller's claim (kind-3405). collect resolves the delivery against this
        // durable award, not the live-claim set (#540); without it accept-for-collect refuses at "no
        // award" before reaching the pre-pay cosig tooth this test exercises.
        let award_draft = crate::gateway::award_draft(&offer_id, &claim_id, &buyer_hex, &seller_hex);
        let _ = publish(&buyer, &award_draft).await;

        let job_hash = crate::job_lifecycle::job_hash_for_offer(&offer_id, task, amount);
        // A real schnorr signature by an unrelated key — refused at the pre-pay cosig tooth.
        let forged = attacker
            .sign_schnorr(&Message::from_digest([0x11u8; 32]))
            .to_string();
        let git = crate::gateway::GitResultTags {
            repo: "https://example.invalid/repo.git",
            branch: "main",
            commit_sha: &commit_hex,
        };
        let result_draft = crate::gateway::result_draft(
            &offer_id, &buyer_hex, output, amount, &job_hash, &forged, "", Some(git), &[],
        );
        let _ = publish(&seller, &result_draft).await;

        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let error = collect_async(
            &home,
            Some(&mut gate),
            CollectRequest {
                accept_review_verdict: None,
                job_id: offer_id.clone(),
                out: None,
            },
        )
        .await
        .expect_err("forged cosig must block collect");

        assert!(matches!(error, CollectError::Pay(_)), "must be a pay refusal: {error}");
        assert_eq!(gate.spent(), 0, "a pay refusal must burn zero spend");
        assert_eq!(
            BudgetGate::from_home(&home).expect("reload").spent(),
            0,
            "durable spent must stay 0"
        );
        // The fold created the accept-bind before the pay refusal (red-on-revert anchor).
        assert!(
            home.root.join("jobs").join(format!("{offer_id}.json")).exists(),
            "collect must have recorded the accept-bind itself (fold)"
        );
        assert!(
            !home.root.join("payment-journal").exists(),
            "no payment journal on a pre-pay refusal"
        );
        assert!(
            !home.root.join("results").join(&offer_id).exists(),
            "collect must NOT materialize files when the pay refuses"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    fn write_bind(home: &MaxplayerHome, bind: &AcceptedBind) {
        let jobs = home.root.join("jobs");
        std::fs::create_dir_all(&jobs).expect("jobs dir");
        std::fs::write(
            jobs.join(format!("{}.json", bind.job_id)),
            serde_json::to_string(bind).expect("serialize bind"),
        )
        .expect("write bind");
    }
}
