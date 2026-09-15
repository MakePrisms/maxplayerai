//! F1 FINITE-STOP, proved on the PRODUCTION path — the seller node's own delivery push.
//!
//! `delivery_executor_platform.rs` proves the mechanism (a child that refuses `SIGTERM` is killed
//! and its exit confirmed). This file proves the mechanism is WIRED: that
//! `seller_git::neutralize_then_push_in_child_off_runtime` — the call the delivery arm in
//! `seller_node/run.rs` makes — stops a local phase that will not stop by itself, hands this seat's
//! turn back only after an exit the kernel confirmed, and keeps the seller key on the parent's side
//! of the pipe throughout.
//!
//! The children here are fixtures, not the shipped binary: `maxplayer/tests/delivery_push_child_binary.rs`
//! gates the real artifact. A fixture is what makes the REFUSING child — the case that matters — and
//! what lets the parent's half be driven through outcomes a cooperating child never produces.

#![cfg(feature = "git-delivery")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::{Exclusion, ExecutorError, REAP_BOUND};
use maxplayer_core::delivery_turn::delivery_turn;
use maxplayer_core::git_transport::{AuthMinter, AuthorityCheck};
use maxplayer_core::seller_git::{
    neutralize_then_push_in_child_off_runtime, turn_after_child_push, SellerGitError,
};

/// True while a pid still exists. Signal 0 performs the existence check and delivers nothing.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "maxplayer-push-child-{label}-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Write an executable `/bin/sh` fixture and return its path.
fn fixture(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("child.sh");
    let mut file = std::fs::File::create(&path).expect("create fixture");
    write!(file, "#!/bin/sh\n{body}").expect("write fixture");
    drop(file);
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

const HELLO: &str = r#"printf '{"t":"Hello","version":1,"argv":[],"env":{}}\n'"#;

/// The object every delivery in this file is gated on. A child may report THIS oid and no other.
const GATED_OID: &str = "0123456789012345678901234567890123456789";

/// The exclusion token the turn carries. In production it is the delivery lock's owned guard; here
/// it is a token that RECORDS its own release, so "the turn was handed back" is an observation
/// rather than an inference from a return value.
struct Token(Arc<AtomicBool>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// F1, on the production path: a delivery whose local phase refuses every polite request to stop is
/// stopped anyway, at its own deadline, and the turn is handed back only after the kernel confirmed
/// the child had exited.
///
/// The fixture is the shape libgit2's delta search puts the seat in: it ignores `SIGTERM`, it never
/// returns to any control flow that could check a flag, and it never speaks again after hello. In
/// process, that delivery holds this seat's one delivery remote until it finishes on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_phase_that_refuses_to_stop_is_ended_at_the_deadline_and_its_exit_is_confirmed() {
    let dir = scratch("refuses");
    let pidfile = dir.join("child.pid");
    let program = fixture(
        &dir,
        &format!(
            "trap '' TERM\necho $$ > {}\n{HELLO}\nwhile :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    );

    // The real budget is DELIVERY_PUSH_TIMEOUT (150s); this is the same arithmetic on a scale a gate
    // can run. What is asserted below is the SHAPE of the bound — deadline + at most REAP_BOUND —
    // which is what makes the production number a claim about mechanism rather than about luck.
    let budget = Duration::from_millis(1_500);
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), Instant::now() + budget);

    let started = Instant::now();
    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        "https://relay.example.invalid/seller.git".to_owned(),
        "delivery/job".to_owned(),
        GATED_OID.to_owned(),
        None,
        None,
        turn,
    )
    .await;
    let elapsed = started.elapsed();

    let error = match outcome {
        Err(SellerGitError::Cancelled(error)) => error,
        other => panic!("a child that never finishes must be killed, not awaited: {other:?}"),
    };
    assert!(
        error.contains("was killed") && error.contains("confirmed the exit"),
        "the refusal must say the child was killed AND that its exit was confirmed: {error}"
    );

    // THE BOUND, MEASURED. Not "it returned eventually": it waited its whole budget (so the kill is
    // the deadline's doing, not an early giveup) and returned inside budget + REAP_BOUND.
    assert!(
        elapsed >= budget,
        "returned before the deadline it was given: {elapsed:?} < {budget:?}"
    );
    assert!(
        elapsed < budget + REAP_BOUND,
        "the delivery turn was held for {elapsed:?}, past its own bound of {:?}",
        budget + REAP_BOUND
    );

    // AND THE CHILD IS ACTUALLY GONE. A bound on the parent's patience is not a bound on the work;
    // this is the difference, and it is the one the whole change exists for.
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("the fixture child must have recorded its pid")
        .trim()
        .parse()
        .expect("pid");
    assert!(
        !alive(pid),
        "pid {pid} still exists after the delivery returned: the work outlived its turn"
    );

    // The turn comes back — but only now, and only because the work stopped.
    assert!(control.work_ended(), "the work must be recorded as ended");
    control.end();
    assert!(
        !control.holds_ownership() && released.load(Ordering::SeqCst),
        "the exclusion token was not handed back after a confirmed exit"
    );
}

