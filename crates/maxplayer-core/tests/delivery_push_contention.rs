//! Two awarded jobs, one delivery remote, one lock — and the question the narrow change exists to
//! answer: when does the SECOND delivery's push token get signed?
//!
//! This is the shape the seat actually runs in. [`serialized_bounded_push`] serializes deliveries to
//! this seat's one remote, so a job that arrives while another is pushing waits — for the whole of
//! the first job's advertisement, pack POST and whatever the relay took to answer. A token minted
//! when the job was picked up would spend that entire wait ageing, and would be at its oldest
//! exactly when the relay finally checks it. Minted per request, from inside the transport, it is
//! signed on the far side of the wait instead.
//!
//! # What these tests are allowed to use as proof
//!
//! Not a sleep, and not a clock. "Token A was stamped before token B" is an inequality between two
//! readings, and it stays true whether or not the lock caused it; "the second delivery slept 250ms
//! so it must be waiting" is a guess about the scheduler. Both were rejected, rightly.
//!
//! What is used instead is ORDER IN A SINGLE LOG. Every interesting moment — a delivery taking the
//! lock, minting a token, releasing the lock — appends to one `Vec` behind one mutex, so the log's
//! order IS a happens-before chain. The push body only runs once `serialized_bounded_push` holds the
//! lock, so `Enter` means "owns the lock" and `Exit` means "released it"; a mint recorded between
//! them provably happened while that delivery held it. `Enter(2)` appearing after `Exit(1)` is then
//! not evidence of serialization, it is serialization.
//!
//! Where a test needs the first delivery to be mid-flight, the fixture holds the request and TELLS
//! the test it is holding it ([`git_http_fixture::RequestGate`]) — an appointment, not a delay.
//!
//! Everything else here is the production article: the real serialized wrapper, the real signer
//! actor (the seller key never leaves it), the real delivery wrapper, the real transport, and the
//! real HTTPS git fixture recording the exact `Authorization` bytes each request arrived with.
//!
//! Its own test binary, not a unit test, for a concrete reason: the transport's HTTP client is built
//! once per process, baking in whether it accepts this fixture's self-signed certificate. Only a
//! process this test owns can stage that trust before anything else builds the client.
#![cfg(all(unix, feature = "git-delivery"))]

mod git_http_fixture;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use git_http_fixture::{FixtureOptions, GitHttpAuthServer, RequestGate};
use maxplayer_core::git_transport::{self, AuthMinter};
use maxplayer_core::home::bootstrap;
use maxplayer_core::seller_git;
use maxplayer_core::seller_node::run::{
    DELIVERY_PUSH_TIMEOUT, DeliveryPushErr, PushAuthority, serialized_bounded_push,
};
use maxplayer_core::seller_node::signer::{self, SignerHandle};

static ENV_INIT: Once = Once::new();

/// Stage the process env once, before the transport's shared HTTP client exists.
fn init_test_env() {
    ENV_INIT.call_once(|| {
        // SAFETY (edition 2024 set_var): every test in this binary funnels through this Once before
        // it touches the transport; racing test threads block in call_once until the env is staged.
        unsafe {
            std::env::set_var("GIT_SSL_NO_VERIFY", "1");
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
            std::env::set_var("no_proxy", "127.0.0.1,localhost");
        }
    });
}

/// One moment in the life of a delivery, in the order it happened. See the module docs: the ORDER of
/// these is the proof, so every one of them is appended under the same mutex.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Moment {
    /// This delivery's push body began — which is to say `serialized_bounded_push` has handed it the
    /// lock. Nothing else can be running a push at this point.
    Enter(u8),
    /// This delivery asked for the lock. The next thing its task does is await it.
    Requested(u8),
    /// The minter returned a token for this delivery. Always inside that delivery's push.
    Mint(u8, String),
    /// The minter refused. The leg it was for was never sent.
    Refused(u8, String),
    /// This delivery's push body returned; the lock is released as this settles.
    Exit(u8),
}

/// The single ordered record every delivery in a test appends to.
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<Moment>>>);

impl Journal {
    fn record(&self, moment: Moment) {
        self.0.lock().expect("journal").push(moment);
    }

    fn entries(&self) -> Vec<Moment> {
        self.0.lock().expect("journal").clone()
    }

