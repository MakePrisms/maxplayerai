//! The persistent per-home **maxplayer buyer** (step 1 of the stateful-buyer design, #127).
//!
//! One daemon owns a home. It takes an exclusive OS lock on `$MAXPLAYER_HOME/buyer.lock`
//! (a second daemon on the same home fails closed), opens the CDK wallet and the
//! Nostr identity behind serialized in-process actors, opens the durable state DB
//! `$MAXPLAYER_HOME/buyer.sqlite`, and serves a small JSON-RPC surface over the
//! user-only Unix socket `$MAXPLAYER_HOME/buyer.sock`. Every other process is a thin,
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

/// #574 platform-contract test that the client drops signature-invalid events at relay ingest.
#[cfg(test)]
mod ingest_sig_it;
pub mod wallet_actor;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::budget::BudgetGate;
use crate::buyer_fund::{self, FundError};
use crate::collect::{self, CollectRequest};
use crate::home::{self, HomeError, MaxplayerHome};
use crate::job_lifecycle::{
    self, AwardClaimRequest, ContributionSpec, GetJobRequest, JobKind, PostJobRequest, WaitFor,
};
use crate::payment::{PaymentMachine, PaymentRecord, PaymentState};
use lifecycle::{
    AwardError, AwardFilters, AwardOutcome, MissingOfferAction, PaymentProgress, RearmAction,
    SettleError,
};
use lock::{HomeLock, LockError};
use protocol::{CODE_INTERNAL, CODE_METHOD_NOT_FOUND, CODE_NOT_IMPLEMENTED, Request, Response};
use reservations::{Dispositions, JobDisposition, ReconcileReport};
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
/// How many consecutive UNANSWERED job reads the auto-award loop tolerates before it parks.
///
/// #291 made an unconfirmed read stop being grounds to park — but a refusal with no bound is an
/// infinite loop, and the deadline check that bounds the normal path lives inside the branch that
/// requires an offer, so it is never reached when the read comes back empty. A ceiling without a
/// floor is half a fix. What this bound must NOT do is smuggle the old conclusion back in: it parks
/// on "we could not read", never on "the offer is gone", and the recorded reason says so.
const AUTO_AWARD_MAX_UNCONFIRMED_READS: u32 = 12;
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
    home: MaxplayerHome,
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

/// Raise this process's soft open-file limit to its hard limit (#774).
///
/// ⚠ THIS IS NOT THE FIX FOR #774, AND MUST NOT BE READ AS ONE. The fix is bounding the buyer
/// store's packfile count (`crate::store_maint`); descriptor demand is what that removes. This is
/// defence in depth for the interval before a compaction runs, and for the first compaction of a
/// store that predates it — the one case where the compaction's own descriptor need is set by the
/// pack count it inherited.
///
/// An unprivileged process can raise its SOFT limit up to the hard one but can never raise the
/// HARD one. On a host whose hard ceiling is also low this buys exactly nothing, which is the
/// reason it cannot be the fix. What it does buy is independence from a launchd plist or a systemd
/// `LimitNOFILE=` being set correctly on every machine an operator installs on.
///
/// Best-effort and deliberately silent on failure: a daemon that cannot raise its own limit should
/// still start, at the limit it was given.
#[cfg(unix)]
fn raise_open_file_limit() {
    // Safety: `getrlimit`/`setrlimit` are plain libc calls over a fully initialized local struct.
    unsafe {
        let mut limit = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        // An unbounded hard limit is not a licence to ask for an unbounded soft one: some
        // platforms cap NOFILE well below `RLIM_INFINITY` and refuse the call outright, and code
        // that sizes descriptor tables from the soft limit behaves badly with it. Ask for a large
        // finite number instead, which is far past anything a delivery store needs.
        const AMPLE: libc::rlim_t = 65_536;
        let target = if limit.rlim_max == libc::RLIM_INFINITY {
            AMPLE
        } else {
            limit.rlim_max
        };
        if target <= limit.rlim_cur {
            return;
        }
        limit.rlim_cur = target;
        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
    }
}

/// Non-unix targets have no `RLIMIT_NOFILE` to raise; the compaction in `crate::store_maint` is
/// the whole of the fix there.
#[cfg(not(unix))]
fn raise_open_file_limit() {}

/// Bring up the buyer's owned resources: take the exclusive lock, open the state
/// DB and record the start, then open the wallet and identity behind their
/// serialized actors. Returns the held lock (keep it alive for the buyer's life),
/// the shared context, and the socket path to bind.
///
/// Fails closed at the lock step if another daemon already owns this home.
async fn bootstrap(home: MaxplayerHome) -> Result<(HomeLock, Arc<BuyerContext>, PathBuf), BuyerError> {
    raise_open_file_limit();

    let lock = HomeLock::acquire(home.root.join(LOCK_FILE))?;

    let store = BuyerStore::open(home.root.join(STATE_DB_FILE))?;
    let started_at_unix = now_unix();
    store.record_start(started_at_unix)?;

    // The daemon is the ONLY opener of the CDK wallet — this is what the exclusive
    // home lock protects. Opening touches the local sqlite store only (no network).
    let wallet = buyer_fund::open_wallet_async(&home).await?;
    // Hand the actor a home clone so it can enumerate per-mint balances for `status` (#496) without a
    // second wallet opener; the daemon's exclusive home lock still guards across processes.
    let wallet = wallet_actor::spawn(wallet, home.clone());

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
pub async fn run(home: MaxplayerHome) -> Result<(), BuyerError> {
    // `_lock` is held for the whole run; dropping it releases the OS lock.
    let (_lock, context, socket_path) = bootstrap(home).await?;
    // Reconcile the reservation ledger against relay + journal truth before serving: a reservation
    // orphaned by a prior crash (dead job → release, paid job → spent) is resolved here, so the
    // daemon starts from a converged ledger. A failure is logged, not fatal — an unreachable relay
    // must not keep the daemon from coming up (the stale reservation is conservative until the next
    // reconcile).
    run_reconcile_pass(&context).await;
    // Finish any cross-mint hop a prior run left mid-flight, before serving. A hop whose melt landed
    // but whose ecash was never issued is money sitting in neither wallet, and nothing else in the
    // daemon goes looking for it — the pay attempt that started it may never be retried. Logged, not
    // fatal, for the same reason as the reconcile above: a daemon that refuses to start is a daemon
    // the operator cannot use to fix anything. An unrecoverable hop prints its own loud line.
    run_hop_sweep(&context).await;
    // Backfill attribution for awards that settled WITHOUT it (#261): the settle-time write is
    // advisory and post-flip, so a crash in that window — or a flip-fail later converged by the
    // reconcile pass above — strands a paid row at NULL while the durable accept-bind still holds
    // the seller's report. NULL must mean "seller never reported", so boot re-reads the bind and
    // lands it through the same row-level write-once gate (it can never rewrite recorded truth).
    heal_award_attribution(&context.store, &context.home);
    // The award-attempt sweep (#322) rides the reconcile task — its boot pass runs there first,
    // spawned off the socket's 10s readiness budget (`daemon::ensure` gives up and every
    // cold-start MCP call fails if one slow relay eats it), and one task means the boot pass and
    // the tick passes can never overlap each other. Ordering against the re-armed tasks below is
    // not load-bearing — every money decision is made under the money lock and converges on the
    // pinned attempt.
    //
    // Re-arm pending auto-awards left by a prior run: a job posted before a crash still gets its
    // award with zero manual commands. Each task re-checks the relay for an existing award first
    // (invariant A), so re-arming never double-awards.
    match context.store.list_pending_awards() {
        Ok(pending) => {
            for intent in pending {
                spawn_auto_award(context.clone(), intent.job_id, intent.max_sats);
            }
        }
        Err(error) => crate::opline!("buyer: could not list pending auto-awards to re-arm: {error}"),
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
    /// Auto-award preferences recorded with the intent. BOTH are also posted on the offer and are
    /// therefore hard award filters: only a seller advertising them can be awarded.
    ///
    /// `harness` names a PRESET and is matched against the claim's `agents`. `model` (#897) is matched
    /// against the family/model PAIR a seat advertises, and so REQUIRES `harness_family` — a model
    /// with no family refuses every claim rather than being ignored (#788).
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// Harness FAMILY the job requires (#897). Posted on the offer and enforced as a hard award
    /// filter on BOTH award paths. Distinct from `harness`, which names a preset: a family spans the
    /// presets sharing a harness, so a family request binds dispatch where a preset binds a
    /// configuration. Both may be given and both are then enforced.
    #[serde(default)]
    harness_family: Option<String>,
    /// Capability tokens the job requires (#897) — a subset of
    /// [`maxplayer_core::capability::CAPABILITIES`]. Posted on the offer and enforced as a hard
    /// award filter on BOTH award paths. Omitted or empty ⇒ no requirement.
    #[serde(default)]
    capabilities: Option<Vec<String>>,
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
        requested_agent: harness.clone(),
        requested_harness_family: params.harness_family,
        // #897: `model` now reaches the WIRE as well as the intent. It stays recorded on the intent
        // because that is a separate fact — what the buyer asked for locally — from what the signed
        // offer says, and the award filter reads only the offer.
        requested_model: model.clone(),
        required_capabilities: params.capabilities.unwrap_or_default(),
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
                crate::opline!(
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

/// The `get_job` wire response: the relay-truth [`job_lifecycle::JobView`] plus the buyer-LOCAL
/// committed award, which the view builder cannot carry (it reads the relay and the local accept
/// file and holds no store handle). #481: once the buyer awards, `get_job` must SHOW the
/// commitment. A signed 3405 award is a spend already decided; telemetry that omits it reads as
/// "nothing committed" at the exact moment that is false, and an operator can misjudge an
/// in-flight, unstoppable award as still stoppable. `accepted` (kind-3406) is a distinct, later
/// fact and is deliberately left unchanged — award, delivery, and accept are three separate facts.
#[derive(Serialize)]
struct GetJobResponse {
    #[serde(flatten)]
    view: job_lifecycle::JobView,
    /// The published award for this job, present iff the buyer has committed one. Sourced from the
    /// local award store — its mere existence IS the "award committed" fact, since the row is
    /// written only after the 3405 is confirmed public.
    #[serde(skip_serializing_if = "Option::is_none")]
    awarded: Option<AwardedView>,
    /// True when: an award is committed (`awarded` is `Some`), the awarded seller has published a
    /// payable+delivered result, AND this job has no local accepted bind — money is owed and
    /// collectable but settlement has not run. Independent of `live_claim_id` (#540's whole point);
    /// any accepted bind makes it false because settlement is per job, not per claim. Always
    /// omitted when false. (#544, fast-follow of #540 point 5.)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    awarded_delivery_pending: bool,
}

/// Serializable projection of a committed [`store::AwardRecord`] for the `get_job` response
/// ([`store::AwardRecord`] is not itself `Serialize`). Same field shape the `award` RPC already
/// returns under `already_awarded`, so the two award surfaces read identically.
#[derive(Serialize)]
struct AwardedView {
    job_id: String,
    claim_id: String,
    award_event_id: String,
    seller_pubkey: String,
    amount_sats: u64,
    awarded_at_unix: i64,
}

impl From<store::AwardRecord> for AwardedView {
    fn from(record: store::AwardRecord) -> Self {
        Self {
            job_id: record.job_id,
            claim_id: record.claim_id,
            award_event_id: record.award_event_id,
            seller_pubkey: record.seller_pubkey,
            amount_sats: record.amount_sats,
            awarded_at_unix: record.awarded_at_unix,
        }
    }
}

/// Refuse a caller-supplied long-poll timeout above the effective ceiling at the RPC boundary,
/// before any relay work can begin. The lifecycle layer keeps its clamp as a defensive backstop for
/// non-RPC callers.
fn get_job_timeout_error(id: Value, timeout_secs: Option<u64>) -> Option<Response> {
    let timeout_secs = timeout_secs?;
    if timeout_secs <= job_lifecycle::WAIT_FOR_CAP_SECS {
        return None;
    }
    Some(Response::err(
        id,
        CODE_METHOD_NOT_FOUND,
        format!(
            "timeout_secs={timeout_secs} exceeds the get_job long-poll cap of {}s \
             (bounded well under the MCP tool deadline); omit timeout_secs for the \
             default or pass a value <= {}",
            job_lifecycle::WAIT_FOR_CAP_SECS,
            job_lifecycle::WAIT_FOR_CAP_SECS,
        ),
    ))
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
    if let Some(error) = get_job_timeout_error(id.clone(), params.timeout_secs) {
        return error;
    }
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
        Ok(view) => {
            // #481: enrich the relay-truth view with the buyer-LOCAL committed award. The view
            // builder has no store handle, so the lookup lives here, at the RPC boundary where
            // `context.store` is in scope. A store read error must NOT sink the whole response —
            // the relay view is still useful — so it degrades to `None` and is logged, never
            // surfaced as a false "no award".
            let awarded = match context.store.award_record(&view.job_id) {
                Ok(record) => record.map(AwardedView::from),
                Err(error) => {
                    crate::opline!("get_job: could not read {}'s award row: {error}", view.job_id);
                    None
                }
            };
            let awarded_delivery_pending = awarded.as_ref().is_some_and(|a| {
                job_lifecycle::awarded_delivery_pending(&view, &a.claim_id, &a.seller_pubkey)
            });
            Response::ok(id, json!(GetJobResponse { view, awarded, awarded_delivery_pending }))
        }
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

/// Whether a manual award call NAMES a claim that contradicts the pinned attempt (#322). Pinned
/// awards are write-once, so a different `claim_id` can never be honored — refusing loudly beats
/// silently awarding a claim the caller did not sanction. Omitting `claim_id`, or naming the
/// pinned one, resolves the pinned attempt. Pure so the refusal is unit-testable.
fn pinned_claim_conflict(pinned_claim_id: &str, named: Option<&str>) -> bool {
    named.is_some_and(|named| named != pinned_claim_id)
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

    // ⚠ The money lock is taken BELOW, after the pinned-attempt handling — deliberately. The
    // expired-attempt resolution called in that block re-enters the chokepoint helper, which
    // takes `money_lock` itself; taking it here first self-deadlocked the daemon (round-2
    // review: tokio's Mutex is non-reentrant, and the wedged task held the guard forever,
    // freezing every money path). The reads in between are advisory — the chokepoint re-derives
    // every decision from fresh reads under its own guard.

    // A pinned attempt short-circuits selection (#322): once a signed award exists for this job —
    // whatever its verdict so far — its claim and its exact bytes ARE the job's award, and this
    // call's task is to resolve THAT attempt, not to choose again. Skipping the view fetch is not
    // an optimisation but the point: re-validating a sealed decision would be deciding twice.
    // (`max_sats` is likewise sealed: the amount was fixed when the attempt was pinned, so the
    // parameter applies to first calls only — documented on the MCP tool.)
    let attempt = match context.store.award_attempt(&params.job_id) {
        Ok(attempt) => attempt,
        Err(error) => return Response::err(id, CODE_INTERNAL, error.to_string()),
    };
    if let Some(attempt) = &attempt {
        if pinned_claim_conflict(&attempt.claim_id, params.claim_id.as_deref()) {
            return Response::err(
                id,
                CODE_REFUSED,
                format!(
                    "job {} already has a pinned award attempt for claim {} (state: {:?}); \
                     awards are write-once per offer, so another claim can never be awarded \
                     here. Retry without claim_id (or with the pinned one) to resolve the \
                     existing attempt",
                    params.job_id, attempt.claim_id, attempt.state
                ),
            );
        }
        // Past the offer deadline nothing may be transmitted — a retry here would inject a LATE
        // award the seller would burn compute on (its award arm has no deadline check). Resolve
        // by probe instead, exactly as the sweep does, and report the durable truth.
        if attempt.state == store::AttemptState::Pending
            && now_unix() > attempt.offer_deadline_unix
        {
            resolve_expired_attempt(context, &keys, attempt).await;
            if let Ok(Some(record)) = context.store.award_record(&params.job_id) {
                let _ = context.store.mark_award_awarded(&params.job_id, now_unix());
                return Response::ok(
                    id,
                    json!({
                        "already_awarded": {
                            "job_id": record.job_id,
                            "claim_id": record.claim_id,
                            "award_event_id": record.award_event_id,
                            "seller_pubkey": record.seller_pubkey,
                            "amount_sats": record.amount_sats,
                            "awarded_at_unix": record.awarded_at_unix,
                        },
                        "published_now": false,
                        "reserved_for": params.job_id,
                    }),
                );
            }
            return match context.store.award_attempt(&params.job_id) {
                Ok(Some(after)) if after.state == store::AttemptState::Refused => Response::err(
                    id,
                    CODE_REFUSED,
                    format!(
                        "the pinned award for job {} was refused: {}",
                        params.job_id,
                        after.detail.unwrap_or_else(|| "no detail recorded".to_owned())
                    ),
                ),
                // The award IS public (the probe confirmed it) but writing its awards row
                // failed — the opposite of "unresolved". Name the truth and the operator action.
                Ok(Some(after)) if after.state == store::AttemptState::Confirmed => Response::err(
                    id,
                    CODE_REFUSED,
                    format!(
                        "award {} for job {} is public on the relay but its local awards row \
                         could not be written; it will not auto-settle until it exists — run \
                         `collect {}` (the sweep also retries the heal)",
                        after.award_event_id, params.job_id, params.job_id
                    ),
                ),
                _ => Response::err(
                    id,
                    CODE_INTERNAL,
                    format!(
                        "the pinned award for job {} is past its offer deadline and its relay \
                         verdict is still unresolved; nothing was transmitted (a late send would \
                         burn seller compute) and the probe re-runs on every pass — retry later; \
                         retries never mint a second award",
                        params.job_id
                    ),
                ),
            };
        }
    }
    let (award_amount, claim_id, send_relay) = match &attempt {
        Some(attempt) => {
            (attempt.amount_sats, attempt.claim_id.clone(), attempt_relay(attempt, &context.home))
        }
        None => {
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
                return Response::err(
                    id,
                    CODE_INTERNAL,
                    format!("no offer on the relay for job {}", params.job_id),
                );
            };
            let offer_amount = offer.amount_sats;
            let max_sats = params.max_sats.unwrap_or(offer_amount);
            // ONE constructor, shared with `drive_auto_award` — the capability request (#897) and
            // every other filter come from the SIGNED OFFER, never from award params, so the request
            // cannot be changed after the fact. Sharing the constructor is what makes "both paths
            // filter identically" structural instead of a convention someone has to keep noticing.
            let filters = lifecycle::award_filters_for_offer(
                offer,
                max_sats,
                context.home.config.default_mint(),
                context.home.config.allow_real_mints,
            );

            // Manual award names the claim but applies the SAME hard filters as auto-award —
            // max_sats, price, mint AND the #784 capability request. Naming a claim chooses which
            // claim is judged, never whether it is. Auto-award selects the first live payable
            // claim that passes those same filters.
            let claim_id = match params.claim_id.clone() {
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
                            format!(
                                "no awardable claim for job {} (none live/payable/mint-compatible)",
                                params.job_id
                            ),
                        );
                    }
                },
            };
            (offer_amount, claim_id, context.home.config.relay_url.clone())
        }
    };

    // Serialize with collect: the reserve below reads a balance/spent snapshot that must not race
    // a concurrent melt. Held across the whole chokepoint call (see the deadlock note above for
    // why not earlier).
    let _guard = context.money_lock.lock().await;
    // Re-derive BOTH pinned-attempt gates from a fresh read under the guard. The reads above ran
    // before the lock, and the wait to get here (view fetches, then the guard itself — unbounded
    // behind a settle) is long enough for an attempt to be pinned by a concurrent path or for
    // the deadline to cross.
    if let Ok(Some(current)) = context.store.award_attempt(&params.job_id) {
        // A claim named by the caller must still be refused when an attempt pinned MEANWHILE
        // names another: silently resolving a claim the caller never sanctioned is the thing
        // `pinned_claim_conflict` exists to prevent, and the pre-lock check cannot see a pin
        // that landed after it.
        if pinned_claim_conflict(&current.claim_id, params.claim_id.as_deref()) {
            return Response::err(
                id,
                CODE_REFUSED,
                format!(
                    "job {} was pinned to award claim {} while this call was queued (state: \
                     {:?}); awards are write-once per offer, so another claim can never be \
                     awarded here. Retry without claim_id (or with the pinned one) to resolve \
                     the existing attempt",
                    params.job_id, current.claim_id, current.state
                ),
            );
        }
        // The ResumeAttempt arm re-sends without re-deriving liveness, so a crossed deadline
        // must bounce to the probe path instead of transmitting late.
        if resume_crossed_deadline(&current, now_unix()) {
            return Response::err(
                id,
                CODE_REFUSED,
                format!(
                    "the offer deadline for job {} passed while this call was queued; nothing \
                     was transmitted (a late send would burn seller compute) — retry to resolve \
                     the pinned attempt by probe",
                    params.job_id
                ),
            );
        }
    }
    let (balance, _spent) = match money_snapshot(context).await {
        Ok(snapshot) => snapshot,
        Err(error) => return Response::err(id, CODE_INTERNAL, error),
    };

    let job_id = params.job_id.clone();
    let home = context.home.clone();
    let publish_claim = claim_id.clone();
    let probe_home = context.home.clone();
    let probe_keys = keys.clone();
    let probe_job = params.job_id.clone();
    let send_keys = keys.clone();
    let result = lifecycle::award_with_reservation(
        &context.store,
        &params.job_id,
        award_amount,
        balance,
        now_unix(),
        move || async move {
            job_lifecycle::award_presence_async(&probe_home, &probe_keys, &probe_job, RELAY_TIMEOUT)
                .await
        },
        move || async move {
            job_lifecycle::prepare_award_async(
                &home,
                AwardClaimRequest { job_id, claim_id: publish_claim },
            )
            .await
        },
        // Targets the PINNED relay on a resume (the bytes' relay), live config on a first call.
        move |event_json: String, expected_event_id: String| async move {
            job_lifecycle::send_signed_award_async(
                &send_keys,
                &send_relay,
                &expected_event_id,
                &event_json,
            )
            .await
        },
        // These paths transmit from inside the chokepoint, so it owns the license.
        None,
    )
    .await;

    // #291, second defect: the intent row is advanced only by the AUTO path, so a manual award left
    // a stale row behind and `status` went on reporting `parked / offer no longer on the relay` for
    // a job that was awarded and running. BOTH success shapes clear it, `AlreadyAwarded` included —
    // a parked reason sitting beside a recorded award is false whichever call published the award.
    if matches!(
        result,
        Ok(AwardOutcome::Published(_) | AwardOutcome::AlreadyAwarded(_))
    ) {
        let _ = context.store.mark_award_awarded(&params.job_id, now_unix());
    }

    match result {
        Ok(AwardOutcome::Published(outcome)) => Response::ok(
            id,
            json!({
                "awarded": outcome,
                "reserved_sats": award_amount,
                "reserved_for": params.job_id,
            }),
        ),
        // Already awarded by this buyer: report the recorded award and say plainly that nothing was
        // published now, rather than returning a shape indistinguishable from a fresh award.
        Ok(AwardOutcome::AlreadyAwarded(record)) => Response::ok(
            id,
            json!({
                "already_awarded": {
                    "job_id": record.job_id,
                    "claim_id": record.claim_id,
                    "award_event_id": record.award_event_id,
                    "seller_pubkey": record.seller_pubkey,
                    "amount_sats": record.amount_sats,
                    "awarded_at_unix": record.awarded_at_unix,
                },
                "published_now": false,
                "reserved_for": params.job_id,
            }),
        ),
        Err(AwardError::Reserve(refused)) => {
            // #539: the operator console records the affordability refusal, not only the RPC caller —
            // a script-driven award otherwise fails with nothing in the daemon log.
            crate::opline!("{}", cannot_afford_award_line(&params.job_id, &refused));
            Response::err(id, CODE_REFUSED, refused.to_string())
        }
        // Presence refusals are REFUSED, not INTERNAL: nothing broke, the daemon declined to
        // publish a second award. The message names the operator action. A relay REFUSAL of the
        // pinned event is the same class — the daemon is reporting a terminal verdict, not a
        // fault; the message names the recovery (a new offer).
        Err(
            error @ (AwardError::PublishedButUnrecorded { .. }
            | AwardError::PresenceUnverified { .. }
            | AwardError::Refused { .. }),
        ) => Response::err(id, CODE_REFUSED, error.to_string()),
        // Unresolved is transient and RETRY-SAFE by construction (the retry re-sends the same
        // signed event), which the message says outright — an agent that reads it can converge
        // by simply calling `award_claim` again.
        Err(error @ AwardError::Unresolved { .. }) => {
            Response::err(id, CODE_INTERNAL, error.to_string())
        }
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
    let outcome = lifecycle::settle_after_pay(
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
    })?;

    // Attribute the settled award to the worker that EARNED it (#261): the seller-claimed
    // harness/model captured at accept off the delivered result. Settlement is the first moment
    // an earner exists (truth-only — never the requested harness written upfront). Advisory,
    // never a gate: every non-Written outcome logs and the settle outcome stands, because the
    // payment already happened and refusing here could only strand a paid job.
    match context.store.attribute_award(
        job_id,
        outcome.agent_used.as_deref(),
        outcome.model_used.as_deref(),
    ) {
        // First settled attribution recorded, or an idempotent re-settle of an already-attributed
        // row (the bind is immutable per job, so a repeat carries identical values).
        Ok(store::AttributeAward::Written) | Ok(store::AttributeAward::AlreadyAttributed) => {}
        // No awards row to land on (externally-accepted job, or an award whose record_award
        // failed and was collected manually). Only a real report is a real drop: a metadata-less
        // outcome has nothing to lose, so those stay silent. A re-collect of an external job
        // whose seller DID report still logs every time — deliberately: each re-collect genuinely
        // re-drops a real report.
        Ok(store::AttributeAward::NoAwardRow) => {
            if outcome.agent_used.is_some() || outcome.model_used.is_some() {
                crate::opline!(
                    "buyer: settled {job_id} has no awards row to attribute (externally accepted \
                     or unrecorded award) — seller-reported attribution dropped"
                );
            }
        }
        Err(error) => {
            crate::opline!("buyer: award attribution write failed for {job_id} (continuing): {error}")
        }
    }

    // #469: this settle just converted a reservation reserved→spent, so any reconcile-on-start
    // snapshot still listing this job under `kept` is now stale — a `status` money-read would show
    // already-settled funds as held, exactly when trust matters. Drop the snapshot here; the next
    // reconcile pass repopulates it from live truth. Reached only on the Ok path (a refused settle
    // returns via `?` above and leaves the snapshot untouched).
    invalidate_reconcile_snapshot(context).await;

    Ok(outcome)
}

/// Drop the reconcile-on-start snapshot (`last_reconcile`) so `status` cannot surface an entry for a
/// reservation that has since settled (#469). Display-only and idempotent: nothing reads
/// `last_reconcile` for a decision — reconcile decisions re-derive from the store — so clearing it
/// removes only a stale display, never a held reservation. The next reconcile pass re-populates it.
async fn invalidate_reconcile_snapshot(context: &BuyerContext) {
    *context.last_reconcile.lock().await = None;
}

/// #539: compose the operator-console line for a spend the buyer cannot afford — an award reservation
/// refused, or a settle that failed at the pay step. The typed refusals already carry the shortfall
/// numbers on their `Display` (`ReserveRefused` names need vs available; the cross-mint `HopError`
/// names need/held/mint), so these funnel that detail onto the operator surface through ONE formatter
/// each. Routing the handler and its test through the same formatter makes a dropped or transposed
/// number red-provable — a money-debugging diagnostic that names the wrong numbers is worse than
/// silence, and the raw `opline!` stderr boundary can only be eyeballed.
fn cannot_afford_award_line(job_id: &str, refused: &impl std::fmt::Display) -> String {
    format!("buyer: cannot afford to award job {job_id} — {refused}")
}

fn could_not_settle_line(job_id: &str, error: &impl std::fmt::Display) -> String {
    format!("buyer: could not settle {job_id}: {error}")
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
                },
                "commit_oid": outcome.commit_oid,
                "path": outcome.path,
                "files": outcome.files,
                // Seller-claimed attribution of the settled delivery (#261); null = unreported.
                "agent_used": outcome.agent_used,
                "model_used": outcome.model_used,
            }),
        ),
        Err(error @ SettleJobError::Pay(_)) => {
            // #539: surface the settle failure on the operator console — the collect RPC path, unlike
            // the delivery watcher, otherwise tells only the RPC caller. `error` carries the shortfall
            // detail (a cross-mint hop names need/held/mint).
            crate::opline!("{}", could_not_settle_line(&params.job_id, &error));
            Response::err(id, CODE_REFUSED, error.to_string())
        }
        Err(error) => Response::err(id, CODE_INTERNAL, error.to_string()),
    }
}