/// The same path on the ordinary outcome: a child that finishes returns its oid, is reaped anyway,
/// and hands the turn back. Without this, the test above would also pass on an executor that killed
/// every push.
///
/// The fixture reports THE GATED OID, because that is now the only oid a child is allowed to
/// report: a `Done` naming anything else is a protocol fault, gated just below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_that_finishes_returns_its_oid_and_hands_the_turn_back() {
    let dir = scratch("finishes");
    let program = fixture(
        &dir,
        &format!("{HELLO}\nprintf '{{\"t\":\"Done\",\"oid\":\"{GATED_OID}\",\"error\":null}}\\n'\n"),
    );
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(
        Token(Arc::clone(&released)),
        Instant::now() + Duration::from_secs(10),
    );

    let started = Instant::now();
    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        "https://relay.example.invalid/seller.git".to_owned(),
        "delivery/job".to_owned(),
        GATED_OID.to_owned(),
        None,
        None,
        turn,
    )
    .await;

    assert_eq!(outcome.expect("the push reported an oid"), GATED_OID);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a finished push must not wait out the deadline"
    );
    control.end();
    assert!(
        released.load(Ordering::SeqCst),
        "a completed push must hand the turn back"
    );
}

/// A delivery revoked before dispatch never gets a child AT ALL. The child is the whole local phase,
/// so this is the cheapest refusal in the system and the one that must not be skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_delivery_never_spawns_a_child() {
    let dir = scratch("revoked");
    let pidfile = dir.join("child.pid");
    let program = fixture(
        &dir,
        &format!(
            "echo $$ > {}\n{HELLO}\nwhile :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    );
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(
        Token(Arc::clone(&released)),
        Instant::now() + Duration::from_secs(10),
    );
    // Revoked BEFORE the work is dispatched: the queue-admission gate, not a late check.
    control.end();

    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        "https://relay.example.invalid/seller.git".to_owned(),
        "delivery/job".to_owned(),
        GATED_OID.to_owned(),
        None,
        None,
        turn,
    )
    .await;

    assert!(
        matches!(outcome, Err(SellerGitError::Cancelled(_))),
        "a revoked delivery must be refused: {outcome:?}"
    );
    assert!(
        !pidfile.exists(),
        "a revoked delivery spawned a child anyway"
    );
    assert!(
        released.load(Ordering::SeqCst),
        "a delivery that never ran must hand its turn straight back"
    );
}

/// The credential property, proved from the child's side of the pipe: the child holds no key and no
/// token, it ASKS, and what it receives is exactly what the parent's minter produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_child_receives_a_minted_header_it_never_held_and_the_parent_minted_it() {
    let dir = scratch("mint");
    let answer = dir.join("answer.json");
    let program = fixture(
        &dir,
        &format!(
            "{HELLO}\nIFS= read -r _request\nprintf '{{\"t\":\"Mint\",\"destination\":\"https://relay.example.invalid/seller.git\"}}\\n'\nIFS= read -r line\nprintf '%s' \"$line\" > {}\nprintf '{{\"t\":\"Done\",\"oid\":null,\"error\":\"reported\"}}\\n'\n",
            answer.display()
        ),
    );
    let minted = Arc::new(AtomicUsize::new(0));
    let mint: AuthMinter = {
        let minted = Arc::clone(&minted);
        Arc::new(move |destination: &str| {
            assert_eq!(
                destination, "https://relay.example.invalid/seller.git",
                "the minter is asked for the destination the child named"
            );
            minted.fetch_add(1, Ordering::SeqCst);
            Ok("Nostr SENTINEL-HEADER-VALUE".to_owned())
        })
    };
    let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(10));

    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        "https://relay.example.invalid/seller.git".to_owned(),
        "delivery/job".to_owned(),
        GATED_OID.to_owned(),
        Some(mint),
        None,
        turn,
    )
    .await;
    control.end();

    assert!(
        matches!(&outcome, Err(SellerGitError::Transport(error)) if error.contains("reported")),
        "the fixture reports rather than pushes: {outcome:?}"
    );
    assert_eq!(
        minted.load(Ordering::SeqCst),
        1,
        "the parent minted exactly once, for the one leg the child asked about"
    );
    let handed = std::fs::read_to_string(&answer).expect("the child recorded the parent's answer");
    assert!(
        handed.contains("SENTINEL-HEADER-VALUE"),
        "the child did not receive the header the parent minted: {handed}"
    );
}

