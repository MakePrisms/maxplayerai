//! The persistent per-home **mobee buyer** (step 1 of the stateful-buyer design, #127).
//!
//! One daemon owns a home. It takes an exclusive OS lock on `$MOBEE_HOME/buyer.lock`
//! (a second daemon on the same home fails closed), opens the CDK wallet and the
//! Nostr identity behind serialized in-process actors, opens the durable state DB
//! `$MOBEE_HOME/buyer.sqlite`, and serves a small JSON-RPC surface over the
//! user-only Unix socket `$MOBEE_HOME/buyer.sock`. Every other process is a thin,
//! stateless [`client`] over that socket.
//!
//! This module is deliberately the *shell*: the boundary that makes financial
//! authority singular and durable. The reservation ledger, auto-award, lifecycle
//! engine, and crash-safe payment saga are later phases that build on this state
//! home.
//!
//! This is the **buyer** daemon. If a seller daemon is ever built, a shared
//! buyer/seller core can be extracted then — do not generalize preemptively. The
//! structure (this module, the `wallet` feature flag) is under reassessment in
//! issue #133.
//!
//! Concurrency is 1: the queue behind each actor — not SQLite locking — is the
//! in-process concurrency boundary, mirroring the home lock across processes.

pub mod client;
pub mod lifecycle;
pub mod lock;
pub mod protocol;
pub mod relay;
pub mod reservations;
pub mod signer;
pub mod store;
pub mod wallet_actor;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::budget::BudgetGate;
use crate::buyer_fund::{self, FundError};
use crate::collect::{self, CollectRequest};
use crate::home::{self, HomeError, MobeeHome};
use crate::job_lifecycle::{
    self, AwardClaimRequest, ContributionSpec, GetJobRequest, JobKind, PostJobRequest, WaitFor,
};
use crate::payment::{PaymentMachine, PaymentRecord, PaymentState};
use lifecycle::{AwardError, AwardFilters, PaymentProgress, RearmAction, SettleError};
use lock::{HomeLock, LockError};
use protocol::{CODE_INTERNAL, CODE_METHOD_NOT_FOUND, CODE_NOT_IMPLEMENTED, Request, Response};
use reservations::{Dispositions, ReconcileReport};
use signer::SignerHandle;
use store::{BuyerStore, StoreError};
use wallet_actor::WalletHandle;

/// A recognized trade method was refused by its money guard (reservation refused, budget refused).
pub const CODE_REFUSED: i64 = -32002;
/// Timeout for the daemon's relay fetches (job view / auto-award selection / reconcile liveness).
const RELAY_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the background auto-award task re-checks the relay for a payable claim, until one
/// appears or the offer deadline passes. Bounded polling (no tight spin on a live-but-unpayable claim).
const AUTO_AWARD_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How often the delivery watcher re-checks awarded-unsettled jobs with no event to wake it. The
/// subscription is the fast path; this backstop is what makes a dropped or missed result event a
/// latency cost rather than a stranded payment.
const DELIVERY_RECHECK_INTERVAL: Duration = Duration::from_secs(60);
/// How often the reservation reconcile re-runs while the daemon serves. This is what frees a
/// reservation stranded by a seller that simply stopped, without waiting for a restart.
///
/// Deliberately SLOW: a pass makes one relay fetch per still-reserved job, which is the unbounded
/// per-job fetch pattern tracked in #180. Until that moves onto the persistent relay session, the
/// cadence is the thing keeping the load down — raise it only together with that fix.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(600);

/// Lock file leaf under the home.
pub const LOCK_FILE: &str = "buyer.lock";
/// State DB leaf under the home.
pub const STATE_DB_FILE: &str = "buyer.sqlite";
/// Socket leaf under the home.
pub const SOCKET_FILE: &str = "buyer.sock";

/// Buyer startup / run failure.
#[derive(Debug)]
pub enum BuyerError {
    Lock(LockError),
    Store(StoreError),
    Wallet(FundError),
    Identity(HomeError),
    /// The relay could not be REGISTERED — a malformed url or a pool refusal. An unreachable relay
    /// is not this: the daemon comes up and serves with the network down.
    Relay(String),
    Io(String),
}

impl std::fmt::Display for BuyerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Wallet(error) => write!(formatter, "buyer wallet error: {error}"),
            Self::Identity(error) => write!(formatter, "buyer identity error: {error}"),
            Self::Relay(message) => write!(formatter, "buyer relay error: {message}"),
            Self::Io(message) => write!(formatter, "buyer io error: {message}"),
        }
    }
}

impl std::error::Error for BuyerError {}

impl From<LockError> for BuyerError {
    fn from(value: LockError) -> Self {
        Self::Lock(value)
    }
}
impl From<StoreError> for BuyerError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<FundError> for BuyerError {
    fn from(value: FundError) -> Self {
        Self::Wallet(value)
    }
}
impl From<HomeError> for BuyerError {
    fn from(value: HomeError) -> Self {
        Self::Identity(value)
    }
}

/// Shared, immutable-after-startup handles the connection handlers reach into.
struct BuyerContext {
    home: MobeeHome,
    store: BuyerStore,
    wallet: WalletHandle,
    signer: SignerHandle,
    /// The buyer's one long-lived relay session. Writes and (from the delivery watcher on)
    /// subscriptions ride this instead of a fresh `Client` per operation.
    relay: relay::RelayHandle,
    started_at_unix: i64,
    /// Serializes the money-state-mutating RPCs (`award` reserves, `collect` flips) so a
    /// reservation's balance/spent snapshot is never read while a concurrent collect is melting.
    /// The wallet actor's balance reads run independently (reads never race a serialized send).
    money_lock: Mutex<()>,
    /// The last reconcile-on-start report, surfaced in `status` so kept-uncertain reservations
    /// (funds committed to an ambiguous payment) are visible rather than silently discarded.
    last_reconcile: Mutex<Option<ReconcileReport>>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bring up the buyer's owned resources: take the exclusive lock, open the state
/// DB and record the start, then open the wallet and identity behind their
/// serialized actors. Returns the held lock (keep it alive for the buyer's life),
/// the shared context, and the socket path to bind.
///
/// Fails closed at the lock step if another daemon already owns this home.
async fn bootstrap(home: MobeeHome) -> Result<(HomeLock, Arc<BuyerContext>, PathBuf), BuyerError> {
    let lock = HomeLock::acquire(home.root.join(LOCK_FILE))?;

    let store = BuyerStore::open(home.root.join(STATE_DB_FILE))?;
    let started_at_unix = now_unix();
    store.record_start(started_at_unix)?;

    // The daemon is the ONLY opener of the CDK wallet — this is what the exclusive
    // home lock protects. Opening touches the local sqlite store only (no network).
    let wallet = buyer_fund::open_wallet_async(&home).await?;
    let wallet = wallet_actor::spawn(wallet);

    let signer = signer::spawn(&home)?;

    // Registers the relay and hands the session to the actor; it does NOT wait for the socket or
    // the NIP-42 handshake, so an unreachable relay cannot delay the daemon past connect-or-spawn's
    // readiness deadline (see `relay::spawn`).
    let relay_keys = buyer_keys(&home).map_err(BuyerError::Relay)?;
    let relay = relay::spawn(relay_keys, &home.config.relay_url)
        .await
        .map_err(|error| BuyerError::Relay(error.to_string()))?;

    let socket_path = home.root.join(SOCKET_FILE);
    let context = Arc::new(BuyerContext {
        home,
        store,
        wallet,
        signer,
        relay,
        started_at_unix,
        money_lock: Mutex::new(()),
        last_reconcile: Mutex::new(None),
    });
    Ok((lock, context, socket_path))
}

/// Bind the user-only Unix socket, replacing a stale socket file left by a prior
/// run (safe: we already hold the exclusive lock, so no live daemon owns it).
fn bind_socket(path: &std::path::Path) -> Result<UnixListener, BuyerError> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| BuyerError::Io(error.to_string()))?;
    }
    let listener = UnixListener::bind(path).map_err(|error| BuyerError::Io(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| BuyerError::Io(error.to_string()))?;
    }
    Ok(listener)
}

/// Run the buyer until the process is terminated. Acquires the home lock (fail
/// closed if held), binds the socket, and serves connections forever.
pub async fn run(home: MobeeHome) -> Result<(), BuyerError> {
    // `_lock` is held for the whole run; dropping it releases the OS lock.
    let (_lock, context, socket_path) = bootstrap(home).await?;
    // Reconcile the reservation ledger against relay + journal truth before serving: a reservation
    // orphaned by a prior crash (dead job → release, paid job → spent) is resolved here, so the
    // daemon starts from a converged ledger. A failure is logged, not fatal — an unreachable relay
    // must not keep the daemon from coming up (the stale reservation is conservative until the next
    // reconcile).
    run_reconcile_pass(&context).await;
    // Re-arm pending auto-awards left by a prior run: a job posted before a crash still gets its
    // award with zero manual commands. Each task re-checks the relay for an existing award first
    // (invariant A), so re-arming never double-awards.
    match context.store.list_pending_awards() {
        Ok(pending) => {
            for intent in pending {
                spawn_auto_award(context.clone(), intent.job_id, intent.max_sats);
            }
        }
        Err(error) => eprintln!("buyer: could not list pending auto-awards to re-arm: {error}"),
    }
    // Start watching for delivered results. Its own first action is a sweep of awarded-unsettled
    // jobs, which is what collects a delivery that landed while this daemon was down.
    spawn_delivery_watcher(context.clone());
    // Keep reconciling while we serve, so a reservation stranded by a seller that went away is
    // freed within the hour rather than at the next restart.
    spawn_reconcile_loop(context.clone());
    let listener = bind_socket(&socket_path)?;
    accept_loop(listener, context).await
}