/// Honest reserve snapshot: the live wallet balance (through the actor) and the budget spent total
/// (fresh fold, shown in status). Never a sentinel or a stale cached value. Issue #378 removed the
/// total cap, so the wallet balance is the sole reservation ceiling.
async fn money_snapshot(context: &BuyerContext) -> Result<(u64, u64), String> {
    let balance = context
        .wallet
        .balance()
        .await
        .map_err(|error| error.to_string())??;
    let gate = BudgetGate::from_home(&context.home).map_err(|error| error.to_string())?;
    Ok((balance, gate.spent()))
}

/// The buyer nostr identity, parsed from the home secret (the same source the signer actor loads).
fn buyer_keys(home: &MaxplayerHome) -> Result<nostr_sdk::Keys, String> {
    let secret = home::read_secret_key_hex(home).map_err(|error| error.to_string())?;
    nostr_sdk::Keys::parse(&secret).map_err(|error| format!("buyer key parse: {error}"))
}

/// Spawn the background auto-award task for a posted job — the daemon-drives-the-award half of the
/// 2-call trade loop. A task failure never affects the daemon; the intent stays `pending` and is
/// re-armed on the next start.
fn spawn_auto_award(context: Arc<BuyerContext>, job_id: String, max_sats: u64) {
    tokio::spawn(async move {
        if let Err(error) = drive_auto_award(&context, &job_id, max_sats).await {
            crate::opline!("buyer: auto-award for {job_id} did not complete ({error}); left pending for re-arm");
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

    // Invariant A, first pass: skip cheaply if the relay already shows our award. An unknown answer
    // (an error, or an unverified/empty read) does NOT skip — it falls through to
    // `award_with_reservation`, which is where an unverified presence is adjudicated against the
    // local `awards` row + pinned attempt and REFUSED rather than republished. This check is an
    // optimisation; the chokepoint is the guard.
    let award_on_relay = matches!(
        job_lifecycle::award_presence_async(&context.home, &keys, job_id, RELAY_TIMEOUT).await,
        Ok(job_lifecycle::PresenceRead::Present(_))
    );
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

    let mut unconfirmed_reads: u32 = 0;
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
            // An empty offer read is evidence the offer is GONE only if the relay answered us.
            // Parking is terminal — the driver never retries a parked row — so it has to rest on a
            // positive determination, never on the absence of a signal we may simply have missed.
            //
            // #291: this line parked a live, claimed, real-money job with 5.8 hours left on its
            // deadline, because a 5s timeout and an empty relay are the same bytes here. Twenty-five
            // lines up, the award-presence check already treats its empty read as unknown; the two
            // sites disagreed about the same evidence, and only one of them was right.
            if !view.read_confirmed {
                unconfirmed_reads += 1;
            }
            match lifecycle::plan_missing_offer(
                view.read_confirmed,
                unconfirmed_reads,
                AUTO_AWARD_MAX_UNCONFIRMED_READS,
            ) {
                MissingOfferAction::Retry => {
                    tokio::time::sleep(AUTO_AWARD_POLL_INTERVAL).await;
                    continue;
                }
                MissingOfferAction::ParkOfferAbsent => {
                    // A pinned attempt outranks the offer-shaped reason: an offer that aged off
                    // the relay says nothing about the claim that was already selected, signed
                    // for, and possibly awarded (round-3 review — this is the state a crashed
                    // refusal reboots into, and it must not park as "offer absent").
                    if settle_intent_from_attempt(context, &keys, job_id).await {
                        return Ok(());
                    }
                    crate::opline!("{}", auto_award_park_line(job_id, lifecycle::PARK_REASON_OFFER_ABSENT));
                    let _ = context.store.mark_award_parked(
                        job_id,
                        lifecycle::PARK_REASON_OFFER_ABSENT,
                        now_unix(),
                    );
                }
                // The reason states what we OBSERVED. It does not say the offer is gone — we never
                // established that — because a row that said so would be the same false status line
                // #291 was filed about, just arriving a minute later.
                MissingOfferAction::ParkUnreadable { unanswered_reads } => {
                    if settle_intent_from_attempt(context, &keys, job_id).await {
                        return Ok(());
                    }
                    let reason = lifecycle::park_reason_unreadable(unanswered_reads);
                    crate::opline!("{}", auto_award_park_line(job_id, &reason));
                    let _ = context.store.mark_award_parked(job_id, &reason, now_unix());
                }
            }
            return Ok(());
        };
        unconfirmed_reads = 0;
        // THE SAME constructor the manual award path uses, so the two cannot apply different filters.
        // Both selection entry points then consult `claim_meets_capability_request`:
        // `select_awardable_claim` here, `named_claim_awardable` on the manual path.
        let filters = lifecycle::award_filters_for_offer(
            offer,
            max_sats,
            context.home.config.default_mint(),
            context.home.config.allow_real_mints,
        );

        // Built AFTER `filters` so the deadline park can name the capability request that refused
        // everything, instead of only reporting that time ran out. The order of these two blocks is
        // the only thing that makes an actionable reason available here; the decision itself is
        // unchanged, and a job with no request parks with the wording it always did.
        if now_unix() as u64 > offer.deadline_unix {
            // A pinned attempt past its deadline is NOT "no awardable claim appeared" — a claim
            // was selected and signed for. Reflect the ATTEMPT's truth on the intent instead of
            // a false park reason; the periodic sweep continues anything still unresolved.
            if settle_intent_from_attempt(context, &keys, job_id).await {
                return Ok(());
            }
            let reason = lifecycle::park_reason_deadline_passed(
                lifecycle::capability_park_reason(&view, &filters).as_deref(),
            );
            crate::opline!("{}", auto_award_park_line(job_id, &reason));
            let _ = context.store.mark_award_parked(job_id, &reason, now_unix());
            return Ok(());
        }

        if let Some(claim_id) = lifecycle::select_awardable_claim(&view, &filters) {
            return finalize_auto_award(context, job_id, offer.amount_sats, claim_id).await;
        }

        // No awardable claim yet — re-check after a bounded interval (no tight spin on a
        // live-but-unpayable claim). The deadline check above bounds the total wait.
        tokio::time::sleep(AUTO_AWARD_POLL_INTERVAL).await;
    }
}

/// Compose the operator-log line for an auto-award that PARKED (#411, merging #183). Every park in
/// the auto-award path (`drive_auto_award`, `finalize_auto_award`) emits this through `opline!`:
/// parking is the correct handling of an unawardable job, so it returns `Ok`, and the success path
/// printed nothing — a job that lapsed (on budget, on a passed deadline, on an offer gone from the
/// relay) was indistinguishable in the log from the daemon ignoring the claim. The durable reason
/// still lives in the store and shows in the `status` RPC; this only makes the same decision audible
/// in `journalctl`/daemon.log, where an operator who was told the daemon would award the job looks.
///
/// Split from the `opline!` call (not inlined) so the wording — which MUST carry the job id and the
/// reason an operator needs to act — is directly assertable in a test without capturing stderr, per
/// #183's empty-case rule. `PARKED` is a stable, greppable token; the reason already distinguishes
/// the parks. Logging only: no park/award decision is changed.
fn auto_award_park_line(job_id: &str, reason: &str) -> String {
    format!("buyer: auto-award for {job_id} PARKED — {reason}")
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
    // Deadline TOCTOU re-check under the guard (same as the manual RPC): the caller's deadline
    // gate ran before this lock, whose wait is unbounded behind a settle. Err keeps the intent
    // pending — the deadline arm / the sweep resolves the pinned attempt by probe.
    if let Ok(Some(current)) = context.store.award_attempt(job_id) {
        if resume_crossed_deadline(&current, now_unix()) {
            return Err(format!(
                "offer deadline for {job_id} crossed while awaiting the money lock; the pinned \
                 attempt resolves by probe"
            ));
        }
    }
    let (balance, _spent) = money_snapshot(context).await?;
    let home = context.home.clone();
    let job = job_id.to_owned();
    let publish_claim = claim_id.clone();
    let keys = buyer_keys(&context.home)?;
    let probe_keys = keys.clone();
    let probe_home = context.home.clone();
    let probe_job = job_id.to_owned();
    // A resume must transmit to the relay the bytes were PINNED for; only a first call may use
    // live config (and pins it, so the two agree by construction). Read under the same money
    // lock as the chokepoint's own read, so no pin can slip between the two.
    let send_relay = context
        .store
        .award_attempt(job_id)
        .ok()
        .flatten()
        .map(|attempt| attempt_relay(&attempt, &context.home))
        .unwrap_or_else(|| context.home.config.relay_url.clone());
    let send_keys = keys;
    let result = lifecycle::award_with_reservation(
        &context.store,
        job_id,
        offer_amount,
        balance,
        now_unix(),
        move || async move {
            job_lifecycle::award_presence_async(&probe_home, &probe_keys, &probe_job, RELAY_TIMEOUT)
                .await
        },
        move || async move {
            job_lifecycle::prepare_award_async(
                &home,
                AwardClaimRequest { job_id: job, claim_id: publish_claim },
            )
            .await
        },
        move |event_json: String, expected_event_id: String| async move {
            job_lifecycle::send_signed_award_async(
                &send_keys,
                &send_relay,
                &expected_event_id,
                &event_json,
            )
            .await
        },
        // These paths transmit from inside the chokepoint, so it owns the license.
        None,
    )
    .await;

    match result {
        // Both outcomes mean the job IS awarded by this buyer — freshly published, or already
        // published and recorded. The intent is satisfied either way.
        Ok(_) => {
            let _ = context.store.mark_award_awarded(job_id, now_unix());
            Ok(())
        }
        Err(AwardError::Reserve(refused)) => {
            // #411/#539 reconcile: this arm PARKS the job — the daemon's "cannot afford this award"
            // decision. Without a line it silently declines and looks idle; the parked reason is only
            // visible later via `status`. It emits through `auto_award_park_line` like every other
            // auto-award park site, so a `PARKED`-token grep finds EVERY park — including this budget
            // park, which is #411's canonical case. `refused.to_string()` still names need vs available
            // (the shortfall #539 requires on the console); the reservation is against aggregate
            // available balance, so no single source mint applies. `cannot_afford_award_line` is
            // reserved for the RPC award path, where a refusal is returned to the caller, not parked.
            crate::opline!("{}", auto_award_park_line(job_id, &refused.to_string()));
            let _ = context.store.mark_award_parked(
                job_id,
                &format!("reservation refused: {refused}"),
                now_unix(),
            );
            Ok(())
        }
        // An award is public but unrecorded: TERMINAL. No retry can fix it — the missing row will
        // not appear on its own — so park it, which surfaces the refusal (with its operator action)
        // in `status` via `parked_awards` rather than retrying forever against a fixed state.
        Err(error @ AwardError::PublishedButUnrecorded { .. }) => {
            crate::opline!("{}", auto_award_park_line(job_id, &error.to_string()));
            let _ = context
                .store
                .mark_award_parked(job_id, &error.to_string(), now_unix());
            Ok(())
        }
        // Presence could not be verified, local state could not be read, or the send got no
        // verdict: UNKNOWN, and possibly a transient relay/DB blip. Return Err so the intent stays
        // `pending` and re-arms on the next start (this function's contract) instead of demanding
        // an operator for a 10-second hiccup. Re-arming is safe by construction: the chokepoint
        // refuses again on every unverified pass, and an UNRESOLVED attempt re-sends its own
        // pinned bytes — so retrying can never publish a duplicate; it can only converge.
        Err(
            error @ (AwardError::PresenceUnverified { .. }
            | AwardError::Presence(_)
            | AwardError::Unresolved { .. }),
        ) => Err(error.to_string()),
        Err(error) => {
            crate::opline!("{}", auto_award_park_line(job_id, &format!("award failed: {error}")));
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
            // A feedback arrived: release the reservation IF it is a post-award failure report
            // (delivery / execution / no-sentinel) from the seller this buyer awarded that job
            // (#562, widened in #574). A no-op for every other feedback.
            WatchWake::Feedback(event) => {
                release_on_failure_feedback(context, &event).await
            }
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
    /// A seller FEEDBACK arrived — inspect it for a delivery-failure report against one of this
    /// buyer's awarded-unsettled jobs (#562) and promptly free that reservation.
    Feedback(Arc<nostr_sdk::Event>),
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
///    on (the `continue` below) would reset it WITHOUT sweeping — so a steady claim stream
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
                        // The buyer-keyed subscription carries the seller's RESULT (settle it) and
                        // FEEDBACK (a delivery-failure report may free a held reservation, #562);
                        // every other kind is noise the loop drops so it cannot starve the sweep.
                        if event.kind == nostr_sdk::Kind::Custom(crate::kinds::JOB_RESULT_KIND) {
                            WatchWake::Delivered(event)
                        } else if event.kind == nostr_sdk::Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND)
                        {
                            WatchWake::Feedback(event)
                        } else {
                            continue;
                        }
                    }
                    // Lagged is NOT an error — the buffer overflowed and a result may have been
                    // missed. Treating a busy relay as a failure would strand a payment; the right
                    // response is to widen to a full sweep.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => WatchWake::Sweep,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        crate::opline!(
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

/// Render a seller-claimed attribution string safely for a single operator-log line. Stripped:
/// control characters (newline injection could forge a whole settled/paid line; ESC/C1-CSI could
/// repaint the terminal) AND the invisible Unicode format characters `is_control` misses (bidi
/// reordering, zero-widths, line/paragraph separators). Length is capped. The guarantee is
/// single-line, no terminal control, bounded — spoofed PRINTABLE text (a fake `agent=…`) is
/// inherent to printing seller text and stays possible. The stored/RPC value stays raw — JSON
/// encoding escapes it there; only the bare eprintln needs this. `unreported` = the seller
/// stamped nothing; `unprintable` = it stamped only stripped bytes (reported, but garbage).
fn log_safe_agent(value: Option<&str>) -> String {
    match value {
        None => "unreported".to_owned(),
        Some(raw) => {
            let cleaned = crate::gateway::log_safe_text(raw);
            if cleaned.is_empty() {
                "unprintable".to_owned()
            } else {
                cleaned
            }
        }
    }
}

/// Backfill attribution for awards that SETTLED without it (#261) — the boot heal. Work set:
/// [`store::BuyerStore::unattributed_settled_award_job_ids`] (spent + wholly-NULL). Source of
/// truth: the durable accept-bind, which froze the seller-reported harness/model at accept —
/// the same values the settle-time write would have carried. Landing goes through the same
/// row-level write-once gate, so healing can never rewrite a recorded attribution. Advisory like
/// the settle write: every failure logs and boot proceeds; a bind that reported nothing leaves
/// the honest NULL (and logs nothing — there is nothing to heal).
///
/// Accepted cost: never-healable rows (metadata-less sellers, pre-attribution settles, missing
/// binds) stay in the work set FOREVER, so every boot re-runs one SELECT plus a local JSON read
/// per such row — linear, local, silent. If fleets ever accumulate enough settled no-metadata
/// trades for this to matter at boot, add a terminal "nothing to heal" marker; today it is
/// milliseconds and not worth a schema state.
fn heal_award_attribution(store: &store::BuyerStore, home: &MaxplayerHome) {
    let jobs = match store.unattributed_settled_award_job_ids() {
        Ok(jobs) => jobs,
        Err(error) => {
            crate::opline!("buyer: attribution heal could not enumerate settled awards ({error}); skipping");
            return;
        }
    };
    for job_id in jobs {
        let bind = match job_lifecycle::load_accepted_bind(home, &job_id) {
            Ok(Some(bind)) => bind,
            // Settled with no local bind (pre-bind legacy state) — nothing to heal from.
            Ok(None) => continue,
            Err(error) => {
                crate::opline!("buyer: attribution heal could not read bind for {job_id} ({error}); skipping");
                continue;
            }
        };
        if bind.agent_used.is_none() && bind.model_used.is_none() {
            continue;
        }
        match store.attribute_award(&job_id, bind.agent_used.as_deref(), bind.model_used.as_deref()) {
            Ok(store::AttributeAward::Written) => crate::opline!(
                "buyer: healed award attribution for settled {job_id} (agent={})",
                log_safe_agent(bind.agent_used.as_deref())
            ),
            Ok(_) => {}
            Err(error) => {
                crate::opline!("buyer: attribution heal write failed for {job_id} (continuing): {error}")
            }
        }
    }
}

/// The relay an attempt's resolution must target: the PINNED url, or live config when the pin is
/// the migration sentinel `''` (rows from the pre-column build, which only ever sent to its
/// configured relay). Without the fallback that population could neither send nor probe —
/// `add_relay("")` errors — so it would hold its funds forever.
fn attempt_relay(attempt: &store::AwardAttempt, home: &MaxplayerHome) -> String {
    if attempt.relay_url.is_empty() {
        home.config.relay_url.clone()
    } else {
        attempt.relay_url.clone()
    }
}

/// Whether `now` is past the deadline's pay-window grace — the earliest moment a
/// confirmed-absent award may be terminalized. Pure for the boundary tests. (`saturating_add`
/// is the intended overflow mechanism: a deadline near `i64::MAX` clamps to `i64::MAX` and can
/// never read as past its own window — the test pins the semantics, not a debug-overflow panic.)
fn past_pay_window(now: i64, offer_deadline_unix: i64) -> bool {
    now > offer_deadline_unix.saturating_add(job_lifecycle::DELIVERY_PAY_WINDOW_SECS as i64)
}

/// Whether a pinned attempt's LIVE re-send must be blocked because the offer deadline crossed
/// while the caller was queued (view fetches, then an unbounded money-lock wait behind a settle):
/// the pre-lock deadline gate is a TOCTOU without this re-check under the guard, and the
/// `ResumeAttempt` arm re-sends without re-deriving liveness (round-3 review). Only a PENDING
/// attempt transmits, so only it can cross. Pure for the test.
fn resume_crossed_deadline(attempt: &store::AwardAttempt, now: i64) -> bool {
    attempt.state == store::AttemptState::Pending && now > attempt.offer_deadline_unix
}

/// Reflect a pinned attempt's truth on its auto-award INTENT when the offer can no longer drive
/// the loop — its deadline passed, or it aged off the relay entirely. Terminal attempt states
/// are finished through the chokepoint (heal / release) and stamped with their REAL reason; a
/// pending attempt past its own deadline resolves by probe. Returns `true` when an attempt
/// existed and was handled — the caller's offer-shaped park must then NOT run, because "no
/// awardable claim appeared" / "offer no longer on the relay" are false for a job whose claim
/// was selected and signed for (round-3 review: without this, the boot re-arm parked crashed
/// refusals under `PARK_REASON_OFFER_ABSENT` and the round-2 finisher arms were dead code for
/// their own population).
///
/// A pending attempt still INSIDE its deadline returns `false`: the sweep owns its resolution,
/// and the caller's own report about the offer remains accurate.
async fn settle_intent_from_attempt(
    context: &BuyerContext,
    keys: &nostr_sdk::Keys,
    job_id: &str,
) -> bool {
    let attempt = match context.store.award_attempt(job_id) {
        Ok(Some(attempt)) => attempt,
        _ => return false,
    };
    match attempt.state {
        store::AttemptState::Pending => {
            if now_unix() <= attempt.offer_deadline_unix {
                return false;
            }
            // Resolve by probe (never a late transmit).
            resolve_expired_attempt(context, keys, &attempt).await;
            if context.store.award_record(job_id).ok().flatten().is_some() {
                let _ = context.store.mark_award_awarded(job_id, now_unix());
            }
            // A refusal already parked the intent with its reason; anything else stays pending
            // for the sweep.
            true
        }
        store::AttemptState::Confirmed => {
            // The award is public; heal row/funds if a crash left them behind. The intent is
            // stamped `awarded` only when the heal actually LANDED — an unrepairable heal
            // (`PublishedButUnrecorded`: the row cannot be written or funded) parks with that
            // error exactly as `finalize_auto_award` does, and a transient failure leaves the
            // intent for the sweep's retry (round-3 review: an unconditional `awarded` mark hid
            // a permanently-unrepairable job from every surface).
            match resolve_attempt_via_chokepoint(context, keys, job_id, attempt.amount_sats, None, None)
                .await
            {
                Ok(_) => {
                    let _ = context.store.mark_award_awarded(job_id, now_unix());
                }
                Err(error @ AwardError::PublishedButUnrecorded { .. }) => {
                    let _ = context.store.mark_award_parked(job_id, &error.to_string(), now_unix());
                }
                Err(error) => crate::opline!(
                    "buyer: confirmed attempt for {job_id} could not be healed ({error}); the \
                     sweep retries"
                ),
            }
            true
        }
        store::AttemptState::Refused => {
            // Finish a crashed refusal (RefusedTerminal releases any held funds) and surface
            // the REAL reason on the intent.
            let _ =
                resolve_attempt_via_chokepoint(context, keys, job_id, attempt.amount_sats, None, None)
                    .await;
            let detail = attempt
                .detail
                .unwrap_or_else(|| "the relay refused the award event".to_owned());
            let _ = context.store.mark_award_parked(job_id, &detail, now_unix());
            true
        }
    }
}

/// Terminalize a pending attempt whose award is CONFIRMED ABSENT past the pay window: refuse,
/// release, park — but ONLY if (a) no `awards` row exists and (b) this call wins the
/// `pending → refused` transition.
///
/// (a) mirrors the chokepoint's own first read: the awards row is written exclusively on an ack
/// or a presence-verified repair, so its existence is direct proof the award IS public, and no
/// probe emptiness (relay pruning, months later) may overrule it. Without this guard the
/// confirm-write-failed crash state (attempt still `pending`, row present) could be refused and
/// its RECORDED award's funds released (round-3 review). (b) is the transition license: `false`
/// means another resolver already terminalized the attempt, and releasing anyway would strip
/// funds it may have just re-held. Pure over the store so both gates are unit-testable; the
/// caller holds `money_lock`.
fn terminalize_absent_attempt(
    store: &store::BuyerStore,
    job_id: &str,
    detail: &str,
    now_unix: i64,
) -> Result<bool, StoreError> {
    if store.award_record(job_id)?.is_some() {
        return Ok(false);
    }
    if !store.mark_attempt_refused(job_id, detail, now_unix)? {
        return Ok(false);
    }
    store.release(job_id, now_unix)?;
    store.mark_award_parked(job_id, detail, now_unix)?;
    Ok(true)
}

/// Drive one attempt-holding job through the [`lifecycle::award_with_reservation`] chokepoint —
/// the money mechanics (re-hold, repair, confirm, record, refuse+release) live THERE, never
/// re-implemented here. `verdict` chooses the send leg: `Some(outcome)` replays a transmission
/// the caller already performed OUTSIDE the money lock (the lock must not be held across 45s of
/// relay I/O), `None` is the defensive no-op for callers that must not transmit at all (heals of
/// confirmed attempts and finishes of refused ones, where no send arm is reachable).
async fn resolve_attempt_via_chokepoint(
    context: &BuyerContext,
    keys: &nostr_sdk::Keys,
    job_id: &str,
    amount_sats: u64,
    verdict: Option<job_lifecycle::SendOutcome>,
    licensed_prior_sends: Option<u64>,
) -> Result<AwardOutcome, AwardError> {
    let _guard = context.money_lock.lock().await;
    let (balance, _spent) = match money_snapshot(context).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Err(AwardError::Presence(StoreError(format!(
                "money snapshot unavailable (wallet/budget): {error}"
            ))));
        }
    };
    let probe_home = context.home.clone();
    let probe_keys = keys.clone();
    let probe_job = job_id.to_owned();
    lifecycle::award_with_reservation(
        &context.store,
        job_id,
        amount_sats,
        balance,
        now_unix(),
        move || async move {
            job_lifecycle::award_presence_async(&probe_home, &probe_keys, &probe_job, RELAY_TIMEOUT)
                .await
        },
        // A pinned attempt never re-prepares; defensive error rather than a daemon panic.
        || async {
            Err(job_lifecycle::JobLifecycleError::Input(
                "a pinned attempt must not re-prepare".to_owned(),
            ))
        },
        move |_event_json: String, _expected_event_id: String| async move {
            verdict.unwrap_or(job_lifecycle::SendOutcome::Unresolved {
                detail: "this resolution path does not transmit".to_owned(),
            })
        },
        licensed_prior_sends,
    )
    .await
}

