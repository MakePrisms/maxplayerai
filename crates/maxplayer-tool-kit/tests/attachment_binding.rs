//! A connection is bound to one attachment instance, and a job cannot hold the holder's threads.
//!
//! Four review findings drive this file. First: the holder used to resolve a job id through its
//! table at call time, so a connection held across a detach and a re-attach of the same id read
//! and wrote the NEW attachment's directory. Second: a FIFO a job planted at an input or output
//! name blocked a holder thread in `open`, past the job's detach, and an output FIFO could receive
//! bytes after the detach. Third: a call that passed its last detach check could pause before it
//! wrote, outlive the detach, and write into the directory a later attachment of the same id owned
//! at the same pathname. Fourth: a silent connection held its slot for as long as the job stayed
//! attached.
//!
//! Every test here runs the real daemon, the real fake vendor, real Unix sockets and real files.

mod common;

use common::Fixture;
use maxplayer_tool_kit::proto;
use serde_json::{json, Value};
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// One raw connection to a job socket, held open across calls. `client::call` opens a new
/// connection per call; the finding is about a connection that outlives its attachment, so the
/// tests hold one.
struct RawConn {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl RawConn {
    fn open(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect to the job socket");
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(20))).unwrap();
        let writer = stream.try_clone().unwrap();
        RawConn { writer, reader: BufReader::new(stream) }
    }

    /// Send one request and read its one reply line, the whole envelope. `Err` means the holder
    /// gave no reply on this connection: closed, or the write failed.
    fn send(&mut self, id: u64, method: &str, params: Value) -> Result<Value, String> {
        self.writer
            .write_all(proto::request_line(id, method, params).as_bytes())
            .and_then(|_| self.writer.write_all(b"\n"))
            .and_then(|_| self.writer.flush())
            .map_err(|e| format!("write: {e}"))?;
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("the holder closed the connection".into());
        }
        serde_json::from_str(line.trim()).map_err(|e| format!("reply is not JSON: {e}"))
    }
}

fn transform(mode: &str) -> Value {
    json!({"name": "transform-file", "arguments": {"input": "input.txt", "output": "out.txt", "mode": mode}})
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
}