    /// Position of the first matching moment, or a failure naming the whole journal — a missing
    /// moment is a different bug from an out-of-order one and should not read as one.
    fn first(&self, want: impl Fn(&Moment) -> bool, what: &str) -> usize {
        let entries = self.entries();
        entries
            .iter()
            .position(want)
            .unwrap_or_else(|| panic!("no {what} in the journal: {entries:?}"))
    }

    fn last(&self, want: impl Fn(&Moment) -> bool, what: &str) -> usize {
        let entries = self.entries();
        entries
            .iter()
            .rposition(want)
            .unwrap_or_else(|| panic!("no {what} in the journal: {entries:?}"))
    }

    /// Every token this delivery minted, in order.
    fn tokens(&self, delivery: u8) -> Vec<String> {
        self.entries()
            .iter()
            .filter_map(|moment| match moment {
                Moment::Mint(who, header) if *who == delivery => Some(header.clone()),
                _ => None,
            })
            .collect()
    }

    fn refusals(&self, delivery: u8) -> Vec<String> {
        self.entries()
            .iter()
            .filter_map(|moment| match moment {
                Moment::Refused(who, why) if *who == delivery => Some(why.clone()),
                _ => None,
            })
            .collect()
    }
}

fn temp(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "maxplayer-delivery-contention-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir root");
    root
}

/// A committed workdir for one job: a single commit on that job's delivery branch.
fn job_workdir(root: &Path, name: &str, branch: &str) -> (PathBuf, String) {
    let workdir = root.join(name);
    let repo = git2::Repository::init(&workdir).expect("init workdir");
    std::fs::write(
        workdir.join("deliverable.txt"),
        format!("work from {name}\n"),
    )
    .expect("write");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("deliverable.txt")).expect("add");
    index.write().expect("write index");
    let tree = repo
        .find_tree(index.write_tree().expect("tree"))
        .expect("find tree");
    let sig = git2::Signature::new("s", "s@example.invalid", &git2::Time::new(1_700_000_000, 0))
        .expect("sig");
    let oid = repo
        .commit(
            Some(&git_transport::delivery_ref(branch)),
            &sig,
            &sig,
            "delivery",
            &tree,
            &[],
        )
        .expect("commit");
    (workdir, oid.to_string())
}

/// The minter the delivery push runs with, built exactly as `execute` builds it — bound to this
/// job's remote, scoped to this job's ref, signed through the actor, refusing once this delivery's
/// authority has ended, bounded by the push deadline — wrapped so every call lands in the journal.
fn production_shaped_minter(
    delivery: u8,
    signer: SignerHandle,
    remote: &str,
    scope: &str,
    deadline: Instant,
    authority: &PushAuthority,
    journal: Journal,
) -> AuthMinter {
    let intended = remote.to_owned();
    let scope = scope.to_owned();
    let check = authority.check();
    Arc::new(move |destination: &str| {
        let refuse = |why: String| {
            journal.record(Moment::Refused(delivery, why.clone()));
            Err(why)
        };
        if !git_transport::same_destination(&intended, destination) {
            return refuse(format!(
                "refusing to authorize a leg to {destination}: this delivery is bound to {intended}"
            ));
        }
        if let Err(ended) = check() {
            return refuse(ended);
        }
        if Instant::now() >= deadline {
            return refuse("this delivery's push deadline has passed".to_owned());
        }
        let header =
            match signer.http_auth_header_blocking(destination.to_owned(), Some(scope.clone()), deadline)
            {
                Ok(header) => header,
                Err(error) => return refuse(error),
            };
        journal.record(Moment::Mint(delivery, header.clone()));
        Ok(header)
    })
}

/// Decode the NIP-98 event JSON out of an `Authorization: Nostr <base64>` header.
fn token_json(header: &str) -> String {
    use base64::Engine as _;
    let b64 = header.strip_prefix("Nostr ").expect("Nostr scheme");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64");
    String::from_utf8(bytes).expect("utf8")
}

/// Everything a delivery in these tests needs, assembled the way the seller node assembles it.
struct Delivery {
    id: u8,
    workdir: PathBuf,
    branch: &'static str,
    oid: String,
}

