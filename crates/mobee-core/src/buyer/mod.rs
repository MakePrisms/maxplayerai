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
    match reconcile_on_start(&context).await {
        Ok(report) => *context.last_reconcile.lock().await = Some(report),
        Err(error) => eprintln!(
            "buyer: reconcile-on-start did not complete ({error}); serving with the ledger as-is"
        ),
    }
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
    match job_lifecycle::get_job_async(&context.home, request).await {
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
async fn collect(context: &BuyerContext, id: Value, params: Value) -> Response {
    let params: CollectParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => return Response::err(id, CODE_METHOD_NOT_FOUND, format!("collect params: {error}")),
    };

    // Serialize with award + other collects: at most one wallet-melting op in flight daemon-wide.
    let _guard = context.money_lock.lock().await;

    let mut gate = match BudgetGate::from_home(&context.home) {
        Ok(gate) => gate,
        Err(error) => return Response::err(id, CODE_INTERNAL, error.to_string()),
    };
    let request = CollectRequest {
        job_id: params.job_id.clone(),
        out: params.out,
    };

    // Pay FIRST (append + melt), flip AFTER — the #123/#126 ordering, via the tested seam.
    let settled = lifecycle::settle_after_pay(
        &context.store,
        &params.job_id,
        now_unix(),
        || collect::collect_async(&context.home, &mut gate, request),
        |outcome| outcome.pay.amount_sats,
    )
    .await;

    match settled {
        Ok((outcome, _converted)) => Response::ok(
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
        Err(SettleError::Pay(error)) => Response::err(id, CODE_REFUSED, error.to_string()),
        Err(SettleError::Store(error)) => Response::err(id, CODE_INTERNAL, error.to_string()),
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

/// Reconcile every still-`Reserved` job against relay + payment-journal truth: a job the relay no
/// longer shows payable (and that has left no funds) is released; a job whose payment journal shows
/// a `Closed` attempt is converted to `spent`; an ambiguous (Sent-not-Closed) payment is KEPT (the
/// phase-3 saga owns it). Pure classification is [`lifecycle::classify_disposition`]; this gathers
/// its inputs and applies the batch through [`BuyerStore::reconcile`].
async fn reconcile_on_start(context: &BuyerContext) -> Result<ReconcileReport, String> {
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
    context
        .store
        .reconcile(&dispositions, now_unix())
        .map_err(|error| error.to_string())
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
