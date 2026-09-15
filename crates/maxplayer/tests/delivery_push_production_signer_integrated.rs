//! T2: the delivery's tokens come from the REAL production signer actor, and the stop is timed
//! against a signer that cannot answer.
//!
//! # What was missing
//!
//! The held/saturated signer already had a gate (`delivery_push_shipped_child.rs`), and it was
//! credited: it calls the real `SignerHandle::http_auth_header_blocking` against a real actor that
//! cannot run, and proves the call RETURNS at its own deadline rather than parking. What it does
//! not do is run a DELIVERY. It calls the signer directly, from the test, with no child, no
//! transport and no seat — so it says nothing about whether a delivery whose minter is parked
//! inside that signer can still be stopped.
//!
//! That is the question here, and it is asked with the production wiring copied from
//! `seller_node/run.rs:7722-7757`: destination binding through `same_destination`, the authority
//! check before signing, the deadline check, and then the real
//! `signer.http_auth_header_blocking(destination, Some(scope), push_deadline)`. Same order, same
//! calls, same actor. No stub minter anywhere in this file.
//!
//! # Why the signer's deadline is LONGER than the turn, and why that is the production case
//!
//! Production gives the minter `push_deadline = now + DELIVERY_PUSH_TIMEOUT` — 150 seconds
//! (`run.rs:7712`). The TURN can end long before that: a cancelled delivery drops its
//! [`PushAuthority`] immediately, and a seat that is being shut down does not wait 150 seconds to
//! find out. So the hazard is not a signer that outruns its own bound; it is a signer parked well
//! inside a bound that is far looser than the turn it belongs to, holding the thread that mints for
//! a delivery nobody is waiting for any more.
//!
//! These gates reproduce exactly that: a turn budget of ~2.5s and a signer deadline 60 seconds out,
//! against an actor that is not polled at all. If the stop had to wait for the minter to come back,
//! every test here would take a minute and fail its bound. The stop must come from somewhere else.
//!
//! # The bound being asserted
//!
//! `budget + REAP_BOUND`, plus a stated allowance for scheduler latency and process teardown on a
//! loaded test host — not "eventually", and not a number tuned until it passed.

#![cfg(all(unix, feature = "wallet"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::REAP_BOUND;
use maxplayer_core::delivery_turn::delivery_turn;
use maxplayer_core::git_transport::{self, AuthMinter, AuthorityCheck};
use maxplayer_core::seller_git::{SellerGitError, neutralize_then_push_in_child_off_runtime};
use maxplayer_core::seller_node::run::PushAuthority;
use maxplayer_core::seller_node::signer::SignerHandle;

#[path = "../../maxplayer-core/tests/git_http_fixture/mod.rs"]
mod git_http_fixture;

use git_http_fixture::GitHttpAuthServer;

/// What a stop is allowed to cost beyond `budget + REAP_BOUND`: waking the thread that owns the
/// deadline, delivering a signal, and the kernel tearing down a process that has an open TLS socket
/// and a memory-resident pack buffer. This is scheduler latency on a loaded host, and it is stated
/// rather than discovered — the gates below fail if the stop needs more than this.
const TEARDOWN_ALLOWANCE: Duration = Duration::from_millis(1_500);

/// Dropped when the seat's turn is handed back, so custody is observed and not inferred.
struct Token(Arc<AtomicBool>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "maxplayer-signer-integrated-{label}-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn shipped_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_maxplayer"))
}

fn job_workdir(root: &Path, branch: &str) -> (PathBuf, String) {
    let workdir = root.join("workdir");
    let repo = git2::Repository::init(&workdir).expect("init workdir");
    std::fs::write(workdir.join("deliverable.txt"), "signed delivery\n").expect("write");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("deliverable.txt")).expect("add");
    index.write().expect("write index");
    let tree_oid = index.write_tree().expect("tree");
    let sig = git2::Signature::new("s", "s@example.invalid", &git2::Time::new(1_700_000_000, 0))
        .expect("sig");
    let oid = {
        let tree = repo.find_tree(tree_oid).expect("find tree");
        repo.commit(
            Some(&git_transport::delivery_ref(branch)),
            &sig,
            &sig,
            "delivery",
            &tree,
            &[],
        )
        .expect("commit")
    };
    (workdir, oid.to_string())
}

fn stage_env(ca: &Path) {
    // SAFETY (edition 2024 `set_var`): called at the top of the test body, before any task is
    // spawned, and this binary's tests stage the same values under one lock.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", ca);
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
        std::env::remove_var("GIT_SSL_NO_VERIFY");
    }
}

