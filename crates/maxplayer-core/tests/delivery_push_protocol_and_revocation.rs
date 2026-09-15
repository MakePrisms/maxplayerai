//! What the parent does with a child that speaks out of turn, and what it does with an owner that
//! goes away while the child is working.
//!
//! Two families, both failed in round 3 for the same reason: the parent accepted more than its
//! protocol describes, and the only thing that acted on a revocation was the deadline.
//!
//! - **Protocol.** A second hello was accepted, a result was accepted from a child that had never
//!   said hello, the object the child reported was never compared with the object this delivery was
//!   told to deliver, and there was no limit on how many authorizations one child could ask for.
//!   Each of those is a claim the module made about its protocol that the protocol did not enforce.
//! - **Revocation in transit.** The parent answers a child's authority check with the truth at the
//!   moment it writes the answer, and the child transmits some time after reading it. Nothing on the
//!   parent's side looked again until the clock ran out, so a delivery revoked with two minutes left
//!   kept running for two minutes. The window is not closed \u2014 it cannot be, from this side of a
//!   pipe \u2014 but it is now bounded by [`CANCELLATION_POLL`] instead of by the deadline, and these
//!   gates measure that.

#![cfg(feature = "git-delivery")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maxplayer_core::delivery_executor::{CANCELLATION_POLL, MAX_MINT_REQUESTS, REAP_BOUND};
use maxplayer_core::delivery_turn::delivery_turn;
use maxplayer_core::git_transport::{AuthMinter, AuthorityCheck};
use maxplayer_core::seller_git::{neutralize_then_push_in_child_off_runtime, SellerGitError};

const OID: &str = "0123456789012345678901234567890123456789";
const OTHER_OID: &str = "fedcba9876543210fedcba9876543210fedcba98";
const REMOTE: &str = "https://relay.example.invalid/seller.git";
const HELLO: &str = r#"printf '{"t":"Hello","version":1,"argv":[],"env":{}}\n'"#;

/// A deadline nothing in this file is allowed to reach. Every gate here must end for its own
/// reason, and an outcome that took this long is an outcome the deadline produced.
const UNREACHABLE: Duration = Duration::from_secs(30);

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mp-proto-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

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

/// True while a pid still exists. Signal 0 performs the existence check and delivers nothing.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn pid_of(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .expect("the child must have recorded its pid")
        .trim()
        .parse()
        .expect("pid")
}

