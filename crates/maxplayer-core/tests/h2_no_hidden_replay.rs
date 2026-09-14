//! The retry nobody wrote: reqwest's own.
//!
//! The narrow change mints the delivery token inside `HttpStream::send`, one per request, so a token
//! is never older than the leg it is on. That argument covers every attempt the code makes. It does
//! NOT cover an attempt the HTTP client makes by itself — and the client makes them.
//!
//! The workspace enables reqwest's `http2` feature (root `Cargo.toml`), so the transport client
//! offers h2 by ALPN, and reqwest ships a default retry policy for HTTP/2 protocol nacks: a peer that
//! answers `GOAWAY(NO_ERROR)` or resets the stream with `REFUSED_STREAM` has told the client the
//! request was never processed, and the client replays it — cloning method, URI, body AND headers
//! (`reqwest`'s `retry.rs`). That clone happens inside `Client::execute`, one layer BELOW
//! `HttpStream::send`. The minter is not called again. The replayed request therefore carries the
//! token minted for the first attempt, and the pack body goes out a second time under an
//! authorization nobody re-checked.
//!
//! A peer controls when it refuses. It can hold the stream and refuse late, which makes the replayed
//! token arbitrarily older than the single leg it was signed for — the precise property the minter
//! exists to prevent, reintroduced beneath it.
//!
//! So the transport client refuses hidden replay (`no_hidden_replay`), and this test is the proof:
//! a real HTTP/2 peer, a real delay, a real `REFUSED_STREAM`, and the production push path above it.
//!
//! Why a real h2 peer and not a mock: the retry lives inside reqwest's connection layer and is
//! triggered by h2 frames. Nothing above the framing can stage it, and nothing below h2 can observe
//! whether a replay happened. The fixture has to be an HTTP/2 server.
//!
//! Its own test binary: the transport's HTTP client is built once per process, baking in whether it
//! accepts this fixture's self-signed certificate.
#![cfg(all(unix, feature = "git-delivery"))]

mod git_http_fixture;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use maxplayer_core::git_transport::{self, AuthMinter};

static ENV_INIT: Once = Once::new();

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

/// How long the peer holds the stream before refusing it. The point of refusing LATE: a replay of
/// the first attempt's token is a replay of a token that has already spent this long ageing.
const REFUSAL_DELAY: Duration = Duration::from_millis(600);

/// One request the HTTP/2 peer received, as it arrived on the wire.
#[derive(Clone, Debug)]
struct H2Request {
    /// Seconds since the fixture started, so a replay is visibly LATER than the attempt it copies.
    at: Duration,
    path: String,
    authorization: Option<String>,
}

/// An HTTP/2 peer that answers every request the same way: hold it, then refuse the stream with
/// `REFUSED_STREAM` — the nack reqwest's default policy treats as "never processed, send it again".
struct RefusingH2Peer {
    port: u16,
    seen: Arc<Mutex<Vec<H2Request>>>,
    shutdown: Arc<AtomicUsize>,
}

impl RefusingH2Peer {
    /// Bind, and serve until dropped. ALPN offers `h2` ONLY: if the client under test were not
    /// speaking HTTP/2 the handshake would fail outright rather than quietly proving nothing.
    async fn spawn() -> Self {
        let mut tls = git_http_fixture::self_signed_tls_config();
        tls.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind h2 fixture");
        let port = listener.local_addr().expect("addr").port();
        let seen: Arc<Mutex<Vec<H2Request>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicUsize::new(0));