/// Resolve one pending attempt whose offer deadline has PASSED. Never transmits — re-sending
/// would knowingly inject a late award — so the verdict comes from the by-id probe against the
/// attempt's pinned relay, and it terminalizes only with maximal honesty:
///
/// - **Present** → the award IS public: confirm the attempt, then heal row + funds through the
///   chokepoint's repair arm.
/// - **Confirmed absent, pay window also passed, and NO delivery exists for the job** → nothing
///   can settle inside its window anymore: refuse the attempt, return the funds, park the
///   intent — gated on winning the `pending → refused` transition, under the money lock. The pay
///   window ([`job_lifecycle::DELIVERY_PAY_WINDOW_SECS`]) is the grace that keeps one relay
///   hiccup at one boot from repudiating work a seller may be mid-delivery on; the delivery
///   check is the stronger guard — a kind-3403 for the job is positive evidence the award WAS
///   public (a seller executed) and has merely aged out of the probe's view, so refusing then
///   would repudiate work that happened.
/// - **Confirmed absent inside the window / unverified / probe error** → hold everything;
///   re-probed next pass.
///
/// All relay reads here run WITHOUT the money lock (they take seconds); only the verdict
/// application takes it.
async fn resolve_expired_attempt(
    context: &BuyerContext,
    keys: &nostr_sdk::Keys,
    attempt: &store::AwardAttempt,
) {
    let job_id = &attempt.job_id;
    let relay_url = attempt_relay(attempt, &context.home);
    match job_lifecycle::event_present_async(
        keys,
        &relay_url,
        &attempt.award_event_id,
        RELAY_TIMEOUT,
    )
    .await
    {
        Ok(job_lifecycle::PresenceRead::Present(())) => {
            // The transition result is load-bearing: `false` with a REFUSED row means a
            // concurrent resolver terminalized on a divergent (absent) probe while we hold
            // positive proof the award is public — an unhealable one-way divergence that must be
            // surfaced truthfully, never logged as a retryable heal failure (round-3 review; no
            // work set revisits a refused+released attempt).
            match context.store.mark_attempt_confirmed(job_id, now_unix()) {
                Ok(true) => {}
                Ok(false) => {
                    let state = context
                        .store
                        .award_attempt(job_id)
                        .ok()
                        .flatten()
                        .map(|after| after.state);
                    if state == Some(store::AttemptState::Refused) {
                        crate::opline!(
                            "buyer: ⚠ award {} for {job_id} IS on the relay, but a concurrent \
                             resolver terminalized the attempt as refused (probes diverged); \
                             the refusal is one-way and this cannot self-heal — if the seller \
                             delivers, settle manually with `collect {job_id}`",
                            attempt.award_event_id
                        );
                        return;
                    }
                    // Already confirmed (by us on an earlier pass, or a concurrent resolver) —
                    // proceed to the heal exactly as if our mark had won.
                }
                Err(error) => {
                    crate::opline!(
                        "buyer: award attempt for {job_id} could not be confirmed ({error}); \
                         will retry next pass"
                    );
                    return;
                }
            }
            match resolve_attempt_via_chokepoint(context, keys, job_id, attempt.amount_sats, None, None)
                .await
            {
                Ok(_) => {
                    let _ = context.store.mark_award_awarded(job_id, now_unix());
                    crate::opline!(
                        "buyer: pending award attempt for {job_id} resolved — award {} is on the \
                         relay (found by probe)",
                        attempt.award_event_id
                    );
                }
                Err(error) => crate::opline!(
                    "buyer: award {} for {job_id} is on the relay but the row/funds heal failed \
                     ({error}); will retry next pass",
                    attempt.award_event_id
                ),
            }
        }
        Ok(job_lifecycle::PresenceRead::ConfirmedAbsent) => {
            if !past_pay_window(now_unix(), attempt.offer_deadline_unix) {
                crate::opline!(
                    "buyer: pending award attempt for {job_id} confirmed absent but inside the \
                     pay window; holding"
                );
                return;
            }
            // The stronger guard before the terminal write: a delivery BY THE AWARDED SELLER is
            // positive evidence the award WAS public and the probe's absence is retention, not
            // history. Filtered to the pinned seller — an unauthenticated hold here would let
            // any pubkey pin the buyer's funds forever with one junk 3403 (round-3 review).
            //
            // This guard also carries a fail-safe that #329's kind split retired elsewhere: while
            // ACCEPT shared kind 3405, an accept made the award-presence probe answer "present"
            // even for a job whose award itself had aged off the relay. Post-split it no longer
            // does — and a job that reached accept has a delivery by definition (an accept binds
            // a verified result), so this delivery probe is what now holds that population.
            match job_lifecycle::job_has_results_async(
                keys,
                &relay_url,
                job_id,
                &attempt.seller_pubkey,
                RELAY_TIMEOUT,
            )
            .await
            {
                Ok(job_lifecycle::PresenceRead::ConfirmedAbsent) => {}
                Ok(job_lifecycle::PresenceRead::Present(())) => {
                    crate::opline!(
                        "buyer: award attempt for {job_id} probes absent past its pay window, \
                         but the job HAS deliveries — holding (the award likely aged out of the \
                         probe; settle via `collect {job_id}`)"
                    );
                    return;
                }
                Ok(job_lifecycle::PresenceRead::Unverified) | Err(_) => {
                    crate::opline!(
                        "buyer: award attempt for {job_id} probes absent past its pay window, \
                         but the delivery check did not answer; holding until it does"
                    );
                    return;
                }
            }
            let detail = "offer deadline and pay window passed with the award (and any \
                          delivery) confirmed absent from its relay; nothing can settle inside \
                          the window now";
            // Serialize the verdict application with every other money decision, and act only
            // if THIS call wins the pending→refused transition (see terminalize_absent_attempt).
            let _guard = context.money_lock.lock().await;
            match terminalize_absent_attempt(&context.store, job_id, detail, now_unix()) {
                Ok(true) => {
                    crate::opline!("buyer: pending award attempt for {job_id} refused: {detail}");
                }
                Ok(false) => {
                    crate::opline!(
                        "buyer: award attempt for {job_id} was resolved concurrently; leaving \
                         the other resolver's verdict in place"
                    );
                }
                Err(error) => {
                    crate::opline!(
                        "buyer: award attempt for {job_id} could not be terminalized ({error}); \
                         will retry next pass"
                    );
                }
            }
        }
        Ok(job_lifecycle::PresenceRead::Unverified) => {
            crate::opline!(
                "buyer: pending award attempt for {job_id} unresolved (relay did not confirm \
                 presence or absence); will retry next pass"
            );
        }
        Err(error) => {
            crate::opline!("buyer: pending award attempt for {job_id} probe failed: {error}");
        }
    }
}

/// Resolve award attempts a prior run left open (#322) — runs as the first act of the reconcile
/// task and again on each of its ticks (ONE task, so two sweeps can never overlap), keeping the
/// boot instance off the socket's 10s readiness budget and bounding a lost `OK`'s stall to
/// minutes, not the daemon's lifetime.
///
/// Three work sets, all from durable state:
///
/// - **Heal**: attempts CONFIRMED public whose `awards` row is missing (the crash window between
///   the relay's ack and `record_award`). Healed through the chokepoint's repair arm — which
///   re-holds the funds `record_award` alone would leave dangling (the reconcile pass may have
///   released them while the row was missing; an awards row without its reservation is invisible
///   to the delivery watcher and over-states `available`).
/// - **Finish**: attempts REFUSED whose reservation is still held (the crash window between the
///   refusal mark and its release) — the chokepoint's RefusedTerminal arm releases them.
/// - **Resolve**: attempts still PENDING. Before the offer deadline the pinned bytes are re-sent
///   (transmission OUTSIDE the money lock; verdict folded in through the chokepoint — an
///   already-stored event acks as `duplicate:`, so the send doubles as the probe). Past the
///   deadline nothing is transmitted — [`resolve_expired_attempt`] probes by id and terminalizes
///   only past the pay window, only without deliveries, and only under the money lock, gated on
///   winning the refusal transition.
///
/// Concurrency-safe by construction: every money decision happens under `money_lock` — inside
/// the chokepoint, or in [`terminalize_absent_attempt`] under the caller's guard — so this can
/// run alongside the serving RPCs and re-armed auto-award tasks.
async fn resolve_award_attempts(context: &BuyerContext) {
    let keys = match buyer_keys(&context.home) {
        Ok(keys) => keys,
        Err(error) => {
            crate::opline!("buyer: award attempt sweep skipped (no keys): {error}");
            return;
        }
    };

    match context.store.confirmed_attempts_without_award_row() {
        Ok(attempts) => {
            for attempt in attempts {
                match resolve_attempt_via_chokepoint(
                    context,
                    &keys,
                    &attempt.job_id,
                    attempt.amount_sats,
                    None,
                    None,
                )
                .await
                {
                    Ok(_) => {
                        // Clear any stale parked reason: a heal that lands the row supersedes an
                        // earlier failed-repair park, and this attempt now leaves the work set,
                        // so nothing later would correct the intent (round-3 review).
                        let _ = context.store.mark_award_awarded(&attempt.job_id, now_unix());
                        crate::opline!(
                            "buyer: healed awards row for {} from its confirmed attempt ({})",
                            attempt.job_id, attempt.award_event_id
                        );
                    }
                    // Only an UNREPAIRABLE heal parks: the award is public but its row cannot be
                    // written or funded (`PublishedButUnrecorded` — e.g. the balance shrank below
                    // the pinned amount), and a confirmed attempt is in no other status surface
                    // (`pending_award_attempts` selects only pending rows), so a seller owed
                    // money would otherwise be invisible outside stderr (round-5 review). A
                    // TRANSIENT error (an unreadable money snapshot, a store blip) must NOT park:
                    // parking says "the award could not be placed" about a job whose award is
                    // public and whose seller is executing, and an operator acting on that list
                    // could post a duplicate offer. Transients stay for the next tick's retry —
                    // the same discipline `settle_intent_from_attempt`'s Confirmed arm keeps.
                    Err(error @ AwardError::PublishedButUnrecorded { .. }) => {
                        let _ = context.store.mark_award_parked(
                            &attempt.job_id,
                            &error.to_string(),
                            now_unix(),
                        );
                        crate::opline!(
                            "buyer: award for {} is public but unrepairable ({error}); parked for \
                             an operator — the sweep keeps retrying the heal",
                            attempt.job_id
                        );
                    }
                    Err(error) => crate::opline!(
                        "buyer: healing awards row for {} failed ({error}); will retry next pass",
                        attempt.job_id
                    ),
                }
            }
        }
        Err(error) => crate::opline!("buyer: could not enumerate confirmed attempts to heal: {error}"),
    }

    // Finish refusals a crash left half-done (refused attempt, funds still reserved): the
    // chokepoint's RefusedTerminal arm releases exactly this state. Without this leg the state
    // is invisible — refused attempts appear in no other work set.
    match context.store.refused_attempts_still_reserved() {
        Ok(attempts) => {
            for attempt in attempts {
                match resolve_attempt_via_chokepoint(
                    context,
                    &keys,
                    &attempt.job_id,
                    attempt.amount_sats,
                    None,
                    None,
                )
                .await
                {
                    // RefusedTerminal reports as an error by design; the release is the point.
                    // Surface the REAL refusal reason on the intent too — a crash between the
                    // release and the park otherwise leaves the intent pending until the boot
                    // re-arm parks it under a wrong reason (round-3 review).
                    Err(AwardError::Refused { .. }) | Ok(_) => {
                        let detail = attempt
                            .detail
                            .clone()
                            .unwrap_or_else(|| "the relay refused the award event".to_owned());
                        let _ = context.store.mark_award_parked(&attempt.job_id, &detail, now_unix());
                        crate::opline!(
                            "buyer: finished the crashed refusal for {} — funds released",
                            attempt.job_id
                        );
                    }
                    Err(error) => crate::opline!(
                        "buyer: finishing the crashed refusal for {} failed ({error}); will \
                         retry next pass",
                        attempt.job_id
                    ),
                }
            }
        }
        Err(error) => crate::opline!("buyer: could not enumerate crashed refusals: {error}"),
    }

    let pending = match context.store.pending_award_attempts() {
        Ok(pending) => pending,
        Err(error) => {
            crate::opline!("buyer: could not enumerate pending award attempts: {error}");
            return;
        }
    };
    for attempt in pending {
        let job_id = attempt.job_id.clone();
        if now_unix() > attempt.offer_deadline_unix {
            resolve_expired_attempt(context, &keys, &attempt).await;
            continue;
        }
        // An awards row means the award already resolved (ack recorded, or probe-repaired) and
        // only the attempt's confirm is owed — transmit nothing; the AlreadyAwarded arm drains
        // it, and skipping here saves a send license + a relay round trip per tick.
        match context.store.award_record(&job_id) {
            Ok(Some(_)) => {
                let _ = resolve_attempt_via_chokepoint(
                    context,
                    &keys,
                    &job_id,
                    attempt.amount_sats,
                    None,
                    None,
                )
                .await;
                let _ = context.store.mark_award_awarded(&job_id, now_unix());
                crate::opline!(
                    "buyer: pending award attempt for {job_id} re-confirmed from its recorded \
                     award"
                );
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                crate::opline!("buyer: attempt sweep could not read {job_id}'s award row: {error}");
                continue;
            }
        }
        // THE LICENSE SECTION — under the money lock, but for milliseconds only. Serializing the
        // license here is what keeps the `prior == 0` refusal rule sound: an in-flight RPC send
        // holds this lock for its whole transmission, so by the time we hold it that send has a
        // verdict and the re-read below sees it — the sweep can never put a concurrent copy on
        // the wire beside a FIRST transmission whose refusal would then terminalize (round-3
        // review). Funding precedes the license (reserve is a local write; a spent row is
        // bookkeeping-only and proceeds), so bytes never race ahead of their funding either.
        // The 45s transmission itself happens after the guard drops.
        //
        // The prior count this section takes is CARRIED to the chokepoint rather than re-taken
        // there: counting twice would push a genuinely-first transmission to `prior == 1`, and a
        // deliberate relay refusal of it would then hold the funds for the whole pay window
        // instead of releasing at once (round-4 review).
        let licensed_prior = {
            let _guard = context.money_lock.lock().await;
            // The snapshot is read INSIDE the guard: the wallet-ceiling check below must not decide
            // on a balance a concurrent settle's melt has already invalidated — the same invariant
            // every other reserve site in this file holds to.
            let snapshot = money_snapshot(context).await;
            match context.store.award_attempt(&job_id) {
                // The deadline is re-checked HERE, under the guard, for the same reason the two
                // award paths do it (`resume_crossed_deadline`): the gate above ran pre-lock, and
                // the wait to get here — a guard held across a whole settle — is unbounded. A
                // deadline that crossed in that window must send NOTHING; the expired path
                // (probe only) owns the attempt from then on.
                Ok(Some(current)) if resume_crossed_deadline(&current, now_unix()) => {
                    crate::opline!(
                        "buyer: award attempt for {job_id} crossed its offer deadline while \
                         awaiting the money lock; not transmitting — it resolves by probe"
                    );
                    None
                }
                Ok(Some(current)) if current.state == store::AttemptState::Pending => {
                    match &snapshot {
                        Ok((balance, _spent)) => {
                            match context.store.reserve(
                                &job_id,
                                attempt.amount_sats,
                                *balance,
                                now_unix(),
                            ) {
                                Ok(_)
                                | Err(reservations::ReserveRefused::AlreadySpent { .. }) => {
                                    match context.store.record_attempt_send(&job_id, now_unix()) {
                                        Ok(prior) => Some(prior),
                                        Err(error) => {
                                            crate::opline!(
                                                "buyer: attempt sweep for {job_id} could not \
                                                 license a send: {error}"
                                            );
                                            None
                                        }
                                    }
                                }
                                Err(refused) => {
                                    crate::opline!(
                                        "buyer: attempt sweep cannot fund {job_id}'s re-send \
                                         ({refused}); holding until funds return"
                                    );
                                    None
                                }
                            }
                        }
                        Err(error) => {
                            crate::opline!(
                                "buyer: attempt sweep for {job_id} has no money snapshot \
                                 ({error}); holding"
                            );
                            None
                        }
                    }
                }
                Ok(_) => None, // resolved while we gathered — nothing to send
                Err(error) => {
                    crate::opline!("buyer: attempt sweep could not re-read {job_id}: {error}");
                    None
                }
            }
        };
        let Some(licensed_prior) = licensed_prior else {
            continue;
        };
        let verdict = job_lifecycle::send_signed_award_async(
            &keys,
            &attempt_relay(&attempt, &context.home),
            &attempt.award_event_id,
            &attempt.event_json,
        )
        .await;
        let result = resolve_attempt_via_chokepoint(
            context,
            &keys,
            &job_id,
            attempt.amount_sats,
            Some(verdict),
            Some(licensed_prior),
        )
        .await;
        match result {
            Ok(AwardOutcome::Published(outcome)) => {
                let _ = context.store.mark_award_awarded(&job_id, now_unix());
                crate::opline!(
                    "buyer: pending award attempt for {job_id} resolved — award {} is on the relay",
                    outcome.award_event_id
                );
            }
            Ok(AwardOutcome::AlreadyAwarded(record)) => {
                let _ = context.store.mark_award_awarded(&job_id, now_unix());
                crate::opline!(
                    "buyer: pending award attempt for {job_id} resolved — award {} was already \
                     recorded",
                    record.award_event_id
                );
            }
            Err(error @ AwardError::Refused { .. }) => {
                // Terminal: surfaced on the intent row (no-op for manual-path attempts) AND logged.
                let _ = context.store.mark_award_parked(&job_id, &error.to_string(), now_unix());
                crate::opline!("buyer: pending award attempt for {job_id} refused: {error}");
            }
            Err(error @ AwardError::Unresolved { .. }) => {
                crate::opline!(
                    "buyer: pending award attempt for {job_id} still unresolved ({error}); will \
                     retry next pass"
                );
            }
            Err(error) => {
                crate::opline!("buyer: pending award attempt for {job_id} not resolved: {error}");
            }
        }
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
            crate::opline!("buyer: delivery watcher could not read awarded jobs ({error}); will retry");
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
            // `agent=` is the seller-claimed attribution off the settled result (#261);
            // "unreported" is honest absence, never a guess at what was requested. Rendered
            // through `log_safe_agent`: this is the one place seller-authored free text reaches
            // the operator's terminal, so control bytes must not survive into the log line.
            Ok(outcome) => crate::opline!(
                "buyer: delivery watcher settled {job_id} — paid {} sat for commit {} ({} file(s); agent={})",
                outcome.pay.amount_sats,
                outcome.commit_oid,
                outcome.files.len(),
                log_safe_agent(outcome.agent_used.as_deref())
            ),
            // Nothing delivered yet is the ordinary state of an awarded job, not a failure: the job
            // stays in the set and the next event or tick retries. Every OTHER outcome is a real
            // refusal — a gate said no — and is named so an operator sees which job stopped and why.
            Err(SettleJobError::Pay(collect::CollectError::Lifecycle(
                job_lifecycle::JobLifecycleError::NotFound(_),
            ))) => {}
            Err(error) => crate::opline!("buyer: delivery watcher could not settle {job_id}: {error}"),
        }
    }
}

