//! T3: a shipped delivery parked on a real wire leg is stopped — by TIMEOUT and by TASK ABORT —
//! while a second delivery is polled **continuously, through the reap**, and never overlaps it.
//!
//! # What the existing gate does and where it stops
//!
//! `delivery_push_shipped_child.rs::a_second_delivery_is_observed_pending_behind_a_held_shipped_pack_upload`
//! parks the shipped child at `POST /git-receive-pack` and polls a second delivery while the pack
//! is on the wire. It was credited for that. But its polling loop ENDS before the first delivery's
//! deadline fires, and it then does a single `first.await` — so across the window that actually
//! decides whether two deliveries can overlap (the deadline firing, the `SIGKILL`, and the wait for
//! the exit) the second delivery is not polled at all. The seat is unobserved for exactly the
//! interval the contract is about.
//!
//! These gates poll delivery B through that window and record every sample. The claim is checkable
//! rather than rhetorical, and it is stated so that a descheduled observer cannot decide it: *every
//! poll of B during A's stop returned `Pending`, A's child was never seen ALIVE at or after the
//! instant B entered its push body, and B's own entry is stamped no earlier than A's own release of
//! the turn.* How often the observer got to look is reported as coverage — see [`MAX_SAMPLE_GAP`] —
//! because that is a fact about the observer, not about whether two deliveries overlapped.
//!
//! # Task abort is a different path from timeout, and it is the dangerous one
//!
//! A timeout runs `serialized_bounded_push`'s own timeout arm. An ABORT drops the whole delivery
//! future where it stands — the case `PushAuthority` and the turn's ownership transfer exist for
//! (`run.rs:1880-1883`: the seat's lock guard is moved INTO the turn, so nothing that happens to the
//! awaiting task can release it early). If the guard had stayed on the task's side, an aborted
//! delivery would free the seat instantly while its child was still pushing. That is the overlap
//! this file is here to rule out, and abort is how you provoke it.
//!
//! # What actually ends an aborted delivery here (round 3, item 2)
//!
//! The abort does NOT cancel the push. The push runs under `spawn_blocking`, and dropping that
//! JoinHandle leaves the closure running to completion. What the abort does is drop the
//! supervisor's `TurnControl`, whose `Drop` revokes the turn; the still-running closure holds a
//! `WorkLifetime` over that same turn, wired into the transport as its per-leg/per-chunk gate, so
//! the next gate check fails the leg and the child is killed and reaped.
//!
//! Turn revocation observed by the transport gate — NOT authority propagation, which is not even
//! wired on this path (`authority: None` below), and NOT task cancellation. That is why the two
//! authority mutants tried in round 2 both survived. `scripts/oracle-red-before-green.sh` proves
//! this mechanism at product level with `R3-2/M-NO-REVOKE-ON-DROP`: making `TurnControl::drop` a
//! no-op sends both abort gates red at the bound below (B took the seat 58.7s and 60.1s after the
//! abort, against the 13.05s allowed) while both TIMEOUT gates in this file stay green.
//!
//! # Bound
//!
//! B may not enter its push body before A's turn is handed back, and A's turn is handed back only
//! after its child's exit is confirmed. Both instants are recorded and compared; neither is
//! inferred from a sleep.

#![cfg(all(unix, feature = "wallet"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maxplayer_core::delivery_turn::DeliveryTurn;
use maxplayer_core::git_transport::{self, AuthMinter};
use maxplayer_core::seller_git::{SellerGitError, neutralize_then_push_in_child_off_runtime};
use maxplayer_core::seller_node::run::{DeliveryPushErr, serialized_bounded_push};

#[path = "../../maxplayer-core/tests/git_http_fixture/mod.rs"]
mod git_http_fixture;

use git_http_fixture::{FixtureOptions, GitHttpAuthServer, RequestGate};