/// `SSL_CERT_FILE` is per-process, so fixture-backed deliveries in this binary run one at a time.
static TRUST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exclusive_trust() -> std::sync::MutexGuard<'static, ()> {
    TRUST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A real signer actor on a runtime this test can stop polling.
///
/// Nothing about the actor is faked: it is `signer::spawn` over a real bootstrapped home holding a
/// real seller key. What the test controls is whether its runtime gets to RUN it. Phase 0 = the
/// only worker thread is occupied by a blocking sleep, so the actor task is never polled and
/// commands pile up in its queue. Phase 1 = polled normally. Phase 2 = shut down.
struct HeldSigner {
    handle: SignerHandle,
    phase: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HeldSigner {
    fn spawn_held(home_dir: PathBuf) -> Self {
        let home = maxplayer_core::home::bootstrap(home_dir).expect("bootstrap a home");
        let phase = Arc::new(AtomicUsize::new(0));
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let thread = {
            let phase = Arc::clone(&phase);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("actor runtime");
                runtime.block_on(async move {
                    let signer = maxplayer_core::seller_node::signer::spawn(&home)
                        .expect("spawn the signer actor");
                    handle_tx.send(signer).expect("hand the handle to the test");
                    while phase.load(Ordering::SeqCst) == 0 {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    while phase.load(Ordering::SeqCst) == 1 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                });
            })
        };
        let handle = handle_rx.recv().expect("the signer handle");
        Self {
            handle,
            phase,
            thread: Some(thread),
        }
    }

    fn is_still_held(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == 0
    }

    fn release(&self) {
        self.phase.store(1, Ordering::SeqCst);
    }
}

impl Drop for HeldSigner {
    fn drop(&mut self) {
        self.phase.store(2, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The production minter, rebuilt call for call from `seller_node/run.rs:7722-7757`.
///
/// Destination binding, then the authority check, then the deadline check, then the real signer.
/// `deadline` is this delivery's push deadline — the parameter production fills with
/// `now + DELIVERY_PUSH_TIMEOUT`.
///
/// `authority` is the REAL [`PushAuthority::check`] closure, not a stand-in: production asks its
/// authority here, between binding the destination and signing, because the signer call below can
/// block and the answer can change while it does. Rebuilding every other call in this chain while
/// leaving this one out would have made "same order, same calls" false in the one place the chain
/// is about — a minter parked in the signer is exactly when an authority can end underneath it.
fn production_minter(
    signer: SignerHandle,
    intended: String,
    scope: String,
    authority: AuthorityCheck,
    deadline: Instant,
    asked: Arc<AtomicUsize>,
) -> AuthMinter {
    Arc::new(move |destination: &str| {
        asked.fetch_add(1, Ordering::SeqCst);
        if !git_transport::same_destination(&intended, destination) {
            return Err(format!(
                "refusing to authorize a leg to {destination}: this delivery is bound to {intended}"
            ));
        }
        // Before signing, with the same wrapping production gives it.
        authority().map_err(|ended| format!("{ended}; refusing to authorize another leg"))?;
        if Instant::now() >= deadline {
            return Err(
                "this delivery's push deadline has passed; refusing to authorize another leg"
                    .to_owned(),
            );
        }
        signer.http_auth_header_blocking(destination.to_owned(), Some(scope.clone()), deadline)
    })
}

/// T2a. A delivery whose minter is parked inside the real signer is still stopped on time.
///
/// The signer is held for the whole delivery and is STILL held when it returns — asserted, not
/// assumed — so no part of this stop can have come from the minter finishing. The child is blocked
/// at the advertisement leg waiting for a token that will never be minted; the parent's minting
/// thread is blocked in `http_auth_header_blocking` with 60 seconds left on its clock. The only
/// thing that can end this delivery inside its bound is a stop that does not go through either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_parked_in_the_real_signer_is_stopped_at_its_own_deadline_not_the_signers() {
    let _trust = exclusive_trust();
    let root = scratch("held-signer");
    let branch = "maxplayer/5161a001";
    let (workdir, oid) = job_workdir(&root, branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&bare, "/git/seller/r.git");
    stage_env(&relay.ca_file(&root));

    let signer = HeldSigner::spawn_held(root.join("home"));
    let asked = Arc::new(AtomicUsize::new(0));

    // 60 seconds, standing in for production's 150: a signer bound far looser than the turn.
    let signer_deadline = Instant::now() + Duration::from_secs(60);
    // The real authority, live for this delivery exactly as production's is: created before the
    // push, asked by the minter before signing and by the transport before each request leaves.
    let push_authority = PushAuthority::new();
    let minter = production_minter(
        signer.handle.clone(),
        relay.repo_url(),
        git_transport::delivery_ref(branch),
        push_authority.check(),
        signer_deadline,
        Arc::clone(&asked),
    );

    let budget = Duration::from_millis(2_500);
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), Instant::now() + budget);