/// The post-mint gate. A mint is a call into the signer actor and it can BLOCK; authority can end
/// while it does. The header is therefore offered to the authority AFTER it exists and BEFORE it
/// crosses the pipe — so a token for a leg this delivery no longer owns never reaches the child at
/// all, let alone the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authority_that_ends_during_the_mint_keeps_the_token_on_this_side_of_the_pipe() {
    let dir = scratch("late-revoke");
    let answer = dir.join("answer.json");
    let program = fixture(
        &dir,
        &format!(
            "{HELLO}\nIFS= read -r _request\nprintf '{{\"t\":\"Mint\",\"destination\":\"https://relay.example.invalid/seller.git\"}}\\n'\nIFS= read -r line\nprintf '%s' \"$line\" > {}\nprintf '{{\"t\":\"Done\",\"oid\":null,\"error\":\"reported\"}}\\n'\n",
            answer.display()
        ),
    );
    // Authority ends WHEN THE MINT RETURNS, and the trigger says exactly that.
    //
    // It used to be a call count: answer Ok twice, then Err. That counted on the parent asking
    // exactly twice before the mint, which stopped being true when the parent began re-asking its
    // authority on a cancellation poll every CANCELLATION_POLL — a tick or two of that, on a loaded
    // machine, spent the two Oks before the child ever requested a mint, and the delivery was
    // killed before reaching the moment under test. A count of calls was never the condition; the
    // mint returning was.
    //
    // So: the minter sets `has_minted`, and authority ends the FIRST time it is asked after that.
    // That is the post-mint check, the one this gate is about. Later ticks answer Ok again, which
    // keeps this test to its own question — whether a token minted under authority that has since
    // ended crosses the pipe — and leaves "what the parent does about a revocation" to the gates
    // that exist for it, instead of racing a kill against the child's write here.
    let has_minted = Arc::new(AtomicBool::new(false));
    let refused_once = Arc::new(AtomicBool::new(false));
    let authority: AuthorityCheck = {
        let has_minted = Arc::clone(&has_minted);
        let refused_once = Arc::clone(&refused_once);
        Arc::new(move || {
            if has_minted.load(Ordering::SeqCst) && !refused_once.swap(true, Ordering::SeqCst) {
                Err("this delivery was cancelled".to_owned())
            } else {
                Ok(())
            }
        })
    };
    let mint: AuthMinter = {
        let has_minted = Arc::clone(&has_minted);
        Arc::new(move |_: &str| {
            has_minted.store(true, Ordering::SeqCst);
            Ok("Nostr SENTINEL-HEADER-VALUE".to_owned())
        })
    };
    let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(10));

    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        "https://relay.example.invalid/seller.git".to_owned(),
        "delivery/job".to_owned(),
        GATED_OID.to_owned(),
        Some(mint),
        Some(authority),
        turn,
    )
    .await;
    control.end();
    assert!(outcome.is_err(), "the push cannot succeed: {outcome:?}");

    let handed = std::fs::read_to_string(&answer).expect("the child recorded the parent's answer");
    assert!(
        !handed.contains("SENTINEL-HEADER-VALUE"),
        "a token minted for a revoked delivery crossed the pipe: {handed}"
    );
    assert!(
        handed.contains("refused") && handed.contains("cancelled"),
        "the child must be told the leg was refused, and why: {handed}"
    );
}