/// Run one delivery through the real wrapper, journalling its lock ownership and every mint.
///
/// Returns the outcome and this delivery's authority — the caller holds it, exactly as the delivery
/// arm does, so dropping the returned value is what ends the authority.
async fn run_delivery(
    delivery: Delivery,
    lock: Arc<tokio::sync::Mutex<()>>,
    signer: SignerHandle,
    url: String,
    journal: Journal,
) -> Result<String, DeliveryPushErr> {
    let deadline = Instant::now() + DELIVERY_PUSH_TIMEOUT;
    let authority = PushAuthority::new();
    let minter = production_shaped_minter(
        delivery.id,
        signer,
        &url,
        &git_transport::delivery_ref(delivery.branch),
        deadline,
        &authority,
        journal.clone(),
    );
    let check = authority.check();
    let id = delivery.id;
    let Delivery {
        workdir,
        branch,
        oid,
        ..
    } = delivery;
    journal.record(Moment::Requested(id));
    let body_journal = journal.clone();
    let outcome = serialized_bounded_push(&lock, DELIVERY_PUSH_TIMEOUT, move || async move {
        body_journal.record(Moment::Enter(id));
        let result = seller_git::neutralize_then_push_off_runtime(
            workdir,
            url,
            branch.to_owned(),
            oid,
            Some(minter),
            Some(check),
        )
        .await;
        body_journal.record(Moment::Exit(id));
        result
    })
    .await;
    drop(authority);
    outcome
}