    let started = Instant::now();
    let outcome = neutralize_then_push_in_child_off_runtime(
        shipped_binary(),
        workdir,
        relay.repo_url(),
        branch.to_owned(),
        oid.clone(),
        Some(minter),
        Some(push_authority.check()),
        turn,
    )
    .await;
    let elapsed = started.elapsed();

    // THE INDEPENDENCE ASSERTION, taken before anything else can release the hold.
    assert!(
        signer.is_still_held(),
        "the signer was released during the delivery, so this gate no longer proves the stop was \
         independent of it"
    );
    assert!(
        signer_deadline > Instant::now(),
        "the signer's own deadline expired during this test: the stop could have been the signer \
         giving up rather than the executor stopping the delivery"
    );

    let error = outcome.expect_err("a delivery that never obtained a token must not report success");
    assert!(
        matches!(error, SellerGitError::Cancelled(_)),
        "a delivery stopped at its deadline must be reported as cancelled, not as some other \
         failure: {error:?}"
    );

    assert!(
        elapsed >= budget,
        "the delivery ended after {elapsed:?}, before its own {budget:?} budget"
    );
    assert!(
        elapsed < budget + REAP_BOUND + TEARDOWN_ALLOWANCE,
        "the delivery took {elapsed:?}: past budget + REAP_BOUND + {TEARDOWN_ALLOWANCE:?}, which \
         is what a stop that waits on the signer looks like"
    );

    // The minter WAS reached — the parent really did park in the production signer, rather than
    // this being a delivery that failed before it ever needed a token.
    assert!(
        asked.load(Ordering::SeqCst) >= 1,
        "no leg ever asked the signer for a token, so nothing here was parked in the signer at all"
    );

    // Nothing was authorized, so nothing was uploaded.
    let seen: Vec<String> = relay
        .requests()
        .iter()
        .map(|request| format!("{} {}", request.method, request.target))
        .collect();
    assert!(
        !seen
            .iter()
            .any(|line| line.starts_with("POST") && line.contains("git-receive-pack")),
        "a delivery that never minted a token uploaded a pack anyway: {seen:?}"
    );
    assert!(
        git2::Repository::open_bare(&bare)
            .expect("open bare")
            .find_reference(&format!("refs/heads/{branch}"))
            .is_err(),
        "the remote ref moved for a delivery that was never authorized"
    );

    // The work is recorded as stopped, and the exclusion token comes back — while the signer that
    // was supposed to authorize it is still parked.
    assert!(control.work_ended(), "the work must be recorded as ended");
    control.end();
    assert!(
        !control.holds_ownership() && released.load(Ordering::SeqCst),
        "the exclusion token was not handed back after a confirmed stop"
    );
    assert_eq!(
        maxplayer_core::delivery_executor::unconfirmed_children(),
        0,
        "a stop that cannot confirm its child's exit must not be reported as a clean cancellation"
    );

    signer.release();
}