/// Accept connections and service each on its own task.
async fn accept_loop(listener: UnixListener, context: Arc<BuyerContext>) -> Result<(), BuyerError> {
    loop {
        let (stream, _addr) = listener
            .accept()
            .await
            .map_err(|error| BuyerError::Io(error.to_string()))?;
        let context = context.clone();
        tokio::spawn(async move {
            // A handler failure never takes down the daemon; the connection is
            // just dropped.
            let _ = handle_connection(stream, context).await;
        });
    }
}

/// One request line in, one response line out.
async fn handle_connection(stream: UnixStream, context: Arc<BuyerContext>) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Ok(());
    }

    let response = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => dispatch(&context, request).await,
        Err(error) => Response::err(Value::Null, CODE_METHOD_NOT_FOUND, format!("malformed request: {error}")),
    };

    let mut encoded = serde_json::to_string(&response).unwrap_or_else(|error| {
        format!("{{\"id\":null,\"error\":{{\"code\":{CODE_INTERNAL},\"message\":\"encode failed: {error}\"}}}}")
    });
    encoded.push('\n');
    write_half.write_all(encoded.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Map a request to a response. `status`/`health` is live; the buyer trade
/// methods are recognized but deferred to later phases (they return a structured
/// not-implemented error rather than silently succeeding).
async fn dispatch(context: &Arc<BuyerContext>, request: Request) -> Response {
    let id = request.id.clone();
    match request.method.as_str() {
        "status" | "health" => status(context, id).await,
        "post_job" => post_job(context, id, request.params).await,
        "get_job" => get_job(context, id, request.params).await,
        "award" => award(context, id, request.params).await,
        "collect" => collect(context, id, request.params).await,
        "accept_claim" | "authorize_pay" => Response::err(
            id,
            CODE_NOT_IMPLEMENTED,
            format!(
                "{} is folded into collect (accept-if-needed + pay); call collect",
                request.method
            ),
        ),
        other => Response::err(id, CODE_METHOD_NOT_FOUND, format!("unknown method: {other}")),
    }
}

/// Params for the `post_job` RPC. From-scratch by default; the four contribution pins
/// (`target_repo_owner`/`target_repo_url`/`base_branch`/`base_oid`) are ALL-OR-NOTHING — all four
/// select a contribution offer, a partial set is refused. `job_id` returned is the offer event id.
#[derive(Debug, Deserialize)]
struct PostJobParams {
    task: String,
    output: String,
    amount_sats: u64,
    #[serde(default)]
    seller_pubkey: Option<String>,
    #[serde(default)]
    untargeted: bool,
    #[serde(default)]
    deadline_unix: Option<u64>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    target_repo_owner: Option<String>,
    #[serde(default)]
    target_repo_url: Option<String>,
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    base_oid: Option<String>,
    #[serde(default)]
    accepts: Option<Vec<String>>,
    /// Per-job spend ceiling for the background auto-award (defaults to `amount_sats`). The daemon
    /// never auto-awards a claim it cannot pay or priced above this.
    #[serde(default)]
    max_sats: Option<u64>,
    /// Auto-award preferences recorded with the intent. Not yet hard filters — no offer/claim wire
    /// field carries harness/model, so they are stored, not matched (added when the wire does).
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Resolve the offer kind from the contribution pins: all four present ⇒ contribution; none ⇒
/// from-scratch; a partial set is refused so the core never sees a half-specified contribution.
fn post_job_kind(params: &PostJobParams) -> Result<JobKind, String> {
    match (
        &params.target_repo_owner,
        &params.target_repo_url,
        &params.base_branch,
        &params.base_oid,
    ) {
        (None, None, None, None) => Ok(JobKind::FromScratch),
        (Some(owner), Some(url), Some(branch), Some(oid)) => {
            Ok(JobKind::Contribution(ContributionSpec {
                target_repo_owner: owner.clone(),
                target_repo_url: url.clone(),
                base_branch: branch.clone(),
                base_oid: oid.clone(),
                accepts: params.accepts.clone(),
            }))
        }
        _ => Err(
            "post_job contribution mode requires ALL of target_repo_owner, target_repo_url, \
             base_branch, base_oid (a partial set is refused)"
                .to_owned(),
        ),
    }
}

/// Publish an offer (reuses [`job_lifecycle::post_job_async`], the same money-checked post path the
/// CLI/MCP use), record its auto-award intent, and spawn the background auto-award task — the
/// daemon-drives-the-award half of the 2-call trade loop (post_job → collect). No reservation is
/// taken at post — funds are reserved at award.
async fn post_job(context: &Arc<BuyerContext>, id: Value, params: Value) -> Response {
    let params: PostJobParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => return Response::err(id, CODE_METHOD_NOT_FOUND, format!("post_job params: {error}")),
    };
    let job = match post_job_kind(&params) {
        Ok(job) => job,
        Err(message) => return Response::err(id, CODE_METHOD_NOT_FOUND, message),
    };
    let max_sats = params.max_sats.unwrap_or(params.amount_sats);
    let harness = params.harness.clone();
    let model = params.model.clone();
    let request = PostJobRequest {
        task: params.task,
        output: params.output,
        amount_sats: params.amount_sats,
        seller_pubkey: params.seller_pubkey,
        untargeted: params.untargeted,
        deadline_unix: params.deadline_unix,
        repo: params.repo,
        branch: params.branch,
        job,
    };
    match job_lifecycle::post_job_async(&context.home, request).await {
        Ok(outcome) => {
            // Record the intent BEFORE spawning so a crash right after post still re-arms on restart.
            if let Err(error) = context.store.put_pending_award(
                &outcome.job_id,
                max_sats,
                harness.as_deref(),
                model.as_deref(),
                now_unix(),
            ) {
                eprintln!(
                    "buyer: could not record auto-award intent for {}: {error}",
                    outcome.job_id
                );
            } else {
                spawn_auto_award(context.clone(), outcome.job_id.clone(), max_sats);
            }
            Response::ok(id, json!(outcome))
        }
        Err(error) => Response::err(id, CODE_INTERNAL, error.to_string()),
    }
}

/// Params for the `get_job` RPC — the reconcile/pull primitive (#127): read one job's relay view,
/// optionally long-polling for a claim/result.
#[derive(Debug, Deserialize)]
struct GetJobParams {
    job_id: String,
    #[serde(default)]
    wait_for: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    include_display_names: bool,
}

async fn get_job(context: &BuyerContext, id: Value, params: Value) -> Response {
    let params: GetJobParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => return Response::err(id, CODE_METHOD_NOT_FOUND, format!("get_job params: {error}")),
    };
    let wait_for = match params.wait_for.as_deref().map(WaitFor::parse).transpose() {
        Ok(wait_for) => wait_for,
        Err(error) => return Response::err(id, CODE_METHOD_NOT_FOUND, error.to_string()),
    };
    let request = GetJobRequest {
        job_id: params.job_id,
        wait_for,
        timeout_secs: params.timeout_secs,
        include_display_names: params.include_display_names,
    };
    // Subscribe BEFORE the wait begins its first fetch — an event landing in the gap between the
    // fetch and the subscribe would be lost, and the wait would then sleep on a view that was
    // already stale when it read it.
    let events = context.relay.subscribe_events();
    match job_lifecycle::get_job_awaiting_events_async(&context.home, request, events).await {
        Ok(view) => Response::ok(id, json!(view)),
        Err(error) => Response::err(id, CODE_INTERNAL, error.to_string()),
    }
}

/// Params for the `award` RPC. `claim_id` present ⇒ MANUAL award of that claim (the fine-grain
/// flag from #126); absent ⇒ AUTO-award the first claim passing the hard filters. `max_sats`
/// caps the price the buyer will commit to (defaults to the offer amount).
#[derive(Debug, Deserialize)]
struct AwardParams {
    job_id: String,
    #[serde(default)]
    claim_id: Option<String>,
    #[serde(default)]
    max_sats: Option<u64>,
}

