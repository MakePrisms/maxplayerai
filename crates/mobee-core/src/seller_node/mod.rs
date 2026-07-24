//! The persistent per-home **mobee seller node**.
//!
//! One node owns a home. It takes an exclusive OS lock on `$MOBEE_HOME/seller.lock` (a second node
//! on the same home fails closed), opens the receiving CDK wallet and the seller Nostr identity
//! behind serialized in-process actors, and opens the durable lifecycle DB `$MOBEE_HOME/seller.sqlite`
//! — the source of truth for offers, claims, awards, jobs, deliveries, receipts, and the nostr event
//! outbox. A single relay ingester ([`ingester`]) writes marketplace events into the store; an async
//! publisher ([`outbox`]) drains published events to the relay with crash-idempotent retries; a
//! deterministic roster ([`roster`]) routes each awarded job to one agent under the single seller
//! identity.
//!
//! Concurrency is 1: the queue behind each actor — not SQLite locking — is the in-process
//! concurrency boundary, mirroring the home lock across processes. The node runs one job at a time,
//! so no two operations ever race the wallet, the signer, or the store.
//!
//! Money-safe boundary: agents produce files; the node signs, commits, publishes, and receives
//! payment. No agent process ever holds the seller key (owned by the [`signer`] actor) or the
//! receiving wallet (owned by the [`wallet_actor`]). This mirrors the buyer daemon's shape; a shared
//! node core is deferred until both consumers exist (issue #131).

pub mod ingester;
pub mod lock;
pub mod outbox;
pub mod roster;
pub mod signer;
pub mod store;
pub mod wallet_actor;

use std::time::{SystemTime, UNIX_EPOCH};

use crate::buyer_fund::{self, FundError};
use crate::home::{HomeError, MobeeHome};
use lock::{HomeLock, LockError};
use signer::SignerHandle;
use store::{HealthSnapshot, SellerStore, StoreError};
use wallet_actor::WalletHandle;

/// Lock file leaf under the home.
pub const LOCK_FILE: &str = "seller.lock";
/// State DB leaf under the home.
pub const STATE_DB_FILE: &str = "seller.sqlite";

/// Node startup / run failure.
#[derive(Debug)]
pub enum NodeError {
    Lock(LockError),
    Store(StoreError),
    Wallet(FundError),
    Identity(HomeError),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Wallet(error) => write!(formatter, "seller node wallet error: {error}"),
            Self::Identity(error) => write!(formatter, "seller node identity error: {error}"),
        }
    }
}

impl std::error::Error for NodeError {}

impl From<LockError> for NodeError {
    fn from(value: LockError) -> Self {
        Self::Lock(value)
    }
}
impl From<StoreError> for NodeError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<FundError> for NodeError {
    fn from(value: FundError) -> Self {
        Self::Wallet(value)
    }
}
impl From<HomeError> for NodeError {
    fn from(value: HomeError) -> Self {
        Self::Identity(value)
    }
}

/// What reconcile-on-start recovered from the durable store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Non-terminal jobs (awarded/executing/delivered) that resume from sqlite after a restart.
    pub resumed_jobs: Vec<(String, store::JobState)>,
    /// Outbox rows whose retry window had elapsed and were marked expired on start.
    pub expired_outbox: usize,
    /// Outbox rows still pending publication after start (the publisher will drain these).
    pub pending_outbox: usize,
}

/// A status view of the running node (never includes the secret key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub pubkey: String,
    pub started_at_unix: i64,
    pub wallet_balance_sats: Option<u64>,
    pub health: HealthSnapshot,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The persistent seller node: exclusive lock + durable store + serialized wallet/identity actors.
pub struct SellerNode {
    home: MobeeHome,
    store: SellerStore,
    wallet: WalletHandle,
    signer: SignerHandle,
    started_at_unix: i64,
    // Held for the node's lifetime; dropping it releases the OS lock.
    _lock: HomeLock,
}

impl SellerNode {
    /// Bring up the node's owned resources: take the exclusive lock, open the state DB and record
    /// the start, then open the receiving wallet and seller identity behind their serialized actors.
    ///
    /// Fails closed at the lock step if another node already owns this home.
    pub async fn open(home: MobeeHome) -> Result<Self, NodeError> {
        let lock = HomeLock::acquire(home.root.join(LOCK_FILE))?;

        let store = SellerStore::open(home.root.join(STATE_DB_FILE))?;
        let started_at_unix = now_unix();
        store.record_start(started_at_unix)?;

        // The node is the ONLY opener of the receiving CDK wallet — this is what the exclusive home
        // lock protects. Opening touches the local sqlite store only (no network).
        let wallet = buyer_fund::open_wallet_async(&home).await?;
        let wallet = wallet_actor::spawn(wallet);

        let signer = signer::spawn(&home)?;

        Ok(Self {
            home,
            store,
            wallet,
            signer,
            started_at_unix,
            _lock: lock,
        })
    }

    /// The seller public key (hex).
    pub fn seller_pubkey(&self) -> &str {
        self.signer.public_key_hex()
    }

    /// The durable lifecycle store.
    pub fn store(&self) -> &SellerStore {
        &self.store
    }

    /// The serialized signer actor (owns the seller key).
    pub fn signer(&self) -> &SignerHandle {
        &self.signer
    }

    /// The home this node owns.
    pub fn home(&self) -> &MobeeHome {
        &self.home
    }