/// Poll the holder's status until the one attached job shows `connections == want`.
fn wait_for_connections(fx: &Fixture, job_id: &str, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = fx.ctl("holder/status", json!({})).expect("status");
        let seen = status["attached_jobs"]
            .as_array()
            .and_then(|jobs| jobs.iter().find(|j| j["job_id"] == json!(job_id)))
            .and_then(|j| j["connections"].as_u64());
        if seen == Some(want) {
            return;
        }
        assert!(Instant::now() < deadline, "expected {want} connections on {job_id}, status shows {seen:?}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The finding itself. Attach `j1` to directory A and hold a connection. Detach. Attach `j1`
/// again, to directory B. The old connection must be refused, must run nothing, and must create
/// nothing in B. A new connection to the new socket serves B.
#[test]
fn an_old_connection_is_refused_after_the_same_job_id_is_attached_elsewhere() {
    let fx = Fixture::start();
    let a = fx.make_job("j1");
    std::fs::write(a.join("input.txt"), "from a").unwrap();
    let mut old = RawConn::open(&fx.job_socket("j1"));
    let first = old.send(1, "tools/call", transform("upper")).expect("a call while attached is served");
    assert_eq!(first["result"]["isError"], json!(false), "{first}");
    assert_eq!(read(&a.join("out.txt")), "FROM A");

    let detached = fx.detach_job("j1");
    assert_eq!(detached["detached"], json!(true));

    // The same id, a different directory.
    let b = fx.jobs_dir.join("j1-second-directory");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join("input.txt"), "from b").unwrap();
    fx.ctl("holder/attach_job", json!({"job_id": "j1", "job_root": b}))
        .expect("the same id attaches again after its detach");

    // The old connection holds the first attachment, which is detached: refused, with its id.
    let second = old
        .send(2, "tools/call", transform("upper"))
        .expect("the holder answers the old connection with a refusal, not silence");
    assert_eq!(second["id"], json!(2));
    assert_eq!(second["error"]["code"], json!(proto::CODE_REJECTED), "{second}");
    let message = second["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("detached"), "the refusal names the cause: {second}");
    // After the refusal the holder closes the connection.
    assert!(old.send(3, "tools/list", json!({})).is_err(), "the old connection is closed after the refusal");

    // Nothing ran and nothing landed: not in B, not in A.
    assert!(!b.join("out.txt").exists(), "the old connection must create nothing in the new directory");
    assert_eq!(read(&a.join("out.txt")), "FROM A", "the old directory is untouched");
    assert_eq!(fx.vendor_stats()["transform_count"], json!(1), "the refused call never reached the tool");

    // A new connection to the new socket serves the new directory.
    let mut fresh = RawConn::open(&fx.job_socket("j1"));
    let served = fresh.send(1, "tools/call", transform("upper")).expect("the new attachment serves");
    assert_eq!(served["result"]["isError"], json!(false), "{served}");
    assert_eq!(read(&b.join("out.txt")), "FROM B");
    assert_eq!(fx.vendor_stats()["transform_count"], json!(2));
}

/// An id that is attached now is not replaced under the job that holds it.
#[test]
fn attaching_a_live_job_id_is_refused() {
    let fx = Fixture::start();
    let a = fx.make_job("j-live");
    std::fs::write(a.join("input.txt"), "payload").unwrap();
    let other = fx.jobs_dir.join("j-live-other");
    std::fs::create_dir_all(&other).unwrap();

    let err = fx
        .ctl("holder/attach_job", json!({"job_id": "j-live", "job_root": other}))
        .expect_err("a second attach of a live id is refused");
    assert!(err.contains("already attached"), "the refusal names the cause: {err}");

    // The first job's endpoint still serves its own directory.
    let res = fx.job_call("j-live", "tools/call", transform("upper")).expect("the first attachment serves");
    assert_eq!(res["isError"], json!(false));
    assert_eq!(read(&a.join("out.txt")), "PAYLOAD");
    assert!(!other.join("out.txt").exists());
}

/// A wrapper that makes the vendor CLI slow: it sleeps, then runs the real fake. The holder runs
/// it with a cleared environment and `PATH=/usr/local/bin:/usr/bin:/bin`.
fn slow_cli(root: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join("slow-vendor-cli.sh");
    std::fs::write(&script, format!("#!/bin/sh\nsleep 1\nexec \"{}\" \"$@\"\n", common::VENDOR_CLI)).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A detach that lands while the tool runs: the tool's result is not published into the job's
/// directory, and the caller is told so.
#[test]
fn a_detach_while_the_tool_runs_publishes_nothing() {
    let scratch = std::env::temp_dir().join(format!("mtk-slow-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let fx = Fixture::start_with(|_| {}, &slow_cli(&scratch));
    let root = fx.make_job("j-slow");
    std::fs::write(root.join("input.txt"), "slow payload").unwrap();

    let socket = fx.job_socket("j-slow");
    let call = std::thread::spawn(move || RawConn::open(&socket).send(1, "tools/call", transform("upper")));
    // The wrapper sleeps one second before the tool runs; the detach lands inside that second.
    std::thread::sleep(Duration::from_millis(300));
    let detached = fx.detach_job("j-slow");
    assert_eq!(detached["detached"], json!(true));

    let reply = call.join().unwrap().expect("the in-flight call is answered");
    assert_eq!(reply["error"]["code"], json!(proto::CODE_REJECTED), "{reply}");
    let message = reply["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("detached") && message.contains("not published"), "{reply}");
    assert!(!root.join("out.txt").exists(), "no output may land in a detached job's directory");
    // The tool did run once: this is the case the publish-time check exists for.
    assert_eq!(fx.vendor_stats()["transform_count"], json!(1));
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The connection bound: the connection over it gets one error line, with its request's id, and
/// is closed. When a connection ends, the next one is served again.
#[test]
fn connections_over_the_bound_get_one_error_line_and_are_closed() {
    let fx = Fixture::start();
    fx.make_job("j-many");
    let socket = fx.job_socket("j-many");

    let mut idle: Vec<UnixStream> = (0..16).map(|_| UnixStream::connect(&socket).expect("connect")).collect();
    wait_for_connections(&fx, "j-many", 16);

    let mut over = RawConn::open(&socket);
    let refused = over.send(7, "tools/list", json!({})).expect("one error line comes back");
    assert_eq!(refused["id"], json!(7), "the refusal carries the request's id: {refused}");
    assert_eq!(refused["error"]["code"], json!(proto::CODE_REJECTED));
    assert!(
        refused["error"]["message"].as_str().unwrap_or("").contains("too many connections"),
        "{refused}"
    );
    assert!(over.send(8, "tools/list", json!({})).is_err(), "the connection over the bound is closed");
    // The idle connections are untouched.
    wait_for_connections(&fx, "j-many", 16);

    // One idle connection ends; the next connection is served.
    drop(idle.pop());
    wait_for_connections(&fx, "j-many", 15);
    let mut next = RawConn::open(&socket);
    let listed = next.send(9, "tools/list", json!({})).expect("served under the bound");
    assert_eq!(listed["result"]["tools"][0]["name"], json!("transform-file"), "{listed}");
    drop(idle);
}

/// A FIFO planted at the input name is refused at once, before the tool runs.
#[test]
fn a_fifo_planted_as_input_is_refused_at_once() {
    let fx = Fixture::start();
    let root = fx.make_job("j-fifo-in");
    mkfifo(&root.join("input.txt"));

    let started = Instant::now();
    let err = fx
        .job_call("j-fifo-in", "tools/call", transform("upper"))
        .expect_err("a FIFO is not an input file");
    assert!(started.elapsed() < Duration::from_secs(5), "the open must not block on the FIFO");
    assert!(err.contains("[1003]") && err.contains("not a regular file"), "{err}");
    assert_eq!(fx.vendor_stats()["transform_count"], json!(0), "the tool must not run");
    assert!(!root.join("out.txt").exists());
}

/// A FIFO planted at the output name, with the job holding a reader on it: the tool runs on the
/// real input, and the publish step refuses the FIFO. No byte reaches the reader.
#[test]
fn a_fifo_planted_as_output_is_refused_and_receives_nothing() {
    let fx = Fixture::start();
    let root = fx.make_job("j-fifo-out");
    std::fs::write(root.join("input.txt"), "payload").unwrap();
    let fifo = root.join("out.txt");
    mkfifo(&fifo);
    // The job's reader, nonblocking, so a blocked write would be visible rather than hang the job.
    let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated string; the descriptor is owned below.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    assert!(fd >= 0, "open a reader on the FIFO: {}", std::io::Error::last_os_error());
    let mut reader = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) };

    let started = Instant::now();
    let err = fx
        .job_call("j-fifo-out", "tools/call", transform("upper"))
        .expect_err("a FIFO is not an output file");
    assert!(started.elapsed() < Duration::from_secs(5), "the publish must not block on the FIFO");
    assert!(err.contains("[1003]") && err.contains("not a regular file"), "{err}");
    // The tool ran on the regular input; the refusal is at the publish step.
    assert_eq!(fx.vendor_stats()["transform_count"], json!(1));

    let mut buf = [0u8; 16];
    let got = reader.read(&mut buf);
    assert!(
        matches!(&got, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock) || matches!(got, Ok(0)),
        "no byte may reach the FIFO, got {got:?}"
    );
    use std::os::unix::fs::FileTypeExt;
    assert!(std::fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo(), "the FIFO was not replaced");
}

/// A wrapper that runs the real fake for `login` and `health`, and for `transform` turns the
/// tool's staged OUTPUT into a FIFO and writes to it two seconds later, from a subshell that holds
/// none of the holder's pipes. The holder's read of the staged output
/// then blocks for those two seconds: a call paused between its last check and its writes, which
/// is the window the publication lock closes. The staged path is holder-private; this wrapper
/// stands in for slow tool output, not for a job's reach into staging.
fn fifo_output_cli(root: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join("fifo-output-vendor-cli.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = transform ] || exec \"{real}\" \"$@\"\nout=\"\"\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--out\" ]; then out=\"$a\"; fi\n  prev=\"$a\"\ndone\n\
             [ -n \"$out\" ] || exit 1\nmkfifo \"$out\" || exit 1\n( sleep 2; printf 'STALE' > \"$out\" ) < /dev/null > /dev/null 2>&1 &\nexit 0\n",
            real = common::VENDOR_CLI
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// The publication race. A call passes its last detach check and pauses before it writes. The
/// job is detached (detach must return at once, not wait two seconds) and the SAME id is attached
/// again at the SAME pathname. When the old call resumes, it must write nothing: the new
/// attachment's file keeps its content, and the old call is told its outputs were not published.
#[test]
fn a_call_that_pauses_before_publishing_cannot_write_after_the_detach() {
    let scratch = std::env::temp_dir().join(format!("mtk-fifo-out-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let fx = Fixture::start_with(|_| {}, &fifo_output_cli(&scratch));
    let root = fx.make_job("j-race");
    std::fs::write(root.join("input.txt"), "payload").unwrap();

    let socket = fx.job_socket("j-race");
    let call = std::thread::spawn(move || RawConn::open(&socket).send(1, "tools/call", transform("upper")));
    // The wrapper exits at once; the holder is now blocked reading the staged FIFO, before the
    // publication lock. Give it a moment to get there.
    std::thread::sleep(Duration::from_millis(400));

    let started = Instant::now();
    let detached = fx.detach_job("j-race");
    assert_eq!(detached["detached"], json!(true));
    assert!(started.elapsed() < Duration::from_secs(1), "detach does not wait for a call that has not taken the publication lock");

    // The same id, the same pathname, a new attachment, with a marker the old call must not touch.
    std::fs::write(root.join("out.txt"), "NEW").unwrap();
    let again = fx.make_job("j-race");
    assert_eq!(again, root, "the re-attach uses the same pathname");

    let reply = call.join().unwrap().expect("the old call is answered");
    assert_eq!(reply["error"]["code"], json!(proto::CODE_REJECTED), "{reply}");
    let message = reply["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("detached") && message.contains("not published"), "{reply}");
    assert_eq!(read(&root.join("out.txt")), "NEW", "the old call wrote nothing into the new attachment's directory");

    // The new attachment serves its own directory with the real tool path untouched.
    let listed = fx.job_call("j-race", "tools/list", json!({})).expect("the new attachment serves");
    assert_eq!(listed["tools"][0]["name"], json!("transform-file"), "{listed}");
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Idle expiry. Three connections that send nothing are closed by the holder after the idle
/// timeout, attached job or not: the slots are released and a new connection is served.
#[test]
fn silent_connections_expire_after_the_idle_timeout() {
    let fx = Fixture::start_with_args(|_| {}, Path::new(common::VENDOR_CLI), &["--job-idle-timeout-secs", "1"]);
    fx.make_job("j-idle");
    let socket = fx.job_socket("j-idle");

    // The read timeout is set now, while the peer is open: a socket option on a Unix socket the
    // peer already closed is refused by the kernel.
    let silent: Vec<UnixStream> = (0..3)
        .map(|_| {
            let stream = UnixStream::connect(&socket).expect("connect");
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream
        })
        .collect();
    wait_for_connections(&fx, "j-idle", 3);

    // The holder closes them; the count goes to zero while the job stays attached.
    wait_for_connections(&fx, "j-idle", 0);
    for stream in &silent {
        let mut buf = [0u8; 8];
        let got = (&*stream).read(&mut buf);
        let closed = matches!(got, Ok(0))
            || matches!(&got, Err(e) if !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
        assert!(closed, "the holder closed the silent connection, got {got:?}");
    }
    drop(silent);

    // The attachment is intact: a new connection is served.
    let mut next = RawConn::open(&socket);
    let listed = next.send(1, "tools/list", json!({})).expect("served after the idle expiry");
    assert_eq!(listed["result"]["tools"][0]["name"], json!("transform-file"), "{listed}");
    let status = fx.ctl("holder/status", json!({})).expect("status");
    assert!(
        status["attached_jobs"].as_array().is_some_and(|jobs| jobs.iter().any(|j| j["job_id"] == json!("j-idle"))),
        "the job is still attached: {status}"
    );
}