/// A remote that takes no authorization cannot obtain one by asking. Two refusals stand behind this,
/// and either alone would do: the parent's proxy has no minter to call, and the executor treats the
/// question itself as a protocol violation and kills the child for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unauthenticated_remote_cannot_obtain_a_token_by_asking() {
    let dir = scratch("unauth");
    let pidfile = dir.join("child.pid");
    let program = fixture(
        &dir,
        &format!(
            "echo $$ > {}\n{HELLO}\nIFS= read -r _request\nprintf '{{\"t\":\"Mint\",\"destination\":\"https://relay.example.invalid/seller.git\"}}\\n'\nwhile :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    );
    let (control, turn) = delivery_turn((), Instant::now() + Duration::from_secs(10));

    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        "https://public.example.invalid/seller.git".to_owned(),
        "delivery/job".to_owned(),
        GATED_OID.to_owned(),
        None,
        None,
        turn,
    )
    .await;
    control.end();

    match &outcome {
        Err(SellerGitError::Transport(error)) => assert!(
            error.contains("unauthenticated"),
            "the refusal must name the reason: {error}"
        ),
        other => {
            panic!("asking for a token on an unauthenticated remote must be refused: {other:?}")
        }
    }
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("pidfile")
        .trim()
        .parse()
        .expect("pid");
    assert!(
        !alive(pid),
        "a child killed for a protocol violation must be gone, not merely signalled"
    );
}

/// The fail-closed rule itself, as a rule.
///
/// **What this gate can and cannot reach.** An unreapable child is a child whose thread is in
/// uninterruptible kernel sleep — a stalled NFS/FUSE mount or a disk that stopped answering. There
/// is no user-space way to manufacture one, so `ExecutorError::Unreaped` cannot be produced by a
/// fixture and this rule is gated at the decision rather than end to end. The decision is the whole
/// of the policy: `neutralize_then_push_in_child_off_runtime` has exactly ONE release site and it
/// asks this function, so an outcome that maps to `Retain` is an outcome that keeps the turn.
/// F2(a), on the production path: cancellation while the SIGNER'S REPLY IS HELD.
///
/// Round 1 parked a test minter before it called the signer and called that a signer interleaving.
/// This holds the minter itself — the call the parent makes into the signer actor — open forever,
/// which is what a full signer queue or a held reply looks like from here. The mint runs OFF the
/// parent's drive thread precisely so that this cannot happen: a parent blocked inside the signer
/// is a parent that never issues the kill, and that is the wedge this seat must not have.
///
/// The assertion is that the deadline still lands: the child is killed, its exit confirmed, and the
/// turn handed back, while the mint is still outstanding.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signer_whose_reply_never_comes_cannot_stop_the_deadline_from_landing() {
    let dir = scratch("heldsigner");
    let pidfile = dir.join("child.pid");
    // The child says hello, asks to mint, and then waits for an answer that will never arrive. It
    // ignores TERM, so only the kill can end it.
    let program = fixture(
        &dir,
        &format!(
            "trap '' TERM\necho $$ > {}\n{HELLO}\nprintf '{{\"t\":\"Mint\",\"destination\":\"https://relay.example.invalid/seller.git\"}}\\n'\nwhile :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    );

    let asked = Arc::new(AtomicBool::new(false));
    let minter: AuthMinter = {
        let asked = Arc::clone(&asked);
        Arc::new(move |_destination: &str| {
            asked.store(true, Ordering::SeqCst);
            // The signer's reply is HELD. Not slow: held.
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };

    let budget = Duration::from_millis(1_500);
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), Instant::now() + budget);

    let started = Instant::now();
    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        "https://relay.example.invalid/seller.git".to_owned(),
        "delivery/job".to_owned(),
        GATED_OID.to_owned(),
        Some(minter),
        None,
        turn,
    )
    .await;
    let elapsed = started.elapsed();

    let error = match outcome {
        Err(SellerGitError::Cancelled(error)) => error,
        other => panic!("a held signer reply must not outlive the deadline: {other:?}"),
    };
    assert!(
        error.contains("was killed") && error.contains("confirmed the exit"),
        "the refusal must say the child was killed AND that its exit was confirmed: {error}"
    );
    assert!(
        asked.load(Ordering::SeqCst),
        "the gate is vacuous unless the child actually reached the mint request"
    );
    assert!(
        elapsed >= budget && elapsed < budget + REAP_BOUND,
        "the deadline must land while the signer is still holding its reply: {elapsed:?}"
    );
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("pidfile")
        .trim()
        .parse()
        .expect("pid");
    assert!(!alive(pid), "the killed child must be gone");

    // The work is recorded as ENDED even though a thread is still sitting in that signer call. This
    // is the assertion that the abandoned mint holds nothing: were it still holding this delivery's
    // lifetime, the work would never be ended and this seat would stay shut with no rule able to
    // see why.
    assert!(
        control.work_ended(),
        "an abandoned mint must not keep this delivery's work alive"
    );
    control.end();
    assert!(
        !control.holds_ownership() && released.load(Ordering::SeqCst),
        "the turn must come back even though the signer never answered"
    );
}