/// Award a claim, reserving its funds FIRST. Reserve refusal ⇒ no award is published and no row is
/// written (the #126 mandatory guard). Snapshots are honest: live wallet balance, budget cap, and
/// budget spent total.
async fn award(context: &BuyerContext, id: Value, params: Value) -> Response {
    let params: AwardParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => return Response::err(id, CODE_METHOD_NOT_FOUND, format!("award params: {error}")),
    };
    let keys = match buyer_keys(&context.home) {
        Ok(keys) => keys,
        Err(error) => return Response::err(id, CODE_INTERNAL, error),
    };

    // Serialize with collect: the reserve below reads a balance/spent snapshot that must not race a
    // concurrent melt.
    let _guard = context.money_lock.lock().await;

    let view = match job_lifecycle::fetch_job_view_async(
        &context.home,
        &keys,
        &params.job_id,
        RELAY_TIMEOUT,
        now_unix() as u64,
    )
    .await
    {
        Ok(view) => view,
        Err(error) => return Response::err(id, CODE_INTERNAL, error.to_string()),
    };
    let Some(offer) = view.offer.as_ref() else {
        return Response::err(id, CODE_INTERNAL, format!("no offer on the relay for job {}", params.job_id));
    };
    let offer_amount = offer.amount_sats;
    let max_sats = params.max_sats.unwrap_or(offer_amount);
    let filters = AwardFilters {
        offer_amount_sats: offer_amount,
        max_sats,
        buyer_mint: context.home.config.default_mint(),
        allow_real_mints: context.home.config.allow_real_mints,
    };

    // Manual award names the claim but applies the SAME hard filters (max_sats, price, mint) as
    // auto-award — max_sats is enforced, not ignored, on the manual path. Auto-award selects the
    // first live payable claim.
    let claim_id = match params.claim_id {
        Some(claim_id) => {
            if let Err(refused) = lifecycle::named_claim_awardable(&view, &claim_id, &filters) {
                return Response::err(id, CODE_REFUSED, refused.to_string());
            }
            claim_id
        }
        None => match lifecycle::select_awardable_claim(&view, &filters) {
            Some(claim_id) => claim_id,
            None => {
                return Response::err(
                    id,
                    CODE_REFUSED,
                    format!("no awardable claim for job {} (none live/payable/mint-compatible)", params.job_id),
                );
            }
        },
    };

    let (balance, total_cap, spent) = match money_snapshot(context).await {
        Ok(snapshot) => snapshot,
        Err(error) => return Response::err(id, CODE_INTERNAL, error),
    };

    let job_id = params.job_id.clone();
    let home = context.home.clone();
    let publish_claim = claim_id.clone();
    let result = lifecycle::award_with_reservation(
        &context.store,
        &params.job_id,
        offer_amount,
        balance,
        total_cap,
        spent,
        now_unix(),
        move || async move {
            job_lifecycle::award_claim_async(
                &home,
                AwardClaimRequest { job_id, claim_id: publish_claim },
            )
            .await
        },
    )
    .await;

    match result {
        Ok(outcome) => Response::ok(
            id,
            json!({
                "awarded": outcome,
                "reserved_sats": offer_amount,
                "reserved_for": params.job_id,
            }),
        ),
        Err(AwardError::Reserve(refused)) => Response::err(id, CODE_REFUSED, refused.to_string()),
        Err(error) => Response::err(id, CODE_INTERNAL, error.to_string()),
    }
}

/// Params for the `collect` RPC.
#[derive(Debug, Deserialize)]
struct CollectParams {
    job_id: String,
    #[serde(default)]
    out: Option<String>,
}

/// Collect a delivered job: run the sealed pay path ([`collect::collect_async`] — accept-if-needed,
/// verify integrity, budget-append + wallet melt, materialize) and, ONLY after it succeeds, flip
/// the reservation `reserved → spent` via [`lifecycle::settle_after_pay`]. The flip is never
/// reached on a pay refusal, so a failed pay never over-states `available`.
/// Failure of [`settle_job`].
#[derive(Debug)]
enum SettleJobError {
    /// The budget gate could not be loaded — nothing was attempted.
    Gate(String),
    /// The sealed pay path refused (or failed). No reservation flip was reached.
    Pay(collect::CollectError),
    /// The pay landed but the `reserved → spent` flip did not; reconcile converges it on next start.
    Store(StoreError),
}

impl std::fmt::Display for SettleJobError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gate(message) => write!(formatter, "{message}"),
            Self::Pay(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

/// The daemon's ONE path to the spend gate: money lock → budget gate → pay-then-flip.
///
/// Both the `collect` RPC and the delivery watcher call this and nothing else. That is deliberate:
/// it makes "can the watcher reach around a money gate?" answerable by construction instead of by
/// audit. Every gate lives inside the sealed [`collect::collect_async`] this composes — accept-time
/// job-hash recomputation, offer-authoritative amount, seller co-signature, creq verification,
/// single-settlement, the pre-spend commit tip-match, the budget ceiling, and pays-exactly-once —
/// and neither caller can compose a different route to them.
///
/// The watcher therefore adds NO money authority. It commits no new money; it converts a
/// reservation the award already created.
async fn settle_job(
    context: &BuyerContext,
    job_id: &str,
    out: Option<String>,
) -> Result<collect::CollectOutcome, SettleJobError> {
    // Serialize with award + other collects: at most one wallet-melting op in flight daemon-wide.
    let _guard = context.money_lock.lock().await;

    let mut gate =
        BudgetGate::from_home(&context.home).map_err(|error| SettleJobError::Gate(error.to_string()))?;
    let request = CollectRequest {
        job_id: job_id.to_owned(),
        out,
    };

    // Pay FIRST (append + melt), flip AFTER — the #123/#126 ordering, via the tested seam.
    lifecycle::settle_after_pay(
        &context.store,
        job_id,
        now_unix(),
        || collect::collect_async(&context.home, &mut gate, request),
        |outcome| outcome.pay.amount_sats,
    )
    .await
    .map(|(outcome, _converted)| outcome)
    .map_err(|error| match error {
        SettleError::Pay(error) => SettleJobError::Pay(error),
        SettleError::Store(error) => SettleJobError::Store(error),
    })
}

async fn collect(context: &BuyerContext, id: Value, params: Value) -> Response {
    let params: CollectParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => return Response::err(id, CODE_METHOD_NOT_FOUND, format!("collect params: {error}")),
    };

    match settle_job(context, &params.job_id, params.out).await {
        Ok(outcome) => Response::ok(
            id,
            json!({
                "pay": {
                    "state": format!("{:?}", outcome.pay.state),
                    "attempt_id": outcome.pay.attempt_id,
                    "amount_sats": outcome.pay.amount_sats,
                    "spent_total_sats": outcome.pay.spent_total_sats,
                    "remaining_sats": outcome.pay.remaining_sats,
                },
                "commit_oid": outcome.commit_oid,
                "path": outcome.path,
                "files": outcome.files,
            }),
        ),
        Err(error @ SettleJobError::Pay(_)) => Response::err(id, CODE_REFUSED, error.to_string()),
        Err(error) => Response::err(id, CODE_INTERNAL, error.to_string()),
    }
}

/// Honest reserve snapshot: the live wallet balance (through the actor), the budget cap, and the
/// budget spent total (fresh fold). Never a sentinel or a stale cached value.
async fn money_snapshot(context: &BuyerContext) -> Result<(u64, u64, u64), String> {
    let balance = context
        .wallet
        .balance()
        .await
        .map_err(|error| error.to_string())??;
    let gate = BudgetGate::from_home(&context.home).map_err(|error| error.to_string())?;
    Ok((balance, gate.total_cap(), gate.spent()))
}

/// The buyer nostr identity, parsed from the home secret (the same source the signer actor loads).
fn buyer_keys(home: &MobeeHome) -> Result<nostr_sdk::Keys, String> {
    let secret = home::read_secret_key_hex(home).map_err(|error| error.to_string())?;
    nostr_sdk::Keys::parse(&secret).map_err(|error| format!("buyer key parse: {error}"))
}

/// Spawn the background auto-award task for a posted job — the daemon-drives-the-award half of the
/// 2-call trade loop. A task failure never affects the daemon; the intent stays `pending` and is
/// re-armed on the next start.
fn spawn_auto_award(context: Arc<BuyerContext>, job_id: String, max_sats: u64) {
    tokio::spawn(async move {
        if let Err(error) = drive_auto_award(&context, &job_id, max_sats).await {
            eprintln!("buyer: auto-award for {job_id} did not complete ({error}); left pending for re-arm");
        }
    });
}

/// Drive one posted job's award under the hood: wait (bounded by the offer deadline) for a payable
/// claim, then reserve-then-award. Honors both #126/#127 invariants:
///
/// - **A (idempotent re-arm):** before doing anything, skip if a buyer AWARD is already on the relay
///   OR the reservation is already `Spent` — never award twice (see [`lifecycle::plan_rearm`]). This
///   is why re-arming on restart is safe: the task re-checks the relay first.
/// - **B (reserve-then-award only):** the award goes exclusively through
///   [`lifecycle::award_with_reservation`] (reserve first, publish second). A refused reservation
///   (e.g. funds shrank) PARKS the intent with a surfaced reason — never a silent drop.
///
/// Returns `Err` only on a transient relay/wallet failure; the intent then stays `pending` and is
/// re-armed on the next start.
async fn drive_auto_award(
    context: &Arc<BuyerContext>,
    job_id: &str,
    max_sats: u64,
) -> Result<(), String> {
    let keys = buyer_keys(&context.home)?;

    // Invariant A: never award twice. A relay error is "unknown" (unwrap_or false) — do not skip;
    // the reserve-then-award path below is itself idempotent on the reserve.
    let award_on_relay = job_lifecycle::has_award_async(&context.home, &keys, job_id, RELAY_TIMEOUT)
        .await
        .unwrap_or(false);
    let reservation = context
        .store
        .reservation(job_id)
        .ok()
        .flatten()
        .map(|(state, _)| state);
    if lifecycle::plan_rearm(award_on_relay, reservation) == RearmAction::Skip {
        let _ = context.store.mark_award_awarded(job_id, now_unix());
        return Ok(());
    }

    loop {
        let view = job_lifecycle::fetch_job_view_async(
            &context.home,
            &keys,
            job_id,
            RELAY_TIMEOUT,
            now_unix() as u64,
        )
        .await
        .map_err(|error| error.to_string())?;
        let Some(offer) = view.offer.as_ref() else {
            let _ = context
                .store
                .mark_award_parked(job_id, "offer no longer on the relay", now_unix());
            return Ok(());
        };
        if now_unix() as u64 > offer.deadline_unix {
            let _ = context.store.mark_award_parked(
                job_id,
                "offer deadline passed before an awardable claim appeared",
                now_unix(),
            );
            return Ok(());
        }

        let filters = AwardFilters {
            offer_amount_sats: offer.amount_sats,
            max_sats,
            buyer_mint: context.home.config.default_mint(),
            allow_real_mints: context.home.config.allow_real_mints,
        };
        if let Some(claim_id) = lifecycle::select_awardable_claim(&view, &filters) {
            return finalize_auto_award(context, job_id, offer.amount_sats, claim_id).await;
        }

        // No awardable claim yet — re-check after a bounded interval (no tight spin on a
        // live-but-unpayable claim). The deadline check above bounds the total wait.
        tokio::time::sleep(AUTO_AWARD_POLL_INTERVAL).await;
    }
}

