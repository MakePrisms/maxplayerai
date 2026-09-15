//! The child's budget must be the parent's REMAINING time, not a fresh copy of it.
//!
//! The parent measures what is left of the delivery's absolute deadline and writes it into the
//! request. The child then starts counting when it READS that frame — so every millisecond between
//! the parent's write and the child's read used to be time the parent had already spent and the
//! child was handed anyway. On a loaded host that transit is not always small, and it is budget
//! spent on the wire by a delivery whose owner may already be gone.
//!
//! The fix stamps the same deadline twice, from one moment: as a remaining duration (a ceiling the
//! child can never exceed) and as an absolute wall-clock instant. The child subtracts its own `now`
//! from the second and takes whichever is smaller, so the transit is charged to it.
//!
//! **These gates do not read an error string to decide whether the fix works.** The remote is a real
//! TCP listener on loopback that counts accepted connections. A child that refused at its pre-wire
//! gate opens none; a child that transmitted opens one. That count is the oracle — the child's own
//! report is only corroboration.

#![cfg(all(unix, feature = "git-delivery"))]

use std::io::{BufReader, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use maxplayer_core::delivery_executor::{child_main, read_frame, write_frame, PushRequest, ToChild, ToParent};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mp-transit-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A real repository with one commit, so the child's local phase has something to offer and the
/// only thing left between it and the wire is the gate under test.
fn repo_with_commit(dir: &Path) -> (String, String) {
    let repo = git2::Repository::init(dir).expect("init workdir");
    std::fs::write(dir.join("payload.txt"), b"delivery payload").expect("write payload");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("payload.txt")).expect("add");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let who = git2::Signature::now("delivery", "delivery@example.invalid").expect("signature");
    let oid = repo
        .commit(Some("HEAD"), &who, &who, "delivery", &tree, &[])
        .expect("commit");
    let head = repo.head().expect("head");
    let branch = head.shorthand().expect("head is on a branch").to_owned();
    (oid.to_string(), branch)
}

/// The remote, reduced to the one question these gates ask: **did anything connect?**
///
/// It accepts and immediately drops, so the TLS handshake above it always fails. That is deliberate.
/// A push that reaches this listener has already crossed the child's pre-wire gate, which is the
/// whole of what is being measured; what happens after the connection is irrelevant to it.
struct CountingRemote {
    url: String,
    accepted: Arc<AtomicUsize>,
}

fn counting_remote() -> CountingRemote {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback remote");
    let port = listener.local_addr().expect("addr").port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    counter.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
                Err(_) => return,
            }
        }
    });
    CountingRemote {
        url: format!("https://127.0.0.1:{port}/seller.git"),
        accepted,
    }
}

struct Run {
    error: String,
    checked: Vec<String>,
    elapsed: Duration,
}

/// Drives the REAL child entry point — the same `child_main` the shipped binary calls — over a real
/// socket pair, answering its authority questions the way a live parent does.
///
/// `transit` is the delay between stamping the request and writing it: the interval this fix exists
/// to charge to the child.
fn drive_child(mut request: PushRequest, budget: Duration, transit: Duration) -> Run {
    let stamped = now_unix_ms();
    request.budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
    request.deadline_unix_ms = stamped.saturating_add(request.budget_ms);

    let (parent, child) = UnixStream::pair().expect("socket pair");
    let child_in = child.try_clone().expect("clone child end");
    let worker = std::thread::spawn(move || child_main(child_in, child));

    let mut reader = BufReader::new(parent.try_clone().expect("clone parent end"));
    let mut writer = parent;
    let hello: ToParent = read_frame(&mut reader)
        .expect("read hello")
        .expect("the child says hello");
    assert!(
        matches!(hello, ToParent::Hello { .. }),
        "the child's first frame is its hello"
    );

    // The transit: stamped above, written now.
    std::thread::sleep(transit);
    write_frame(&mut writer, &ToChild::Push(request)).expect("write the request");
    writer.flush().expect("flush");

    let started = Instant::now();
    let mut checked = Vec::new();
    let error = loop {
        let frame: ToParent = read_frame(&mut reader)
            .expect("read a frame")
            .expect("the child ends with a terminal frame");
        match frame {
            ToParent::Check { phase } => {
                checked.push(phase);
                write_frame(&mut writer, &ToChild::Authority { refused: None })
                    .expect("answer the check");
                writer.flush().expect("flush");
            }
            ToParent::Done { oid, error } => {
                assert!(
                    oid.is_none(),
                    "no delivery can succeed against a remote that only accepts and hangs up"
                );
                break error.expect("a failed delivery names its reason");
            }
            ToParent::Mint { destination } => {
                panic!("the child asked to authorize {destination} on an unauthenticated remote")
            }
            ToParent::Hello { .. } => panic!("the child said hello twice"),
        }
    };
    let elapsed = started.elapsed();
    drop(writer);
    let _ = worker.join();
    Run {
        error,
        checked,
        elapsed,
    }
}