        let seen_bg = Arc::clone(&seen);
        let shutdown_bg = Arc::clone(&shutdown);
        let started = Instant::now();
        tokio::spawn(async move {
            loop {
                if shutdown_bg.load(Ordering::SeqCst) == 1 {
                    return;
                }
                let accepted =
                    tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
                let Ok(Ok((socket, _peer))) = accepted else {
                    continue;
                };
                let acceptor = acceptor.clone();
                let seen = Arc::clone(&seen_bg);
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(socket).await else {
                        return;
                    };
                    let Ok(mut connection) = h2::server::handshake(tls).await else {
                        return;
                    };
                    while let Some(Ok((request, mut stream))) = connection.accept().await {
                        seen.lock().expect("seen").push(H2Request {
                            at: started.elapsed(),
                            path: request.uri().path().to_owned(),
                            authorization: request
                                .headers()
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned),
                        });
                        // Hold the stream, THEN refuse it: "I never processed this, send it again"
                        // — arriving late enough that a replayed token is a stale one.
                        tokio::time::sleep(REFUSAL_DELAY).await;
                        stream.send_reset(h2::Reason::REFUSED_STREAM);
                    }
                });
            }
        });

        Self {
            port,
            seen,
            shutdown,
        }
    }

    /// The repo URL to push at: allowlist-shaped (https, credential-free, relay-git path).
    fn repo_url(&self) -> String {
        format!("https://127.0.0.1:{}/git/seller/r.git", self.port)
    }

    fn seen(&self) -> Vec<H2Request> {
        self.seen.lock().expect("seen").clone()
    }
}

impl Drop for RefusingH2Peer {
    fn drop(&mut self) {
        self.shutdown.store(1, Ordering::SeqCst);
    }
}

fn temp(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "maxplayer-h2-replay-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir root");
    root
}

/// A committed workdir with one commit on `branch`, as a delivery has.
fn job_workdir(root: &Path, branch: &str) -> (PathBuf, String) {
    let workdir = root.join("job");
    let repo = git2::Repository::init(&workdir).expect("init workdir");
    std::fs::write(workdir.join("deliverable.txt"), "work\n").expect("write");
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

/// A minter shaped like the delivery push's: a distinct token per call, recorded in order.
fn counting_minter() -> (AuthMinter, Arc<Mutex<Vec<String>>>) {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let minter: AuthMinter = Arc::new(move |_destination: &str| {
        let mut minted = sink.lock().expect("mint log");
        let header = format!("Nostr leg-{}", minted.len() + 1);
        minted.push(header.clone());
        Ok(header)
    });
    (minter, log)
}

/// A peer that holds a request and then refuses the stream gets ONE request, not two.
///
/// This is the whole finding. With reqwest's default retry policy the same bytes — same
/// `Authorization`, same body — go out again by themselves, after the delay, without the minter
/// being asked; the peer would record two requests carrying one token. With `no_hidden_replay` on
/// the transport's clients the refusal is what it is: a failed leg, surfaced to libgit2, which is
/// then free to open a NEW stream and take a NEW mint if it wants to try again.
///
/// Red-on-revert: drop `.retry(no_hidden_replay())` from `client_default` and the peer records a
/// second request whose `authorization` equals the first's, while `minted` still has one entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_h2_stream_is_never_replayed_under_the_minter() {
    init_test_env();
    let root = temp("refused");
    let branch = "maxplayer/dddd4444";
    let (workdir, oid) = job_workdir(&root, branch);

    let peer = RefusingH2Peer::spawn().await;
    let url = peer.repo_url();
    let (minter, minted) = counting_minter();

    // The production push path, exactly as the delivery uses it.
    let url_for_push = url.clone();
    let workdir_for_push = workdir.clone();
    let branch_owned = branch.to_owned();
    let oid_for_push = oid.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        git_transport::push_branch_with_minter(
            &workdir_for_push,
            &url_for_push,
            &branch_owned,
            &oid_for_push,
            Some(minter),
            None,
        )
    })
    .await
    .expect("push task");

    let error = outcome.expect_err("a peer that refuses every stream cannot accept a delivery");
    let error = error.to_string();

    // Let any replay that is still coming actually arrive before counting. A request the client
    // sends after this assertion would be a worse bug than one it sends before it, so wait out a
    // multiple of the peer's own delay rather than reading the count the instant the push returns.
    tokio::time::sleep(REFUSAL_DELAY * 3).await;

    let seen = peer.seen();
    let minted = minted.lock().expect("mint log").clone();

    assert!(
        !seen.is_empty(),
        "the push never reached the h2 peer at all, so this test proved nothing: {error}"
    );
    // Every request the peer saw was minted for: as many mints as requests, no more requests than
    // mints. A hidden replay breaks the second half of that while leaving the first intact.
    assert_eq!(
        seen.len(),
        minted.len(),
        "a request reached the peer that the minter was never asked about — requests {seen:?}, \
         mints {minted:?}"
    );
    for (index, request) in seen.iter().enumerate() {
        assert_eq!(
            request.authorization.as_deref(),
            Some(minted[index].as_str()),
            "request {index} did not carry the token minted for it: {seen:?} vs {minted:?}"
        );
    }
    // And no two requests carried the same token, which is what a replay looks like on the wire.
    for (index, request) in seen.iter().enumerate() {
        for other in seen.iter().skip(index + 1) {
            assert_ne!(
                request.authorization, other.authorization,
                "the same authorization reached the peer twice — a replay below the minter: \
                 {seen:?}"
            );
        }
    }
    // The refusal is surfaced, not swallowed: the push fails and names the transport error.
    assert!(
        error.contains("http request"),
        "the refused leg must surface as the push's error: {error}"
    );
    // Nothing was accepted, so nothing is on the peer's side to undo.
    assert!(
        seen.iter().all(|request| request.path.contains("/git/seller/r.git")),
        "every request named the authorized repo: {seen:?}"
    );

    drop(peer);
    let _ = std::fs::remove_dir_all(&root);
}