/// Reserve-then-award a selected claim (invariant B), serialized on the money lock so the reserve
/// snapshot never races a collect melt. Marks the intent `awarded` on success, or `parked` (with a
/// surfaced reason) on a refused reservation / publish failure — never a silent drop.
async fn finalize_auto_award(
    context: &Arc<BuyerContext>,
    job_id: &str,
    offer_amount: u64,
    claim_id: String,
) -> Result<(), String> {
    let _guard = context.money_lock.lock().await;
    let (balance, total_cap, spent) = money_snapshot(context).await?;
    let home = context.home.clone();
    let job = job_id.to_owned();
    let publish_claim = claim_id.clone();
    let result = lifecycle::award_with_reservation(
        &context.store,
        job_id,
        offer_amount,
        balance,
        total_cap,
        spent,
        now_unix(),
        move || async move {
            job_lifecycle::award_claim_async(&home, AwardClaimRequest { job_id: job, claim_id: publish_claim })
                .await
        },
    )
    .await;

    match result {
        Ok(_) => {
            let _ = context.store.mark_award_awarded(job_id, now_unix());
            Ok(())
        }
        Err(AwardError::Reserve(refused)) => {
            let _ = context.store.mark_award_parked(
                job_id,
                &format!("reservation refused: {refused}"),
                now_unix(),
            );
            Ok(())
        }
        Err(error) => {
            let _ = context
                .store
                .mark_award_parked(job_id, &format!("award failed: {error}"), now_unix());
            Ok(())
        }
    }
}

/// Spawn the delivery watcher — the "seller paid in seconds" half of the trade loop. Subscribes
/// BEFORE the task starts so no result published during scheduling is missed, then hands the
/// receiver to the loop.
///
/// `subscribe_events` is a synchronous, non-blocking channel handout and `tokio::spawn` returns
/// immediately, so this adds nothing measurable to the daemon's readiness path — which matters,
/// because the socket bind that follows is on a 10s deadline.
fn spawn_delivery_watcher(context: Arc<BuyerContext>) {
    let events = context.relay.subscribe_events();
    tokio::spawn(async move { drive_delivery_watch(&context, events).await });
}

/// Watch for delivered results and settle them automatically.
///
/// Structure mirrors the P2 rule this daemon already lives by — **the subscription is the wake, the
/// durable state is the truth**. An arriving result event is never evidence of anything: it only
/// says "look again", and optionally narrows WHICH job to look at. What may be paid is read from
/// the award/reservation ledger and decided entirely by the sealed pay path, so a forged, replayed,
/// or malformed result event can at worst cost a wasted fetch.
///
/// The loop starts with a sweep because a delivery that landed while the daemon was DOWN has no
/// event to replay — durable state is the only thing that can find it.
async fn drive_delivery_watch(
    context: &Arc<BuyerContext>,
    events: tokio::sync::broadcast::Receiver<Arc<nostr_sdk::Event>>,
) {
    watch_loop(events, DELIVERY_RECHECK_INTERVAL, |wake| async move {
        match wake {
            // A result arrived: sweep, narrowed to the jobs that event references.
            WatchWake::Delivered(event) => settle_awarded(context, Some(&event)).await,
            WatchWake::Sweep | WatchWake::SubscriptionLost => settle_awarded(context, None).await,
        }
    })
    .await;
}

/// What woke the watch loop. Modelled explicitly so the loop's control flow is testable without a
/// relay, a wall clock, or a paused clock — the sweep action is injected, and this says why.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WatchWake {
    /// A delivered result arrived — sweep, narrowed to the jobs it references.
    Delivered(Arc<nostr_sdk::Event>),
    /// The backstop fired, or a gap means something may have been missed — sweep everything.
    Sweep,
    /// The subscription ended; the loop continues on the backstop alone.
    SubscriptionLost,
}

/// The watch loop's control flow, with the sweep action injected.
///
/// Two properties here are the whole point, and both are the kind that stay silently dead while
/// everything looks fine — so both are toothed directly:
///
/// 1. The backstop is a fixed-cadence `interval`, NOT a sleep re-armed inside the `select!`. A
///    per-iteration sleep is pushed back by every arriving event, and events the loop does not act
///    on (the `continue` below) would reset it WITHOUT sweeping — so a steady claim/feedback stream
///    would starve the sweep indefinitely, defeating it in exactly the case it exists for: a result
///    event that was missed. `Delay` keeps a slow settle pass from bunching the next ticks.
/// 2. A closed subscription DEGRADES, it does not stop the loop. Settling does not depend on the
///    relay handle at all (the collect path opens its own client), so returning here would strand
///    every future delivery. It drops to timer-only and says so.
async fn watch_loop<S, Fut>(
    events: tokio::sync::broadcast::Receiver<Arc<nostr_sdk::Event>>,
    interval: Duration,
    mut sweep: S,
) where
    S: FnMut(WatchWake) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    // A delivery that landed while the daemon was DOWN has no event to replay, so the durable set
    // is the only thing that can find it. That is why the loop sweeps before it ever waits.
    sweep(WatchWake::Sweep).await;

    let mut backstop = tokio::time::interval(interval);
    backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    backstop.tick().await; // The first tick resolves immediately; the boot sweep above WAS it.

    // `None` once the subscription is gone — the loop then runs on the backstop alone.
    let mut events = Some(events);

    loop {
        let wake = match events.as_mut() {
            Some(stream) => tokio::select! {
                received = stream.recv() => match received {
                    Ok(event) => {
                        if event.kind != nostr_sdk::Kind::Custom(crate::kinds::JOB_RESULT_KIND) {
                            continue;
                        }
                        WatchWake::Delivered(event)
                    }
                    // Lagged is NOT an error — the buffer overflowed and a result may have been
                    // missed. Treating a busy relay as a failure would strand a payment; the right
                    // response is to widen to a full sweep.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => WatchWake::Sweep,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        eprintln!(
                            "buyer: delivery watcher lost the relay subscription; falling back to \
                             periodic sweeps every {}s (deliveries still settle, just later)",
                            interval.as_secs()
                        );
                        events = None;
                        WatchWake::SubscriptionLost
                    }
                },
                _ = backstop.tick() => WatchWake::Sweep,
            },
            None => {
                backstop.tick().await;
                WatchWake::Sweep
            }
        };
        sweep(wake).await;
    }
}

/// Settle awarded-but-unsettled jobs through the daemon's single spend path.
///
/// `wake` narrows the sweep to the jobs a just-arrived result references (the fast path); `None`
/// sweeps the whole set (boot, the backstop tick, and after a `Lagged` gap).
async fn settle_awarded(context: &Arc<BuyerContext>, wake: Option<&nostr_sdk::Event>) {
    let jobs = match context.store.awarded_unsettled_job_ids() {
        Ok(jobs) => jobs,
        Err(error) => {
            eprintln!("buyer: delivery watcher could not read awarded jobs ({error}); will retry");
            return;
        }
    };
    for job_id in jobs {
        if let Some(event) = wake {
            if !job_lifecycle::event_references_job(event, &job_id) {
                continue;
            }
        }
        match settle_job(context, &job_id, None).await {
            Ok(outcome) => eprintln!(
                "buyer: delivery watcher settled {job_id} — paid {} sat for commit {} ({} file(s))",
                outcome.pay.amount_sats,
                outcome.commit_oid,
                outcome.files.len()
            ),
            // Nothing delivered yet is the ordinary state of an awarded job, not a failure: the job
            // stays in the set and the next event or tick retries. Every OTHER outcome is a real
            // refusal — a gate said no — and is named so an operator sees which job stopped and why.
            Err(SettleJobError::Pay(collect::CollectError::Lifecycle(
                job_lifecycle::JobLifecycleError::NotFound(_),
            ))) => {}
            Err(error) => eprintln!("buyer: delivery watcher could not settle {job_id}: {error}"),
        }
    }
}