struct Token(Arc<AtomicBool>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Run one delivery against `program` with an optional minter and authority, and report what came
/// back, how long it took, and whether the seat was handed on.
struct Run {
    outcome: Result<String, SellerGitError>,
    took: Duration,
    released: bool,
    work_ended: bool,
}

async fn deliver(
    program: PathBuf,
    workdir: PathBuf,
    mint: Option<AuthMinter>,
    authority: Option<AuthorityCheck>,
    budget: Duration,
) -> Run {
    let released = Arc::new(AtomicBool::new(false));
    let (control, turn) = delivery_turn(Token(Arc::clone(&released)), Instant::now() + budget);
    let started = Instant::now();
    let outcome = neutralize_then_push_in_child_off_runtime(
        program,
        workdir,
        REMOTE.to_owned(),
        "delivery/job".to_owned(),
        OID.to_owned(),
        mint,
        authority,
        turn,
    )
    .await;
    let took = started.elapsed();
    let work_ended = control.work_ended();
    control.end();
    Run {
        outcome,
        took,
        released: released.load(Ordering::SeqCst),
        work_ended,
    }
}

fn message(outcome: &Result<String, SellerGitError>) -> String {
    match outcome {
        Ok(oid) => panic!("expected a refusal, got a delivered oid: {oid}"),
        Err(error) => error.to_string(),
    }
}

/// THE REVOCATION FENCE, measured. The owner goes away while the child is working; the parent must
/// act on that within its poll interval rather than at the deadline, and the child must be gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_while_the_child_works_stops_it_long_before_the_deadline() {
    let dir = scratch("revoke-mid-work");
    let pidfile = dir.join("child.pid");
    // Says hello, takes the request, and then does what the delta search does: nothing this parent
    // can interrupt by asking.
    let program = fixture(
        &dir,
        &format!(
            "echo $$ > {}\n{HELLO}\nIFS= read -r _request\nwhile :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    );

    let live = Arc::new(AtomicBool::new(true));
    let authority: AuthorityCheck = {
        let live = Arc::clone(&live);
        Arc::new(move || {
            if live.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("the owner of this delivery went away".to_owned())
            }
        })
    };

    // Revoked once the child is demonstrably working, not before it starts.
    let revoke_at = {
        let live = Arc::clone(&live);
        let pidfile = pidfile.clone();
        tokio::spawn(async move {
            loop {
                if pidfile.exists() && alive(pid_of(&pidfile)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            let at = Instant::now();
            live.store(false, Ordering::SeqCst);
            at
        })
    };

    let run = deliver(program, dir.join("workdir"), None, Some(authority), UNREACHABLE).await;
    let revoked_at = revoke_at.await.expect("revoker");
    let reacted_in = Instant::now().saturating_duration_since(revoked_at);

    assert!(
        matches!(run.outcome, Err(SellerGitError::Cancelled(_))),
        "a revoked delivery must come back cancelled: {}",
        message(&run.outcome)
    );
    assert!(
        message(&run.outcome).contains("revoked"),
        "a revocation must not be reported as a deadline breach: {}",
        message(&run.outcome)
    );
    // THE BOUND. Nothing about the deadline ended this delivery: it had most of 30 seconds left.
    assert!(
        reacted_in < CANCELLATION_POLL + REAP_BOUND + Duration::from_secs(2),
        "the parent took {reacted_in:?} to act on a revocation it polls for every \
         {CANCELLATION_POLL:?}"
    );
    assert!(
        run.took < UNREACHABLE / 2,
        "this delivery ran to its deadline instead of stopping when it was revoked: {:?}",
        run.took
    );
    // And the child is GONE, not merely told to stop.
    let pid = pid_of(&pidfile);
    assert!(
        !alive(pid),
        "the revoked delivery's child {pid} is still running"
    );
    assert!(
        run.work_ended && run.released,
        "a revocation whose child was reaped must hand the seat on"
    );
}

/// A handshake happens once. A second hello used to be accepted and the delivery carried on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_that_says_hello_twice_is_stopped() {
    let dir = scratch("double-hello");
    let pidfile = dir.join("child.pid");
    let program = fixture(
        &dir,
        &format!(
            "echo $$ > {}\n{HELLO}\n{HELLO}\nwhile :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    );

    let run = deliver(program, dir.join("workdir"), None, None, UNREACHABLE).await;

    assert!(
        message(&run.outcome).contains("said hello twice"),
        "a second hello must end the delivery: {}",
        message(&run.outcome)
    );
    assert!(run.took < UNREACHABLE / 2, "this ran to the deadline");
    assert!(
        !alive(pid_of(&pidfile)),
        "the child that broke the protocol is still running"
    );
}

/// A result from a child that never introduced itself is not this delivery's result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_that_reports_a_result_before_saying_hello_is_refused() {
    let dir = scratch("done-first");
    let program = fixture(
        &dir,
        &format!("printf '{{\"t\":\"Done\",\"oid\":\"{OID}\",\"error\":null}}\\n'\nsleep 5\n"),
    );

    let run = deliver(program, dir.join("workdir"), None, None, UNREACHABLE).await;

    assert!(
        message(&run.outcome).contains("before saying hello"),
        "a result before the handshake must be refused, not returned as a delivery: {}",
        message(&run.outcome)
    );
}

/// The parent held the object it asked for the whole time and never compared it with the one the
/// child reported. A delivery that names a different object did not deliver this job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_that_reports_an_object_this_delivery_never_asked_for_is_refused() {
    let dir = scratch("wrong-oid");
    let program = fixture(
        &dir,
        &format!(
            "{HELLO}\nIFS= read -r _request\nprintf '{{\"t\":\"Done\",\"oid\":\"{OTHER_OID}\",\"error\":null}}\\n'\n"
        ),
    );

    let run = deliver(program, dir.join("workdir"), None, None, UNREACHABLE).await;

    let why = message(&run.outcome);
    assert!(
        why.contains(OTHER_OID) && why.contains(OID),
        "a result naming another object must be refused and both objects named: {why}"
    );
}

/// The count that makes the token claim a claim about something. A child that keeps asking is
/// stopped at the cap, and the cap is what the minter was actually called.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_that_keeps_asking_for_authorizations_is_stopped_at_the_cap() {
    let dir = scratch("mint-storm");
    let pidfile = dir.join("child.pid");
    let program = fixture(
        &dir,
        &format!(
            "echo $$ > {}\n{HELLO}\nIFS= read -r _request\ni=0\nwhile [ $i -lt 40 ]; do printf '{{\"t\":\"Mint\",\"destination\":\"{REMOTE}\"}}\\n'; IFS= read -r _reply; i=$((i+1)); done\nsleep 5\n",
            pidfile.display()
        ),
    );

    let minted = Arc::new(AtomicUsize::new(0));
    let mint: AuthMinter = {
        let minted = Arc::clone(&minted);
        Arc::new(move |_destination: &str| {
            minted.fetch_add(1, Ordering::SeqCst);
            Ok("Nostr fixture-token".to_owned())
        })
    };

    let run = deliver(
        program,
        dir.join("workdir"),
        Some(mint),
        None,
        UNREACHABLE,
    )
    .await;

    assert!(
        message(&run.outcome).contains("authorizations"),
        "a child asking without limit must be stopped: {}",
        message(&run.outcome)
    );
    // THE BEHAVIOUR, not the message: the signer was asked exactly as many times as the cap allows,
    // and the delivery ended on the ask after it.
    assert_eq!(
        minted.load(Ordering::SeqCst) as u32,
        MAX_MINT_REQUESTS,
        "the parent minted a different number of tokens than its own cap permits"
    );
    assert!(
        !alive(pid_of(&pidfile)),
        "the child that exceeded the cap is still running"
    );
}

/// REVOCATION WHILE THE CHILD IS LOUD.
///
/// The bound the module states is a property of the parent's clock: while a child runs, the owner is
/// re-asked at least every [`CANCELLATION_POLL`]. It was not. The only code that acted on a
/// revocation was the arm that runs when NO frame arrived within the slice, so the interval was
/// really "every poll, as long as the child stays quiet". This child is the opposite of quiet: it
/// asks whether it still holds its turn in a tight loop, faster than the poll, and reads every
/// answer. Nothing ever times out, so under the old shape nothing ever re-asked and the delivery ran
/// to its deadline no matter when authority ended — which is precisely the child the fence exists
/// for, since a real one checks before every leg of its transmission.
///
/// Two separate facts are asserted, because "it stopped" is not the whole claim: the child WAS being
/// answered (the traffic was live, not a child parked on a read), and the delivery ended for
/// revocation within the poll interval rather than at the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_during_a_flood_of_authority_checks_is_acted_on_within_the_poll() {
    let dir = scratch("revoke-busy-checks");
    let pidfile = dir.join("child.pid");
    let answers = dir.join("answers");
    // Asks, reads the answer, records it, repeats — with no pause. Every iteration is a frame the
    // parent must serve, so the frame wait never expires.
    let program = fixture(
        &dir,
        &format!(
            "echo $$ > {}\n{HELLO}\nIFS= read -r _request\nwhile :; do \
             printf '{{\"t\":\"Check\",\"phase\":\"send-pack\"}}\\n'; \
             IFS= read -r reply || exit 0; printf '%s\\n' \"$reply\" >> {}; done\n",
            pidfile.display(),
            answers.display()
        ),
    );

    let live = Arc::new(AtomicBool::new(true));
    let asked = Arc::new(AtomicUsize::new(0));
    let authority: AuthorityCheck = {
        let live = Arc::clone(&live);
        let asked = Arc::clone(&asked);
        Arc::new(move || {
            asked.fetch_add(1, Ordering::SeqCst);
            if live.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("the owner of this delivery went away".to_owned())
            }
        })
    };

    let revoke_at = {
        let live = Arc::clone(&live);
        let answers = answers.clone();
        tokio::spawn(async move {
            // Wait until the traffic is demonstrably flowing: several answers already written back
            // by the child, so the revocation lands in the middle of the flood and not before it.
            loop {
                let served = std::fs::read_to_string(&answers)
                    .map(|text| text.lines().count())
                    .unwrap_or(0);
                if served > 20 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let at = Instant::now();
            live.store(false, Ordering::SeqCst);
            at
        })
    };

    let run = deliver(program, dir.join("workdir"), None, Some(authority), UNREACHABLE).await;
    let revoked_at = revoke_at.await.expect("revoker");
    let reacted_in = Instant::now().saturating_duration_since(revoked_at);

    let why = message(&run.outcome);
    assert!(
        matches!(run.outcome, Err(SellerGitError::Cancelled(_))) && why.contains("revoked"),
        "a delivery revoked under load must come back revoked, not as a deadline breach: {why}"
    );
    assert!(
        reacted_in < CANCELLATION_POLL + REAP_BOUND + Duration::from_secs(2),
        "the parent took {reacted_in:?} to act on a revocation while the child was busy; a poll          interval that only holds for a QUIET child is not the bound this module claims"
    );
    assert!(
        run.took < UNREACHABLE / 2,
        "this delivery ran to its deadline instead of stopping when it was revoked: {:?}",
        run.took
    );
    // The traffic was real: the child was being served throughout, which is what makes this a load
    // case rather than a child sitting on a read.
    let served = std::fs::read_to_string(&answers)
        .map(|text| text.lines().count())
        .unwrap_or(0);
    assert!(
        served > 20,
        "the child was answered {served} times; this gate is only meaningful if the parent was          kept busy"
    );
    assert!(
        asked.load(Ordering::SeqCst) > served,
        "the owner was asked no more often than the child asked; the parent's poll must be its own          clock, not a consequence of the child's traffic"
    );
    assert!(
        !alive(pid_of(&pidfile)),
        "the revoked delivery's child is still running"
    );
}