/// The second delivery's tokens are signed AFTER it stopped waiting for the first — through the real
/// lock, the real actor and the real wire.
///
/// Red-on-revert: mint the token before `serialized_bounded_push` (the pre-minted-header shape) and
/// the second job's `Mint` moments appear before `Exit(1)`, failing the ordering assertion; make
/// `HttpStream::send` reuse one token per operation and the per-leg distinctness assertions fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_delivery_that_waited_for_the_lock_signs_after_the_wait() {
    init_test_env();
    let root = temp("two-jobs");
    let first_branch = "maxplayer/aaaa1111";
    let second_branch = "maxplayer/bbbb2222";
    let (first_workdir, first_oid) = job_workdir(&root, "job-a", first_branch);
    let (second_workdir, second_oid) = job_workdir(&root, "job-b", second_branch);

    // One delivery remote for the seat. Its first response is HELD — not for a duration, until this
    // test opens the gate.
    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let gate = RequestGate::new();
    let relay = GitHttpAuthServer::spawn_with(
        &relay_repo,
        "/git/seller/r.git",
        FixtureOptions {
            hold_first_request: Some(Arc::clone(&gate)),
            ..FixtureOptions::default()
        },
    );
    let url = relay.repo_url();

    // The real signer actor: one seller key, owned by the actor, shared by both deliveries.
    let home_root = root.join("home");
    let home = bootstrap(&home_root).expect("bootstrap home");
    let signer = signer::spawn(&home).expect("spawn signer");

    // The seat's ONE delivery lock.
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let journal = Journal::default();

    let first = tokio::spawn(run_delivery(
        Delivery {
            id: 1,
            workdir: first_workdir,
            branch: first_branch,
            oid: first_oid.clone(),
        },
        Arc::clone(&lock),
        signer.clone(),
        url.clone(),
        journal.clone(),
    ));

    // Deterministic: this returns when the fixture has a request parked. The first delivery is
    // therefore holding the lock, has minted for that leg, and cannot proceed.
    let gate_for_wait = Arc::clone(&gate);
    tokio::task::spawn_blocking(move || gate_for_wait.wait_held())
        .await
        .expect("gate wait");
    assert_eq!(
        journal.entries(),
        vec![
            Moment::Requested(1),
            Moment::Enter(1),
            Moment::Mint(1, journal.tokens(1).first().cloned().unwrap_or_default()),
        ],
        "with the first leg held, exactly one delivery has the lock and exactly one token exists"
    );

    let second = tokio::spawn(run_delivery(
        Delivery {
            id: 2,
            workdir: second_workdir,
            branch: second_branch,
            oid: second_oid.clone(),
        },
        Arc::clone(&lock),
        signer.clone(),
        url.clone(),
        journal.clone(),
    ));

    // The second delivery has asked for the lock. `Requested` is recorded with no await between it
    // and `lock.lock()`, so from here it is queued on the mutex or one instruction from it — and
    // either way the journal below proves what it did, not what it intended.
    while !journal.entries().contains(&Moment::Requested(2)) {
        tokio::task::yield_now().await;
    }
    assert!(
        journal.tokens(2).is_empty(),
        "the waiting delivery must not have signed anything yet: {:?}",
        journal.entries()
    );

    gate.release();

    let first_outcome = first.await.expect("first delivery task");
    let second_outcome = second.await.expect("second delivery task");

    assert_eq!(
        first_outcome.expect("first delivery pushed"),
        first_oid,
        "the first delivery shipped its gated object"
    );
    assert_eq!(
        second_outcome.expect("second delivery pushed"),
        second_oid,
        "the second delivery shipped its gated object"
    );

    // THE POINT, as an order rather than an interval: the waiting delivery's FIRST mint comes after
    // the holding delivery released the lock. Everything between `Enter(1)` and `Exit(1)` happened
    // while delivery 1 owned the lock, so this is the lock's guarantee, not a race won.
    let entries = journal.entries();
    let exit_first = journal.last(|m| m == &Moment::Exit(1), "Exit(1)");
    let enter_second = journal.first(|m| m == &Moment::Enter(2), "Enter(2)");
    assert!(
        exit_first < enter_second,
        "the second delivery entered before the first released the lock: {entries:?}"
    );
    let last_mint_first = journal.last(|m| matches!(m, Moment::Mint(1, _)), "a mint by delivery 1");
    let first_mint_second =
        journal.first(|m| matches!(m, Moment::Mint(2, _)), "a mint by delivery 2");
    assert!(
        last_mint_first < exit_first && exit_first < first_mint_second,
        "the waiting delivery's first token must be signed after the wait ended, not before it \
         began: {entries:?}"
    );

    // Each delivery minted one token per wire request, and no two are the same bytes.
    let first_tokens = journal.tokens(1);
    let second_tokens = journal.tokens(2);
    assert_eq!(first_tokens.len(), 2, "{entries:?}");
    assert_eq!(second_tokens.len(), 2, "{entries:?}");
    let all: Vec<&String> = first_tokens.iter().chain(second_tokens.iter()).collect();
    for (index, token) in all.iter().enumerate() {
        for other in all.iter().skip(index + 1) {
            assert_ne!(
                token, other,
                "every leg of every delivery carries its own token"
            );
        }
    }

    // Each delivery's tokens name ITS ref, never the other job's.
    for (tokens, mine, theirs) in [
        (&first_tokens, first_branch, second_branch),
        (&second_tokens, second_branch, first_branch),
    ] {
        for token in tokens.iter() {
            let json = token_json(token);
            assert!(
                json.contains(&git_transport::delivery_ref(mine)),
                "token is scoped to its own job's ref: {json}"
            );
            assert!(
                !json.contains(&git_transport::delivery_ref(theirs)),
                "token must not name the other job's ref: {json}"
            );
            assert!(
                json.contains(&url),
                "token is bound to the seat's remote: {json}"
            );
        }
    }

    // And the wire agrees: four authorized requests, each carrying the exact bytes minted for it,
    // and never two at once.
    let requests = relay.requests();
    assert_eq!(
        requests.len(),
        4,
        "two legs per delivery, and no read-back after either: {requests:?}"
    );
    let on_the_wire: Vec<&str> = requests
        .iter()
        .map(|request| {
            request
                .authorization
                .as_deref()
                .expect("every leg carried a token")
        })
        .collect();
    for token in all.iter() {
        assert!(
            on_the_wire.contains(&token.as_str()),
            "a minted token that never reached the wire: {token:?}"
        );
    }
    assert_eq!(
        relay.peak_concurrent_requests(),
        1,
        "the seat's deliveries must never be uploading to the one remote at the same time"
    );

    // Both deliveries landed, each at its own ref.
    let bare = git2::Repository::open_bare(&relay_repo).expect("relay bare");
    assert_eq!(
        bare.refname_to_id(&git_transport::delivery_ref(first_branch))
            .expect("first ref")
            .to_string(),
        first_oid
    );
    assert_eq!(
        bare.refname_to_id(&git_transport::delivery_ref(second_branch))
            .expect("second ref")
            .to_string(),
        second_oid
    );

    drop(relay);
    let _ = std::fs::remove_dir_all(&root);
}