    /// Recover durable state after a (re)start: expire any outbox rows past their retry window, then
    /// report the non-terminal jobs that resume and the outbox rows still pending. State comes
    /// entirely from sqlite, so a crash mid-lifecycle resumes exactly where it left off.
    pub fn reconcile_on_start(&self, now_unix: i64) -> Result<ReconcileReport, NodeError> {
        let expired_outbox = self.store.expire_outbox(now_unix)?;
        let resumed_jobs = self.store.resumable_jobs()?;
        let pending_outbox = self.store.pending_outbox(now_unix)?.len();
        Ok(ReconcileReport {
            resumed_jobs,
            expired_outbox,
            pending_outbox,
        })
    }

    /// A status snapshot proving the boundary end to end — the store answered, and the wallet actor
    /// answered through its queue. The secret key is never included.
    pub async fn status_snapshot(&self) -> Result<StatusSnapshot, NodeError> {
        let health = self.store.health()?;
        let wallet_balance_sats = match self.wallet.balance().await {
            Ok(Ok(balance)) => Some(balance),
            _ => None,
        };
        Ok(StatusSnapshot {
            pubkey: self.signer.public_key_hex().to_owned(),
            started_at_unix: self.started_at_unix,
            wallet_balance_sats,
            health,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::bootstrap as bootstrap_home;
    use crate::seller_node::outbox::{drain_once, EventPublisher};
    use crate::seller_node::store::{JobState, OutboxItem};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-node-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn claim_draft() -> crate::gateway::EventDraft {
        crate::gateway::claim_draft(&"e".repeat(64), &"b".repeat(64), &"s".repeat(64), "creqA")
    }

    struct FakePublisher {
        calls: RefCell<Vec<String>>,
    }

    impl EventPublisher for FakePublisher {
        async fn publish(&self, item: &OutboxItem) -> Result<String, String> {
            self.calls.borrow_mut().push(item.dedup_key.clone());
            Ok(format!("evt-{}", item.dedup_key))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_snapshot_never_leaks_the_secret() {
        let root = temp_home("status");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap_home(&root).expect("home");
        let secret = crate::home::read_secret_key_hex(&home).expect("secret");

        let node = SellerNode::open(home).await.expect("open node");
        let snapshot = node.status_snapshot().await.expect("status");
        assert_eq!(snapshot.pubkey.len(), 64);
        assert_eq!(snapshot.wallet_balance_sats, Some(0));
        assert_eq!(snapshot.health.schema_version, store::SCHEMA_VERSION);
        assert_ne!(snapshot.pubkey, secret);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_node_on_same_home_fails_closed() {
        let root = temp_home("exclusive");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap_home(&root).expect("home");

        let _first = SellerNode::open(home.clone()).await.expect("first node");
        let second = SellerNode::open(home).await;
        assert!(
            matches!(second, Err(NodeError::Lock(LockError::Held { .. }))),
            "a second node must fail closed on the home lock"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH 1 (charter) — RESTART SURVIVAL. Drive a job mid-lifecycle, publish its claim, then
    // "crash" (drop the node, releasing the lock). Restart on the SAME home and prove state
    // reconciles from sqlite: the job resumes as `executing`, and re-enqueuing + re-draining the
    // already-published claim publishes NOTHING new (outbox dedup) — no duplicate claim on the wire.
    //
    // Red-on-revert: if reconcile did not read jobs from sqlite, `resumed_jobs` would be empty and
    // the resume assertion fails; if the outbox `dedup_key` did not dedupe, the re-enqueue would add
    // a second pending row and the second drain would call the publisher again, failing the
    // "exactly one publish across the restart" assertion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_resumes_job_state_and_never_double_publishes() {
        let root = temp_home("restart");
        let _ = std::fs::remove_dir_all(&root);
        let home = bootstrap_home(&root).expect("home");

        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let award = "w".repeat(64);
        let buyer = "b".repeat(64);
        let publisher = FakePublisher { calls: RefCell::new(vec![]) };

        // ---- Pre-crash: park a claim (enqueue), publish it, take the award, start executing.
        {
            let node = SellerNode::open(home.clone()).await.expect("open");
            let store = node.store();
            store
                .claim_and_enqueue(&job, &offer, &claim_draft(), 1000, 9_999, 1)
                .expect("claim");
            let confirmed = drain_once(store, &publisher, 2).await.expect("drain");
            assert_eq!(confirmed.confirmed, 1, "the claim was published pre-crash");
            store.record_award(&award, &job, &buyer, 3).expect("award");
            store.mark_executing(&job, 4).expect("executing");
            // node drops here — the process "crashed" mid-execution; the DB persists.
        }
        assert_eq!(publisher.calls.borrow().len(), 1, "exactly one publish so far");

        // ---- Restart on the same home: state must come back from sqlite.
        let node = SellerNode::open(home).await.expect("reopen");
        let report = node.reconcile_on_start(5).expect("reconcile");
        assert_eq!(
            report.resumed_jobs,
            vec![(job.clone(), JobState::Executing)],
            "the executing job resumes from durable state"
        );
        assert_eq!(report.pending_outbox, 0, "the claim was already confirmed, nothing pending");

        // Re-enqueuing the same claim is a dedup no-op; a fresh drain publishes nothing new.
        let replay = node
            .store()
            .claim_and_enqueue(&job, &offer, &claim_draft(), 1000, 9_999, 6)
            .expect("replay claim");
        assert_eq!(replay, store::Claimed::Idempotent);
        let after = drain_once(node.store(), &publisher, 7).await.expect("drain2");
        assert_eq!(after.confirmed, 0);
        assert_eq!(
            publisher.calls.borrow().len(),
            1,
            "no duplicate publish across the restart — outbox dedup held"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