/// Reconcile every still-`Reserved` job against relay + payment-journal truth: a job the relay no
/// longer shows payable (and that has left no funds) is released; a job whose payment journal shows
/// a `Closed` attempt is converted to `spent`; an ambiguous (Sent-not-Closed) payment is KEPT (the
/// phase-3 saga owns it). Pure classification is [`lifecycle::classify_disposition`]; this gathers
/// its inputs and applies the batch through [`BuyerStore::reconcile`].
///
/// This runs at boot AND on a slow timer, which is what releases a reservation stranded by a seller
/// that simply stopped — no feedback event required, because a claim stops being live at the offer
/// deadline whether or not the seller ever says so.
///
/// Gather is done WITHOUT the money lock (it is per-job relay I/O and would block the trade RPCs for
/// seconds); only the apply takes it. That ordering matters: `settle_job` holds the money lock
/// across pay-then-flip, so taking it here makes it impossible to release a reservation in the
/// window after a payment's melt but before its `reserved → spent` flip — which would transiently
/// free funds that had in fact already left the wallet. The apply itself is additionally
/// state-guarded inside its transaction ([`BuyerStore::reconcile`] re-reads each row and acts only
/// on the state it expects), so a disposition that went stale during gather is a no-op rather than a
/// wrong write.
async fn reconcile_reservations(context: &BuyerContext) -> Result<ReconcileReport, String> {
    let reserved = context
        .store
        .reserved_job_ids()
        .map_err(|error| error.to_string())?;
    if reserved.is_empty() {
        return Ok(ReconcileReport::default());
    }
    let keys = buyer_keys(&context.home)?;
    let progress = scan_payment_progress(&context.home);

    // Gather each reserved job's "still payable" signal (the only I/O). A job is payable if a claim
    // is still live on the relay OR a local delivery bind exists (#140: a delivered job whose relay
    // events expired is not dead — its bind + retained git objects still let collect settle it). An
    // unreachable relay is treated as still-payable (conservative — never release what we cannot
    // verify is dead).
    let mut payable: BTreeMap<String, bool> = BTreeMap::new();
    for job_id in &reserved {
        let has_bind = job_lifecycle::load_accepted_bind(&context.home, job_id)
            .map(|bind| bind.is_some())
            .unwrap_or(false);
        let claim_live = match job_lifecycle::fetch_job_view_async(
            &context.home,
            &keys,
            job_id,
            RELAY_TIMEOUT,
            now_unix() as u64,
        )
        .await
        {
            Ok(view) => view.live_claim_id.is_some(),
            Err(_) => true,
        };
        payable.insert(job_id.clone(), claim_live || has_bind);
    }

    let dispositions = plan_reconcile(&reserved, &progress, &payable);
    let _guard = context.money_lock.lock().await;
    context
        .store
        .reconcile(&dispositions, now_unix())
        .map_err(|error| error.to_string())
}

/// Run one reconcile pass, publish it to `status`, and report it — always.
async fn run_reconcile_pass(context: &Arc<BuyerContext>) {
    match reconcile_reservations(context).await {
        Ok(report) => {
            report_reconcile(&report);
            *context.last_reconcile.lock().await = Some(report);
        }
        // An unreachable relay must never be fatal: every job it could not verify was treated as
        // still-payable, so the ledger is conservative until the next pass.
        Err(error) => {
            eprintln!("buyer: reconcile pass did not complete ({error}); serving with the ledger as-is")
        }
    }
}

/// Report a reconcile pass UNCONDITIONALLY — including the pass that changed nothing.
///
/// Releasing a reservation is a money-visible decision: it returns funds to `available`. A path that
/// prints only when it acts is indistinguishable, in a log, from a path that has stopped running —
/// so the quiet pass is exactly the one worth printing. Released jobs are named, because "something
/// was released" is not an answer to "which job, and why did my budget move".
///
/// The examined count is in the line deliberately: it is also the number of per-job relay fetches
/// this pass made, so #180's amplification is visible while it grows instead of when it bites.
fn report_reconcile(report: &ReconcileReport) {
    eprintln!("{}", reconcile_line(report));
}

/// Build the reconcile report line. Split from the printing so the wording — including the
/// released-nothing case that exists precisely because it is easy to leave out — is directly
/// testable rather than something a reader has to trust.
fn reconcile_line(report: &ReconcileReport) -> String {
    let examined = report.released.len() + report.converted.len() + report.kept.len();
    if report.released.is_empty() {
        format!(
            "buyer: reconcile examined {examined} reserved job(s) — released nothing, converted {}, kept {}",
            report.converted.len(),
            report.kept.len()
        )
    } else {
        format!(
            "buyer: reconcile examined {examined} reserved job(s) — RELEASED {} (no longer payable \
             on the relay and no funds left: {}), converted {}, kept {}",
            report.released.len(),
            report.released.join(", "),
            report.converted.len(),
            report.kept.len()
        )
    }
}

/// Re-run the reconcile on a slow timer for the daemon's lifetime, with the pass injected.
///
/// Sequential BY CONSTRUCTION: the pass is awaited inside the loop, and the interval's `Delay`
/// behaviour defers a tick that arrives mid-pass. Two passes can therefore never overlap, so the
/// per-job relay fetches cannot compound into each other — which matters while #180 stands, because
/// overlapping passes would multiply exactly the load the slow cadence is holding down.
async fn reconcile_loop<P, Fut>(interval: Duration, mut pass: P)
where
    P: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // Resolves immediately; the boot pass in `run` WAS it.
    loop {
        ticker.tick().await;
        pass().await;
    }
}

fn spawn_reconcile_loop(context: Arc<BuyerContext>) {
    tokio::spawn(async move {
        reconcile_loop(RECONCILE_INTERVAL, || run_reconcile_pass(&context)).await;
    });
}

/// Pure reconcile planning: map each reserved job to a disposition from its folded payment progress
/// and whether it is still payable. Kept pure (no relay/disk I/O) so the reserved-job → disposition
/// mapping is exhaustively testable; [`reconcile_on_start`] gathers the inputs. A job absent from
/// `payable` defaults to payable (conservative — never release without positive evidence of death).
fn plan_reconcile(
    reserved: &[String],
    progress: &BTreeMap<String, PaymentProgress>,
    payable: &BTreeMap<String, bool>,
) -> Dispositions {
    let mut dispositions: Dispositions = BTreeMap::new();
    for job_id in reserved {
        let payment = progress.get(job_id).copied().unwrap_or(PaymentProgress::None);
        let claim_payable = payable.get(job_id).copied().unwrap_or(true);
        dispositions.insert(
            job_id.clone(),
            lifecycle::classify_disposition(payment, claim_payable),
        );
    }
    dispositions
}

/// Fold every payment-journal attempt under the home into a `job_id → progress` map. Each record
/// carries its [`crate::payment::PaymentKey`] (hence its `job_id`), so no attempt-id recomputation
/// is needed. A journal that cannot be read/folded is treated as `Uncertain` (kept, never
/// released) — reconcile must fail safe, never free funds on ambiguous evidence.
fn scan_payment_progress(home: &MobeeHome) -> BTreeMap<String, PaymentProgress> {
    let mut progress: BTreeMap<String, PaymentProgress> = BTreeMap::new();
    let dir = home.root.join("payment-journal");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return progress; // no journal yet ⇒ no payments
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let records: Result<Vec<PaymentRecord>, _> = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<PaymentRecord>)
            .collect();
        let Ok(records) = records else { continue };
        let Some(first) = records.first() else { continue };
        let job_id = first.key.job_id.as_str().to_owned();
        let folded = match PaymentMachine::fold(&first.key, &records) {
            Ok(state) => progress_from_state(state.as_ref()),
            // A journal that will not fold is ambiguous — keep the reservation, never release it.
            Err(_) => PaymentProgress::Uncertain,
        };
        // If two journals map to one job (retries under distinct attempt ids), the more-advanced
        // progress wins so a Closed attempt is never masked by an earlier Intent.
        let merged = merge_progress(progress.get(&job_id).copied(), folded);
        progress.insert(job_id, merged);
    }
    progress
}

/// Map a folded payment state to reconcile progress. `Closed` ⇒ funds+receipt durable;
/// `Sent`/`ReceiptPublished` ⇒ ambiguous (funds may have left); `Intent`/`Locked`/none ⇒ no funds
/// left yet.
fn progress_from_state(state: Option<&PaymentState>) -> PaymentProgress {
    match state {
        Some(PaymentState::Closed { .. }) => PaymentProgress::Closed,
        Some(PaymentState::Sent { .. }) | Some(PaymentState::ReceiptPublished { .. }) => {
            PaymentProgress::Uncertain
        }
        Some(PaymentState::Intent { .. }) | Some(PaymentState::Locked { .. }) | None => {
            PaymentProgress::None
        }
    }
}

/// The more-advanced of two progresses (`Closed` > `Uncertain` > `None`) — a job with any Closed
/// attempt is Paid regardless of an earlier abandoned attempt.
fn merge_progress(existing: Option<PaymentProgress>, next: PaymentProgress) -> PaymentProgress {
    fn rank(progress: PaymentProgress) -> u8 {
        match progress {
            PaymentProgress::None => 0,
            PaymentProgress::Uncertain => 1,
            PaymentProgress::Closed => 2,
        }
    }
    match existing {
        Some(existing) if rank(existing) >= rank(next) => existing,
        _ => next,
    }
}