/// Consume a seller's post-award FEEDBACK (kind-3404) and, when it reports a POST-AWARD FAILURE
/// (delivery / execution / no-sentinel) for one of this buyer's awarded-unsettled jobs, release that
/// job's held reservation PROMPTLY — instead of leaving the funds parked until the periodic deadline
/// reconcile ([`run_reconcile_pass`]) frees them at the offer deadline (#562, widened in #574). The
/// failure report is the seller's EXISTING signal; this adds no new state, no new terminal label, no
/// shared-type change.
///
/// The `reason_code` discriminator is read WITHOUT the money lock: a seller's ordinary progress
/// feedback shares this buyer-keyed subscription and must never even contend the lock that serializes
/// award/collect. Only a genuine failure report takes the lock, under which the authorization +
/// release mirrors [`terminalize_absent_attempt`]'s money-serialized verdict.
async fn release_on_failure_feedback(context: &BuyerContext, event: &nostr_sdk::Event) {
    if !is_releasable_failure_feedback(event) {
        return; // progress / other feedback — not actionable, and not worth taking the money lock.
    }
    // A release returns funds to `available`, so serialize it with every other money decision (mirror
    // `reconcile_reservations` / `terminalize_absent_attempt`): it must never fall inside a concurrent
    // collect's melt→flip window and transiently free ecash that has already left the wallet. The
    // store re-reads state under this lock, so a job that settled while we waited is a no-op here,
    // never a wrong release.
    let _guard = context.money_lock.lock().await;
    match release_reservation_on_failure_feedback(&context.store, event, now_unix()) {
        Ok(Some((job_id, amount))) => crate::opline!(
            "buyer: delivery watcher released {job_id} on a seller-reported post-award failure \
             (freed {amount} sat; budget NOT spent)"
        ),
        // No awarded-unsettled job of ours, a non-awarded author (anti-griefing), or an
        // already-terminal reservation (settled/released — idempotent): a deliberate silent no-op.
        Ok(None) => {}
        Err(error) => crate::opline!(
            "buyer: delivery watcher could not release on a failure feedback ({error}); the \
             periodic reconcile will still free it at the deadline"
        ),
    }
}

/// Is this FEEDBACK an authoritative POST-AWARD FAILURE report — the class that frees a held
/// reservation? The `reason_code` TAG is the authoritative class discriminator (`content` is
/// human-readable and MUST NOT be parsed); the `status` tag must independently be `error`, as every
/// emitting site pairs them ([`crate::gateway::error_draft`]) — requiring both keeps a malformed
/// event inert.
///
/// The releasable set is exactly the three POST-AWARD failure codes — `delivery_failed`,
/// `execution_failed`, and `no_sentinel` ([`crate::gateway::ReasonCode`]): each means the awarded
/// seller will not deliver a payable result, so the buyer's held reservation should be freed (#562
/// shipped `delivery_failed`; #574 widened to its two siblings). The siblings ride the byte-identical
/// wire — same kind, same tags, `status=error` — so this broadens ONLY the discriminator; the author
/// gate + idempotency + money lock downstream are reason-code-agnostic and inherited unchanged. The
/// remaining codes (`below_rate`, `unsupported_version`, `mint_incompatible`, `at_capacity`) are
/// PRE-award offer declines: no award — hence no reservation — exists when they are emitted, so they
/// are deliberately NOT releasable. A future POST-AWARD failure code must be added here explicitly;
/// an unrecognised code is inert (funds stay held until the deadline reconcile — the conservative
/// default).
fn is_releasable_failure_feedback(event: &nostr_sdk::Event) -> bool {
    let tag_value = |name: &str| {
        event.tags.iter().find_map(|tag| {
            let parts = tag.as_slice();
            (parts.first().map(String::as_str) == Some(name))
                .then(|| parts.get(1).map(String::as_str))
                .flatten()
        })
    };
    if tag_value("status") != Some("error") {
        return false;
    }
    let code = tag_value("reason_code");
    code == Some(crate::gateway::ReasonCode::DeliveryFailed.as_str())
        || code == Some(crate::gateway::ReasonCode::ExecutionFailed.as_str())
        || code == Some(crate::gateway::ReasonCode::NoSentinel.as_str())
}

/// The authorized core of the post-award failure release, split out so its authorization +
/// idempotency are testable without a relay or the money lock (the caller owns both), mirroring
/// [`terminalize_absent_attempt`].
///
/// Releases the reservation of the awarded-unsettled job this FEEDBACK names — but ONLY when the
/// event's AUTHOR is the very seller THIS buyer awarded that job. That author gate is the
/// anti-griefing tooth (mirror the delivery guard's pinned-seller author filter and the seller
/// node's `event.pubkey.to_hex() != buyer` reject): a third party must not be able to strand a
/// reservation for a job that is in fact being delivered — releasing a still-delivering job's
/// reservation could drop a legitimate payment or let a stranger free the buyer's committed funds
/// at will.
///
/// Returns `Ok(Some((job_id, freed_sats)))` ONLY on an actual release; every no-op is `Ok(None)`:
/// a feedback naming no awarded-unsettled job of ours, one signed by anyone but the awarded seller,
/// or a job already terminal. Reuses [`store::BuyerStore::release`] — the same primitive the deadline
/// reconcile's `Dead` arm writes, so a failure-reported release and a deadline release converge on
/// the identical `Released` state — and inherits its idempotency: `Spent` (truly delivered) and
/// `Released` (a duplicate relay redelivery) are both inert, so a job that delivered is never freed.
///
/// The releasable reason-code set lives entirely in [`is_releasable_failure_feedback`]; this core is
/// reason-code-agnostic, so #574's widening from `delivery_failed` to its siblings changed only that
/// discriminator, never the authorization / idempotency below.
fn release_reservation_on_failure_feedback(
    store: &store::BuyerStore,
    event: &nostr_sdk::Event,
    now_unix: i64,
) -> Result<Option<(String, u64)>, store::StoreError> {
    if !is_releasable_failure_feedback(event) {
        return Ok(None);
    }
    let author = event.pubkey.to_hex();
    // Scope is the awarded-AND-unsettled set (the JOIN in `awarded_unsettled_job_ids`), so a feedback
    // for a job we never awarded, or one already spent/released, matches nothing here.
    for job_id in store.awarded_unsettled_job_ids()? {
        if !job_lifecycle::event_references_job(event, &job_id) {
            continue;
        }
        // The root `e` tag names an awarded-unsettled job of ours; authorize on the awarded seller.
        let Some(award) = store.award_record(&job_id)? else {
            return Ok(None); // unreachable (the set JOINs `awards`), but fail closed if it raced away.
        };
        if author != award.seller_pubkey {
            return Ok(None); // ANTI-GRIEFING: not the seller we awarded for this job — a strict no-op.
        }
        return Ok(match store.release(&job_id, now_unix)? {
            reservations::Released::Freed { amount } => Some((job_id, amount)),
            // Already spent (delivered) or already released (duplicate feedback): never re-release.
            _ => None,
        });
    }
    Ok(None)
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
    // Jobs the attempt machinery owns hold their funds ON PURPOSE: a PENDING attempt's relay
    // verdict is still open, and a CONFIRMED attempt without its awards row has a provably PUBLIC
    // award awaiting the sweep's heal. Reconciling either here manufactures a release→re-reserve
    // flip-flop with the sweep (round-2 review) or, for the confirmed case, releases the funds of
    // an award that IS public — #322's harm ledger (round-6 review). The sweep owns their
    // resolution: re-send, probe, heal, or the gated pay-window terminalization, which does its
    // own release.
    // They stay IN the batch so the Paid arm can still converge them (round-7 review); only
    // their Dead verdict is downgraded, in `plan_reconcile`.
    let attempt_held: std::collections::BTreeSet<String> =
        match context.store.attempt_held_job_ids() {
            Ok(held) => held.into_iter().collect(),
            Err(error) => {
                // Fail toward keeping funds: an unreadable attempt set must not license releases.
                return Err(format!("could not read attempt-held jobs: {error}"));
            }
        };
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

    // ONE `now` for both the ages and the reconcile write: reading the clock twice would let a
    // reservation be aged against one instant and released against another.
    let now = now_unix();
    let ages = context
        .store
        .reserved_ages(now)
        .map_err(|error| error.to_string())?;
    let floor_config = &context.home.config.buyer_reservation_floor;
    let floor = lifecycle::UnattemptedFloor {
        enabled: floor_config.enabled,
        grace_secs: floor_config.grace_secs,
    };
    let dispositions = plan_reconcile(&reserved, &progress, &payable, &attempt_held, &ages, floor);
    let _guard = context.money_lock.lock().await;
    context
        .store
        .reconcile(&dispositions, now)
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
            crate::opline!("buyer: reconcile pass did not complete ({error}); serving with the ledger as-is")
        }
    }
}

/// Complete cross-mint hops a prior run left in flight, reporting UNCONDITIONALLY.
///
/// Silence here would be indistinguishable from a sweep that has stopped running, and this is the
/// one path that notices a buyer's sats melted at one mint with no ecash at the other — so the pass
/// that found nothing says so too.
async fn run_hop_sweep(context: &Arc<BuyerContext>) {
    match crate::crossmint_hop::sweep_hops(&context.home).await {
        Ok(swept) if swept.is_empty() => {
            crate::opline!("buyer: cross-mint hop sweep found no hop in flight")
        }
        Ok(swept) => {
            let recovered = swept
                .iter()
                .filter(|hop| {
                    hop.result
                        .as_ref()
                        .is_ok_and(|settled| settled.recovered_strand)
                })
                .count();
            let failed = swept.iter().filter(|hop| hop.result.is_err()).count();
            crate::opline!(
                "buyer: cross-mint hop sweep resumed {} hop(s): {recovered} stranded and recovered, \
                 {failed} still unfinished",
                swept.len()
            );
        }
        // A journal we cannot read is not a reason to refuse to serve, but it IS a reason to say so:
        // an unreadable journal means any hop it holds is invisible until someone looks.
        Err(error) => crate::opline!(
            "buyer: cross-mint hop sweep could not read its journal ({error}); a hop left in flight \
             by a prior run would not have been noticed"
        ),
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
    crate::opline!("{}", reconcile_line(report));
}

/// Build the reconcile report line. Split from the printing so the wording — including the
/// released-nothing case that exists precisely because it is easy to leave out — is directly
/// testable rather than something a reader has to trust.
fn reconcile_line(report: &ReconcileReport) -> String {
    let examined = report.released.len() + report.converted.len() + report.kept.len();
    let age = oldest_held_phrase(report.oldest_kept_age_secs);
    if report.released.is_empty() {
        format!(
            "buyer: reconcile examined {examined} reserved job(s) — released nothing, converted {}, kept {}{age}",
            report.converted.len(),
            report.kept.len()
        )
    } else {
        format!(
            "buyer: reconcile examined {examined} reserved job(s) — RELEASED {} (no longer payable \
             on the relay and no funds left: {}), converted {}, kept {}{age}",
            report.released.len(),
            report.released.join(", "),
            report.converted.len(),
            report.kept.len()
        )
    }
}

/// Render the oldest-still-held age as a log suffix, or nothing when the buyer holds no
/// reservation. Split out so the wording is testable, and so the empty case — the one that must
/// NOT print a misleading `oldest held 0m` — is a single explicit branch.
fn oldest_held_phrase(age_secs: Option<u64>) -> String {
    match age_secs {
        None => String::new(),
        Some(secs) if secs < 3_600 => format!(", oldest held {}m", secs / 60),
        Some(secs) => format!(", oldest held {:.1}h", secs as f64 / 3_600.0),
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
        // Boot pass of the attempt sweep, in the SAME task as the tick passes so two sweeps can
        // never overlap. The attribution heal re-runs after it because the sweep CREATES the
        // awards rows it backfills (write-once gated, so it can never rewrite what the boot-path
        // run already landed).
        resolve_award_attempts(&context).await;
        heal_award_attribution(&context.store, &context.home);
        reconcile_loop(RECONCILE_INTERVAL, || async {
            run_reconcile_pass(&context).await;
            // Attempts are resolved on the same cadence, not only at boot: the lost-OK failure
            // this ledger exists for (#322) otherwise stalls a live trade for the daemon's
            // whole lifetime — award public, seller delivering, nothing settled until a
            // restart. Ten minutes bounds that staleness.
            resolve_award_attempts(&context).await;
        })
        .await;
    });
}

/// Pure reconcile planning: map each reserved job to a disposition from its folded payment progress
/// and whether it is still payable. Kept pure (no relay/disk I/O) so the reserved-job → disposition
/// mapping is exhaustively testable; [`reconcile_reservations`] gathers the inputs. A job absent
/// from `payable` defaults to payable (conservative — never release without positive evidence of
/// death).
///
/// `attempt_held` is the set whose RELEASE decision the award-attempt machinery owns (see
/// [`store::BuyerStore::attempt_held_job_ids`]): their `Dead` verdict is downgraded to `Payable`
/// so the funds stay, while every other verdict — notably `Paid` — is left to act.
fn plan_reconcile(
    reserved: &[String],
    progress: &BTreeMap<String, PaymentProgress>,
    payable: &BTreeMap<String, bool>,
    attempt_held: &std::collections::BTreeSet<String>,
    ages: &BTreeMap<String, u64>,
    floor: lifecycle::UnattemptedFloor,
) -> Dispositions {
    let mut dispositions: Dispositions = BTreeMap::new();
    for job_id in reserved {
        let payment = progress.get(job_id).copied().unwrap_or(PaymentProgress::None);
        let claim_payable = payable.get(job_id).copied().unwrap_or(true);
        let verdict = lifecycle::classify_disposition(payment, claim_payable);
        // The local-clock floor runs BEFORE the attempt-held downgrade below, so a job the attempt
        // machinery owns still wins: the floor can propose `Dead`, and the downgrade then takes it
        // back to `Payable`. Ordering them the other way would let the floor override the one
        // component that knows a send is in flight.
        let verdict =
            lifecycle::apply_unattempted_floor(verdict, payment, ages.get(job_id).copied(), floor);
        // The attempt machinery owns the RELEASE decision for jobs it holds — but ONLY that
        // decision. Downgrading `Dead → Payable` (keep the funds) leaves `Paid → spent`
        // untouched, which matters: reconcile's Paid arm is the only converger for a pay whose
        // `reserved → spent` flip failed, so dropping these jobs from the batch entirely would
        // suppress a correction that frees a double-count (round-7 review).
        let verdict = match verdict {
            JobDisposition::Dead if attempt_held.contains(job_id) => JobDisposition::Payable,
            other => other,
        };
        dispositions.insert(job_id.clone(), verdict);
    }
    dispositions
}

/// Fold every payment-journal attempt under the home into a `job_id → progress` map. Each record
/// carries its [`crate::payment::PaymentKey`] (hence its `job_id`), so no attempt-id recomputation
/// is needed. A journal that cannot be read/folded is treated as `Uncertain` (kept, never
/// released) — reconcile must fail safe, never free funds on ambiguous evidence.
fn scan_payment_progress(home: &MaxplayerHome) -> BTreeMap<String, PaymentProgress> {
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
        Some(PaymentState::Intent { .. }) | Some(PaymentState::Locked { .. }) => {
            PaymentProgress::Attempted
        }
        None => PaymentProgress::None,
    }
}