/// REVOCATION WHILE THE PARENT'S OWN ANSWER IS STUCK IN THE PIPE.
///
/// The child asks whether it still holds its turn and then never reads what it is told. The answers
/// pile up in the kernel's pipe buffer until it is full, and the parent is left inside the write,
/// holding a frame the child will not take.
///
/// That wait used to be one uninterrupted block sized by the whole remaining deadline. It is the
/// worst place for a revocation to arrive — the parent is stuck precisely because the child is
/// misbehaving — and it was the one wait that never looked again. This gate revokes while the write
/// is outstanding and requires the same bound as every other phase.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_while_an_unread_answer_is_stuck_in_the_pipe_is_acted_on_within_the_poll() {
    let dir = scratch("revoke-stuck-write");
    let pidfile = dir.join("child.pid");
    let asking = dir.join("asking");
    // Asks without limit and reads NOTHING back. A pipe buffer is finite, so the parent's answers
    // fill it and the next write cannot complete.
    let program = fixture(
        &dir,
        &format!(
            "echo $$ > {}\n{HELLO}\nIFS= read -r _request\ntouch {}\ni=0\nwhile [ $i -lt 20000 ]; do \
             printf '{{\"t\":\"Check\",\"phase\":\"send-pack\"}}\\n'; i=$((i+1)); done\n\
             while :; do sleep 0.05; done\n",
            pidfile.display(),
            asking.display()
        ),
    );

    let live = Arc::new(AtomicBool::new(true));
    let authority: AuthorityCheck = {
        let live = Arc::clone(&live);
        Arc::new(move || {
            if live.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("the owner of this delivery went away".to_owned())
            }
        })
    };

    let revoke_at = {
        let live = Arc::clone(&live);
        let asking = asking.clone();
        tokio::spawn(async move {
            while !asking.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Long enough for the unread answers to fill the buffer and leave the parent inside a
            // write, and still a small fraction of the deadline.
            tokio::time::sleep(Duration::from_millis(600)).await;
            let at = Instant::now();
            live.store(false, Ordering::SeqCst);
            at
        })
    };

    let run = deliver(program, dir.join("workdir"), None, Some(authority), UNREACHABLE).await;
    let revoked_at = revoke_at.await.expect("revoker");
    let reacted_in = Instant::now().saturating_duration_since(revoked_at);

    let why = message(&run.outcome);
    assert!(
        matches!(run.outcome, Err(SellerGitError::Cancelled(_))) && why.contains("revoked"),
        "a delivery revoked while its own write was outstanding must come back revoked, and not as          the deadline breach that wait used to become: {why}"
    );
    assert!(
        reacted_in < CANCELLATION_POLL + REAP_BOUND + Duration::from_secs(2),
        "the parent took {reacted_in:?} to act on a revocation that arrived while it was blocked          writing to a child that had stopped reading"
    );
    assert!(
        run.took < UNREACHABLE / 2,
        "a child that stops reading must not be able to park the delivery until its deadline: {:?}",
        run.took
    );
    assert!(
        !alive(pid_of(&pidfile)),
        "the revoked delivery's child is still running"
    );
}