fn request_for(workdir: PathBuf, remote_url: String, oid: String, branch: String) -> PushRequest {
    PushRequest {
        workdir,
        remote_url,
        branch,
        gated_oid: oid,
        authenticated: false,
        // Both are restamped inside `drive_child`, from one moment, exactly as the parent does.
        budget_ms: 0,
        deadline_unix_ms: 0,
    }
}

/// THE POSITIVE CONTROL, and it comes first on purpose: a refusal proves nothing unless the same
/// fixture, the same repository and the same child can be shown to transmit when the budget is
/// there. Without this, "no connection" could just as well mean the local phase never worked.
#[test]
fn a_child_with_its_whole_budget_in_hand_reaches_the_wire() {
    let dir = scratch("spent");
    let (oid, branch) = repo_with_commit(&dir);
    let remote = counting_remote();
    let run = drive_child(
        request_for(dir.clone(), remote.url.clone(), oid, branch),
        // The ceiling. A child that only reads this has a full minute of budget in hand.
        Duration::from_secs(60),
        // The transit — longer than the absolute deadline this request was stamped with.
        Duration::from_millis(0),
    );
    // Stamped with 60s and no transit, this one MUST reach the wire; it is the control that proves
    // the listener and the local phase work at all. The spent case is the sibling test below.
    assert_eq!(
        remote.accepted.load(Ordering::SeqCst),
        1,
        "a delivery with its whole budget in hand must transmit; it opened no connection, so this \
         fixture proves nothing about the refusing case. error was: {}",
        run.error
    );
    assert!(
        !run.checked.is_empty(),
        "a child that reached the wire asked its parent first"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The same request, the same remote, the same child — and a transit that outlives the budget.
#[test]
fn a_child_that_read_its_request_after_the_deadline_opens_no_connection() {
    let dir = scratch("transit");
    let (oid, branch) = repo_with_commit(&dir);
    let remote = counting_remote();
    let run = drive_child(
        request_for(dir.clone(), remote.url.clone(), oid, branch),
        // The absolute deadline is 700ms away...
        Duration::from_millis(700),
        // ...and the frame does not reach the child for 1.5s.
        Duration::from_millis(1_500),
    );
    assert_eq!(
        remote.accepted.load(Ordering::SeqCst),
        0,
        "the child transmitted for a delivery whose deadline had already passed when it read the \
         request; the remote accepted a connection. error was: {}",
        run.error
    );
    assert!(
        run.elapsed < Duration::from_secs(20),
        "the child took {:?} to refuse a delivery that was already over",
        run.elapsed
    );
    // Corroboration only — the connection count above is what decides.
    assert!(
        run.error.contains("budget is spent"),
        "the child refused for some reason other than its spent budget: {}",
        run.error
    );
    std::fs::remove_dir_all(&dir).ok();
}

// The remaining half of the rule — that the duration CEILING still binds when the absolute stamp
// is the larger of the two — is gated at the decision itself, in
// `delivery_executor::tests::the_child_takes_whichever_of_its_two_bounds_is_smaller`.
//
// It is not gated end to end here, and the reason is a real limit rather than a preference: the
// ceiling starts when the child READS, so no delay this harness can insert before the write
// consumes it, and once a wire leg is in flight the child's pre-wire gate is behind it — what
// bounds a child that is already transmitting is the PARENT's kill, and this harness has no
// parent. A held-remote test here would measure libgit2's timeout, not the budget.