/// A delivery cancelled mid-push never puts another request on the wire, and the next delivery does
/// not start until the cancelled one has actually stopped.
///
/// The cancellation happens where it hurts: the advertisement leg is held at the server, so the push
/// is on the wire with a blocking thread inside libgit2 that nothing can interrupt. The delivery
/// future is then aborted. When the held leg is finally answered, that thread wakes up and tries to
/// send the pack POST — and the authority it must ask has been dropped with the delivery.
///
/// Red-on-revert (authority): end authority with a `store(false)` after the push instead of by
/// `Drop` and the abort never ends it, so the pack POST goes out and the request count is 2.
/// Red-on-revert (ownership): release the lock when the caller stops waiting instead of when the
/// push is quiescent, and the probe below acquires the lock while the cancelled push is still on the
/// wire — the peak concurrency the fixture sees goes to 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_delivery_sends_nothing_more_and_keeps_its_turn_until_it_stops() {
    init_test_env();
    let root = temp("cancelled");
    let branch = "maxplayer/cccc3333";
    let (workdir, oid) = job_workdir(&root, "job-c", branch);

    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let gate = RequestGate::new();
    let relay = GitHttpAuthServer::spawn_with(
        &relay_repo,
        "/git/seller/r.git",
        FixtureOptions {
            hold_first_request: Some(Arc::clone(&gate)),
            ..FixtureOptions::default()
        },
    );
    let url = relay.repo_url();

    let home_root = root.join("home");
    let home = bootstrap(&home_root).expect("bootstrap home");
    let signer = signer::spawn(&home).expect("spawn signer");

    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let journal = Journal::default();

    let doomed = tokio::spawn(run_delivery(
        Delivery {
            id: 1,
            workdir,
            branch,
            oid,
        },
        Arc::clone(&lock),
        signer.clone(),
        url.clone(),
        journal.clone(),
    ));

    // The advertisement leg is parked at the server: the push is genuinely in flight.
    let gate_for_wait = Arc::clone(&gate);
    tokio::task::spawn_blocking(move || gate_for_wait.wait_held())
        .await
        .expect("gate wait");
    assert_eq!(relay.requests().len(), 1, "one leg is on the wire");
    assert_eq!(journal.tokens(1).len(), 1, "one token was minted for it");

    // Cancel the delivery exactly as a dropped delivery future is cancelled. The blocking push
    // thread does not stop — that is the whole hazard.
    doomed.abort();
    let _ = doomed.await;

    // Let the held leg finish. What the push thread does NEXT is the test.
    gate.release();

    // The lock is only free once the abandoned push is quiescent, so acquiring it is the wait —
    // no sleep, no poll. Bounded so a regression fails instead of hanging.
    let settled = tokio::time::timeout(Duration::from_secs(60), lock.lock()).await;
    let settled = settled.expect("the abandoned push must release the seat's delivery lock");
    drop(settled);

    let entries = journal.entries();
    assert_eq!(
        relay.requests().len(),
        1,
        "a cancelled delivery must never put another request on the wire: {:?}",
        relay.requests()
    );
    assert_eq!(
        journal.tokens(1).len(),
        1,
        "and must never mint again after it was cancelled: {entries:?}"
    );
    let refusals = journal.refusals(1);
    assert!(
        refusals.iter().any(|why| why.contains("authority has ended")),
        "the abandoned push must be refused by name, got {refusals:?} in {entries:?}"
    );
    assert_eq!(
        relay.peak_concurrent_requests(),
        1,
        "nothing else may reach the remote while an abandoned push is still on it"
    );
    // Nothing landed: the ref the cancelled delivery was pushing does not exist on the remote.
    let bare = git2::Repository::open_bare(&relay_repo).expect("relay bare");
    assert!(
        bare.refname_to_id(&git_transport::delivery_ref(branch))
            .is_err(),
        "a cancelled delivery must not leave a ref behind"
    );

    drop(relay);
    let _ = std::fs::remove_dir_all(&root);
}