/// The polling cadence these gates aim for while delivery A is being stopped: the slack on the
/// window-span check, and the yardstick for the coverage number they report.
///
/// It is deliberately NOT an exclusion oracle. The gap between two polls is a fact about when the
/// observer was scheduled, not about whether two deliveries overlapped — under real gate load the
/// poller is descheduled and the gap grows on a run where nothing overlapped at all. Exclusion is
/// carried instead by evidence that does not depend on the observer's punctuality: the stamps the
/// participants record themselves, and the positive alive-samples of A's child.
const MAX_SAMPLE_GAP: Duration = Duration::from_millis(50);

/// **THE ABORT-RELATIVE BOUND, AND WHY THE ABORT CASE IS WORTHLESS WITHOUT ONE.**
///
/// A's budget in the abort case is 60 seconds, deliberately long so the deadline cannot fire first
/// and steal the stop under test. But that same length is what made the gate weak: a cancellation
/// that was IGNORED ENTIRELY, with A left to die at its natural 60-second deadline kill, satisfied
/// every assertion in this file. No-overlap held, the child was gone before B ran, the ref had not
/// moved — all true of a delivery that simply ran its full course. The gate proved the seat came
/// back, not that aborting is what brought it back.
///
/// So the stop is measured FROM THE ABORT. The terms are the product's own: one cancellation poll
/// for the executor to notice, two reap windows for the kill and the confirmation the seat requires,
/// and a few seconds of scheduling slack. It is far below the 60-second budget on purpose — that
/// gap is exactly the difference between "the abort stopped it" and "the deadline did".
fn abort_stop_bound() -> Duration {
    maxplayer_core::delivery_executor::CANCELLATION_POLL
        + maxplayer_core::delivery_executor::REAP_BOUND * 2
        + Duration::from_secs(3)
}

/// Records the instant the seat's exclusion token is handed back. The turn releases ownership only
/// once the work is recorded stopped, which for a child delivery is after `kill_and_reap` confirmed
/// the exit — so this instant IS "A's child is gone and the seat is free", taken from the
/// production type rather than from a sleep in the test.
struct Token {
    released_at: Arc<std::sync::Mutex<Option<Instant>>>,
}

impl Drop for Token {
    fn drop(&mut self) {
        self.released_at
            .lock()
            .expect("release clock")
            .get_or_insert_with(Instant::now);
    }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "maxplayer-wire-abort-{label}-{}-{id}",
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
    std::fs::write(workdir.join("deliverable.txt"), "wire abort\n").expect("write");
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
    // SAFETY (edition 2024 `set_var`): staged at the top of the test body before any task is
    // spawned, and every test in this binary stages the same values under one lock.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", ca);
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
        std::env::remove_var("GIT_SSL_NO_VERIFY");
    }
}

static TRUST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exclusive_trust() -> std::sync::MutexGuard<'static, ()> {
    TRUST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The pids of this process's direct children.
///
/// The delivery child is spawned by this process, so it appears here while it lives. Read from
/// `ps` rather than from anything the executor reports, because the point of asking is to check the
/// executor's report against the operating system.
///
/// `ps` is itself a direct child of this process and lists itself, so its own pid is captured and
/// removed — otherwise the probe finds a second "delivery child" that is really the probe.
fn direct_children() -> Vec<i32> {
    let me = std::process::id();
    let mut probe = std::process::Command::new("ps")
        .args(["-ax", "-o", "pid=,ppid="])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn ps");
    let probe_pid = probe.id() as i32;
    let output = probe.wait_with_output().expect("ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: i32 = fields.next()?.parse().ok()?;
            let ppid: u32 = fields.next()?.parse().ok()?;
            (ppid == me && pid != probe_pid).then_some(pid)
        })
        .collect()
}