/// T2b. The same, with the signer's QUEUE saturated rather than its reply held.
///
/// A different refusal path in the signer — leg 1, `try_send` against a full bounded queue, rather
/// than leg 2's `recv_timeout` — and the same requirement of the executor. The queue is filled by
/// real `http_auth_header_blocking` callers that abandon at their own deadlines and leave their
/// commands queued behind an actor that cannot drain them: the state a saturated seat is actually
/// in, not a mock of one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_behind_a_saturated_real_signer_is_stopped_at_its_own_deadline() {
    let _trust = exclusive_trust();
    let root = scratch("saturated-signer");
    let branch = "maxplayer/5161a002";
    let (workdir, oid) = job_workdir(&root, branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&bare, "/git/seller/r.git");
    stage_env(&relay.ca_file(&root));

    let signer = HeldSigner::spawn_held(root.join("home"));

    // Fill the actor's bounded queue with real calls that give up quickly. Their commands stay
    // queued, because the actor is not being polled.
    let mut fillers = Vec::new();
    for _ in 0..80 {
        let handle = signer.handle.clone();
        let destination = relay.repo_url();
        fillers.push(tokio::task::spawn_blocking(move || {
            handle.http_auth_header_blocking(
                destination,
                None,
                Instant::now() + Duration::from_millis(200),
            )
        }));
    }
    for filler in fillers {
        let _ = filler.await.expect("filler leg");
    }

    // Confirm the queue really is full before the delivery starts, otherwise this test is T2a again
    // under a different name.
    let probe = {
        let handle = signer.handle.clone();
        let destination = relay.repo_url();
        tokio::task::spawn_blocking(move || {
            handle.http_auth_header_blocking(
                destination,
                None,
                Instant::now() + Duration::from_millis(200),
            )
        })
        .await
        .expect("probe leg")
    };
    let why = probe.expect_err("a held signer must not mint");
    assert!(
        why.contains("signer queue stayed full past this push's deadline"),
        "this gate needs a SATURATED queue and did not get one: {why}"
    );

    let asked = Arc::new(AtomicUsize::new(0));
    let signer_deadline = Instant::now() + Duration::from_secs(60);
    let push_authority = PushAuthority::new();
    let minter = production_minter(
        signer.handle.clone(),
        relay.repo_url(),
        git_transport::delivery_ref(branch),
        push_authority.check(),
        signer_deadline,
        Arc::clone(&asked),
    );

    let budget = Duration::from_millis(2_500);
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), Instant::now() + budget);

    let started = Instant::now();
    let outcome = neutralize_then_push_in_child_off_runtime(
        shipped_binary(),
        workdir,
        relay.repo_url(),
        branch.to_owned(),
        oid.clone(),
        Some(minter),
        Some(push_authority.check()),
        turn,
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        signer.is_still_held(),
        "the signer drained during the delivery, so the saturation this gate needs was not in force"
    );
    let error = outcome.expect_err("a delivery behind a saturated signer must not report success");
    assert!(
        matches!(error, SellerGitError::Cancelled(_)),
        "a delivery stopped at its deadline must be reported as cancelled: {error:?}"
    );
    assert!(
        elapsed >= budget && elapsed < budget + REAP_BOUND + TEARDOWN_ALLOWANCE,
        "the delivery took {elapsed:?}, outside [budget, budget + REAP_BOUND + \
         {TEARDOWN_ALLOWANCE:?}) for a {budget:?} budget"
    );
    assert!(
        asked.load(Ordering::SeqCst) >= 1,
        "no leg asked for a token, so nothing was behind the saturated queue"
    );

    let seen: Vec<String> = relay
        .requests()
        .iter()
        .map(|request| format!("{} {}", request.method, request.target))
        .collect();
    assert!(
        !seen
            .iter()
            .any(|line| line.starts_with("POST") && line.contains("git-receive-pack")),
        "a pack was uploaded behind a signer that authorized nothing: {seen:?}"
    );
    assert!(control.work_ended(), "the work must be recorded as ended");
    control.end();
    assert!(
        !control.holds_ownership() && released.load(Ordering::SeqCst),
        "the exclusion token was not handed back after a confirmed stop"
    );

    signer.release();
}

/// T2c. A turn that ended BEFORE the first wire leg mints nothing and sends nothing.
///
/// The signer here is live and perfectly capable of minting — the refusal has to come from the
/// turn, not from an actor that could not answer. What must hold is that an ended turn is caught
/// before the token exists: no header is produced for a delivery nobody is waiting for, and no
/// request reaches the remote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_that_ended_before_the_first_leg_mints_nothing_and_touches_no_remote() {
    let _trust = exclusive_trust();
    let root = scratch("pre-http-cancel");
    let branch = "maxplayer/5161a003";
    let (workdir, oid) = job_workdir(&root, branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&bare, "/git/seller/r.git");
    stage_env(&relay.ca_file(&root));

    // A LIVE signer: released before the delivery runs.
    let signer = HeldSigner::spawn_held(root.join("home"));
    signer.release();
    let minted = {
        let handle = signer.handle.clone();
        let destination = relay.repo_url();
        tokio::task::spawn_blocking(move || {
            handle.http_auth_header_blocking(
                destination,
                None,
                Instant::now() + Duration::from_secs(10),
            )
        })
        .await
        .expect("liveness leg")
    };
    let header = minted.expect("this gate needs a signer that CAN mint");
    assert!(
        header.starts_with("Nostr "),
        "the live-signer control did not produce a NIP-98 header: {header}"
    );

    let asked = Arc::new(AtomicUsize::new(0));
    let push_authority = PushAuthority::new();
    let minter = production_minter(
        signer.handle.clone(),
        relay.repo_url(),
        git_transport::delivery_ref(branch),
        push_authority.check(),
        Instant::now() + Duration::from_secs(60),
        Arc::clone(&asked),
    );

    // The turn is already over when the delivery starts. The AUTHORITY, however, is live: this
    // gate is about the turn stopping the delivery, so the one thing that must not do the stopping
    // is an authority that was already dead before the push began.
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(
        Token(Arc::clone(&released)),
        Instant::now() - Duration::from_millis(1),
    );

    let started = Instant::now();
    let outcome = neutralize_then_push_in_child_off_runtime(
        shipped_binary(),
        workdir,
        relay.repo_url(),
        branch.to_owned(),
        oid.clone(),
        Some(minter),
        Some(push_authority.check()),
        turn,
    )
    .await;
    let elapsed = started.elapsed();

    let error = outcome.expect_err("an expired turn must not deliver");
    assert!(
        matches!(error, SellerGitError::Cancelled(_)),
        "an expired turn must be reported as cancelled: {error:?}"
    );
    assert!(
        elapsed < REAP_BOUND + TEARDOWN_ALLOWANCE,
        "an already-expired turn took {elapsed:?} to refuse"
    );
    assert_eq!(
        relay.requests().len(),
        0,
        "a delivery whose turn had already ended still reached the remote: {:?}",
        relay
            .requests()
            .iter()
            .map(|request| format!("{} {}", request.method, request.target))
            .collect::<Vec<_>>()
    );
    control.end();
    assert!(
        !control.holds_ownership() && released.load(Ordering::SeqCst),
        "the exclusion token was not handed back"
    );

    // The signer was live throughout, and it was never asked to mint for a turn that had already
    // ended: the refusal came from the turn, ahead of the token.
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "an expired turn still reached the minter"
    );
}

