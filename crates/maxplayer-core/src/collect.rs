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
    // 1. Load the buyer's accept-bind — the delivered commit + pay terms it recorded at accept.
    // Never caller input, so collect always settles the accepted, tip-matched delivery. When no bind
    // exists yet, run the accept step ITSELF (fetch the delivered result, record the co-signed
    // pay-bind through the SAME accept path) so collect is a true one-call. Re-collect loads the
    // existing bind and never re-accepts.
    let bind = match job_lifecycle::load_accepted_bind(home, &request.job_id)
        .map_err(CollectError::Lifecycle)?
    {
        Some(bind) => bind,
        None => job_lifecycle::accept_for_collect_async(home, &request.job_id)
            .await
            .map_err(CollectError::Lifecycle)?,
    };

    // Resolve + validate the destination BEFORE spending, so a bad `out` name never pays then fails.
    let dest = results_dest(home, &request.job_id, request.out.as_deref())
        .map_err(CollectError::Input)?;

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
        let files = collect_free(home, &bind, &dest)?;
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
    let store = delivery_store_path(home);
    let store_ref = PayPathDeliveryVerifier::store_ref_for(&bind.commit_oid);
    let files = materialize_delivery(&store, &store_ref, &bind.commit_oid, &dest)
        .map_err(CollectError::Materialize)?;

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
    let path = free_collect_record_path(home, &bind.job_id);
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

    std::fs::create_dir_all(dest).map_err(|error| error.to_string())?;
    let mut files: Vec<String> = Vec::new();
    let mut walk_err: Option<String> = None;
    tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return git2::TreeWalkResult::Ok;
        }
        let name = match entry.name() {
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
        let path = dest.join(&rel);
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                walk_err = Some(error.to_string());
                return git2::TreeWalkResult::Abort;
            }
        }
        if let Err(error) = std::fs::write(&path, blob.content()) {
            walk_err = Some(error.to_string());
            return git2::TreeWalkResult::Abort;
        }
        files.push(rel);
        git2::TreeWalkResult::Ok
    })
    .map_err(|error| error.to_string())?;
    if let Some(error) = walk_err {
        return Err(format!("failed: {error}"));
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home;
    use crate::job_lifecycle::AcceptedBind;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

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

    // Helper: a from-scratch accept-bind pinning `commit_oid` (used by refuse-path tests).
    fn bind_for(job_id: &str, seller_hex: &str, commit_oid: &str) -> AcceptedBind {
        AcceptedBind {
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