/// Does this pid still exist?
///
/// `kill(pid, 0)` performs the permission and existence checks and sends nothing. It succeeds for a
/// ZOMBIE too — a child that exited but has not been waited for — so this returns false only once
/// the parent has actually reaped it. That is the property this gate needs: "gone" must mean gone
/// from the process table, not merely stopped.
fn pid_exists(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// ONE real poll of a real future, and the `Poll` it returned handed straight back.
async fn poll_once<F: std::future::Future>(
    mut future: std::pin::Pin<&mut F>,
) -> std::task::Poll<F::Output> {
    std::future::poll_fn(move |cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await
}

/// Which wire leg the fixture parks. The advertisement is request 1; the pack upload is request 2.
#[derive(Clone, Copy)]
enum Leg {
    Advertisement,
    PackUpload,
}

impl Leg {
    fn held_request_number(self) -> usize {
        match self {
            Leg::Advertisement => 1,
            Leg::PackUpload => 2,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Leg::Advertisement => "GET /info/refs",
            Leg::PackUpload => "POST /git-receive-pack",
        }
    }
}

/// How delivery A is stopped.
#[derive(Clone, Copy, PartialEq)]
enum Stop {
    /// `serialized_bounded_push`'s own timeout arm.
    Timeout,
    /// The whole delivery future dropped where it stands.
    TaskAbort,
}

/// The body shared by all four gates.
///
/// Delivery A is the shipped binary, parked by the fixture on `leg`. Delivery B asks the same
/// serializer for the same seat. B is polled continuously from before A is stopped until after it
/// gets the seat, and every sample instant is kept so the cadence can be asserted rather than
/// claimed.
async fn a_parked_leg_is_stopped_and_b_never_overlaps(label: &str, leg: Leg, stop: Stop) {
    let _trust = exclusive_trust();
    let root = scratch(label);
    let branch = "maxplayer/7c3a0001";
    let (workdir, oid) = job_workdir(&root, branch);

    let bare = root.join("relay.git");
    git2::Repository::init_bare(&bare).expect("relay bare");
    let gate = RequestGate::new();
    let relay = GitHttpAuthServer::spawn_with(
        &bare,
        "/git/seller/r.git",
        FixtureOptions {
            hold_request_number: Some((leg.held_request_number(), Arc::clone(&gate))),
            ..FixtureOptions::default()
        },
    );
    stage_env(&relay.ca_file(&root));

    let lock: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));
    let released_at: Arc<std::sync::Mutex<Option<Instant>>> = Arc::new(std::sync::Mutex::new(None));

    // Whatever this process already had as children before the delivery starts. Tests in this
    // binary run in one process, and a neighbouring harness thread may hold one of its own; the
    // child under test is identified as the one that APPEARS, not as "the only one there".
    let before: std::collections::HashSet<i32> = direct_children().into_iter().collect();

    // A's budget. For the abort case the budget is long: the stop under test is the abort, and a
    // deadline that could fire first would let this gate pass without ever exercising it.
    let budget = match stop {
        Stop::Timeout => Duration::from_secs(3),
        Stop::TaskAbort => Duration::from_secs(60),
    };
    // The serializer's outer wait is deliberately far longer than the budget in BOTH cases. It is
    // not the control under test: if it fired first, `serialized_bounded_push` would return
    // `TimedOut` from its own arm and this gate would never reach the executor's deadline kill —
    // which is the thing that has to hold. Left at the production shape (`DELIVERY_DRAIN_BOUND`
    // scale) so the stop observed here is the delivery's own.
    let serializer_timeout = Duration::from_secs(120);
    // Hoisted so the assertions can compare against the deadline A would have died at ANYWAY. In
    // the abort case that instant is the gate's whole discriminator: a stop that happens at or
    // after it is the deadline's work, not the abort's.
    let a_deadline = Instant::now() + budget;

    let first = {
        let lock = Arc::clone(&lock);
        let url = relay.repo_url();
        let branch = branch.to_owned();
        let oid = oid.clone();
        let released_at = Arc::clone(&released_at);
        let deadline = a_deadline;
        tokio::spawn(async move {
            let started = Instant::now();
            let outcome = serialized_bounded_push(
                &lock,
                serializer_timeout,
                deadline,
                move |turn: DeliveryTurn| async move {
                    let minter: AuthMinter = Arc::new(|_| Ok("Nostr fixture-token".to_owned()));
                    let _keep = Token { released_at };
                    neutralize_then_push_in_child_off_runtime(
                        shipped_binary(),
                        workdir,
                        url,
                        branch,
                        oid,
                        Some(minter),
                        None,
                        turn,
                    )
                    .await
                },
            )
            .await;
            (outcome, started, Instant::now())
        })
    };

    // Until the fixture has actually parked the leg, A is not in the state this gate is about.
    tokio::task::spawn_blocking({
        let gate = Arc::clone(&gate);
        move || gate.wait_held()
    })
    .await
    .expect("the fixture must park the leg under test");
    let parked_at = Instant::now();

    // A's CHILD, as the operating system sees it: the process that appeared between the baseline
    // above and this leg being parked on the wire. Identified by difference rather than by count,
    // so an unrelated child of this test binary cannot be mistaken for the delivery's.
    let appeared: Vec<i32> = direct_children()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    assert_eq!(
        appeared.len(),
        1,
        "expected exactly one NEW child while the leg is parked, found {appeared:?} (baseline \
         {before:?}): this gate's liveness probe would otherwise be watching the wrong process"
    );
    let a_child = appeared[0];
    assert!(
        pid_exists(a_child),
        "A's child {a_child} was already gone while its leg was still parked on the wire"
    );

    // DELIVERY B: same serializer, same lock, same seat.
    let acquired_at: Arc<std::sync::Mutex<Option<Instant>>> = Arc::new(std::sync::Mutex::new(None));
    let second = serialized_bounded_push(&lock, Duration::from_secs(60), Instant::now() + Duration::from_secs(90), {
        let at = Arc::clone(&acquired_at);
        move |turn| async move {
            at.lock().expect("clock").replace(Instant::now());
            drop(turn);
            Ok::<_, SellerGitError>("b-delivered".to_owned())
        }
    });
    tokio::pin!(second);

    assert!(
        poll_once(second.as_mut()).await.is_pending(),
        "B's first poll returned Ready while A held the seat parked on {}",
        leg.label()
    );

    // The abort is issued once A is demonstrably parked on the wire — not before, or there would be
    // nothing to abort out of.
    let aborted_at = if stop == Stop::TaskAbort {
        first.abort();
        Some(Instant::now())
    } else {
        None
    };

    // THE OBSERVED WINDOW. B is polled until it takes the seat; every poll instant is recorded, and
    // the polls do not stop while A is being killed and reaped.
    let mut samples: Vec<Instant> = Vec::new();
    let mut b_ready_at: Option<Instant> = None;
    let mut b_outcome: Option<Result<String, DeliveryPushErr>> = None;
    // The first instant A's child was observed absent from the process table.
    let mut child_gone_at: Option<Instant> = None;
    // The last instant A's child was observed ALIVE. This is the stamp the overlap check is built
    // on, because presence is positive evidence: a sample that found the child alive proves it was
    // alive at that instant, and starving the observer takes such samples away rather than moving
    // them later.
    let mut child_last_alive_at: Option<Instant> = None;
    // WHEN THE OBSERVER LAST LOOKED, whatever it saw. `child_last_alive_at` records only sightings
    // of a LIVING child, so by itself it cannot tell "the child was gone" from "nobody looked" --
    // and starvation produces the second. Recording every look is what lets the silence below be
    // turned into a failure.
    let mut last_sample_at: Option<Instant> = None;
    // The instant observation starts, recorded BEFORE the first poll so the leading interval is
    // measured like every other one. Without it the gap between "the stop was ordered" and the
    // first sample was the one interval this test never looked at.
    let polling_began = Instant::now();
    let watchdog = Instant::now() + budget + Duration::from_secs(45);
    while Instant::now() < watchdog {
        let at = Instant::now();
        // Checked on EVERY iteration, not merely until the child is first seen absent: a child that
        // is still alive after B takes the seat is exactly the overlap this gate exists to catch,
        // and a check that stopped looking once it saw an absence could never witness it.
        last_sample_at = Some(at);
        if pid_exists(a_child) {
            child_last_alive_at = Some(at);
        } else if child_gone_at.is_none() {
            child_gone_at = Some(at);
        }
        let polled = poll_once(second.as_mut()).await;
        // Sampled AGAIN, after the poll. B stamps its acquisition DURING the poll, so a sample
        // taken only before it can never fall after that stamp: on the last iteration -- the one
        // where B takes the seat and may finish -- the pre-poll sample is earlier than the
        // acquisition by construction, and the single interval this check exists to witness would
        // be invisible. Presence after the acquisition is the whole evidence, so it is looked for
        // on both sides of the poll.
        // Stamped BEFORE the lookup so the recorded instant is never later than the look itself:
        // the claim "this run looked at or after T" must stay conservative.
        let post_at = Instant::now();
        last_sample_at = Some(post_at);
        if pid_exists(a_child) {
            child_last_alive_at = Some(post_at);
        }
        match polled {
            std::task::Poll::Pending => samples.push(at),
            std::task::Poll::Ready(outcome) => {
                b_ready_at = Some(at);
                b_outcome = Some(outcome);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let b_ready_at = b_ready_at.expect("B never took the seat: A's stop did not hand it back");
    assert_eq!(
        b_outcome
            .expect("B's outcome")
            .expect("B must be able to deliver once the seat is free"),
        "b-delivered"
    );

    // The seat was handed back, and the instant it happened is the production type's, not a sleep.
    let released_at = released_at
        .lock()
        .expect("release clock")
        .expect("A's turn was never handed back");
    let acquired_at = acquired_at
        .lock()
        .expect("clock")
        .expect("B never entered its push body");

    // NO OVERLAP, ASKED OF THE OPERATING SYSTEM. B's push body ran only after A's child had left
    // the process table entirely.
    //
    // This is the assertion that does not depend on the executor's own bookkeeping being honest.
    // The turn, the token and the error string are all things the code under test produces; the pid
    // is not. A stop that released the seat while its child was still pushing would satisfy every
    // other assertion in this file and fail here.
    // **THE OVERLAP CHECK, STATED SO A DESCHEDULED OBSERVER CANNOT DECIDE IT.** This assertion used
    // to demand that A's child be OBSERVED ABSENT before B acquired the seat. Absence is observed
    // late under load: the poller is descheduled, the first absent sample lands after B has already
    // started, and the gate reds on a run where nothing overlapped. Presence cannot drift that way
    // — a sample that found the child alive proves it WAS alive then, and a starved observer takes
    // fewer samples rather than later ones. So the overlap is asserted from the evidence that can
    // actually witness it: A's child alive at or after the instant B entered its push body.
    // **COVERAGE BEFORE CONCLUSION: THIS DEGRADES TO FAILURE, NOT TO SILENCE.** The check below
    // fires only on a sighting of a LIVING child, so with no samples at all it would simply pass --
    // and starving the observer is exactly what removes samples. That is fail-open: under the very
    // conditions this assertion exists to survive it would go quiet rather than red, and a later
    // reader could not tell "nothing overlapped" from "nobody looked". So the run must first show
    // it looked at the interval it judges: some sample taken at or after the instant B entered its
    // push body. Absent evidence is not evidence of absence, and here it is a failure.
    let last_sample_at =
        last_sample_at.expect("the observation loop never sampled the process table at all");
    assert!(
        last_sample_at >= acquired_at,
        "no sample of A's child was taken at or after B entered its push body -- the last look was \
         {:?} BEFORE it -- so this run observed nothing about the interval it exists to judge and \
         cannot corroborate exclusion",
        acquired_at.saturating_duration_since(last_sample_at)
    );
    if let Some(alive_at) = child_last_alive_at {
        assert!(
            alive_at < acquired_at,
            "A's child was observed ALIVE {:?} AFTER B entered its push body: two deliveries were \
             live against the same workdir at once",
            alive_at.saturating_duration_since(acquired_at)
        );
    }
    assert!(
        !pid_exists(a_child),
        "A's child {a_child} is still alive after B took the seat"
    );
    // Whether the child was ever SEEN to go is reported below, not required here. Demanding a
    // positive sighting of absence is the scheduling-dependent form this file just moved away
    // from: under load the first absent sample lands late, or never, on a run where nothing
    // overlapped. A child that really did outlive the handover is caught by the assertion above
    // and by the process-table check beside it, neither of which needs that sighting.
    // And the seat's own token agrees with the operating system.
    assert!(
        acquired_at >= released_at,
        "B entered its push body {:?} BEFORE A handed the seat back: the two deliveries overlapped",
        released_at.saturating_duration_since(acquired_at)
    );

    // **THE ABORT ACTUALLY STOPPED IT.** Only meaningful in the abort case, and there it is the
    // assertion that makes the case a test of cancellation rather than of patience. See
    // `abort_stop_bound`.
    if let Some(aborted_at) = aborted_at {
        let stopped_in = acquired_at.saturating_duration_since(aborted_at);
        assert!(
            stopped_in <= abort_stop_bound(),
            "B took the seat {stopped_in:?} after A was aborted, past the {:?} this stop is \
             allowed: a cancellation that is merely ignored until the delivery's own {budget:?} \
             deadline kills it would look exactly like this",
            abort_stop_bound()
        );
        assert!(
            acquired_at < a_deadline,
            "B took the seat {:?} AFTER A's own deadline had already passed, so this run does not \
             show the abort stopping anything — the deadline would have stopped it regardless",
            acquired_at.saturating_duration_since(a_deadline)
        );
    }

    // OBSERVATION WAS REAL, which is a claim about the product: every poll of B taken during A's
    // stop returned Pending, and there was at least one of them.
    assert!(
        !samples.is_empty(),
        "B was never polled Pending during A's stop: that is not observation at all"
    );
    let mut widest = samples[0].saturating_duration_since(polling_began);
    for pair in samples.windows(2) {
        widest = widest.max(pair[1].saturating_duration_since(pair[0]));
    }
    // From the last Pending sample to the instant B was Ready, too: the interesting gap is the last
    // one, and leaving it out would let the loop stop polling exactly when it matters.
    widest = widest.max(b_ready_at.saturating_duration_since(
        *samples.last().expect("at least one sample"),
    ));
    // COVERAGE REPORTING, NOT AN EXCLUSION CLAIM. Read that literally, and do not let this number
    // be cited later as proof that two deliveries did not overlap: it is not one, and it never was.
    // It says how often the observer got to look.
    //
    // It does not say whether anything overlapped, and it never could: the gap grows when the poller is
    // descheduled under load, so a ceiling on it reds for the machine rather than for a defect.
    // Raising that ceiling would be the worse repair — a wider ceiling enlarges the interval in
    // which the seat goes unwatched while still proving nothing, turning a failing oracle into a
    // silent one. The exclusion this file is about is decided above, by stamps the participants
    // record themselves (`acquired_at` against `released_at`) and by the positive alive-samples of
    // A's child. The gap is printed because it tells a reader how strongly THIS run corroborates
    // those checks: a run whose gap is wide is a weakly corroborated run, not a failing one. The
    // exclusion claims of this file are the assertions above; this line is coverage reporting.
    eprintln!(
        "observation coverage: {} Pending samples across {:?} of stop, widest gap between looks \
         {widest:?} (cadence aimed at {MAX_SAMPLE_GAP:?}; reported, not asserted), A's child first \
         seen gone {:?} into the window",
        samples.len(),
        b_ready_at.saturating_duration_since(polling_began),
        child_gone_at.map(|at| at.saturating_duration_since(polling_began)),
    );
    // The observation really does span the stop: it starts while A is parked on the wire and ends
    // after the seat changed hands.
    assert!(
        *samples.first().expect("first sample") >= parked_at
            && *samples.last().expect("last sample") >= released_at.min(b_ready_at) - MAX_SAMPLE_GAP,
        "the polling window did not span A's stop"
    );

    // A's own outcome.
    let (outcome, started, returned) = match stop {
        Stop::TaskAbort => {
            let joined = first.await;
            // CANCELLED, not merely "not Ok". `is_err` is also satisfied by a PANIC inside the
            // delivery task, which is a different defect wearing the same shape: it would end the
            // task, free the seat, and pass this gate while proving nothing about cancellation.
            assert!(
                joined
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.is_cancelled()),
                "the aborted delivery task did not end as CANCELLED ({joined:?}); a task that \
                 returned normally was never aborted, and one that panicked freed the seat by \
                 failing rather than by being cancelled"
            );
            (None, None, None)
        }
        Stop::Timeout => {
            let (outcome, started, returned) = first.await.expect("A's task");
            (Some(outcome), Some(started), Some(returned))
        }
    };
    if let (Some(outcome), Some(started), Some(returned)) = (outcome, started, returned) {
        match outcome {
            Err(DeliveryPushErr::Push(SellerGitError::Cancelled(why))) => {
                assert!(
                    why.contains("was killed") && why.contains("confirmed the exit"),
                    "A must report the kill AND the confirmed exit: {why}"
                );
            }
            other => panic!("A must be killed at its deadline, not awaited: {other:?}"),
        }
        let held = returned.saturating_duration_since(started);
        assert!(
            held >= budget && held < budget + Duration::from_secs(10),
            "A held the seat for {held:?}, outside its budget {budget:?} + reap bound"
        );
        assert!(
            released_at <= returned,
            "A returned before its own turn was handed back"
        );
    }

    // Nothing was delivered by the stopped delivery.
    assert!(
        git2::Repository::open_bare(&bare)
            .expect("open bare")
            .find_reference(&format!("refs/heads/{branch}"))
            .is_err(),
        "the remote ref moved for a delivery stopped on {}",
        leg.label()
    );
    assert_eq!(
        maxplayer_core::delivery_executor::unconfirmed_children(),
        0,
        "an unconfirmed child was left behind, so the seat was handed on without custody"
    );

    gate.release();
}

/// T3a. Pack upload parked on the wire, A stopped by its own TIMEOUT.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_pack_upload_stopped_by_timeout_never_overlaps_a_second_delivery_polled_through_the_reap()
{
    a_parked_leg_is_stopped_and_b_never_overlaps("post-timeout", Leg::PackUpload, Stop::Timeout)
        .await;
}

/// T3b. Advertisement parked on the wire, A stopped by its own TIMEOUT.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn an_advertisement_stopped_by_timeout_never_overlaps_a_second_delivery_polled_through_the_reap()
 {
    a_parked_leg_is_stopped_and_b_never_overlaps("get-timeout", Leg::Advertisement, Stop::Timeout)
        .await;
}

/// T3c. Pack upload parked on the wire, A's whole delivery future ABORTED.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_pack_upload_whose_task_is_aborted_never_overlaps_a_second_delivery_polled_through_the_reap()
 {
    a_parked_leg_is_stopped_and_b_never_overlaps("post-abort", Leg::PackUpload, Stop::TaskAbort)
        .await;
}

/// T3d. Advertisement parked on the wire, A's whole delivery future ABORTED.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn an_advertisement_whose_task_is_aborted_never_overlaps_a_second_delivery_polled_through_the_reap()
 {
    a_parked_leg_is_stopped_and_b_never_overlaps("get-abort", Leg::Advertisement, Stop::TaskAbort)
        .await;
}