/// The same peer, seen from the other side: what the transport does with a LATE refusal.
///
/// A replay is only interesting because the peer chooses when to trigger it. Here the refusal
/// arrives after [`REFUSAL_DELAY`], and the assertion is that no request arrives after that moment
/// carrying the token minted before it — the staleness the hidden retry would have created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_refusal_cannot_put_an_aged_token_back_on_the_wire() {
    init_test_env();
    let root = temp("late");
    let branch = "maxplayer/eeee5555";
    let (workdir, oid) = job_workdir(&root, branch);

    let peer = RefusingH2Peer::spawn().await;
    let url = peer.repo_url();
    let (minter, minted) = counting_minter();

    let url_for_push = url.clone();
    let workdir_for_push = workdir.clone();
    let branch_owned = branch.to_owned();
    let oid_for_push = oid.clone();
    let _ = tokio::task::spawn_blocking(move || {
        git_transport::push_branch_with_minter(
            &workdir_for_push,
            &url_for_push,
            &branch_owned,
            &oid_for_push,
            Some(minter),
            None,
        )
    })
    .await
    .expect("push task");

    tokio::time::sleep(REFUSAL_DELAY * 3).await;

    let seen = peer.seen();
    let minted = minted.lock().expect("mint log").clone();
    assert!(!seen.is_empty(), "the push never reached the h2 peer");

    // The first request is refused at `first.at + REFUSAL_DELAY`. Any request after that instant is
    // a later attempt, and a later attempt must carry a token minted for IT.
    let refused_at = seen[0].at + REFUSAL_DELAY;
    for (index, request) in seen.iter().enumerate().skip(1) {
        assert!(
            request.at >= refused_at,
            "the peer recorded a request before it refused the first one: {seen:?}"
        );
        assert_ne!(
            request.authorization, seen[0].authorization,
            "an attempt made after the refusal carried the token minted before it — aged by at \
             least the refusal delay: {seen:?}"
        );
        assert_eq!(
            request.authorization.as_deref(),
            Some(minted[index].as_str()),
            "a later attempt must carry its own mint: {seen:?} vs {minted:?}"
        );
    }

    drop(peer);
    let _ = std::fs::remove_dir_all(&root);
}