/// THE SENSITIVITY CONTROL for T2a and T2b.
///
/// Same home, same actor type, same production minter, same fixture, same budget — with the signer
/// polled normally. It mints real NIP-98 headers, the relay sees them on the wire, the pack is
/// uploaded and the ref moves. Without this, "no POST" and "no ref" above would be satisfied by any
/// delivery that was broken for any reason at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_delivery_with_a_polled_signer_mints_real_tokens_and_lands() {
    let _trust = exclusive_trust();
    let root = scratch("live-signer");
    let branch = "maxplayer/5161a004";
    let (workdir, oid) = job_workdir(&root, branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let relay = GitHttpAuthServer::spawn(&bare, "/git/seller/r.git");
    stage_env(&relay.ca_file(&root));

    let signer = HeldSigner::spawn_held(root.join("home"));
    signer.release();

    let asked = Arc::new(AtomicUsize::new(0));
    let push_authority = PushAuthority::new();
    let minter = production_minter(
        signer.handle.clone(),
        relay.repo_url(),
        git_transport::delivery_ref(branch),
        push_authority.check(),
        Instant::now() + Duration::from_secs(60),
        Arc::clone(&asked),
    );

    let budget = Duration::from_millis(2_500);
    let released = Arc::new(AtomicBool::new(false));
    let (_control, turn) = delivery_turn(Token(Arc::clone(&released)), Instant::now() + budget);

    let started = Instant::now();
    let pushed = neutralize_then_push_in_child_off_runtime(
        shipped_binary(),
        workdir,
        relay.repo_url(),
        branch.to_owned(),
        oid.clone(),
        Some(minter),
        Some(push_authority.check()),
        turn,
    )
    .await
    .expect("a delivery with a live production signer must land");
    let elapsed = started.elapsed();

    assert_eq!(pushed, oid, "the child delivered a different object");
    assert!(
        elapsed < budget,
        "the control delivery took {elapsed:?}, not 'well inside' a {budget:?} budget"
    );
    assert!(
        asked.load(Ordering::SeqCst) >= 1,
        "the control minted nothing, so the signer was never in this delivery's path"
    );

    // Real tokens, from the real actor, observed by the remote — not merely returned to the child.
    let requests = relay.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.authorization.as_deref().is_some_and(|header| header
                .starts_with("Nostr "))),
        "no leg carried a NIP-98 token minted by the signer actor: {:?}",
        requests
            .iter()
            .map(|request| (request.method.clone(), request.authorization.is_some()))
            .collect::<Vec<_>>()
    );
    assert!(
        requests
            .iter()
            .any(|request| request.method == "POST" && request.target.contains("git-receive-pack")),
        "the control never uploaded a pack"
    );
    assert_eq!(
        git2::Repository::open_bare(&bare)
            .expect("open bare")
            .find_reference(&format!("refs/heads/{branch}"))
            .expect("the control must move the remote ref")
            .target()
            .map(|oid| oid.to_string())
            .as_deref(),
        Some(oid.as_str()),
        "the remote ref did not move in the control delivery"
    );
}