/// A token that was signed while the delivery was ending never reaches the wire.
///
/// The minter asks authority before it signs — but signing is a round trip through the signer actor,
/// which has a queue and a mutex-held key, and a delivery can end DURING it: the awarding future is
/// dropped, the job times out, the push panics. Everything the minter checked is then stale by the
/// time it returns, and what it returns is a valid, freshly-stamped, correctly-scoped token for a
/// delivery that no longer exists. If that token is transmitted, the seat has pushed on the strength
/// of authority it no longer had, and the refusal it should have printed is instead a ref on the
/// relay.
///
/// So the transport asks a second time, after the mint and with nothing between the answer and
/// `send()`. This test puts the delivery's death exactly in that window: the mint is parked at the
/// signer, authority is dropped while it is parked, and the mint then completes successfully.
///
/// Red-on-revert: delete the authority re-check in `HttpStream::send` and the signed token goes out
/// — the fixture records one request, and the push fails (or succeeds) on the relay's terms instead
/// of being refused before transmit. The minter's own check cannot catch this: it already ran, and
/// it passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_signed_while_the_delivery_ended_is_never_transmitted() {
    init_test_env();
    let root = temp("ended-mid-mint");
    let branch = "maxplayer/dddd4444";
    let (workdir, oid) = job_workdir(&root, "job-d", branch);

    let relay_repo = root.join("relay.git");
    git2::Repository::init_bare(&relay_repo).expect("relay bare");
    let relay = GitHttpAuthServer::spawn_with(&relay_repo, "/git/seller/r.git", FixtureOptions::default());
    let url = relay.repo_url();

    let home_root = root.join("home");
    let home = bootstrap(&home_root).expect("bootstrap home");
    let signer = signer::spawn(&home).expect("spawn signer");

    let authority = PushAuthority::new();
    let check = authority.check();
    let journal = Journal::default();

    // The signer's queue, made explicit: the minter checks authority, then parks HERE — announcing
    // that it is parked — and only signs once this test lets it.
    let signing = RequestGate::new();

    let deadline = Instant::now() + DELIVERY_PUSH_TIMEOUT;
    let minter: AuthMinter = {
        let check = authority.check();
        let signing = Arc::clone(&signing);
        let signer = signer.clone();
        let scope = git_transport::delivery_ref(branch);
        let journal = journal.clone();
        Arc::new(move |destination: &str| {
            // The check the production minter makes, passing — the delivery is alive right now.
            if let Err(ended) = check() {
                journal.record(Moment::Refused(1, ended.clone()));
                return Err(ended);
            }
            // ... and then the wait that makes that answer stale.
            signing.park();
            let header = signer
                .http_auth_header_blocking(destination.to_owned(), Some(scope.clone()), deadline)
                .map_err(|error| {
                    journal.record(Moment::Refused(1, error.clone()));
                    error
                })?;
            journal.record(Moment::Mint(1, header.clone()));
            Ok(header)
        })
    };

    let url_for_push = url.clone();
    let workdir_for_push = workdir.clone();
    let branch_owned = branch.to_owned();
    let oid_for_push = oid.clone();
    let push = tokio::task::spawn_blocking(move || {
        git_transport::push_branch_with_minter(
            &workdir_for_push,
            &url_for_push,
            &branch_owned,
            &oid_for_push,
            Some(minter),
            Some(check),
        )
    });

    // Deterministic: returns when the mint is genuinely parked mid-signature.
    let signing_for_wait = Arc::clone(&signing);
    tokio::task::spawn_blocking(move || signing_for_wait.wait_held())
        .await
        .expect("signing gate");
    assert!(
        relay.requests().is_empty(),
        "nothing should have reached the relay before the first token exists: {:?}",
        relay.requests()
    );

    // The delivery ends while its token is being signed.
    drop(authority);
    signing.release();

    let outcome = push.await.expect("push task");
    let error = outcome
        .expect_err("a delivery that ended mid-signature must not push")
        .to_string();

    // The token WAS minted — this is not a test of the minter refusing.
    assert_eq!(
        journal.tokens(1).len(),
        1,
        "the mint completed; the question is what was done with it: {:?}",
        journal.entries()
    );
    // And it never left the process.
    assert!(
        relay.requests().is_empty(),
        "a token signed for a delivery that had ended reached the relay: {:?}",
        relay.requests()
    );
    assert!(
        error.contains("refusing to send") && error.contains("authority has ended"),
        "the leg must be refused before transmit, by name: {error}"
    );
    let bare = git2::Repository::open_bare(&relay_repo).expect("relay bare");
    assert!(
        bare.refname_to_id(&git_transport::delivery_ref(branch)).is_err(),
        "nothing may land for a delivery that ended"
    );

    drop(relay);
    let _ = std::fs::remove_dir_all(&root);
}