#[test]
fn the_turn_is_released_on_a_confirmed_exit_and_on_nothing_else() {
    // The one outcome that retains: a kill was issued and no exit was observed.
    assert_eq!(
        turn_after_child_push(&Err(ExecutorError::Unreaped { waited: REAP_BOUND })),
        Exclusion::Retain,
        "an unconfirmed exit must keep this seat's turn: a kill is not a stop"
    );
    // The second retaining outcome: the child WAS reaped, but the write end of its pipe was still
    // held afterwards, so something that inherited it escaped the process group we killed. The
    // process we named is gone; the work is not demonstrably over, so the seat stays shut.
    assert_eq!(
        turn_after_child_push(&Err(ExecutorError::CleanupUnbounded { waited: REAP_BOUND })),
        Exclusion::Retain,
        "a reaped child whose pipe outlived it must keep this seat's turn"
    );
    // A deadline breach releases ONLY because the executor reaped before reporting it — the reap
    // duration it carries is the evidence. A breach whose reap did not complete is Unreaped above.
    assert_eq!(
        turn_after_child_push(&Err(ExecutorError::Killed {
            after: Duration::from_millis(1),
            reap: Duration::from_millis(2)
        })),
        Exclusion::Release
    );
    assert_eq!(
        turn_after_child_push(&Ok("abc123".to_owned())),
        Exclusion::Release
    );
    assert_eq!(
        turn_after_child_push(&Err(ExecutorError::Spawn("no child".to_owned()))),
        Exclusion::Release,
        "a child that never started holds nothing"
    );
    // The third retaining outcome, and the one this rule used to get WRONG. `waitpid` itself can
    // fail; that is an UNKNOWN exit, and it used to be reported as a protocol fault — which this
    // rule releases on. An unknown exit wearing a releasable name is a seat handed on while work
    // may still be running, so it has its own outcome and it retains.
    assert_eq!(
        turn_after_child_push(&Err(ExecutorError::WaitFailed {
            why: "No child processes".to_owned()
        })),
        Exclusion::Retain,
        "an exit the kernel would not report is not an exit we may release on"
    );
    // `Protocol` releases, and may only ever be constructed where the exit WAS confirmed: every
    // arm of `drive` reaps before it returns one, and `run_push_in_child` re-checks that the child
    // is reaped before any outcome leaves it.
    assert_eq!(
        turn_after_child_push(&Err(ExecutorError::Protocol("out of turn".to_owned()))),
        Exclusion::Release
    );
    assert_eq!(
        turn_after_child_push(&Err(ExecutorError::Push("remote refused".to_owned()))),
        Exclusion::Release
    );
}

/// THE OTHER SIDE OF THE SAME RULE: a child that reports a DIFFERENT oid is a protocol fault.
///
/// The gate above now has its fixture report the gated oid, which is correct and also removes the
/// only place this refusal was being exercised — by accident, through a fixture that predated the
/// check. Exercise it on purpose instead.
///
/// Why it matters: the oid is the whole delivery. The parent gated an object, and what the child
/// says it shipped is the only claim that ever comes back. A child that names a different object
/// and is believed turns "we delivered the wrong thing" into a success, and a caller that trusts
/// the returned oid then records a delivery of something nobody gated. So the parent compares, and
/// a mismatch ends the delivery rather than being reported as an oid.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_that_reports_an_oid_nobody_gated_is_a_protocol_fault() {
    let dir = scratch("wrong-oid");
    let program = fixture(
        &dir,
        &format!("{HELLO}\nprintf '{{\"t\":\"Done\",\"oid\":\"abc123\",\"error\":null}}\\n'\n"),
    );
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(
        Token(Arc::clone(&released)),
        Instant::now() + Duration::from_secs(10),
    );

    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        dir.join("workdir"),
        "https://relay.example.invalid/seller.git".to_owned(),
        "delivery/job".to_owned(),
        GATED_OID.to_owned(),
        None,
        None,
        turn,
    )
    .await;

    let error = outcome.expect_err("a child that reported an oid nobody gated must not succeed");
    let text = error.to_string();
    assert!(
        text.contains("abc123") && text.contains(GATED_OID),
        "the refusal must name both the oid the child claimed and the one this delivery gated: \
         {text}"
    );
    // And the seat still comes back: the child broke the protocol, was killed, and its exit was
    // confirmed — a stop, not an unknown.
    control.end();
    assert!(
        released.load(Ordering::SeqCst),
        "a protocol fault whose child was reaped must still hand the turn back"
    );
}