/// The more-advanced of two progresses (`Closed` > `Uncertain` > `Attempted` > `None`) — a job with
/// any Closed attempt is Paid regardless of an earlier abandoned attempt.
///
/// `Attempted` outranks `None` for the same reason the others are ordered: across two journals for
/// one job, evidence that an attempt happened must not be masked by a journal that shows none.
fn merge_progress(existing: Option<PaymentProgress>, next: PaymentProgress) -> PaymentProgress {
    fn rank(progress: PaymentProgress) -> u8 {
        match progress {
            PaymentProgress::None => 0,
            PaymentProgress::Attempted => 1,
            PaymentProgress::Uncertain => 2,
            PaymentProgress::Closed => 3,
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

    // Report every configured and wallet-DB-discovered mint's balance (#266). The top-level `mint`
    // + `balance_sats` (the default mint) stay for back-compat; `total_sats` is whole-DB truth and
    // `configured_total_sats` preserves the configured subset. Read through the wallet actor's
    // single slot, so it never races a spend.
    let mint = context.home.config.default_mint().to_owned();
    let wallet = match context.wallet.balances().await {
        Ok(Ok(rows)) => {
            let total_sats: u64 = rows.iter().map(|row| row.balance_sats).sum();
            let configured_total_sats: u64 = rows
                .iter()
                .filter(|row| row.configured)
                .map(|row| row.balance_sats)
                .sum();
            let default_balance = rows
                .iter()
                .find(|row| row.is_default)
                .map_or(0, |row| row.balance_sats);
            let mints: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "mint": row.mint_url,
                        "role": if !row.configured {
                            "unconfigured"
                        } else if row.is_default {
                            "default"
                        } else {
                            "extra"
                        },
                        "balance_sats": row.balance_sats,
                    })
                })
                .collect();
            json!({
                "mint": mint,
                "balance_sats": default_balance,
                "configured_total_sats": configured_total_sats,
                "total_sats": total_sats,
                "mints": mints,
            })
        }
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

    // Surface open award attempts (#322): a pending attempt HOLDS its reservation on purpose —
    // reconcile skips it while the relay verdict is open — and without this field that hold is
    // invisible everywhere but stderr, leaving "why is my available low?" unanswerable from
    // status (round-3 review). Manual-path attempts have no intent row, so `parked_awards`
    // alone cannot cover them.
    let pending_attempts: Vec<Value> = context
        .store
        .pending_award_attempts()
        .unwrap_or_default()
        .into_iter()
        .map(|attempt| {
            json!({
                "job_id": attempt.job_id,
                "award_event_id": attempt.award_event_id,
                "amount_sats": attempt.amount_sats,
                "offer_deadline_unix": attempt.offer_deadline_unix,
                "send_count": attempt.send_count,
            })
        })
        .collect();

    // Surface awards that are PUBLIC but whose local row is missing (#322 round-6 review): the
    // crash window between the relay's ack and `record_award`, or a repair the wallet cannot
    // currently fund. A seller is owed money in this state and it is enumerable in no other
    // field — `pending_award_attempts` selects only pending rows, and on the manual path there is
    // no intent row for `parked_awards` to carry. Intent-independent, so it covers both paths.
    let unrecorded_confirmed_awards: Vec<Value> = context
        .store
        .confirmed_attempts_without_award_row()
        .unwrap_or_default()
        .into_iter()
        .map(|attempt| {
            json!({
                "job_id": attempt.job_id,
                "award_event_id": attempt.award_event_id,
                "amount_sats": attempt.amount_sats,
                "seller_pubkey": attempt.seller_pubkey,
            })
        })
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
            "pending_award_attempts": pending_attempts,
            "unrecorded_confirmed_awards": unrecorded_confirmed_awards,
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
    /// Every pre-existing reconcile test runs with the local-clock floor OFF, which is how it
    /// ships. Naming it here means each call site states that rather than leaving it implied.
    const FLOOR_OFF: lifecycle::UnattemptedFloor = lifecycle::UnattemptedFloor {
        enabled: false,
        grace_secs: 0,
    };

    use super::*;
    use crate::home::bootstrap as bootstrap_home;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    // #774 defence in depth. `raise_open_file_limit` claims it lifts the soft descriptor limit
    // toward the hard one; a comment saying so is not evidence, so this executes it.
    //
    // In a CHILD process, for the same reason the #774 reproduction uses one: the call mutates a
    // process-wide limit, and doing that inside the shared test binary would change the ceiling
    // every sibling test runs under. `ulimit -S` lowers only the SOFT limit, leaving the hard one
    // where it was — which is precisely the situation the function exists to recover from, and
    // would be untestable had the fixture used a bare `ulimit -n` (that lowers BOTH, and no
    // unprivileged process can raise a hard limit back).
    const RAISE_CHILD_ENV: &str = "MAXPLAYER_RAISE_NOFILE_CHILD";
    #[cfg(target_os = "linux")]
    const RAISE_CHILD_SOFT: usize = 64;

    #[cfg(target_os = "linux")]
    fn soft_nofile() -> usize {
        let limits = std::fs::read_to_string("/proc/self/limits").expect("read /proc/self/limits");
        for line in limits.lines() {
            let Some(rest) = line.strip_prefix("Max open files") else {
                continue;
            };
            return rest
                .split_whitespace()
                .next()
                .expect("soft-limit column")
                .parse()
                .expect("numeric soft limit");
        }
        panic!("no 'Max open files' row in /proc/self/limits");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn raise_nofile_child_leg() {
        if std::env::var(RAISE_CHILD_ENV).is_err() {
            return;
        }
        let before = soft_nofile();
        assert_eq!(
            before, RAISE_CHILD_SOFT,
            "fixture must hand this child a lowered SOFT limit; got {before}"
        );

        raise_open_file_limit();

        let after = soft_nofile();
        assert!(
            after > before,
            "raise_open_file_limit must lift the soft limit above {before}, got {after}"
        );
        // Positive control for the parent: a child that took the early return above would also
        // exit 0, so success alone does not prove the body ran. This line does.
        println!("RAISED {before} -> {after}");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn raise_open_file_limit_lifts_a_lowered_soft_limit() {
        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "ulimit -S -n {RAISE_CHILD_SOFT}; exec \"$0\" \
                 --exact buyer::tests::raise_nofile_child_leg --nocapture --test-threads=1"
            ))
            .arg(&exe)
            .env(RAISE_CHILD_ENV, "1")
            .output()
            .expect("spawn soft-limited child");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "child leg failed:\n{combined}");
        assert!(
            combined.contains("RAISED "),
            "the child exited 0 without running its body — the env guard or the test name is \
             wrong, and this test proves nothing:\n{combined}"
        );
    }

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("maxplayer-buyer-mod-{label}-{}-{id}", std::process::id()))
    }

    #[test]
    fn get_job_timeout_rpc_boundary_rejects_above_cap_and_accepts_cap_or_default() {
        let rejected = get_job_timeout_error(json!(7), Some(300))
            .expect("a timeout above the cap must be rejected");
        let error = rejected.error.expect("rejection is an RPC error");
        assert_eq!(error.code, CODE_METHOD_NOT_FOUND);
        assert!(
            error.message.contains("timeout_secs=300 exceeds"),
            "message: {}",
            error.message
        );
        assert!(
            error.message.contains("10s"),
            "message: {}",
            error.message
        );

        assert!(
            get_job_timeout_error(json!(8), Some(job_lifecycle::WAIT_FOR_CAP_SECS)).is_none(),
            "the exact cap must pass boundary validation"
        );
        assert!(
            get_job_timeout_error(json!(9), None).is_none(),
            "omitting timeout_secs must retain the default behavior"
        );
    }

    /// #539: the operator-console diagnostics for an unaffordable award and a failed settle must
    /// carry the shortfall NUMBERS in their roles, not merely announce that something was refused — a
    /// money-debugging line that names no numbers (or transposes them) is the misdirection this issue
    /// exists to kill. Asserting the composed line through the SAME formatter the handlers emit makes
    /// a dropped or swapped field red-provable, which the raw `opline!` stderr boundary cannot be.
    #[test]
    fn spend_refusal_lines_carry_the_shortfall_numbers_in_role() {
        use crate::crossmint_hop::HopError;
        use reservations::{Ceiling, ReserveRefused};

        // Award reservation refusal → need vs available, un-transposed.
        let refused = ReserveRefused::InsufficientAvailable {
            requested: 150,
            available: 42,
            bound: Ceiling::Wallet,
        };
        let line = cannot_afford_award_line("job-abc", &refused);
        assert!(line.contains("job-abc"), "names the job: {line}");
        assert!(
            line.contains("150 sat exceeds available 42"),
            "carries need (150) and available (42) without swapping them: {line}"
        );

        // Cross-mint settle shortfall → need, held, AND the mint that is short.
        let hop = HopError::InsufficientSource {
            mint: "https://mint.example/Bitcoin".to_owned(),
            balance: 42,
            planned_cost: 150,
        };
        let line = could_not_settle_line("job-xyz", &hop);
        assert!(line.contains("job-xyz"), "names the job: {line}");
        assert!(line.contains("holds 42 sats"), "carries what the buyer holds: {line}");
        assert!(line.contains("costs 150 sats"), "carries the hop cost (need): {line}");
        assert!(line.contains("mint.example"), "names the mint that is short: {line}");
    }

    // #322: the manual award RPC's write-once gate. A named claim contradicting the pinned one
    // is the ONLY conflict; omitting the claim, or naming the pinned one, resolves the attempt.
    #[test]
    fn pinned_claim_conflict_refuses_only_a_contradicting_name() {
        assert!(!pinned_claim_conflict("claim-a", None), "omitting claim_id resolves the pin");
        assert!(!pinned_claim_conflict("claim-a", Some("claim-a")), "naming the pin resolves it");
        assert!(
            pinned_claim_conflict("claim-a", Some("claim-b")),
            "naming another claim is refused — awards are write-once per offer"
        );
    }

    fn attempt_fixture(job: &str, relay_url: &str) -> store::AwardAttempt {
        store::AwardAttempt {
            job_id: job.to_owned(),
            claim_id: "c".repeat(64),
            // A REAL point: the delivery probe parses this as a nostr pubkey (its author filter
            // is the round-3 anti-griefing guard), so a non-hex placeholder would turn every
            // probe into an error-hold and mask the arm under test.
            seller_pubkey: nostr_sdk::prelude::Keys::generate().public_key().to_hex(),
            award_event_id: "e".repeat(64),
            event_json: "{\"id\":\"x\"}".to_owned(),
            amount_sats: 40,
            quoted_mints_json: "[]".to_owned(),
            offer_deadline_unix: 1_000,
            send_count: 1,
            relay_url: relay_url.to_owned(),
            state: store::AttemptState::Pending,
            detail: None,
        }
    }

    // #322 round 2: the pay-window boundary is the ONE comparison the terminalize decision rests
    // on — pin both edges and the saturation.
    #[test]
    fn past_pay_window_flips_exactly_after_the_grace() {
        let deadline = 1_000i64;
        let end = deadline + job_lifecycle::DELIVERY_PAY_WINDOW_SECS as i64;
        assert!(!past_pay_window(end, deadline), "AT the window end is still inside it");
        assert!(past_pay_window(end + 1, deadline), "one second past the end terminalizes");
        assert!(
            !past_pay_window(i64::MAX, i64::MAX),
            "a saturating deadline never reads as past its own window"
        );
    }

    // #322 round 2: migrated attempts carry the '' relay sentinel; resolution must fall back to
    // live config or that population can neither send nor probe, ever.
    #[test]
    fn attempt_relay_falls_back_to_config_for_the_migration_sentinel() {
        let root = temp_home("attempt-relay");
        let home = bootstrap_home(&root).expect("home");
        assert_eq!(
            attempt_relay(&attempt_fixture(&"a".repeat(64), "ws://pinned.example"), &home),
            "ws://pinned.example",
            "a real pin wins over config"
        );
        assert_eq!(
            attempt_relay(&attempt_fixture(&"a".repeat(64), ""), &home),
            home.config.relay_url,
            "the migration sentinel resolves to live config"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ★ #322 round 2: the refusal transition is the LICENSE for the release. A resolver that
    // loses the pending→refused race (the attempt is already confirmed — its award public, its
    // funds re-held) must write NOTHING: releasing anyway strips funds from a recorded award.
    #[test]
    fn terminalize_absent_attempt_releases_only_when_it_wins_the_transition() {
        let root = temp_home("terminalize-gate");
        std::fs::create_dir_all(&root).expect("temp dir");
        let store =
            store::BuyerStore::open(root.join("buyer.sqlite")).expect("open store");
        let job = "a".repeat(64);
        store.reserve(&job, 40, 100, 1).expect("reserve");
        store
            .begin_award_attempt(&attempt_fixture(&job, "ws://relay.test"), 1)
            .expect("pin");

        // Case 1: the attempt was CONFIRMED by a concurrent resolver — the gate must refuse to
        // act, leaving the reservation exactly as it found it.
        assert!(store.mark_attempt_confirmed(&job, 2).expect("confirm"));
        assert!(
            !terminalize_absent_attempt(&store, &job, "absent past window", 3).expect("gated"),
            "losing the transition must return false"
        );
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(super::reservations::ReservationState::Reserved),
            "a lost race must not touch the funds"
        );
        assert_eq!(
            store.award_attempt(&job).expect("read").expect("row").state,
            store::AttemptState::Confirmed,
            "and must not rewrite the verdict"
        );

        // Case 2: a genuinely pending attempt — the gate wins, refuses, releases, AND parks the
        // intent (the third write, so the refusal reason reaches `status`; without an intent row
        // the park would be a silent no-op and the assertion below could not see it).
        let pending_job = "b".repeat(64);
        store.put_pending_award(&pending_job, 40, None, None, 4).expect("intent");
        store.reserve(&pending_job, 40, 100, 4).expect("reserve");
        store
            .begin_award_attempt(&attempt_fixture(&pending_job, "ws://relay.test"), 4)
            .expect("pin");
        assert!(
            terminalize_absent_attempt(&store, &pending_job, "absent past window", 5)
                .expect("terminalize"),
            "winning the transition returns true"
        );
        let after = store.award_attempt(&pending_job).expect("read").expect("row");
        assert_eq!(after.state, store::AttemptState::Refused);
        assert_eq!(after.detail.as_deref(), Some("absent past window"));
        assert_eq!(
            store.reservation(&pending_job).expect("read").map(|(state, _)| state),
            Some(super::reservations::ReservationState::Released),
            "the winner releases the funds"
        );
        assert!(
            store
                .parked_awards()
                .expect("parked")
                .iter()
                .any(|(job, reason)| job == &pending_job && reason == "absent past window"),
            "and parks the intent with the refusal reason — never a silent drop"
        );

        // Case 3: idempotent replay — the second call loses the (now spent) transition.
        assert!(
            !terminalize_absent_attempt(&store, &pending_job, "absent past window", 6)
                .expect("replay"),
            "a replay must be a no-op"
        );

        // Case 4 (round 3): an awards ROW outranks any probe emptiness — it is written only on
        // an ack or a presence-verified repair, so a pending attempt WITH its row (the
        // confirm-write-failed crash) must never be refused, however absent the probes read.
        let recorded_job = "d".repeat(64);
        store.reserve(&recorded_job, 40, 100, 7).expect("reserve");
        store
            .begin_award_attempt(&attempt_fixture(&recorded_job, "ws://relay.test"), 7)
            .expect("pin");
        store
            .record_award(&recorded_job, &"c".repeat(64), &"e".repeat(64), &"s".repeat(64), 40, 8)
            .expect("record");
        assert!(
            !terminalize_absent_attempt(&store, &recorded_job, "absent past window", 9)
                .expect("gated"),
            "a recorded award must never be terminalized by probe emptiness"
        );
        assert_eq!(
            store.reservation(&recorded_job).expect("read").map(|(state, _)| state),
            Some(super::reservations::ReservationState::Reserved),
            "the recorded award keeps its funds"
        );
        assert_eq!(
            store.award_attempt(&recorded_job).expect("read").expect("row").state,
            store::AttemptState::Pending,
            "and its attempt stays pending for the AlreadyAwarded confirm"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // #322 round 3: the deadline TOCTOU predicate — only a PENDING attempt can cross (terminal
    // states never transmit), and the crossing is strict.
    #[test]
    fn resume_crossed_deadline_blocks_only_a_pending_attempt_past_its_deadline() {
        let mut attempt = attempt_fixture(&"a".repeat(64), "ws://relay.test");
        attempt.offer_deadline_unix = 1_000;
        assert!(!resume_crossed_deadline(&attempt, 1_000), "AT the deadline still sends");
        assert!(resume_crossed_deadline(&attempt, 1_001), "past it blocks");
        attempt.state = store::AttemptState::Confirmed;
        assert!(!resume_crossed_deadline(&attempt, 1_001), "a terminal state never transmits");
        attempt.state = store::AttemptState::Refused;
        assert!(!resume_crossed_deadline(&attempt, 1_001));
    }

    // ★ #322 round 3: reconcile must LEAVE pending-attempt jobs alone — their funds are held on
    // purpose while the award's relay verdict is open. Premise-checked like the mid-settle tooth:
    // the job WOULD classify Dead, so without the skip it is genuinely at risk. Red-on-revert:
    // drop the `Dead if attempt_held.contains(..) => Payable` downgrade in `plan_reconcile` (or
    // pass an empty held set) and the release
    // lands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reconcile_leaves_a_pending_attempts_reservation_alone() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let root = temp_home("reconcile-skips-attempts");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let job = "a".repeat(64);
        context.store.reserve(&job, 4, 1_000, now_unix()).expect("reserve");
        let mut pinned = attempt_fixture(&job, &context.home.config.relay_url);
        pinned.amount_sats = 4;
        context.store.begin_award_attempt(&pinned, now_unix()).expect("pin");

        // Non-vacuity: without the skip this job classifies Dead and WOULD be released.
        let would_release = plan_reconcile(
            &[job.clone()],
            &BTreeMap::new(),
            &BTreeMap::from([(job.clone(), false)]),
            &std::collections::BTreeSet::new(),
            &BTreeMap::new(),
            FLOOR_OFF,
        );
        assert_eq!(would_release[&job], reservations::JobDisposition::Dead);

        let report = reconcile_reservations(&context).await.expect("reconcile");
        assert!(
            !report.released.contains(&job),
            "a pending attempt's reservation is held ON PURPOSE; reconcile released it: {report:?}"
        );
        // ATTRIBUTION (round-8 review): the held job must be KEPT, not absent. The two negatives
        // above are satisfied identically by dropping it from the batch — which is exactly the
        // round-6 defect, and which would also suppress reconcile's `Paid → spent` convergence
        // (the only converger for a pay whose flip failed). Only a positive `kept` assertion
        // proves the shield downgrades the VERDICT and leaves the job in the pass.
        assert!(
            report.kept.contains(&job),
            "the held job must stay IN the batch (kept), not be dropped from it: {report:?}"
        );
        assert_eq!(
            context.store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Reserved),
            "the funds stay committed while the verdict is open"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #322 round 3, the triple tooth: the manual award RPC on a past-pay-window pinned attempt
    // (award confirmed absent) must TERMINALIZE and ANSWER — not deadlock. This is the exact call
    // shape that self-deadlocked in round 2 (the RPC held money_lock into a helper that re-locks
    // it), so the whole call runs under a hard timeout: a re-introduced nesting wedges the task
    // and the timeout fails the test. It also pins the expired-branch response and the
    // ConfirmedAbsent-past-window arm end to end against a real (empty) relay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_expired_award_retry_terminalizes_and_answers_without_wedging_the_daemon() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let root = temp_home("expired-award-rpc");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let job = "a".repeat(64);
        context.store.reserve(&job, 4, 1_000, now_unix()).expect("reserve");
        let mut pinned = attempt_fixture(&job, &context.home.config.relay_url);
        pinned.amount_sats = 4;
        // Past the offer deadline AND the 7-day pay window: the probe (confirmed absent against
        // the empty relay, twice — no award event, no delivery) licenses the terminalization.
        pinned.offer_deadline_unix = now_unix() - 9 * 24 * 3_600;
        context.store.begin_award_attempt(&pinned, now_unix()).expect("pin");

        let response = tokio::time::timeout(
            Duration::from_secs(60),
            award(&context, json!(1), json!({ "job_id": job })),
        )
        .await
        .expect("the award RPC must ANSWER — a timeout here is the round-2 deadlock resurfacing");

        let error = response.error.expect("a refused terminal attempt is an error response");
        assert!(
            error.message.contains("was refused"),
            "the response names the refusal: {}",
            error.message
        );
        let after = context.store.award_attempt(&job).expect("read").expect("row");
        assert_eq!(after.state, store::AttemptState::Refused, "terminalized");
        assert_eq!(
            context.store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Released),
            "the funds came back"
        );
        assert!(context.store.award_record(&job).expect("read").is_none(), "nothing recorded");
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #322 round 4: the anti-griefing author filter on the delivery guard. A third party's junk
    // kind-3403 for the job must NOT hold the refund — without the `.author(pinned seller)`
    // filter, one signed event from any pubkey pins the buyer's funds forever (reconcile skips
    // pending attempts, and a forged result can never be collected, so there is no exit).
    // Red-on-revert: drop the author filter in `job_has_results_async` and the terminalization
    // below stops landing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_third_partys_junk_delivery_cannot_hold_the_refund() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, EventBuilder, Keys, Kind, Tag};

        let root = temp_home("junk-delivery");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let job = "a".repeat(64);
        context.store.reserve(&job, 4, 1_000, now_unix()).expect("reserve");
        let mut pinned = attempt_fixture(&job, &context.home.config.relay_url);
        pinned.amount_sats = 4;
        pinned.offer_deadline_unix = now_unix() - 9 * 24 * 3_600; // past deadline AND pay window
        context.store.begin_award_attempt(&pinned, now_unix()).expect("pin");

        // A THIRD PARTY (never the pinned seller) publishes a result e-tagging the job.
        let griefer = Keys::generate();
        let client = Client::new(griefer.clone());
        client.add_relay(&context.home.config.relay_url).await.expect("add relay");
        client.connect().await;
        let junk = EventBuilder::new(Kind::Custom(3403), "")
            .tag(Tag::parse(vec!["e".to_owned(), job.clone()]).expect("e tag"))
            .tag(Tag::parse(vec!["t".to_owned(), crate::gateway::MAXPLAYER_TAG.to_owned()]).expect("t tag"))
            .sign_with_keys(&griefer)
            .expect("sign junk");
        client.send_event(&junk).await.expect("relay stores the junk result");
        client.disconnect().await;

        resolve_award_attempts(&context).await;

        let after = context.store.award_attempt(&job).expect("read").expect("row");
        assert_eq!(
            after.state,
            store::AttemptState::Refused,
            "a stranger's 3403 must not hold the terminalization — only the AWARDED seller's \
             delivery is evidence our award was public"
        );
        assert_eq!(
            context.store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Released),
            "and the buyer's funds must come back"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #602: `fetch_job_view_async` must certify offer-ABSENCE from the OFFER read alone. The bug
    // was `read_confirmed = offer || feedback || result || probe`, so a present CLAIM certified an
    // empty offer read as absence and `drive_auto_award` terminally parked a retryable offer. Here a
    // seller publishes a claim e-tagging the job while NO offer exists — the exact asymmetric shape
    // the #291/#298 tests never exercised (they drove `plan_missing_offer` as a pure fn over a bool).
    // The view must surface the claim AND report `read_confirmed` established by the offer's own
    // probe+re-fetch (the relay answered our REQ), not by the claim. Red-on-revert of the PROBE:
    // drop the probe/re-fetch and `read_confirmed` goes false on this input; the original blend is
    // blocked structurally (the claim read runs after the decision) and by `offer_read_answered`'s
    // signature.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_present_claim_does_not_certify_an_empty_offer_read_as_absence() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, EventBuilder, Keys, Kind, Tag};

        let root = temp_home("offer-602-asymmetric");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();
        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");

        // A synthetic job id: no offer with this id can exist on the relay, so the offer read is
        // genuinely empty — while a seller's claim e-tagging it is served.
        let job = "b".repeat(64);
        let seller = Keys::generate();
        let publisher = Client::new(seller.clone());
        publisher
            .add_relay(&context.home.config.relay_url)
            .await
            .expect("add relay");
        publisher.connect().await;
        let claim = EventBuilder::new(Kind::Custom(crate::kinds::JOB_CLAIM_KIND), "")
            .tag(Tag::parse(vec!["e".to_owned(), job.clone()]).expect("e tag"))
            .tag(Tag::parse(vec!["t".to_owned(), crate::gateway::MAXPLAYER_TAG.to_owned()]).expect("t tag"))
            .tag(Tag::parse(vec!["status".to_owned(), "processing".to_owned()]).expect("status tag"))
            .sign_with_keys(&seller)
            .expect("sign claim");
        publisher.send_event(&claim).await.expect("relay stores the claim");
        publisher.disconnect().await;

        let keys = buyer_keys(&context.home).expect("buyer keys");
        let view =
            job_lifecycle::fetch_job_view_async(&context.home, &keys, &job, RELAY_TIMEOUT, now_unix() as u64)
                .await
                .expect("fetch view");

        assert!(view.offer.is_none(), "no offer for this job exists on the relay");
        assert_eq!(
            view.claims.len(),
            1,
            "the claim IS served — this is the asymmetric shape (#602), not an unreachable relay"
        );
        assert!(
            view.read_confirmed,
            "the relay proved it served our OFFER REQ (EOSE + re-fetch), so the empty offer read is \
             confirmed — established by the offer's own probe, never by the present claim (#602)"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #602 companion: the fast path + the id-hardening. When the offer IS on the relay it is
    // returned (never parked) with no probe round trip, and the view reports exactly the queried id
    // — the `event.id == offer_id` assertion the award path already carries.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_present_offer_is_returned_confirmed_and_by_exact_id() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, EventBuilder, Kind, Tag};

        let root = temp_home("offer-602-present");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();
        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let keys = buyer_keys(&context.home).expect("buyer keys");

        // A real offer event — its id is a genuine content hash, and that id IS the job id.
        let offer_event = EventBuilder::new(Kind::Custom(crate::kinds::JOB_OFFER_KIND), "")
            .tag(Tag::parse(vec!["t".to_owned(), crate::gateway::MAXPLAYER_TAG.to_owned()]).expect("t tag"))
            .sign_with_keys(&keys)
            .expect("sign offer");
        let job = offer_event.id.to_hex();
        let publisher = Client::new(keys.clone());
        publisher
            .add_relay(&context.home.config.relay_url)
            .await
            .expect("add relay");
        publisher.connect().await;
        publisher.send_event(&offer_event).await.expect("relay stores the offer");
        publisher.disconnect().await;

        let view =
            job_lifecycle::fetch_job_view_async(&context.home, &keys, &job, RELAY_TIMEOUT, now_unix() as u64)
                .await
                .expect("fetch view");

        let offer = view.offer.expect("the published offer is returned, not parked");
        assert_eq!(offer.event_id, job, "the returned offer is exactly the one queried, by id");
        assert!(view.read_confirmed, "an offer in hand is itself the answer");
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #602 ACCEPTANCE-BAR REGRESSION. A relay `QueryPolicy` that REFUSES exactly the by-id OFFER
    // read (`.id(offer_id).kind(3401)`) with a `CLOSED`, while serving every other REQ — the claim
    // read, the liveness probe (`kind 3401` but `limit(0)`, no ids), all of it. This stages the
    // literal #602 asymmetry — the offer read FAILS (never an EOSE) while the relay is otherwise
    // fully alive and the claim is served — as a DETERMINISTIC, NON-BLOCKING, FILTER-SCOPED starve.
    // A sleep-based starve would serialize `LocalRelay` admission and stall the claim too (a
    // different situation that would lie green); a `Reject` blocks nothing and touches only the
    // offer filter. `admit_query` receives the REQ's filter, so the scoping is exact.
    #[derive(Debug)]
    struct RefuseOfferReads;

    impl nostr_relay_builder::prelude::QueryPolicy for RefuseOfferReads {
        fn admit_query<'a>(
            &'a self,
            query: &'a nostr_sdk::Filter,
            _addr: &'a std::net::SocketAddr,
        ) -> nostr_relay_builder::prelude::BoxedFuture<
            'a,
            nostr_relay_builder::prelude::PolicyResult,
        > {
            Box::pin(async move {
                // The offer read is the ONLY by-id request for the offer kind; the claim/feedback/
                // result reads carry `#e` not `.id`, and the probe carries no ids. Scoping to
                // `ids.is_some()` refuses the offer read alone and leaves the relay provably alive.
                let refuses_offer_read = query.ids.is_some()
                    && query.kinds.as_ref().is_some_and(|kinds| {
                        kinds.contains(&nostr_sdk::Kind::Custom(crate::kinds::JOB_OFFER_KIND))
                    });
                if refuses_offer_read {
                    nostr_relay_builder::prelude::PolicyResult::Reject(
                        "offer read starved (#602 regression)".to_owned(),
                    )
                } else {
                    nostr_relay_builder::prelude::PolicyResult::Accept
                }
            })
        }
    }

    // ★ #602 (unit): a REFUSED offer read is UNKNOWN, never absence — even with a live claim. The
    // #603 tests above assert `read_confirmed == true` (offer present / genuinely-empty-with-probe);
    // neither stages the bug — an offer read that FAILS (relay CLOSED the REQ, no EOSE) WHILE a
    // claim is served. Post-#603 the offer read goes single-relay `ExitOnEOSE`, so the refusal
    // surfaces as `Err` and `fetch_job_view_async` returns `Err` (the caller then retries, never
    // parks). Under the pre-#603 blend the pool swallowed the CLOSED to `Ok(empty)` and the served
    // claim drove `read_confirmed = true` → `Ok(view)` with the offer absent-certified. So this
    // REDS on the blend (returns `Ok`, `is_err()` fails). The served-claim assertion first proves
    // the starve is NON-CONFOUNDED (the relay is alive) so the `Err` is the offer read's alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_refused_offer_read_is_not_certified_as_absence_even_with_a_live_claim() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, EventBuilder, EventId, Filter, Keys, Kind, Tag};

        let root = temp_home("offer-602-refused");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default().query_policy(RefuseOfferReads));
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();
        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");

        // A synthetic job id: no offer with this id exists, and the relay refuses the by-id offer
        // REQ regardless — but the seller's claim e-tagging it IS served (only the offer read is
        // starved).
        let job = "b".repeat(64);
        let seller = Keys::generate();
        let publisher = Client::new(seller.clone());
        publisher.add_relay(&context.home.config.relay_url).await.expect("add relay");
        publisher.connect().await;
        let claim = EventBuilder::new(Kind::Custom(crate::kinds::JOB_CLAIM_KIND), "")
            .tag(Tag::parse(vec!["e".to_owned(), job.clone()]).expect("e tag"))
            .tag(Tag::parse(vec!["t".to_owned(), crate::gateway::MAXPLAYER_TAG.to_owned()]).expect("t tag"))
            .tag(Tag::parse(vec!["status".to_owned(), "processing".to_owned()]).expect("status tag"))
            .sign_with_keys(&seller)
            .expect("sign claim");
        publisher.send_event(&claim).await.expect("relay stores the claim");

        // NON-CONFOUND: the claim read is genuinely served under this policy — the starve is
        // filter-scoped to the offer REQ, not a relay outage that would stall everything and lie.
        let served = publisher
            .fetch_events(
                Filter::new()
                    .kind(Kind::Custom(crate::kinds::JOB_CLAIM_KIND))
                    .hashtag(crate::gateway::MAXPLAYER_TAG)
                    .event(EventId::from_hex(&job).expect("job id")),
                RELAY_TIMEOUT,
            )
            .await
            .expect("claim read is served");
        assert_eq!(served.len(), 1, "the claim IS served — a NON-confounded, filter-scoped starve (relay alive)");
        publisher.disconnect().await;

        // THE OFFER READ IS REFUSED ⇒ the view read is UNKNOWN, not absence. Post-#603 the
        // single-relay `ExitOnEOSE` offer read surfaces the CLOSED as `Err`. Under the pre-#603
        // blend this returned `Ok(view)` with `read_confirmed == true`, certifying false absence.
        let keys = buyer_keys(&context.home).expect("buyer keys");
        let view =
            job_lifecycle::fetch_job_view_async(&context.home, &keys, &job, RELAY_TIMEOUT, now_unix() as u64)
                .await;
        assert!(
            view.is_err(),
            "a REFUSED offer read (relay CLOSED the REQ, no EOSE) is UNKNOWN, never absence — a \
             served claim must NOT turn it into Ok(read_confirmed=true) (#602). got: {view:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #602 (e2e — THE acceptance test): under a starved offer read + a live claim, the auto-award
    // driver must NEVER terminally park the retryable intent as offer-absent. This is the literal
    // #602 field harm — a ~302s re-claim loop — driven end-to-end through `drive_auto_award` /
    // `parked_awards()`. Post-#603 the refused offer read is `Err` → treated as UNKNOWN → the intent
    // is left retry-eligible (not parked). REDS on the pre-#603 BLEND: the served claim certifies
    // the empty offer read as absence → `ParkOfferAbsent` → `parked_awards()` non-empty.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_starved_offer_with_a_live_claim_never_parks_the_retryable_intent() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, EventBuilder, Keys, Kind, Tag};

        let root = temp_home("offer-602-no-park");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default().query_policy(RefuseOfferReads));
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();
        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");

        // A retryable intent: a pending award + a live reservation, and deliberately NO pinned
        // attempt — so the attempt-outranks-offer guard (`settle_intent_from_attempt`) cannot mask
        // the park, and the park path is genuinely reachable as it is in the field. The pending
        // intent row makes a park VISIBLE; without it `mark_award_parked` is a silent no-op.
        let job = "b".repeat(64);
        context.store.put_pending_award(&job, 40, None, None, now_unix()).expect("intent");
        context.store.reserve(&job, 40, 1_000, now_unix()).expect("reserve");

        // The seller's claim e-tagging the job IS served — only the by-id offer REQ is refused.
        let seller = Keys::generate();
        let publisher = Client::new(seller.clone());
        publisher.add_relay(&context.home.config.relay_url).await.expect("add relay");
        publisher.connect().await;
        let claim = EventBuilder::new(Kind::Custom(crate::kinds::JOB_CLAIM_KIND), "")
            .tag(Tag::parse(vec!["e".to_owned(), job.clone()]).expect("e tag"))
            .tag(Tag::parse(vec!["t".to_owned(), crate::gateway::MAXPLAYER_TAG.to_owned()]).expect("t tag"))
            .tag(Tag::parse(vec!["status".to_owned(), "processing".to_owned()]).expect("status tag"))
            .sign_with_keys(&seller)
            .expect("sign claim");
        publisher.send_event(&claim).await.expect("relay stores the claim");
        publisher.disconnect().await;

        // Drive the real auto-award path once. Post-#603 the refused offer read errors out of
        // `fetch_job_view_async` → out of `drive_auto_award` (never reaching a park). We ignore the
        // returned `Err` on purpose: the ASSERTION is on the durable effect — the intent stays
        // unparked. Under the blend this same call parks it.
        let _ = drive_auto_award(&context, &job, 40).await;

        let parked = context.store.parked_awards().expect("parked");
        assert!(
            !parked.iter().any(|(parked_job, _)| parked_job == &job),
            "a starved offer read + a live claim must NEVER terminally park a retryable intent as \
             offer-absent (#602) — the offer read FAILED, it was not answered-empty. parked: {parked:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ─────────────────────────────────────────────────────────────────────────────────────────
    // #562 + #574: release a held reservation PROMPTLY on a seller-reported POST-AWARD FAILURE
    // (delivery_failed and its siblings execution_failed / no_sentinel), instead of stranding the
    // funds until the deadline reconcile. These exercise the authorized core
    // (`release_reservation_on_failure_feedback`) directly — no relay, no bootstrap — exactly as the
    // terminalize tests do: the authorization + idempotency are pure store logic, and the money lock
    // (the caller's, a mirror of `terminalize_absent_attempt`) adds nothing a single-threaded test
    // can observe.
    // ─────────────────────────────────────────────────────────────────────────────────────────

    /// Build a seller-authored POST-AWARD FAILURE FEEDBACK (kind-3404) carrying `reason_code`, on the
    /// exact wire [`crate::gateway::error_draft`] emits: `status=error`, a root-marked `e` tag naming
    /// the offer, both `p` tags, and the authoritative `reason_code`. Signed by `author` — the gate
    /// that matters is the AUTHOR, so passing a stranger's keys forges a griefer's event.
    fn failure_feedback(offer_id: &str, author: &nostr_sdk::Keys, reason_code: &str) -> nostr_sdk::Event {
        failure_feedback_with_detail(offer_id, author, reason_code, None)
    }

    fn failure_feedback_with_detail(
        offer_id: &str,
        author: &nostr_sdk::Keys,
        reason_code: &str,
        reason_detail: Option<&str>,
    ) -> nostr_sdk::Event {
        use nostr_sdk::prelude::{EventBuilder, Keys, Kind, Tag};
        let buyer_hex = Keys::generate().public_key().to_hex();
        // The exact wire `error_draft` emits; `Tag::parse` on these vecs is what the production
        // builder's `to_tag` does. `allow_self_tagging` mirrors `gateway::nostr::event_builder` —
        // the seller feedback p-tags the seller, a self-tag the default builder would reject.
        let mut tags = vec![
            vec!["status".to_owned(), "error".to_owned()],
            vec!["e".to_owned(), offer_id.to_owned(), String::new(), "root".to_owned()],
            vec!["p".to_owned(), buyer_hex],
            vec!["p".to_owned(), author.public_key().to_hex()],
            vec!["reason_code".to_owned(), reason_code.to_owned()],
        ];
        if let Some(reason_detail) = reason_detail {
            tags.push(vec!["reason_detail".to_owned(), reason_detail.to_owned()]);
        }
        let mut builder = EventBuilder::new(
            Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND),
            format!("post-award failure: {reason_code}"),
        );
        builder.allow_self_tagging = true;
        for tag in tags {
            builder = builder.tag(Tag::parse(tag).expect("parse feedback tag"));
        }
        builder.sign_with_keys(author).expect("sign feedback")
    }

    /// Open a fresh store under `root` and seed ONE awarded-AND-unsettled job: a held reservation
    /// plus the published-award row that pins `seller` as the awarded party.
    fn awarded_unsettled(
        root: &std::path::Path,
        job: &str,
        seller: &nostr_sdk::Keys,
        amount: u64,
    ) -> store::BuyerStore {
        std::fs::create_dir_all(root).expect("temp dir");
        let store = store::BuyerStore::open(root.join("buyer.sqlite")).expect("open store");
        store.reserve(job, amount, 10_000, now_unix()).expect("reserve");
        store
            .record_award(
                job,
                &"c".repeat(64),
                &"e".repeat(64),
                &seller.public_key().to_hex(),
                amount,
                now_unix(),
            )
            .expect("record award");
        store
    }

    // Red-on-revert: make `release_reservation_on_failure_feedback` skip the `store.release` call (or
    // the watcher drop FEEDBACK) and the reservation stays `Reserved` — both assertions fail.
    #[test]
    fn a_delivery_failed_from_the_awarded_seller_releases_the_reservation() {
        let root = temp_home("df-release");
        let seller = nostr_sdk::Keys::generate();
        let job = "a".repeat(64);
        let store = awarded_unsettled(&root, &job, &seller, 100);

        let event = failure_feedback(&job, &seller, "delivery_failed");
        let outcome =
            release_reservation_on_failure_feedback(&store, &event, now_unix()).expect("no store error");

        assert_eq!(
            outcome,
            Some((job.clone(), 100)),
            "the awarded seller's delivery_failed must free exactly this job's held reservation"
        );
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Released),
            "the reservation must reach the SAME Released state the deadline reconcile produces"
        );
        assert_eq!(
            store.reserved_in_flight().expect("reserved"),
            0,
            "the freed funds must leave the reserved term — released, never spent"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ANTI-GRIEFING (the load-bearing security gate): a delivery_failed for a real awarded-unsettled
    // job but signed by anyone OTHER than the awarded seller MUST NOT release — else a stranger could
    // strand the reservation of a job that is in fact being delivered, dropping a legitimate payment.
    // Red-on-revert: drop the `author != award.seller_pubkey` gate and this releases, failing here.
    #[test]
    fn a_delivery_failed_from_a_non_awarded_pubkey_does_not_release() {
        let root = temp_home("df-griefer");
        let seller = nostr_sdk::Keys::generate();
        let job = "a".repeat(64);
        let store = awarded_unsettled(&root, &job, &seller, 100);

        // A stranger — never the awarded seller — signs an otherwise-perfect delivery_failed.
        let griefer = nostr_sdk::Keys::generate();
        let event = failure_feedback(&job, &griefer, "delivery_failed");
        let outcome =
            release_reservation_on_failure_feedback(&store, &event, now_unix()).expect("no store error");

        assert_eq!(outcome, None, "a non-awarded author must not trigger a release (anti-griefing)");
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Reserved),
            "the reservation must stay held — the job may still be delivering"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // IDEMPOTENCY / never-un-pay-a-delivery: a job already SETTLED (reservation `Spent`) must be a
    // strict no-op — the delivery happened, so a late or duplicate delivery_failed must never corrupt
    // it into `Released`. (`awarded_unsettled_job_ids` excludes spent jobs, so a settled job is never
    // even a release candidate; and `BuyerStore::release` returns `WasSpent` if it somehow were.)
    #[test]
    fn a_delivery_failed_for_an_already_settled_job_is_a_no_op() {
        let root = temp_home("df-settled");
        let seller = nostr_sdk::Keys::generate();
        let job = "a".repeat(64);
        let store = awarded_unsettled(&root, &job, &seller, 100);
        store.convert_to_spent(&job, 100, now_unix()).expect("settle");

        let event = failure_feedback(&job, &seller, "delivery_failed");
        let outcome =
            release_reservation_on_failure_feedback(&store, &event, now_unix()).expect("no store error");

        assert_eq!(outcome, None, "an already-settled job's feedback must not release anything");
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Spent),
            "the settled reservation must stay Spent — a delivered job's payment is never undone"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── #574 SIBLING WIDENING ─────────────────────────────────────────────────────────────────
    // `execution_failed` and `no_sentinel` are POST-AWARD failure codes on the byte-identical wire as
    // `delivery_failed` (same kind, same tags, `status=error` — see `crate::gateway::error_draft`), so
    // each must behave EXACTLY as delivery_failed through `release_reservation_on_failure_feedback`:
    // release from the awarded seller, refuse a non-awarded author (anti-griefing), no-op once settled.
    // The three assertions below are the same three the delivery_failed tests above make, parametrized
    // by reason_code so a sibling cannot silently diverge. Red-on-revert: narrow
    // `is_releasable_failure_feedback` back to delivery_failed only → every `*_releases_the_reservation`
    // sibling test fails at its release assertion.

    /// The awarded seller's `reason_code` failure frees exactly this job's held reservation (Released,
    /// never Spent) — the delivery_failed release property, asserted for a sibling code.
    fn assert_awarded_failure_releases(reason_code: &str) {
        let root = temp_home(&format!("sib-release-{reason_code}"));
        let seller = nostr_sdk::Keys::generate();
        let job = "a".repeat(64);
        let store = awarded_unsettled(&root, &job, &seller, 100);

        let event = failure_feedback_with_detail(
            &job,
            &seller,
            reason_code,
            (reason_code == "execution_failed").then_some("env_unprovisionable"),
        );
        let outcome =
            release_reservation_on_failure_feedback(&store, &event, now_unix()).expect("no store error");

        assert_eq!(
            outcome,
            Some((job.clone(), 100)),
            "the awarded seller's {reason_code} must free exactly this job's held reservation"
        );
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Released),
            "{reason_code} must reach the SAME Released state delivery_failed and the deadline reconcile produce"
        );
        assert_eq!(
            store.reserved_in_flight().expect("reserved"),
            0,
            "the freed funds must leave the reserved term — released, never spent"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A sibling failure from a NON-awarded author must not release — the author gate is inherited.
    fn assert_non_awarded_sibling_does_not_release(reason_code: &str) {
        let root = temp_home(&format!("sib-griefer-{reason_code}"));
        let seller = nostr_sdk::Keys::generate();
        let job = "a".repeat(64);
        let store = awarded_unsettled(&root, &job, &seller, 100);

        let griefer = nostr_sdk::Keys::generate();
        let event = failure_feedback(&job, &griefer, reason_code);
        let outcome =
            release_reservation_on_failure_feedback(&store, &event, now_unix()).expect("no store error");

        assert_eq!(
            outcome, None,
            "a non-awarded author's {reason_code} must not release (anti-griefing inherited)"
        );
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Reserved),
            "the reservation must stay held — the job may still be delivering"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A sibling failure for an already-SETTLED job is a strict no-op — idempotency is inherited.
    fn assert_settled_sibling_is_no_op(reason_code: &str) {
        let root = temp_home(&format!("sib-settled-{reason_code}"));
        let seller = nostr_sdk::Keys::generate();
        let job = "a".repeat(64);
        let store = awarded_unsettled(&root, &job, &seller, 100);
        store.convert_to_spent(&job, 100, now_unix()).expect("settle");

        let event = failure_feedback(&job, &seller, reason_code);
        let outcome =
            release_reservation_on_failure_feedback(&store, &event, now_unix()).expect("no store error");

        assert_eq!(outcome, None, "an already-settled job's {reason_code} must not release anything");
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Spent),
            "the settled reservation must stay Spent — a delivered job's payment is never undone"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_execution_failed_from_the_awarded_seller_releases_the_reservation() {
        assert_awarded_failure_releases("execution_failed");
    }
    #[test]
    fn an_execution_failed_from_a_non_awarded_pubkey_does_not_release() {
        assert_non_awarded_sibling_does_not_release("execution_failed");
    }
    #[test]
    fn an_execution_failed_for_an_already_settled_job_is_a_no_op() {
        assert_settled_sibling_is_no_op("execution_failed");
    }

    #[test]
    fn a_no_sentinel_from_the_awarded_seller_releases_the_reservation() {
        assert_awarded_failure_releases("no_sentinel");
    }
    #[test]
    fn a_no_sentinel_from_a_non_awarded_pubkey_does_not_release() {
        assert_non_awarded_sibling_does_not_release("no_sentinel");
    }
    #[test]
    fn a_no_sentinel_for_an_already_settled_job_is_a_no_op() {
        assert_settled_sibling_is_no_op("no_sentinel");
    }

    // BOUNDARY — the widening must NOT over-broaden to "any status=error feedback". The PRE-award
    // decline codes (below_rate / unsupported_version / mint_incompatible / at_capacity) name an offer
    // decline for which no award — hence no reservation — exists, and an unrecognised code is
    // fail-closed. Authored by the AWARDED seller so ONLY the reason_code discriminator can refuse:
    // none may release. Red-on-revert: widen `is_releasable_failure_feedback` to `status=error` alone
    // (dropping the reason_code allowlist) → every assertion below fails at the release.
    fn assert_awarded_seller_code_does_not_release(reason_code: &str) {
        let root = temp_home(&format!("boundary-{reason_code}"));
        let seller = nostr_sdk::Keys::generate();
        let job = "a".repeat(64);
        let store = awarded_unsettled(&root, &job, &seller, 100);

        let event = failure_feedback(&job, &seller, reason_code);
        let outcome =
            release_reservation_on_failure_feedback(&store, &event, now_unix()).expect("no store error");

        assert_eq!(outcome, None, "{reason_code} is not a post-award failure — it must not release");
        assert_eq!(
            store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Reserved),
            "the reservation must stay held — {reason_code} is not a release trigger"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pre_award_at_capacity_decline_does_not_release() {
        assert_awarded_seller_code_does_not_release("at_capacity");
    }
    #[test]
    fn a_pre_award_below_rate_decline_does_not_release() {
        assert_awarded_seller_code_does_not_release("below_rate");
    }
    #[test]
    fn an_unknown_reason_code_is_fail_closed_and_does_not_release() {
        assert_awarded_seller_code_does_not_release("some_future_code_we_do_not_know");
    }

    // WIRING TOOTH: the watcher must WAKE on FEEDBACK (3404), not only RESULT (3403) — the whole fix
    // is inert if a feedback event is dropped at the subscription filter before it can be considered.
    // Red-on-revert: restore the old `if event.kind != JOB_RESULT_KIND { continue; }` and the feedback
    // wake never fires, failing the `feedback == 1` assertion. The backstop is an hour out so ONLY the
    // two injected events wake the loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_watcher_wakes_on_both_results_and_feedback() {
        use nostr_sdk::prelude::{EventBuilder, Keys, Kind};
        use std::sync::atomic::AtomicUsize;

        let (sender, receiver) = tokio::sync::broadcast::channel(8);
        let (delivered, feedback) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));

        let (d, f) = (delivered.clone(), feedback.clone());
        let loop_task = tokio::spawn(async move {
            watch_loop(receiver, Duration::from_secs(3_600), move |wake| {
                let (d, f) = (d.clone(), f.clone());
                async move {
                    match wake {
                        WatchWake::Delivered(_) => {
                            d.fetch_add(1, Ordering::SeqCst);
                        }
                        WatchWake::Feedback(_) => {
                            f.fetch_add(1, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
            })
            .await;
        });

        let seller = Keys::generate();
        let result = EventBuilder::new(Kind::Custom(crate::kinds::JOB_RESULT_KIND), "")
            .sign_with_keys(&seller)
            .expect("sign result");
        let feedback_event = failure_feedback(&"a".repeat(64), &seller, "delivery_failed");
        let _ = sender.send(Arc::new(result));
        let _ = sender.send(Arc::new(feedback_event));

        // Long enough for the loop to drain both; the backstop tick is an hour away, so nothing else
        // can inflate the counts.
        tokio::time::sleep(Duration::from_millis(200)).await;
        loop_task.abort();

        assert_eq!(
            delivered.load(Ordering::SeqCst),
            1,
            "a RESULT (3403) must wake the watcher to settle"
        );
        assert_eq!(
            feedback.load(Ordering::SeqCst),
            1,
            "a FEEDBACK (3404) must wake the watcher to consider a release (#562)"
        );
    }

    // ★ #322 round 4: the deadline TOCTOU re-check is WIRED, not merely predicate-tested. The
    // pre-lock gate passes (deadline still ahead), the deadline then crosses while this call
    // waits on the money lock, and the under-lock re-check must refuse rather than transmit a
    // late award the seller would burn compute on. Deterministic: the lock holder keeps the lock
    // strictly longer than the deadline gap, and it fails safe (if the ordering ever inverted the
    // call would simply succeed pre-deadline). Red-on-revert: delete the re-check block in
    // `award()` and the RPC transmits, incrementing send_count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_deadline_crossed_while_queued_refuses_instead_of_transmitting_late() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let root = temp_home("deadline-toctou");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let job = "a".repeat(64);
        context.store.reserve(&job, 4, 1_000, now_unix()).expect("reserve");
        let mut pinned = attempt_fixture(&job, &context.home.config.relay_url);
        pinned.amount_sats = 4;
        // REAL signed bytes, not the fixture's placeholder: with unparseable JSON the send would
        // bail locally before transmitting, which would make the `send_count == 0` assertion
        // below vacuous (it must fail if the re-check is ever deleted, not pass either way).
        let keys = buyer_keys(&context.home).expect("keys");
        let event = nostr_sdk::prelude::EventBuilder::new(nostr_sdk::prelude::Kind::Custom(3405), "")
            .sign_with_keys(&keys)
            .expect("sign");
        {
            use nostr_sdk::prelude::JsonUtil;
            pinned.award_event_id = event.id.to_hex();
            pinned.event_json = event.as_json();
        }
        // Ahead of the deadline NOW — so the pre-lock gate passes and the pinned short-circuit
        // skips all relay I/O — but it crosses while the lock is held below.
        pinned.offer_deadline_unix = now_unix() + 2;
        context.store.begin_award_attempt(&pinned, now_unix()).expect("pin");

        let holder = {
            let context = context.clone();
            tokio::spawn(async move {
                let _guard = context.money_lock.lock().await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await; // let the holder take it first

        let response = tokio::time::timeout(
            Duration::from_secs(30),
            award(&context, json!(1), json!({ "job_id": job })),
        )
        .await
        .expect("the RPC must answer");
        holder.await.expect("holder task");

        let error = response.error.expect("a crossed deadline is a refusal, not a send");
        assert!(
            error.message.contains("passed while this call was queued"),
            "the refusal must name the TOCTOU: {}",
            error.message
        );
        assert_eq!(
            context.store.award_attempt(&job).expect("read").expect("row").send_count,
            0,
            "NOTHING may be transmitted for a deadline that crossed during the wait"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #322 round 6: an UNREPAIRABLE heal parks the intent — the only status surface for a job
    // whose award is public but whose row cannot be written or funded. Hermetic: the repair's
    // re-reserve hits AmountMismatch (evaluated before the available-check, so no funded wallet is
    // needed) and yields PublishedButUnrecorded. Red-on-revert: delete the park in the heal leg's
    // PublishedButUnrecorded arm and `parked_awards` goes silent while a seller is owed money.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unrepairable_heal_parks_the_intent_so_the_debt_is_visible() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};

        let root = temp_home("unrepairable-heal-parks");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let job = "a".repeat(64);
        context.store.put_pending_award(&job, 4, None, None, now_unix()).expect("intent");
        context.store.reserve(&job, 4, 1_000, now_unix()).expect("reserve");
        // The attempt's amount DISAGREES with the held reservation, so the repair's re-reserve is
        // refused (AmountMismatch) — the award is public and its row cannot be written.
        let mut pinned = attempt_fixture(&job, &context.home.config.relay_url);
        pinned.amount_sats = 5;
        context.store.begin_award_attempt(&pinned, now_unix()).expect("pin");
        assert!(context.store.mark_attempt_confirmed(&job, now_unix()).expect("confirm"));

        resolve_award_attempts(&context).await;

        assert!(
            context.store.award_record(&job).expect("read").is_none(),
            "premise: the row genuinely could not be written, or nothing is unrepairable"
        );
        let parked = context.store.parked_awards().expect("parked");
        let reason = parked
            .iter()
            .find(|(parked_job, _)| parked_job == &job)
            .map(|(_, reason)| reason.clone())
            .unwrap_or_else(|| {
                panic!("an unrepairable public award must be parked so status can show the debt")
            });
        assert!(
            reason.contains("re-reserving"),
            "the parked reason must name the CAUSE, not just the wrapper (`already published` is \
             in every PublishedButUnrecorded Display): {reason}"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #322 round 6: the MIRROR of the junk-delivery test, and the direction that costs a seller
    // their pay. A delivery by the PINNED seller is positive evidence our award WAS public (the
    // probe's absence is retention, not history), so the terminalization must HOLD. Without this
    // the guard is only protected against being too permissive: an INERT guard — one that never
    // returns `Present` — satisfies the junk test's assertions identically, and would refuse +
    // release funds for work the awarded seller actually delivered, one-way and unrecoverable
    // (round-6 review). Red-on-revert: make the `Present` arm fall through to terminalize, or
    // stub the probe to always answer absent, and this fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_pinned_sellers_delivery_holds_the_refund() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{Client, EventBuilder, Keys, Kind, Tag};

        let root = temp_home("seller-delivery-holds");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let job = "a".repeat(64);
        context.store.reserve(&job, 4, 1_000, now_unix()).expect("reserve");
        let mut pinned = attempt_fixture(&job, &context.home.config.relay_url);
        pinned.amount_sats = 4;
        pinned.offer_deadline_unix = now_unix() - 9 * 24 * 3_600; // past deadline AND pay window
        // The award is pinned to THIS seller — the one whose delivery is evidence.
        let seller = Keys::generate();
        pinned.seller_pubkey = seller.public_key().to_hex();
        context.store.begin_award_attempt(&pinned, now_unix()).expect("pin");

        // The AWARDED seller publishes a result for the job.
        let client = Client::new(seller.clone());
        client.add_relay(&context.home.config.relay_url).await.expect("add relay");
        client.connect().await;
        let delivered = EventBuilder::new(Kind::Custom(3403), "")
            .tag(Tag::parse(vec!["e".to_owned(), job.clone()]).expect("e tag"))
            .tag(Tag::parse(vec!["t".to_owned(), crate::gateway::MAXPLAYER_TAG.to_owned()]).expect("t tag"))
            .sign_with_keys(&seller)
            .expect("sign delivery");
        client.send_event(&delivered).await.expect("relay stores the seller's result");
        client.disconnect().await;

        assert!(
            past_pay_window(now_unix(), pinned.offer_deadline_unix),
            "premise: the fixture must be past the pay window, or the hold below proves nothing \
             (every early return in resolve_expired_attempt leaves the same Pending/Reserved)"
        );

        resolve_award_attempts(&context).await;

        let after = context.store.award_attempt(&job).expect("read").expect("row");
        assert_eq!(
            after.state,
            store::AttemptState::Pending,
            "the awarded seller DELIVERED — refusing here would repudiate work that happened, \
             one-way and unrecoverable"
        );
        assert_eq!(
            context.store.reservation(&job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Reserved),
            "and the funds must stay held for the seller, not returned to the buyer"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #322 round 5: the SWEEP's under-lock deadline re-check is wired too, not just `award()`'s
    // twin. The pre-lock gate passes (deadline ahead), the deadline crosses while the license
    // section waits on the money lock, and the sweep must transmit NOTHING — a late award is
    // compute the seller burns unpaid. Red-on-revert: delete the sweep's
    // `resume_crossed_deadline` arm and the send goes out (send_count becomes 1, not 0).
    //
    // ★ ATTRIBUTION (round-8 review). The two negatives this test wants (`send_count == 0`,
    // still `Pending`) are ALSO produced by the PRE-lock gate diverting to the probe path — so a
    // prologue slower than the deadline margin would make the test pass while never reaching the
    // arm under test. A per-attempt `send_count` on some other job cannot fix that (it witnesses
    // only its own row). The discriminator is the award EVENT: it is published to the relay up
    // front, so a pre-lock divert would probe it, find it Present, and CONFIRM the attempt with
    // an awards row. Asserting `Pending` + no row therefore fails on a divert instead of passing
    // — the test now says "I did not test what I meant to" rather than going quietly green.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_sweep_deadline_crossed_while_waiting_for_the_money_lock_transmits_nothing() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{EventBuilder, JsonUtil, Kind};

        let root = temp_home("sweep-deadline-toctou");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let job = "a".repeat(64);
        context.store.reserve(&job, 4, 1_000, now_unix()).expect("reserve");
        let keys = buyer_keys(&context.home).expect("keys");
        // Real signed bytes: with the fixture placeholder the send would bail locally on parse,
        // making the no-transmit assertion vacuous.
        let event = EventBuilder::new(Kind::Custom(3405), "")
            .sign_with_keys(&keys)
            .expect("sign");
        let mut pinned = attempt_fixture(&job, &context.home.config.relay_url);
        pinned.amount_sats = 4;
        pinned.award_event_id = event.id.to_hex();
        pinned.event_json = event.as_json();
        // THE DISCRIMINATOR: the pinned award is already ON the relay. A pre-lock divert probes
        // by id, finds it, and confirms the attempt (+ heals its awards row) — a state visibly
        // different from the under-lock refusal's `Pending` + no row. Without this the two paths
        // are byte-identical in durable state and the test cannot tell them apart.
        //
        // Published BEFORE the pin so the deadline margin below covers only the 200ms lock
        // handoff, not a connect/send/disconnect round-trip as well (round-9 review).
        {
            use nostr_sdk::prelude::Client;
            let publisher = Client::new(keys.clone());
            publisher
                .add_relay(&context.home.config.relay_url)
                .await
                .expect("add relay");
            publisher.connect().await;
            publisher.send_event(&event).await.expect("relay stores the pinned award");
            publisher.disconnect().await;
        }

        // Ahead of the deadline now (so the pre-lock gate passes), crossing during the lock hold.
        pinned.offer_deadline_unix = now_unix() + 2;
        context.store.begin_award_attempt(&pinned, now_unix()).expect("pin");

        // A CONTROL job in the same sweep, far from its deadline. It does NOT attribute the
        // crossed job's zero (see the ATTRIBUTION note above for what does), and it is NOT the
        // tooth for the carried license — the sweep test pins that independently. What it
        // uniquely proves is that the sweep CONTINUES past a job whose license was refused: the
        // bounce is a `continue`, not a `return`. Nothing else in the suite notices if one
        // diverted job aborts the whole pass (round-9 review).
        let control = "c".repeat(64);
        context.store.reserve(&control, 4, 1_000, now_unix()).expect("reserve");
        let control_event = EventBuilder::new(Kind::Custom(3405), "control")
            .sign_with_keys(&keys)
            .expect("sign control");
        let mut control_pinned = attempt_fixture(&control, &context.home.config.relay_url);
        control_pinned.amount_sats = 4;
        control_pinned.award_event_id = control_event.id.to_hex();
        control_pinned.event_json = control_event.as_json();
        control_pinned.offer_deadline_unix = now_unix() + 3_600;
        context.store.begin_award_attempt(&control_pinned, now_unix()).expect("pin control");

        let holder = {
            let context = context.clone();
            tokio::spawn(async move {
                let _guard = context.money_lock.lock().await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await; // let the holder take it first

        resolve_award_attempts(&context).await;
        holder.await.expect("holder task");

        let after = context.store.award_attempt(&job).expect("read").expect("row");
        assert_eq!(
            after.send_count, 0,
            "a deadline that crossed while the license section queued must transmit NOTHING"
        );
        assert_eq!(
            after.state,
            store::AttemptState::Pending,
            "a PRE-lock divert would have probed the (published) award and CONFIRMED it, so \
             `Pending` proves the under-lock re-check is what stopped this send"
        );
        assert!(
            context.store.award_record(&job).expect("read").is_none(),
            "and no awards row: the divert path would have healed one from the probe's Present"
        );
        assert_eq!(
            context.store.award_attempt(&control).expect("read").expect("row").send_count,
            1,
            "the sweep must CONTINUE past the bounced job and license this one — a `return` \
             instead of a `continue` would strand every later attempt in the pass"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #322 round 7: the shield covers the RELEASE decision only. A held job's `Dead` verdict is
    // downgraded to `Payable` (funds stay), but its `Paid` verdict must still convert — reconcile's
    // Paid arm is the ONLY converger for a pay whose `reserved → spent` flip failed, and dropping
    // held jobs from the batch entirely would suppress a correction that frees a double-count.
    // Red-on-revert: drop held jobs from `reserved` instead of downgrading their verdict, and the
    // Paid assertion below fails.
    #[test]
    fn the_attempt_shield_keeps_dead_jobs_but_still_converges_paid_ones() {
        let held_dead = "a".repeat(64);
        let held_paid = "b".repeat(64);
        let free_dead = "c".repeat(64);
        let held: std::collections::BTreeSet<String> =
            [held_dead.clone(), held_paid.clone()].into_iter().collect();

        let dispositions = plan_reconcile(
            &[held_dead.clone(), held_paid.clone(), free_dead.clone()],
            &BTreeMap::from([(held_paid.clone(), PaymentProgress::Closed)]),
            &BTreeMap::from([
                (held_dead.clone(), false),
                (held_paid.clone(), false),
                (free_dead.clone(), false),
            ]),
            &held,
            &BTreeMap::new(),
            FLOOR_OFF,
        );

        assert_eq!(
            dispositions[&held_dead],
            reservations::JobDisposition::Payable,
            "a held job's Dead verdict is downgraded — the attempt machinery owns its release"
        );
        assert_eq!(
            dispositions[&held_paid],
            reservations::JobDisposition::Paid,
            "but a held job that PAID must still converge: this arm frees a double-count and is \
             the only path that does"
        );
        assert_eq!(
            dispositions[&free_dead],
            reservations::JobDisposition::Dead,
            "and an unheld dead job is still released — the shield is not a blanket"
        );
    }

    // ★ #322 round 3: the sweep WIRES its legs — heal (confirmed-without-row lands the row),
    // finish (refused+reserved releases), resolve (pending pre-deadline re-sends the pinned
    // bytes and confirms on the relay's ack), and the expired-inside-window HOLD (nothing
    // transmitted, nothing released). Each leg is unit-tested at the chokepoint/store layer;
    // this proves resolve_award_attempts actually drives them. Red-on-revert: no-op the sweep
    // and every assertion below fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_attempt_sweep_drives_all_three_legs_and_holds_inside_the_window() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        use nostr_sdk::prelude::{EventBuilder, JsonUtil, Kind};

        let root = temp_home("sweep-wiring");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();

        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");
        let relay_url = context.home.config.relay_url.clone();
        let now = now_unix();

        // (a) HEAL: confirmed attempt, no awards row, reservation held.
        let heal_job = "a".repeat(64);
        context.store.reserve(&heal_job, 4, 1_000, now).expect("reserve");
        let mut heal = attempt_fixture(&heal_job, &relay_url);
        heal.amount_sats = 4;
        context.store.begin_award_attempt(&heal, now).expect("pin");
        assert!(context.store.mark_attempt_confirmed(&heal_job, now).expect("confirm"));

        // (b) FINISH: refused attempt whose release crashed — funds still reserved.
        let finish_job = "b".repeat(64);
        context.store.reserve(&finish_job, 4, 1_000, now).expect("reserve");
        let mut finish = attempt_fixture(&finish_job, &relay_url);
        finish.amount_sats = 4;
        context.store.begin_award_attempt(&finish, now).expect("pin");
        assert!(context.store.mark_attempt_refused(&finish_job, "blocked: policy", now).expect("refuse"));

        // (c) RESOLVE: pending attempt, deadline ahead, REAL signed bytes the relay will ack.
        let keys = buyer_keys(&context.home).expect("keys");
        let event = EventBuilder::new(Kind::Custom(3405), "")
            .sign_with_keys(&keys)
            .expect("sign");
        let resolve_job = "c".repeat(64);
        context.store.reserve(&resolve_job, 4, 1_000, now).expect("reserve");
        let resolve = store::AwardAttempt {
            job_id: resolve_job.clone(),
            claim_id: "c".repeat(64),
            seller_pubkey: "s".repeat(64),
            award_event_id: event.id.to_hex(),
            event_json: event.as_json(),
            amount_sats: 4,
            quoted_mints_json: "[]".to_owned(),
            offer_deadline_unix: now + 3_600,
            send_count: 0,
            relay_url: relay_url.clone(),
            state: store::AttemptState::Pending,
            detail: None,
        };
        context.store.begin_award_attempt(&resolve, now).expect("pin");

        // (d) HOLD: pending, deadline passed but inside the 7-day pay window — the sweep must
        // neither transmit (late award) nor release (window still open).
        let hold_job = "d".repeat(64);
        context.store.reserve(&hold_job, 4, 1_000, now).expect("reserve");
        let mut hold = attempt_fixture(&hold_job, &relay_url);
        hold.amount_sats = 4;
        hold.offer_deadline_unix = now - 100;
        context.store.begin_award_attempt(&hold, now).expect("pin");

        resolve_award_attempts(&context).await;

        // (a) healed: the row exists, written from the attempt.
        let healed = context.store.award_record(&heal_job).expect("read").expect("healed row");
        assert_eq!(healed.award_event_id, heal.award_event_id);

        // (b) finished: the crashed refusal's funds are back.
        assert_eq!(
            context.store.reservation(&finish_job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Released),
            "the finisher leg releases the crashed refusal"
        );

        // (c) resolved: the relay acked the pinned bytes; confirmed + recorded.
        assert_eq!(
            context.store.award_attempt(&resolve_job).expect("read").expect("row").state,
            store::AttemptState::Confirmed,
            "the pending leg transmits and folds the ack in"
        );
        assert!(
            context.store.award_record(&resolve_job).expect("read").is_some(),
            "and records the award"
        );
        assert_eq!(
            context.store.award_attempt(&resolve_job).expect("read").expect("row").send_count,
            1,
            "the sweep's license is CARRIED to the chokepoint, counted exactly once — a second \
             count would make a genuinely-first transmission read as a re-send, so a deliberate \
             refusal of it would hold the funds for the whole pay window instead of releasing"
        );

        // (d) held: still pending, still funded, nothing on the relay for it.
        let held = context.store.award_attempt(&hold_job).expect("read").expect("row");
        assert_eq!(held.state, store::AttemptState::Pending, "inside the window: hold");
        assert_eq!(
            held.send_count, 0,
            "and NOTHING was transmitted for it — no send license was ever taken (a re-send \
             past the deadline is the late-award injection the design forbids)"
        );
        assert_eq!(
            context.store.reservation(&hold_job).expect("read").map(|(state, _)| state),
            Some(reservations::ReservationState::Reserved),
            "its funds stay held"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // #261: the sanitizer is the only guard between seller-authored bytes and the operator's
    // terminal — pin exactly what it strips (Cc controls incl. ESC and the one-byte C1 CSI, plus
    // invisible Unicode format chars) and what it deliberately keeps (printable residue).
    #[test]
    fn log_safe_agent_strips_control_and_invisible_bytes_and_caps() {
        assert_eq!(log_safe_agent(None), "unreported");
        assert_eq!(log_safe_agent(Some("claude-agent-acp")), "claude-agent-acp");
        // Newline forgery, ESC-[ repaint, one-byte C1 CSI, bidi override + ALM, zero-width, soft
        // hyphen, tag-block smuggling — stripped; printable residue stays (spoofed printable text
        // is inherent to printing seller text).
        assert_eq!(
            log_safe_agent(Some(
                "a\nb\u{1b}[31mc\u{9b}d\u{202e}e\u{200b}f\u{61c}\u{ad}\u{e0041}g"
            )),
            "ab[31mcdefg"
        );
        // Only-stripped-bytes input is REPORTED garbage — distinct from unreported.
        assert_eq!(log_safe_agent(Some("\u{1b}\u{9b}\r\n\u{202e}")), "unprintable");
        assert_eq!(log_safe_agent(Some(&"x".repeat(200))).len(), 64, "length capped");
    }

    // #261 boot heal, end to end: an award that SETTLED while its attribution write was lost (the
    // crash-after-flip window, or a flip-fail later converged by reconcile's Paid arm) is
    // backfilled at boot from the durable accept-bind — through the same write-once gate, so a
    // recorded attribution is never rewritten.
    #[test]
    fn heal_backfills_attribution_for_settled_awards_from_the_bind() {
        let root = temp_home("attribution-heal");
        let _ = std::fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("home");
        let store = BuyerStore::open(home.root.join(STATE_DB_FILE)).expect("store");

        // A settled award whose attribution never landed: reserved → awarded → spent, NULL/NULL.
        let job = "a".repeat(64);
        store.reserve(&job, 5, 100, 1).expect("reserve");
        store
            .record_award(&job, &"1".repeat(64), &"2".repeat(64), &"3".repeat(64), 5, 1)
            .expect("award");
        store.convert_to_spent(&job, 5, 2).expect("spend");

        // The durable bind holds the seller's report (frozen at accept). Written at the exact
        // path the production loader reads — this test doubles as a pin on that convention.
        let jobs_dir = home.root.join("jobs");
        std::fs::create_dir_all(&jobs_dir).expect("jobs dir");
        std::fs::write(
            jobs_dir.join(format!("{job}.json")),
            format!(
                r#"{{"job_id":"{job}","claim_id":"bb","result_id":"cc","seller_pubkey":"dd",
                    "commit_oid":"ee","repo":"https://example.invalid/repo.git","branch":"main",
                    "job_hash":"ff","amount_sats":5,"accept_event_id":"11","accepted_at":1,
                    "seller_signature":"","agent_used":"claude-agent-acp","model_used":"claude-opus-5"}}"#
            ),
        )
        .expect("bind file");

        heal_award_attribution(&store, &home);
        let healed = store.award_record(&job).expect("read").expect("row");
        assert_eq!(healed.agent_used.as_deref(), Some("claude-agent-acp"));
        assert_eq!(healed.model_used.as_deref(), Some("claude-opus-5"));

        // Idempotent and write-once: the healed row leaves the work set, and a second heal
        // changes nothing.
        assert!(
            store.unattributed_settled_award_job_ids().expect("work set").is_empty(),
            "a healed row leaves the work set"
        );
        heal_award_attribution(&store, &home);
        let after = store.award_record(&job).expect("read").expect("row");
        assert_eq!(after.agent_used.as_deref(), Some("claude-agent-acp"));
        let _ = std::fs::remove_dir_all(&root);
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
            .reserve(&job, 4, 1_000, now_unix())
            .expect("reserve");

        // Confirm the premise: with no bind, no payment journal and no live claim, this job WOULD be
        // released by a pass that got to act. Without this the tooth could pass because nothing was
        // ever at risk.
        let would_release = plan_reconcile(
            &[job.clone()],
            &BTreeMap::new(),
            &BTreeMap::from([(job.clone(), false)]),
            &std::collections::BTreeSet::new(),
            &BTreeMap::new(),
            FLOOR_OFF,
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

    // #469: a successful settle drops the reconcile-on-start snapshot, so a `status` money-read
    // cannot show an already-settled reservation as still-`kept`. This pins the invalidation
    // directly; `settle_job` calls it at its one success point (right after `settle_after_pay`
    // flips reserved→spent). A live-melt success of the full `settle_job` has no CI-safe harness
    // (collect runs no funded wallet in-test), so the success path's CALL is covered by that single
    // placement, and the forged-delivery watcher test pins that a REFUSED settle leaves it.
    // Red-on-revert: no-op `invalidate_reconcile_snapshot` and this fails.
    #[tokio::test]
    async fn a_settle_invalidates_the_stale_reconcile_snapshot() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        let root = temp_home("invalidate-reconcile");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();
        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");

        let job = "a".repeat(64);
        *context.last_reconcile.lock().await =
            Some(ReconcileReport { kept: vec![job.clone()], ..Default::default() });
        assert!(context.last_reconcile.lock().await.is_some(), "premise: snapshot seeded");

        invalidate_reconcile_snapshot(&context).await;

        assert!(
            context.last_reconcile.lock().await.is_none(),
            "a settle must clear the reconcile snapshot so status drops the stale `kept`"
        );
        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // #411 (merging #183): an auto-award that cannot place its award PARKS — the correct handling of
    // an unawardable job, so it returns Ok — and the success path printed nothing, leaving a job that
    // lapsed (on budget, on a passed deadline, on an offer gone from the relay) indistinguishable in
    // the log from the daemon ignoring the claim. Found live twice, and reproduced by an outside
    // tester who mis-read the silence as agent behavior. Every park site now emits
    // `auto_award_park_line` through `opline!`. Per #183's empty-case rule the line-building is split
    // from the stderr print, so the operator-visible wording is asserted directly here (no stderr
    // capture): it MUST carry the job id and the reason. Red-on-revert: drop either the job id or the
    // reason from `auto_award_park_line` and this fails.
    #[test]
    fn an_auto_award_park_line_carries_the_job_id_and_the_reason() {
        let job = "a".repeat(64);
        // The #411 canonical case: a budget refusal, whose reason names the shortfall (requested vs
        // available + the binding ceiling) — the numbers an operator needs to see WHY it parked.
        let reason = "reservation refused: 100 sat exceeds available 40 sat \
                      (bound by the wallet ceiling; available = wallet_balance − reserved)";
        let line = auto_award_park_line(&job, reason);
        assert!(line.contains(&job), "park log line must name the job id: {line}");
        assert!(line.contains(reason), "park log line must carry the reason: {line}");
        assert!(
            line.to_lowercase().contains("park"),
            "park log line must be greppable as a park (the token an operator tails for): {line}"
        );
    }

    // ★ #481: an operator reading get_job must SEE a committed award the moment the 3405 exists —
    // BEFORE any delivery or accept — so an in-flight, unstoppable spend never reads as "nothing
    // committed". The incident (job a719a8b5) was exactly this: `accepted` null for minutes after
    // a signed award existed and the seller was already executing, making a committed spend look
    // stoppable. Drives the REAL get_job handler: with no award the `awarded` field is ABSENT (a
    // false Some would be the same bug in reverse); once the 3405 award row exists it surfaces
    // under `awarded`, while `accepted` (kind-3406, a distinct later fact) stays null throughout.
    // Red-on-revert: return `json!(view)` from the Ok arm (drop the enrichment) and the post-award
    // `awarded` assertion fails.
    #[tokio::test]
    async fn get_job_surfaces_a_committed_award_while_accepted_stays_null() {
        use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
        let root = temp_home("get-job-awarded");
        let _ = std::fs::remove_dir_all(&root);
        let mut home = bootstrap_home(&root).expect("bootstrap home");
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        home.config.relay_url = relay.url().await.to_string();
        let (_lock, context, _socket) = bootstrap(home).await.expect("buyer bootstrap");

        let job = "a".repeat(64);

        // Pre-award: no commitment, so no `awarded`, and nothing accepted.
        let before = get_job(&context, json!(1), json!({ "job_id": job.clone() }))
            .await
            .result
            .expect("get_job (pre-award) returns a view");
        assert_eq!(before["job_id"], json!(job.clone()), "the flattened JobView carries the job_id");
        assert!(before.get("awarded").is_none(), "no award committed yet ⇒ no `awarded` field");
        assert!(before["accepted"].is_null(), "and nothing is accepted yet");

        // Commit an award (the 3405 record) — no delivery, no accept bind.
        let claim = "c".repeat(64);
        let award_event = "e".repeat(64);
        let seller = "5".repeat(64);
        context
            .store
            .record_award(&job, &claim, &award_event, &seller, 100, 7)
            .expect("record award");

        // Post-award: the commitment surfaces under `awarded` with its 3405 identity + amount,
        // while `accepted` is STILL null — award and accept are separate facts (#481).
        let after = get_job(&context, json!(2), json!({ "job_id": job.clone() }))
            .await
            .result
            .expect("get_job (post-award) returns a view");
        let awarded =
            after.get("awarded").expect("a committed award now surfaces under `awarded`");
        assert_eq!(awarded["award_event_id"], json!(award_event), "carries the 3405 event id");
        assert_eq!(awarded["amount_sats"], json!(100), "and the committed amount");
        assert_eq!(awarded["seller_pubkey"], json!(seller));
        assert_eq!(awarded["claim_id"], json!(claim));
        assert!(
            after["accepted"].is_null(),
            "accepted (kind-3406) is unchanged by the award — the two are separate facts"
        );

        let _ = std::fs::remove_dir_all(&root);
        drop(relay);
    }

    // ★ #481 (serialize contract, harness-free): the committed award serializes UNDER `awarded` as
    // a peer of the flattened JobView, carrying the 3405 identity + amount; with no award the field
    // is absent (never a false empty object). Pins the wire shape the handler relies on, free of any
    // relay. Red-on-revert: drop `awarded` from GetJobResponse and the Some case loses the field.
    #[test]
    fn get_job_response_serializes_a_committed_award_beside_the_flattened_view() {
        let view = job_lifecycle::JobView {
            job_id: "a".repeat(64),
            offer: None,
            claims: vec![],
            results: vec![],
            live_claim_id: None,
            accepted: None,
            pending: false,
            read_confirmed: true,
        };
        let record = store::AwardRecord {
            job_id: "a".repeat(64),
            claim_id: "c".repeat(64),
            award_event_id: "e".repeat(64),
            seller_pubkey: "5".repeat(64),
            amount_sats: 100,
            awarded_at_unix: 7,
            agent_used: None,
            model_used: None,
        };

        // A committed award: JobView fields flatten to the top level and `awarded` sits beside them.
        let with_award = serde_json::to_value(GetJobResponse {
            view: view.clone(),
            awarded: Some(AwardedView::from(record)),
            awarded_delivery_pending: false,
        })
        .expect("serialize");
        assert_eq!(with_award["job_id"], json!("a".repeat(64)), "JobView is flattened, not nested");
        assert_eq!(with_award["awarded"]["award_event_id"], json!("e".repeat(64)));
        assert_eq!(with_award["awarded"]["amount_sats"], json!(100));
        assert_eq!(with_award["awarded"]["seller_pubkey"], json!("5".repeat(64)));
        assert_eq!(with_award["awarded"]["claim_id"], json!("c".repeat(64)));
        assert!(with_award["accepted"].is_null(), "accepted is a distinct field, untouched");

        // No award: `awarded` is omitted entirely (skip_serializing_if), never null/empty.
        let no_award = serde_json::to_value(GetJobResponse {
            view,
            awarded: None,
            awarded_delivery_pending: false,
        })
        .expect("serialize");
        assert!(no_award.get("awarded").is_none(), "no commitment ⇒ `awarded` absent, not null");
    }

    // ★ #544 (serialize contract, harness-free): a committed award whose seller has delivered on
    // a still-payable claim surfaces the distinct ready-to-settle fact. It is independent of the
    // exclusive live claim and disappears once accepted, before delivery, or without an award.
    #[test]
    fn get_job_response_serializes_awarded_delivery_pending_only_while_unsettled() {
        fn serialize(
            view: job_lifecycle::JobView,
            record: Option<store::AwardRecord>,
        ) -> Value {
            let awarded = record.map(AwardedView::from);
            let awarded_delivery_pending = awarded.as_ref().is_some_and(|a| {
                job_lifecycle::awarded_delivery_pending(&view, &a.claim_id, &a.seller_pubkey)
            });
            serde_json::to_value(GetJobResponse {
                view,
                awarded,
                awarded_delivery_pending,
            })
            .expect("serialize")
        }

        fn award() -> store::AwardRecord {
            store::AwardRecord {
                job_id: "a".repeat(64),
                claim_id: "c".repeat(64),
                award_event_id: "e".repeat(64),
                seller_pubkey: "5".repeat(64),
                amount_sats: 100,
                awarded_at_unix: 7,
                agent_used: None,
                model_used: None,
            }
        }

        fn claim_with(status: &str, seller_pubkey: String) -> job_lifecycle::ClaimView {
            job_lifecycle::ClaimView {
                // The UNSTATED capability — a seat advertising nothing. Set explicitly because `ClaimView`
                // has no `Default` derive and must not gain one: a default `SandboxConfig` is a meaningful
                // object, but a default `ClaimView` is a claim that never existed.
                capability: crate::heartbeat::SeatCapability::default(),
                claim_id: "c".repeat(64),
                created_at: 1,
                seller_pubkey,
                display_name: None,
                status: status.to_owned(),
                live: false,
                creq: None,
                agents: vec![],
            }
        }

        fn result(commit_oid: Option<String>) -> job_lifecycle::ResultView {
            job_lifecycle::ResultView {
                result_id: "r".repeat(64),
                created_at: 2,
                seller_pubkey: "5".repeat(64),
                display_name: None,
                job_hash: None,
                repo: None,
                branch: None,
                commit_oid,
                amount_sats: None,
                seller_signature: None,
                harness: None,
                model: None,
                contribution: None,
            }
        }

        let pending = serialize(
            job_lifecycle::JobView {
                job_id: "a".repeat(64),
                offer: None,
                claims: vec![claim_with("delivered", "5".repeat(64))],
                results: vec![result(Some("d".repeat(40)))],
                live_claim_id: Some("f".repeat(64)),
                accepted: None,
                pending: false,
                read_confirmed: true,
            },
            Some(award()),
        );
        assert_eq!(pending["awarded_delivery_pending"], json!(true));

        let settled = serialize(
            job_lifecycle::JobView {
                job_id: "a".repeat(64),
                offer: None,
                claims: vec![claim_with("delivered", "5".repeat(64))],
                results: vec![result(Some("d".repeat(40)))],
                live_claim_id: None,
                accepted: Some(job_lifecycle::AcceptedBind {
                    job_id: "a".repeat(64),
                    claim_id: "c".repeat(64),
                    result_id: "r".repeat(64),
                    seller_pubkey: "5".repeat(64),
                    commit_oid: "d".repeat(40),
                    repo: "https://example.invalid/repo.git".to_owned(),
                    branch: "main".to_owned(),
                    job_hash: "h".repeat(64),
                    amount_sats: 100,
                    accept_event_id: "b".repeat(64),
                    accepted_at: 3,
                    seller_signature: String::new(),
                    creq_hash: None,
                    accepted_mints: vec![],
                    funding_mint: None,
                    delivery_mint: None,
                    agent_used: None,
                    model_used: None,
                    contribution: None,
                }),
                pending: false,
                read_confirmed: true,
            },
            Some(award()),
        );
        assert!(
            settled.get("awarded_delivery_pending").is_none(),
            "settled award ⇒ false field is omitted"
        );

        let accepted_other_claim = serialize(
            job_lifecycle::JobView {
                job_id: "a".repeat(64),
                offer: None,
                claims: vec![claim_with("delivered", "5".repeat(64))],
                results: vec![result(Some("d".repeat(40)))],
                live_claim_id: None,
                accepted: Some(job_lifecycle::AcceptedBind {
                    job_id: "a".repeat(64),
                    claim_id: "b".repeat(64),
                    result_id: "q".repeat(64),
                    seller_pubkey: "6".repeat(64),
                    commit_oid: "e".repeat(40),
                    repo: "https://example.invalid/other.git".to_owned(),
                    branch: "main".to_owned(),
                    job_hash: "h".repeat(64),
                    amount_sats: 100,
                    accept_event_id: "f".repeat(64),
                    accepted_at: 3,
                    seller_signature: String::new(),
                    creq_hash: None,
                    accepted_mints: vec![],
                    funding_mint: None,
                    delivery_mint: None,
                    agent_used: None,
                    model_used: None,
                    contribution: None,
                }),
                pending: false,
                read_confirmed: true,
            },
            Some(award()),
        );
        assert!(
            accepted_other_claim.get("awarded_delivery_pending").is_none(),
            "award A + accepted B occupies the job-wide settlement slot"
        );

        let undelivered = serialize(
            job_lifecycle::JobView {
                job_id: "a".repeat(64),
                offer: None,
                claims: vec![claim_with("delivered", "5".repeat(64))],
                results: vec![result(None)],
                live_claim_id: None,
                accepted: None,
                pending: false,
                read_confirmed: true,
            },
            Some(award()),
        );
        assert!(
            undelivered.get("awarded_delivery_pending").is_none(),
            "missing commit oid ⇒ false field is omitted"
        );

        let expired = serialize(
            job_lifecycle::JobView {
                job_id: "a".repeat(64),
                offer: None,
                claims: vec![claim_with("expired", "5".repeat(64))],
                results: vec![result(Some("d".repeat(40)))],
                live_claim_id: None,
                accepted: None,
                pending: false,
                read_confirmed: true,
            },
            Some(award()),
        );
        assert!(
            expired.get("awarded_delivery_pending").is_none(),
            "expired awarded claim ⇒ false field is omitted"
        );

        let absent_claim = serialize(
            job_lifecycle::JobView {
                job_id: "a".repeat(64),
                offer: None,
                claims: vec![],
                results: vec![result(Some("d".repeat(40)))],
                live_claim_id: None,
                accepted: None,
                pending: false,
                read_confirmed: true,
            },
            Some(award()),
        );
        assert!(
            absent_claim.get("awarded_delivery_pending").is_none(),
            "absent awarded claim ⇒ false field is omitted"
        );

        let claim_seller_mismatch = serialize(
            job_lifecycle::JobView {
                job_id: "a".repeat(64),
                offer: None,
                claims: vec![claim_with("delivered", "6".repeat(64))],
                results: vec![result(Some("d".repeat(40)))],
                live_claim_id: None,
                accepted: None,
                pending: false,
                read_confirmed: true,
            },
            Some(award()),
        );
        assert!(
            claim_seller_mismatch.get("awarded_delivery_pending").is_none(),
            "claim seller must match the stored award seller"
        );

        let no_award = serialize(
            job_lifecycle::JobView {
                job_id: "a".repeat(64),
                offer: None,
                claims: vec![claim_with("delivered", "5".repeat(64))],
                results: vec![result(Some("d".repeat(40)))],
                live_claim_id: None,
                accepted: None,
                pending: false,
                read_confirmed: true,
            },
            None,
        );
        assert!(
            no_award.get("awarded_delivery_pending").is_none(),
            "no award ⇒ pending field is omitted"
        );
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

    // ★ THE AGE IS THE ONLY TERM THAT SEPARATES A HEALTHY HOLD FROM A STUCK ONE.
    //
    // #273: a reservation sat `reserved` for 20 hours while 92 consecutive passes printed
    // `examined 1 reserved job(s) — released nothing, converted 0, kept 1`. Every one of those is
    // also what a perfectly healthy pass prints, so the ramp was invisible while it happened. The
    // job-id lists cannot distinguish the two cases; only the age can.
    //
    // Red-on-revert: drop the age from the line and the 10-minute hold and the 20-hour hold become
    // byte-identical again — exactly the state that hid #273.
    #[test]
    fn a_kept_reservation_reports_its_age_so_a_ramp_is_visible_while_it_happens() {
        let fresh = ReconcileReport {
            kept: vec!["a".repeat(64)],
            oldest_kept_age_secs: Some(600),
            ..ReconcileReport::default()
        };
        let stuck = ReconcileReport {
            kept: vec!["a".repeat(64)],
            oldest_kept_age_secs: Some(20 * 3_600),
            ..ReconcileReport::default()
        };
        let fresh_line = reconcile_line(&fresh);
        let stuck_line = reconcile_line(&stuck);
        assert!(fresh_line.contains("oldest held 10m"), "unexpected: {fresh_line}");
        assert!(stuck_line.contains("oldest held 20.0h"), "unexpected: {stuck_line}");
        assert_ne!(
            fresh_line, stuck_line,
            "a 10-minute hold and a 20-hour hold must not print the same line — that identity IS #273",
        );
    }

    // Holding NOTHING must not render as an age. `oldest held 0m` on an empty ledger would be a
    // number that answers a question nobody asked, and it would make the genuinely-informative
    // `0m` (a reservation taken seconds ago) unreadable.
    #[test]
    fn a_pass_holding_no_reservation_prints_no_age_at_all() {
        assert_eq!(oldest_held_phrase(None), "");
        let line = reconcile_line(&ReconcileReport::default());
        assert!(!line.contains("oldest held"), "empty ledger must not claim an age: {line}");
    }

    // ★ THE DISCRIMINATOR #273 NEEDED WAS ALREADY ON DISK, DISCARDED ONE FUNCTION EARLY.
    //
    // `Intent`/`Locked` mean a payment was attempted and never left funds; no journal means nothing
    // was ever attempted. Only the second is a leaked reservation — the first is a debt being
    // retried, and the real-money row `2ba4045a` proved it by settling 4.5h after it looked stuck.
    // Folding both into `None` is what made those two indistinguishable.
    //
    // Red-on-revert: map `Intent`/`Locked` back to `None` and the two branches collide again.
    #[test]
    fn an_attempted_payment_is_distinguishable_from_one_never_attempted() {
        let key = payment_key(&"a".repeat(64));
        let attempted = progress_from_state(Some(&PaymentState::Intent {
            attempt_id: key.attempt_id(),
        }));
        let locked = progress_from_state(Some(&PaymentState::Locked {
            attempt_id: key.attempt_id(),
        }));
        assert_eq!(attempted, PaymentProgress::Attempted);
        assert_eq!(locked, PaymentProgress::Attempted, "Locked is also an attempt");
        assert_eq!(progress_from_state(None), PaymentProgress::None);
        assert_ne!(
            progress_from_state(None),
            attempted,
            "never-attempted and attempted-but-unsent must not be the same value",
        );
    }

    // ★ THE FLOOR SHIPS OFF, AND OFF MUST BE THE IDENTITY — no age, payment state, or verdict may
    // produce a release while `enabled` is false. This is the property the enable decision rests
    // on, so it is asserted across the whole cross-product rather than at one point.
    //
    // Red-on-revert: drop the `if !floor.enabled` early return and this fails on the first row.
    #[test]
    fn the_disabled_floor_is_the_identity_function_across_every_input() {
        let ages = [None, Some(0), Some(u64::MAX)];
        let payments = [
            PaymentProgress::None,
            PaymentProgress::Attempted,
            PaymentProgress::Uncertain,
            PaymentProgress::Closed,
        ];
        let verdicts = [
            reservations::JobDisposition::Payable,
            reservations::JobDisposition::Dead,
            reservations::JobDisposition::Paid,
        ];
        let mut checked = 0;
        for age in ages {
            for payment in payments {
                for verdict in verdicts {
                    checked += 1;
                    assert_eq!(
                        lifecycle::apply_unattempted_floor(verdict, payment, age, FLOOR_OFF),
                        verdict,
                        "disabled floor changed {verdict:?} for {payment:?} at age {age:?}",
                    );
                }
            }
        }
        assert_eq!(checked, 36, "the cross-product must actually have been walked");
    }

    // ★ ENABLED, THE FLOOR MUST STILL REFUSE EVERY CASE THAT IS NOT A LEAK.
    //
    // Each row is a distinct way this could free money that is genuinely owed. The `Attempted` row
    // is the real one: `2ba4045a` sat `reserved` with `updated_at == created_at` well past its
    // deadline and looked exactly like a leak — it was a debt, and it settled 4.5h later.
    #[test]
    fn the_enabled_floor_releases_only_a_never_attempted_reservation_past_its_grace() {
        const ON: lifecycle::UnattemptedFloor = lifecycle::UnattemptedFloor {
            enabled: true,
            grace_secs: 3_600,
        };
        use reservations::JobDisposition as D;
        let old = Some(7_200);

        // The one release this feature exists for.
        assert_eq!(
            lifecycle::apply_unattempted_floor(D::Payable, PaymentProgress::None, old, ON),
            D::Dead,
        );
        // A debt being retried is NOT a leak, however old it looks.
        assert_eq!(
            lifecycle::apply_unattempted_floor(D::Payable, PaymentProgress::Attempted, old, ON),
            D::Payable,
            "an attempted-but-unsent payment is owed — releasing it is the 2ba4045a mistake",
        );
        // Ambiguous payments belong to the phase-3 saga, never to a clock.
        assert_eq!(
            lifecycle::apply_unattempted_floor(D::Payable, PaymentProgress::Uncertain, old, ON),
            D::Payable,
        );
        // Paid is untouched, so reconcile's Paid arm stays the sole converger for a failed flip.
        assert_eq!(
            lifecycle::apply_unattempted_floor(D::Paid, PaymentProgress::None, old, ON),
            D::Paid,
        );
        // Inside the grace: not yet a candidate. The boundary is strictly greater-than.
        assert_eq!(
            lifecycle::apply_unattempted_floor(D::Payable, PaymentProgress::None, Some(3_600), ON),
            D::Payable,
            "a reservation exactly at the grace must not release — the boundary is >, not >=",
        );
        // An unreadable row has an unknown age; absence of evidence is not evidence of a leak.
        assert_eq!(
            lifecycle::apply_unattempted_floor(D::Payable, PaymentProgress::None, None, ON),
            D::Payable,
            "an unknown age must never release",
        );
    }

    // The shipped config must be OFF. A default that quietly enabled an automatic money-state
    // transition would make every other guarantee here moot.
    #[test]
    fn the_reservation_floor_ships_disabled() {
        let shipped = crate::home::BuyerReservationFloorConfig::default();
        assert!(!shipped.enabled, "the floor must ship OFF");
        assert!(
            shipped.grace_secs > 3_600,
            "the grace must exceed the default job deadline so a live job is never a candidate",
        );
        assert!(!crate::home::MaxplayerConfig::default().buyer_reservation_floor.enabled);
    }

    // Across two journals for one job, evidence that an attempt happened must not be masked by a
    // journal showing none — the same reason `Closed` outranks the rest.
    #[test]
    fn merge_progress_ranks_attempted_above_none_and_below_uncertain() {
        assert_eq!(
            merge_progress(Some(PaymentProgress::None), PaymentProgress::Attempted),
            PaymentProgress::Attempted,
        );
        assert_eq!(
            merge_progress(Some(PaymentProgress::Attempted), PaymentProgress::None),
            PaymentProgress::Attempted,
        );
        assert_eq!(
            merge_progress(Some(PaymentProgress::Attempted), PaymentProgress::Uncertain),
            PaymentProgress::Uncertain,
        );
        assert_eq!(
            merge_progress(Some(PaymentProgress::Closed), PaymentProgress::Attempted),
            PaymentProgress::Closed,
        );
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

    /// A non-actionable event for the starvation tooth: a kind the watcher drops via `continue`.
    /// Since #562 the watcher acts on BOTH results and FEEDBACK, so a CLAIM (a seller's bid) is now
    /// the realistic foil — it shares the buyer-keyed subscription yet the delivery watcher ignores
    /// it. It MUST be a dropped kind: a kind that produced a wake would drive a sweep per event, and
    /// then even the broken per-iteration-sleep backstop this tooth guards against would look busy.
    fn non_actionable_event() -> Arc<nostr_sdk::Event> {
        use nostr_sdk::prelude::{EventBuilder, Keys};
        Arc::new(
            EventBuilder::new(nostr_sdk::Kind::Custom(crate::kinds::JOB_CLAIM_KIND), "")
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
        let claim_draft = crate::gateway::claim_draft(&job_id, &buyer_hex, &seller_hex, &creq, &[], &crate::heartbeat::SeatCapability::default());
        let claim_id = publish(&seller, &claim_draft).await.to_hex();

        // The buyer AWARDED this claim on the relay (kind-3405) — collect resolves the delivery
        // against this durable award, not the live-claim set (#540).
        let award_draft = crate::gateway::award_draft(&job_id, &claim_id, &buyer_hex, &seller_hex);
        let award_evt = publish(&buyer, &award_draft).await.to_hex();

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
            .reserve(&job_id, amount, 1_000, now_unix())
            .expect("reserve");
        context
            .store
            .record_award(&job_id, &claim_id, &award_evt, &seller_hex, amount, now_unix())
            .expect("record award");
        assert_eq!(
            context.store.awarded_unsettled_job_ids().expect("awarded"),
            vec![job_id.clone()],
            "the seeded award must be the watcher's work set"
        );

        // #469: seed a reconcile-on-start snapshot that still lists this job under `kept`. A
        // REFUSED settle must NOT clear it — the invalidation is gated on a successful flip
        // (asserted below), so this pins that the clear is success-only, not unconditional.
        *context.last_reconcile.lock().await =
            Some(ReconcileReport { kept: vec![job_id.clone()], ..Default::default() });

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
        // #469: and the reconcile snapshot is untouched — the invalidation is success-gated, so a
        // refused settle never drops it (only a completed flip does).
        assert!(
            context.last_reconcile.lock().await.is_some(),
            "a refused settle must not clear the reconcile snapshot (#469 clear is success-gated)"
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
        // #496: status reports the full configured-mint breakdown, not just the default balance. A
        // fresh home has exactly one configured mint (the default) at 0, so `total_sats` is 0 and the
        // per-mint list carries that one default row. Reverting the per-mint reporting drops these.
        assert_eq!(result["wallet"]["total_sats"], json!(0));
        assert_eq!(result["wallet"]["configured_total_sats"], json!(0));
        let mints = result["wallet"]["mints"]
            .as_array()
            .expect("#496: status.wallet.mints is a per-mint array");
        assert_eq!(mints.len(), 1, "fresh home has one configured mint (the default)");
        assert_eq!(mints[0]["role"], json!("default"));
        assert_eq!(mints[0]["balance_sats"], json!(0));
        assert!(mints[0]["mint"].as_str().is_some(), "each per-mint row names its mint");
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

    // #266 divergence case — the one the fresh-home assertions above cannot catch: with money at an
    // unconfigured mint, `total_sats` must widen to the whole-DB truth while `configured_total_sats`
    // keeps the configured subset, and the stray mint appears as role=unconfigured. Red if the RPC
    // drops the configured total, mis-sums it, or filters the rows on the wrong predicate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_reports_unconfigured_db_proofs_with_both_totals() {
        use std::str::FromStr;

        use cashu::secret::Secret;
        use cashu::{Amount as CashuAmount, CurrencyUnit, Id, MintUrl, Proof, SecretKey, State};
        use cdk::cdk_database::WalletDatabase;
        use cdk::wallet::types::ProofInfo;
        use cdk_sqlite::wallet::WalletSqliteDatabase;

        // Label kept SHORT on purpose: the buyer socket lives inside this home, and macOS's
        // sun_path cap (~104 bytes) sits just past `temp_dir()`'s /var/folders prefix + the
        // "maxplayer-buyer-mod-<label>-<pid>-<id>" stem. A 15-char label made this test flip
        // pass/fail with the DIGIT COUNT of pid/id (solo green, full-suite red, macOS only —
        // Linux's /tmp never comes close). Keep it at or under the sibling "status" test's length.
        let root = temp_home("sdt");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap_home(&root).expect("bootstrap home");

        // Seed an unspent 37-sat proof at a mint OUTSIDE the configured set, directly in the shared
        // wallet sqlite — the legally-reachable state (#266) the status RPC must not render as absent.
        let stray = MintUrl::from_str("https://stray-mint.example/").expect("stray mint URL");
        let store = WalletSqliteDatabase::new(home.wallet_dir.join("cdk-wallet.sqlite"))
            .await
            .expect("open on-disk wallet database");
        let proof = Proof::new(
            CashuAmount::from(37),
            Id::from_str("009a1f293253e41e").expect("keyset id"),
            Secret::new("status-266-unconfigured-db-truth"),
            SecretKey::generate().public_key(),
        );
        let info =
            ProofInfo::new(proof, stray, State::Unspent, CurrencyUnit::Sat).expect("proof info");
        store.update_proofs(vec![info], vec![]).await.expect("seed unconfigured proof");

        let (_lock, context, socket_path) = bootstrap(home).await.expect("buyer bootstrap");
        let listener = bind_socket(&socket_path).expect("bind socket");
        let server = tokio::spawn(accept_loop(listener, context));

        let sock = socket_path.clone();
        let response = tokio::task::spawn_blocking(move || client::status(&sock))
            .await
            .expect("join client")
            .expect("client call");
        let result = response.result.expect("status result");

        assert_eq!(result["wallet"]["total_sats"], json!(37), "whole-DB truth includes the stray");
        assert_eq!(
            result["wallet"]["configured_total_sats"],
            json!(0),
            "configured subset excludes the stray"
        );
        let mints = result["wallet"]["mints"].as_array().expect("per-mint array");
        let stray_row = mints
            .iter()
            .find(|row| row["mint"].as_str() == Some("https://stray-mint.example"))
            .expect("unconfigured mint appears in the per-mint list");
        assert_eq!(stray_row["role"], json!("unconfigured"));
        assert_eq!(stray_row["balance_sats"], json!(37));

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

    // An Intent-only journal folds to Intent ⇒ no funds have left, but an attempt DID happen ⇒
    // PaymentProgress::Attempted. Proves scan walks the dir, parses real PaymentRecords, folds
    // them, and maps by job id.
    //
    // This asserted `None` until #273. Both still classify identically today, so the change is
    // behaviour-preserving — but they are no longer the same VALUE, because "retried and refused"
    // and "never attempted" must be separable before any rule may release the second one. A home
    // with no journal at all is the `None` case, covered by
    // `scan_payment_progress_absent_journal_is_empty` above.
    #[test]
    fn scan_payment_progress_folds_intent_to_attempted() {
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
        assert_eq!(progress.get(&job), Some(&PaymentProgress::Attempted));
        assert_ne!(
            progress.get(&job),
            Some(&PaymentProgress::None),
            "an attempted-but-unsent payment must not read as never-attempted — that collapse is #273",
        );
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

    /// scan_payment_progress reads `<home>/payment-journal`; the fn takes a `MaxplayerHome`, so drive it
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

        let dispositions = plan_reconcile(&reserved, &progress, &payable, &std::collections::BTreeSet::new(), &BTreeMap::new(), FLOOR_OFF);
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
        let dispositions = plan_reconcile(&[job.clone()], &BTreeMap::new(), &BTreeMap::new(), &std::collections::BTreeSet::new(), &BTreeMap::new(), FLOOR_OFF);
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