/// The health/status method: prove the boundary end to end — the state DB, the
/// wallet actor, and the signer actor all answered through the socket. The secret
/// key is never included.
async fn status(context: &BuyerContext, id: Value) -> Response {
    let store = context.store.clone();
    let health = tokio::task::spawn_blocking(move || store.health()).await;

    let (schema_version, jobs) = match health {
        Ok(Ok(snapshot)) => (json!(snapshot.schema_version), json!(snapshot.jobs)),
        Ok(Err(error)) => return Response::err(id, CODE_INTERNAL, error.to_string()),
        Err(error) => return Response::err(id, CODE_INTERNAL, format!("state DB task failed: {error}")),
    };

    let mint = context.home.config.default_mint().to_owned();
    let wallet = match context.wallet.balance().await {
        Ok(Ok(balance_sats)) => json!({ "mint": mint, "balance_sats": balance_sats }),
        Ok(Err(error)) => json!({ "mint": mint, "error": error }),
        Err(error) => json!({ "mint": mint, "error": error.to_string() }),
    };

    // Surface the last reconcile-on-start outcome so kept-uncertain reservations (funds committed to
    // an ambiguous payment the crash-safe saga still owns) are visible, not silently discarded.
    let reconcile = context.last_reconcile.lock().await.as_ref().map(|report| {
        json!({
            "released": report.released,
            "converted": report.converted,
            "kept": report.kept,
        })
    });

    // Surface parked auto-awards (a claim could not be awarded — e.g. funds shrank) so a buyer sees
    // jobs whose award was not placed rather than silently losing them.
    let parked_awards: Vec<Value> = context
        .store
        .parked_awards()
        .unwrap_or_default()
        .into_iter()
        .map(|(job_id, reason)| json!({ "job_id": job_id, "reason": reason }))
        .collect();

    Response::ok(
        id,
        json!({
            "ok": true,
            "version": crate::version(),
            "home": context.home.root.display().to_string(),
            "socket": context.home.root.join(SOCKET_FILE).display().to_string(),
            "pid": std::process::id(),
            "pubkey": context.signer.public_key_hex(),
            "started_at_unix": context.started_at_unix,
            "wallet": wallet,
            "store": {
                "schema_version": schema_version,
                "jobs": jobs,
            },
            "reconcile": reconcile,
            "parked_awards": parked_awards,
            // The relay the buyer's one long-lived session is bound to. Deliberately NOT a liveness
            // probe: `status` is what connect-or-spawn polls to decide the daemon is up, and a probe
            // bounded at 10s would push that poll past its own readiness deadline. Liveness belongs
            // to the watcher's tick, where a slow answer costs nothing.
            "relay": { "url": context.relay.relay_url() },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::bootstrap as bootstrap_home;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-buyer-mod-{label}-{}-{id}", std::process::id()))
    }

    // ★ THE MELT-TO-FLIP WINDOW TOOTH. The reconcile must not be able to release a reservation while
    // a settle is between its wallet melt and its `reserved → spent` flip.
    //
    // `settle_job` holds the money lock across pay-then-flip. The reconcile GATHERS unlocked (it is
    // per-job relay I/O and would block trades for seconds) and takes the lock only for the apply.
    // That is the whole protection: a pass whose gather decided "dead" cannot act on that decision
    // until the settle has finished moving the amount from `reserved` into `spent`. Without it, a
    // pass could free funds that had already left the wallet, and `available` would over-state by
    // the amount for as long as the discrepancy stood.
    //
    // ★ THE ASSERTION IS ON THE REPORT, NOT THE FINAL STATE — and that is the point. The final state
    // converges to `spent` either way (a released row that turns out paid is converted by the very
    // next pass), so a test that only checked the end state would pass with the lock REMOVED. That
    // convergence is exactly what let this ship untoothed: the bug is invisible in the outcome and
    // visible only in the decision.
    //
    // Deterministic: the settle-holder keeps the lock for far longer than a localhost gather takes,
    // so the ordering is not a coin flip. And it fails safe — if the gather were somehow slow enough
    // to apply after the flip, it would observe `spent` and still keep, passing for the right reason.
    //
    // Red-on-revert: delete the `money_lock` acquisition in `reconcile_reservations` and the pass
    // releases a job whose payment was already in flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_reconcile_cannot_release_a_reservation_mid_settle() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let root = temp_home("mid-settle");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");

        // A relay that serves NO claims, so the classification genuinely reaches `Dead` — with an
        // unreachable relay every job is conservatively treated as still-payable and the reconcile
        // would never try to release anything, which would make this test vacuous.
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let job = "a".repeat(64);
        context
            .store
            .reserve(&job, 4, 1_000, u64::MAX, 0, now_unix())
            .expect("reserve");

        // Confirm the premise: with no bind, no payment journal and no live claim, this job WOULD be
        // released by a pass that got to act. Without this the tooth could pass because nothing was
        // ever at risk.
        let would_release = plan_reconcile(
            &[job.clone()],
            &BTreeMap::new(),
            &BTreeMap::from([(job.clone(), false)]),
        );
        assert_eq!(
            would_release[&job],
            reservations::JobDisposition::Dead,
            "premise: this job must classify Dead, or nothing is at risk and the tooth is vacuous"
        );

        // Stand in for a settle that has melted but not yet flipped: hold the money lock, wait long
        // enough that any unlocked apply would have already run, then perform the flip.
        let settling = {
            let context = context.clone();
            let job = job.clone();
            tokio::spawn(async move {
                let _guard = context.money_lock.lock().await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                context.store.convert_to_spent(&job, 4, now_unix()).expect("flip");
            })
        };

        // Let the settle take the lock first, then run a real pass against it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let report = reconcile_reservations(&context).await.expect("reconcile");
        settling.await.expect("settle task");

        assert!(
            !report.released.contains(&job),
            "the reconcile released a reservation whose payment was mid-flight — it must wait for \
             the money lock, not act on a gather that went stale. released={:?}",
            report.released
        );
        assert!(
            report.kept.contains(&job),
            "the pass should have observed the completed flip and kept the row; report={report:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ THE #179/4b TOOTH: the reconcile reports the pass that changed NOTHING.
    //
    // A release moves the buyer's `available` — it is a money-visible decision — and the pass that
    // releases nothing is the one most likely to be dropped as noise. But a path that prints only
    // when it acts reads, in a log, exactly like a path that has stopped running, which is how a
    // dead release path stays invisible until a reservation strands. So the empty case is asserted
    // first here, deliberately.
    //
    // Red-on-revert: make the empty case return an empty string (or skip the log) and this fails.
    #[test]
    fn the_reconcile_reports_every_pass_including_the_one_that_did_nothing() {
        // Nothing reserved at all: still a line, and it still says what it examined.
        let quiet = reconcile_line(&ReconcileReport::default());
        assert!(quiet.contains("examined 0 reserved job(s)"), "unexpected: {quiet}");
        assert!(quiet.contains("released nothing"), "the empty pass must say so: {quiet}");

        // Jobs examined, none released: the count is still reported, so an operator can see the
        // pass ran and how many relay fetches it cost (#180's amplification, visible as it grows).
        let busy_but_quiet = ReconcileReport {
            kept: vec!["a".repeat(64), "b".repeat(64)],
            ..ReconcileReport::default()
        };
        let line = reconcile_line(&busy_but_quiet);
        assert!(line.contains("examined 2 reserved job(s)"), "unexpected: {line}");
        assert!(line.contains("released nothing"), "unexpected: {line}");

        // A release NAMES the job and the reason — "something was released" is not an answer to
        // "which job, and why did my budget move".
        let released = ReconcileReport {
            released: vec!["c".repeat(64)],
            kept: vec!["d".repeat(64)],
            ..ReconcileReport::default()
        };
        let line = reconcile_line(&released);
        assert!(line.contains("examined 2 reserved job(s)"), "unexpected: {line}");
        assert!(line.contains(&"c".repeat(64)), "the released job must be named: {line}");
        assert!(line.contains("no longer payable"), "the reason must be stated: {line}");
    }

    // ★ NO-OVERLAP TOOTH: reconcile passes must never run concurrently.
    //
    // Each pass makes one relay fetch per still-reserved job — the unbounded per-job pattern in
    // #180 — so overlapping passes would multiply precisely the load the slow cadence exists to
    // hold down. `Delay` plus awaiting the pass inside the loop is what prevents it, and both are
    // one-word changes away from being wrong (`Burst`, or spawning the pass).
    //
    // Deterministic: the pass is injected and takes far longer than the tick, so a loop that did
    // NOT serialise would immediately show overlap.
    //
    // Red-on-revert: `tokio::spawn(pass())` instead of awaiting it, and max-in-flight exceeds 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reconcile_passes_never_overlap() {
        use std::sync::atomic::AtomicUsize;

        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let passes = Arc::new(AtomicUsize::new(0));

        let (f, p, n) = (in_flight.clone(), peak.clone(), passes.clone());
        let task = tokio::spawn(async move {
            // Pass duration deliberately several times the tick, so any non-serialising loop
            // overlaps on the very first ticks.
            reconcile_loop(Duration::from_millis(5), move || {
                let (f, p, n) = (f.clone(), p.clone(), n.clone());
                async move {
                    let now = f.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    f.fetch_sub(1, Ordering::SeqCst);
                    n.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await;
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        let (observed_peak, completed) = (peak.load(Ordering::SeqCst), passes.load(Ordering::SeqCst));
        task.abort();

        assert!(completed >= 2, "the loop must keep running passes; saw {completed}");
        assert_eq!(
            observed_peak, 1,
            "reconcile passes must never overlap — peak concurrent passes was {observed_peak}"
        );
    }

    /// A non-actionable event for the starvation tooth: any kind the watcher does not act on. Job
    /// FEEDBACK is the realistic one — it shares the buyer-keyed subscription with results, so a
    /// chatty job produces exactly this traffic.
    fn non_actionable_event() -> Arc<nostr_sdk::Event> {
        use nostr_sdk::prelude::{EventBuilder, Keys};
        Arc::new(
            EventBuilder::new(nostr_sdk::Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND), "")
                .sign_with_keys(&Keys::generate())
                .expect("sign"),
        )
    }

    // ★ LIVENESS TOOTH (a): a steady stream of events the watcher does NOT act on must not starve
    // the backstop sweep.
    //
    // This is the failure that keeps costing us: a liveness mechanism silently dead while
    // everything looks fine. A backstop re-armed per loop iteration is pushed back by every
    // arriving event — and non-actionable events reset it WITHOUT sweeping — so under steady
    // traffic it never fires, and the one case it exists for (a result event we missed) is exactly
    // the case it stops covering. A correctness tooth cannot see this: the loop still returns
    // correct results for every event it DOES get.
    //
    // Deterministic by construction — no relay, no paused clock, no wall-clock assertion. The sweep
    // action is injected and counted, and the event rate is an order of magnitude faster than the
    // backstop, so the two implementations separate hugely: fixed-cadence interval ⇒ many sweeps;
    // re-armed sleep ⇒ exactly the one boot sweep, forever.
    //
    // Red-on-revert: replace the `interval` with `tokio::time::sleep(interval)` inside the select
    // and this fails on `sweeps >= 4`, observing 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn steady_non_actionable_traffic_does_not_starve_the_backstop_sweep() {
        use std::sync::atomic::AtomicUsize;

        let (sender, receiver) = tokio::sync::broadcast::channel(64);
        let sweeps = Arc::new(AtomicUsize::new(0));

        let counter = sweeps.clone();
        let loop_task = tokio::spawn(async move {
            watch_loop(receiver, Duration::from_millis(10), move |_wake| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await;
        });

        // Flood non-actionable events an order of magnitude faster than the backstop for long
        // enough that a healthy loop must tick many times.
        let flood = tokio::spawn(async move {
            for _ in 0..300 {
                // A receiver-less send is fine; the loop is the only receiver and may be busy.
                let _ = sender.send(non_actionable_event());
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            // Hold the sender until the end so the subscription never closes during the flood.
            sender
        });
        let _sender = flood.await.expect("flood");

        let observed = sweeps.load(Ordering::SeqCst);
        loop_task.abort();
        assert!(
            observed >= 4,
            "the backstop must keep sweeping under steady non-actionable traffic; saw {observed} \
             sweep(s) across ~300ms at a 10ms cadence (a per-iteration sleep would show exactly 1)"
        );
    }

    // ★ LIVENESS TOOTH (b): losing the subscription DEGRADES the watcher to timer-only sweeps — it
    // must not stop it. Settling never depended on the relay handle (the collect path opens its own
    // client), so returning on `Closed` would strand every delivery from that moment on, silently.
    //
    // Asserts both halves: the loop observes `SubscriptionLost` exactly once (the arm that emits the
    // unconditional operator line — it cannot fire without executing that line), and sweeps continue
    // afterwards on the backstop alone.
    //
    // Red-on-revert: `return` on `RecvError::Closed` and this fails — no post-close sweeps.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lost_subscription_degrades_to_timer_only_instead_of_stopping() {
        use std::sync::atomic::AtomicUsize;

        let (sender, receiver) = tokio::sync::broadcast::channel(8);
        let lost = Arc::new(AtomicUsize::new(0));
        let after_loss = Arc::new(AtomicUsize::new(0));

        let (lost_counter, after_counter) = (lost.clone(), after_loss.clone());
        let loop_task = tokio::spawn(async move {
            watch_loop(receiver, Duration::from_millis(10), move |wake| {
                let (lost_counter, after_counter) = (lost_counter.clone(), after_counter.clone());
                async move {
                    if wake == WatchWake::SubscriptionLost {
                        lost_counter.fetch_add(1, Ordering::SeqCst);
                    } else if lost_counter.load(Ordering::SeqCst) > 0 {
                        after_counter.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
            .await;
        });

        // Drop the sender: the subscription closes.
        drop(sender);
        // Let the backstop tick several times past the close.
        tokio::time::sleep(Duration::from_millis(120)).await;

        let (losses, sweeps_after) = (lost.load(Ordering::SeqCst), after_loss.load(Ordering::SeqCst));
        loop_task.abort();
        assert_eq!(losses, 1, "the subscription loss must be observed exactly once, not spun on");
        assert!(
            sweeps_after >= 2,
            "the watcher must keep sweeping on the backstop after losing the subscription; saw \
             {sweeps_after} sweep(s) in ~120ms at a 10ms cadence"
        );
    }

    // ★ THE MONEY TOOTH for the delivery watcher. The watcher settles with no human in the loop, so
    // the question that matters is whether its entry to the spend gate is as gated as the RPC's. It
    // is, by construction — both call `settle_job` and nothing else — and this proves it end to end:
    // an awarded job whose delivered result carries a FORGED seller co-signature must cost NOTHING.
    //
    // Non-vacuity anchor: the watcher drives the full accept-then-pay path, and accept writes the
    // pay-bind BEFORE the pre-pay co-signature gate refuses. So the bind file EXISTING is positive
    // evidence the watcher really ran this job — without it, every "nothing happened" assertion
    // below would pass just as well if the watcher had never woken at all.
    //
    // The counterparty events are published from a SEPARATE client than the daemon's own session:
    // one nostr-sdk client cannot observe the events it published itself, and that trap bites
    // fixtures exactly as it bites production.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delivery_watcher_refuses_a_forged_delivery_and_spends_nothing() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, Keys};
        use nostr_sdk::secp256k1::Message;

        let root = temp_home("watch-forged");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");

        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        let relay_url = relay.url().await.to_string();
        home.config.relay_url = relay_url.clone();

        let buyer = Keys::parse(&crate::home::read_secret_key_hex(&home).expect("secret"))
            .expect("buyer keys");
        let buyer_hex = buyer.public_key().to_hex();
        let seller = Keys::generate();
        let seller_hex = seller.public_key().to_hex();
        let attacker = Keys::generate();

        let amount = 2u64;
        let task = "do a task";
        let output = "text";
        let deadline = now_unix() as u64 + 3_600;

        // Publish offer + claim + forged-cosig result from a client that is NOT the daemon's.
        let net = Client::new(Keys::generate());
        net.add_relay(&relay_url).await.expect("add relay");
        net.connect().await;
        net.wait_for_connection(Duration::from_secs(5)).await;
        let publish = |keys: &Keys, draft: &crate::gateway::EventDraft| {
            let builder = crate::gateway::nostr::event_builder(draft).expect("event builder");
            let event = builder.sign_with_keys(keys).expect("sign");
            let net = net.clone();
            let id = event.id;
            async move {
                net.send_event(&event).await.expect("publish");
                id
            }
        };

        let offer_draft =
            crate::gateway::OfferDraft::new(task, output, amount, deadline, &seller_hex)
                .to_event_draft();
        let job_id = publish(&buyer, &offer_draft).await.to_hex();

        let creq = crate::gateway::creq::build_seller_creq(
            &job_id,
            amount,
            "sat",
            &[crate::home::DEFAULT_MINT_URL.to_string()],
            &seller_hex,
        )
        .expect("creq");
        let claim_draft = crate::gateway::claim_draft(&job_id, &buyer_hex, &seller_hex, &creq);
        let _ = publish(&seller, &claim_draft).await;

        let job_hash = job_lifecycle::job_hash_for_offer(&job_id, task, amount);
        let forged = attacker
            .sign_schnorr(&Message::from_digest([0x11u8; 32]))
            .to_string();
        let git = crate::gateway::GitResultTags {
            repo: "https://example.invalid/repo.git",
            branch: "main",
            commit_sha: &"a".repeat(40),
        };
        let result_draft = crate::gateway::result_draft(
            &job_id, &buyer_hex, output, amount, &job_hash, &forged, "", Some(git), &[],
        );
        let _ = publish(&seller, &result_draft).await;

        let (_lock, context, _socket_path) = bootstrap(home).await.expect("buyer bootstrap");

        // Seed exactly what the award path writes: the reservation, then the published-award row.
        context
            .store
            .reserve(&job_id, amount, 1_000, u64::MAX, 0, now_unix())
            .expect("reserve");
        context
            .store
            .record_award(&job_id, &"c".repeat(64), &"e".repeat(64), &seller_hex, amount, now_unix())
            .expect("record award");
        assert_eq!(
            context.store.awarded_unsettled_job_ids().expect("awarded"),
            vec![job_id.clone()],
            "the seeded award must be the watcher's work set"
        );

        // Drive the watcher's settle pass directly — the same call its event and tick paths make.
        settle_awarded(&context, None).await;

        // Non-vacuity: the watcher really drove accept-then-pay for this job.
        assert!(
            context.home.root.join("jobs").join(format!("{job_id}.json")).exists(),
            "the watcher must have driven the accept path (no bind ⇒ it never ran this job, and \
             every assertion below would be vacuous)"
        );
        // And the money gate refused: nothing spent, no payment journal, no files, and the
        // reservation still held (a refused settle must never drop it and over-state `available`).
        assert_eq!(
            BudgetGate::from_home(&context.home).expect("gate").spent(),
            0,
            "a refused delivery must burn zero spend"
        );
        assert!(
            !context.home.root.join("payment-journal").exists(),
            "no payment journal may exist on a pre-pay refusal"
        );
        assert!(
            !context.home.root.join("results").join(&job_id).exists(),
            "a refused delivery must materialize no files"
        );
        assert!(
            matches!(
                context.store.reservation(&job_id).expect("reservation"),
                Some((reservations::ReservationState::Reserved, _))
            ),
            "a refused settle must leave the reservation reserved"
        );

        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_round_trips_over_the_socket() {
        let root = temp_home("status");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap_home(&root).expect("bootstrap home");
        let secret = crate::home::read_secret_key_hex(&home).expect("secret");

        let (_lock, context, socket_path) = bootstrap(home).await.expect("buyer bootstrap");
        let listener = bind_socket(&socket_path).expect("bind socket");
        let server = tokio::spawn(accept_loop(listener, context));

        // The thin client is synchronous; drive it off the runtime.
        let sock = socket_path.clone();
        let response = tokio::task::spawn_blocking(move || client::status(&sock))
            .await
            .expect("join client")
            .expect("client call");

        let result = response.result.expect("status result");
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["wallet"]["balance_sats"], json!(0));
        assert_eq!(result["store"]["schema_version"], json!(store::SCHEMA_VERSION));
        let pubkey = result["pubkey"].as_str().expect("pubkey string");
        assert_eq!(pubkey.len(), 64);
        // The socket surface must never leak the secret key.
        assert!(!response_contains(&result, &secret), "status must not echo the secret key");

        // A recognized-but-folded trade method (accept_claim is folded into collect) returns a
        // structured NOT_IMPLEMENTED error, never a silent success. The live trade methods
        // (post_job/get_job/award/collect) are exercised end to end elsewhere; this only asserts
        // the daemon still answers a recognized-but-unrouted method with a structured error.
        let sock = socket_path.clone();
        let deferred = tokio::task::spawn_blocking(move || {
            client::call(&sock, "accept_claim", json!({}))
        })
        .await
        .expect("join")
        .expect("call");
        let error = deferred.error.expect("accept_claim must be a structured error");
        assert_eq!(error.code, CODE_NOT_IMPLEMENTED);

        // Socket is user-only (0600).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&socket_path)
                .expect("socket metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "buyer.sock must be user-only");
        }

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    fn response_contains(value: &Value, needle: &str) -> bool {
        value.to_string().contains(needle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_buyer_on_same_home_fails_closed() {
        let root = temp_home("exclusive");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap_home(&root).expect("bootstrap home");

        // First buyer holds the lock and the wallet.
        let (_lock, _context, _sock) = bootstrap(home.clone()).await.expect("first buyer");

        // A second bootstrap on the same home must fail closed at the lock — before
        // it ever opens the wallet.
        let second = bootstrap(home).await;
        let failed_closed = matches!(&second, Err(BuyerError::Lock(LockError::Held { .. })));
        // Drop any accidentally-acquired context without needing Debug on it.
        drop(second);
        assert!(
            failed_closed,
            "second buyer must fail closed on the home lock (LockError::Held)"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- reconcile plumbing (scan_payment_progress / plan_reconcile / merge_progress) ----------

    use std::str::FromStr;
    use crate::payment::{
        DeliveryIntegrityHash, JobHash, JobId, PaymentKey, PaymentRecord, PaymentState, PaymentTerms,
        ResultId,
    };

    fn payment_key(job_id: &str) -> PaymentKey {
        let cashu_pk = cashu::SecretKey::from_slice(&[7u8; 32]).expect("secret").public_key();
        let nostr_pk = nostr_sdk::PublicKey::from_hex(&cashu_pk.to_string()[2..]).expect("nostr pk");
        let terms = PaymentTerms::new(
            cashu::MintUrl::from_str("https://testnut.cashu.space").expect("mint"),
            cashu::Amount::from(7),
            cashu::CurrencyUnit::Sat,
            nostr_pk,
            cashu_pk,
        );
        PaymentKey::new(
            JobId::new(job_id).expect("job id"),
            ResultId::new("result").expect("result id"),
            DeliveryIntegrityHash::from_hex("11".repeat(32)).expect("dih"),
            JobHash::from_hex("22".repeat(32)).expect("job hash"),
            &terms,
            None,
        )
    }

    fn write_journal(root: &std::path::Path, filename: &str, records: &[PaymentRecord]) {
        let dir = root.join("payment-journal");
        std::fs::create_dir_all(&dir).expect("journal dir");
        let mut body = String::new();
        for record in records {
            body.push_str(&serde_json::to_string(record).expect("record json"));
            body.push('\n');
        }
        std::fs::write(dir.join(filename), body).expect("write journal");
    }

    // No payment-journal directory ⇒ no payments (empty map). scan must never fabricate progress.
    #[test]
    fn scan_payment_progress_absent_journal_is_empty() {
        let root = temp_home("scan-absent");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("home dir");
        assert!(scan_payment_progress_at(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    // An Intent-only journal folds to Intent ⇒ no funds have left ⇒ PaymentProgress::None. Proves
    // scan walks the dir, parses real PaymentRecords, folds them, and maps by job id.
    #[test]
    fn scan_payment_progress_folds_intent_to_none() {
        let root = temp_home("scan-intent");
        let _ = std::fs::remove_dir_all(&root);
        let job = "a".repeat(64);
        let key = payment_key(&job);
        write_journal(
            &root,
            "attempt.jsonl",
            &[PaymentRecord { key: key.clone(), value: PaymentState::Intent { attempt_id: key.attempt_id() } }],
        );
        let progress = scan_payment_progress_at(&root);
        assert_eq!(progress.get(&job), Some(&PaymentProgress::None));
        let _ = std::fs::remove_dir_all(&root);
    }

    // A journal that does not fold (a lone Locked with no preceding Intent is an illegal transition)
    // is treated as Uncertain — reconcile must KEEP such a reservation, never release it on ambiguous
    // evidence. This is the fail-safe the money path depends on.
    #[test]
    fn scan_payment_progress_unfoldable_journal_is_uncertain() {
        let root = temp_home("scan-uncertain");
        let _ = std::fs::remove_dir_all(&root);
        let job = "b".repeat(64);
        let key = payment_key(&job);
        write_journal(
            &root,
            "attempt.jsonl",
            &[PaymentRecord { key: key.clone(), value: PaymentState::Locked { attempt_id: key.attempt_id() } }],
        );
        let progress = scan_payment_progress_at(&root);
        assert_eq!(progress.get(&job), Some(&PaymentProgress::Uncertain));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// scan_payment_progress reads `<home>/payment-journal`; the fn takes a `MobeeHome`, so drive it
    /// through a bootstrapped home rooted at `root`.
    fn scan_payment_progress_at(root: &std::path::Path) -> BTreeMap<String, PaymentProgress> {
        let home = bootstrap_home(root).expect("home");
        scan_payment_progress(&home)
    }

    // plan_reconcile maps each reserved job by (payment progress, still-payable) via the classifier:
    // Closed ⇒ Paid; Uncertain ⇒ Payable (kept); None+payable ⇒ Payable; None+not-payable ⇒ Dead.
    #[test]
    fn plan_reconcile_maps_each_reserved_job() {
        let paid = "a".repeat(64);
        let uncertain = "b".repeat(64);
        let live = "c".repeat(64);
        let dead = "d".repeat(64);
        let reserved = vec![paid.clone(), uncertain.clone(), live.clone(), dead.clone()];

        let mut progress = BTreeMap::new();
        progress.insert(paid.clone(), PaymentProgress::Closed);
        progress.insert(uncertain.clone(), PaymentProgress::Uncertain);
        // `live` and `dead` have no payment progress (None).

        let mut payable = BTreeMap::new();
        payable.insert(paid.clone(), false); // a Closed payment is Paid regardless of liveness
        payable.insert(uncertain.clone(), false); // Uncertain is kept regardless of liveness
        payable.insert(live.clone(), true);
        payable.insert(dead.clone(), false);

        let dispositions = plan_reconcile(&reserved, &progress, &payable);
        assert_eq!(dispositions[&paid], reservations::JobDisposition::Paid);
        assert_eq!(dispositions[&uncertain], reservations::JobDisposition::Payable);
        assert_eq!(dispositions[&live], reservations::JobDisposition::Payable);
        assert_eq!(dispositions[&dead], reservations::JobDisposition::Dead);
    }

    // A job missing from `payable` defaults to payable — never released without positive evidence of
    // death (the #140 conservative posture: a relay re-read that lost the job is not proof it died).
    #[test]
    fn plan_reconcile_missing_payable_defaults_to_kept() {
        let job = "e".repeat(64);
        let dispositions = plan_reconcile(&[job.clone()], &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(dispositions[&job], reservations::JobDisposition::Payable);
    }

    // merge_progress keeps the MORE-advanced progress so a Closed attempt is never masked by an
    // earlier Intent/Uncertain (Closed > Uncertain > None).
    #[test]
    fn merge_progress_keeps_the_more_advanced() {
        assert_eq!(merge_progress(Some(PaymentProgress::None), PaymentProgress::Closed), PaymentProgress::Closed);
        assert_eq!(merge_progress(Some(PaymentProgress::Closed), PaymentProgress::None), PaymentProgress::Closed);
        assert_eq!(merge_progress(Some(PaymentProgress::Uncertain), PaymentProgress::None), PaymentProgress::Uncertain);
        assert_eq!(merge_progress(None, PaymentProgress::Uncertain), PaymentProgress::Uncertain);
    }
}