/// END OF FILE, A STOPPED READER AND A CONFIRMED EXIT ARE THREE DIFFERENT FACTS.
///
/// This child closes its stdout and keeps running. The parent observes a real end of file — the
/// kernel returning zero bytes — while the process is still very much alive, which is the whole
/// point: EOF is evidence about a DESCRIPTOR. The exit is a separate fact, established by the kill
/// and the reap that follow, and this gate checks both halves separately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_that_closes_its_stdout_is_at_end_of_file_but_not_yet_confirmed_gone() {
    let dir = scratch("eof-alive");
    let pidfile = dir.join("child.pid");
    let program = fixture(
        &dir,
        &format!(
            "echo $$ > {}\n{HELLO}\nIFS= read -r _request\nexec 1>&-\nwhile :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    );

    let run = deliver(program, dir.join("workdir"), None, None, UNREACHABLE).await;

    let why = message(&run.outcome);
    assert!(
        why.contains("end of file"),
        "a pipe the kernel actually ended must be reported as end of file, and not confused with a \
         reader that stopped for its own reasons: {why}"
    );
    assert!(
        run.took < UNREACHABLE / 2,
        "the parent waited out the deadline on a pipe that had already ended: {:?}",
        run.took
    );
    // The process was alive when that EOF was observed; what makes it gone is the kill and the reap
    // this parent does afterwards. That is the fact the seat is released on.
    assert!(
        !alive(pid_of(&pidfile)),
        "the child was released on an end-of-file that was never followed by a confirmed exit"
    );
    assert!(
        run.work_ended && run.released,
        "a confirmed exit must hand the seat on"
    );
}
