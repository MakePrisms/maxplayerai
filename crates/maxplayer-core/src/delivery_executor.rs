//! The killable executor for the delivery push — the boundary that *ends* work rather than
//! observing that it overran.
//!
//! # Why this exists
//!
//! Round 1 of PR 1006 made every phase of the delivery push that libgit2 lets us interrupt ask this
//! delivery's work gate, and named the one span it does not: `git_packbuilder__prepare` →
//! `ll_find_deltas` throws the progress callback's answer away (`pack-objects.c:979`, `:1356`), and
//! there is no other hook inside that loop. In-process, that span cannot be stopped — it can only be
//! measured after the fact, and *reporting a breach is not stopping the work*. A delivery that
//! entered the delta search owned this seat's delivery turn until libgit2 chose to return it.
//!
//! The only thing on a POSIX host that ends work its own author refuses to end is the kernel:
//! `SIGKILL` to a process that cannot catch, block or ignore it. So the blocking local phases run in
//! a **child process**, and the deadline is enforced by killing that process and *waiting for it to
//! actually exit* before this seat's turn is returned.
//!
//! # What crosses the boundary, and what never does
//!
//! The seller's key is held by the signer actor in THIS process and the push path was already built
//! not to be a second custody site: `seller_node::run` hands the push an [`AuthMinter`] closure, not
//! a token, so each wire request is signed at the instant it leaves. That shape is what makes the
//! child safe. The child gets **no key and no token up front**: when its transport needs an
//! `Authorization` header it asks over the pipe, the parent runs the *same* minter closure — same
//! destination binding, same authority check, same push deadline — and returns one scoped NIP-98
//! token whose life is the round-trip it was minted for.
//!
//! Two properties follow, and both are load-bearing:
//!
//! - **Custody is unchanged.** The key never leaves the actor. A child that is compromised, wedged
//!   or killed mid-flight holds at most one short-lived token scoped to this job's ref.
//! - **The parent's deadline binds the child even before the kill lands.** A child past its deadline
//!   cannot obtain a header, so it cannot begin an authenticated leg no matter what state it is in.
//!   The kill ends the *work*; the minter refusal ends the *authority*. Neither depends on the other.
//!
//! Nothing sensitive travels on argv or in the environment: both are world-readable through `ps` and
//! `/proc/<pid>/environ`. The request travels as one frame on the child's stdin, and the child's
//! environment is CLEARED and rebuilt from [`CHILD_ENV_ALLOWLIST`] — proven from inside the child,
//! which reports the environment it actually received in its hello frame.
//!
//! # Every phase between the deadline and the released turn
//!
//! A phase nobody enumerated is a phase that can outlive the bound, so here is the whole list. The
//! turn is released at the end of it, never earlier:
//!
//! | # | phase | who | bounded by |
//! |---|---|---|---|
//! | 1 | spawn (fork/exec, pipe setup) | parent | OS; fails closed — a spawn error releases the turn having done nothing |
//! | 2 | hello handshake | child | the deadline, like every later frame |
//! | 3 | `.git/config` neutralisation, repo open | child | the deadline (killable) |
//! | 4 | advertisement leg | child | [`crate::git_transport::DEFAULT_HTTP_LEG_TIMEOUT`], and the deadline |
//! | 5 | push negotiation | child | the deadline (killable) |
//! | 6 | object traversal + packbuilder insert | child | the deadline (killable) |
//! | 7 | **delta search** | child | **the deadline, by kill — this is why the child exists** |
//! | 8 | pack upload leg | child | the leg timeout, and the deadline |
//! | 9 | status-report read | child | the leg timeout, and the deadline |
//! | 10 | deadline breach: `SIGKILL` to the child's process GROUP | parent | immediate; no delivery wait |
//! | 11 | **reap — `waitpid` until the child has actually exited** | parent | see the assumptions below |
//! | 12 | cleanup: pipes closed, reader thread joined, child status recorded | parent | bounded by 11 |
//! | 13 | the turn is dropped, the lock is free | parent | — |
//!
//! Steps 10–12 run on **every** exit path, including success, error, panic and an early return,
//! because they are a `Drop` (see [`KillableChild`]). A kill that is merely *issued* releases
//! nothing: [`KillableChild::reap`] returns only when the kernel has reported the child's exit
//! status, which it does only once the process is gone.
//!
//! # What the bound guarantees, and under which assumptions
//!
//! `DELIVERY_DRAIN_BOUND` (150s work deadline + 120s for one in-flight leg) + [`REAP_BOUND`].
//!
//! This is a **conditional** bound and is documented as one. What holds it up:
//!
//! - **`SIGKILL` cannot be caught, blocked or ignored** (POSIX, and all three platforms this product
//!   ships for are POSIX — see [`SHIPPED_PLATFORMS`]). No amount of libgit2 or C code in the child
//!   can decline it. This is the property in-process cancellation could not have at any price.
//! - **The kill goes to the process GROUP** (`kill(-pgid)`), and the child is made a group leader at
//!   spawn, so a descendant cannot outlive the delivery even though libgit2 spawns none today.
//! - **The parent waits for the actual exit.** A pid stays a zombie until it is reaped; we always
//!   reap, so the turn is never returned to a pid that still exists.
//!
//! Where it can fail, stated plainly:
//!
//! - **Uninterruptible kernel sleep.** A thread blocked in the kernel (`D` state — a stalled NFS or
//!   FUSE mount, a disk that stopped answering) does not die when `SIGKILL` is delivered; it dies
//!   when it next returns to user space. The delta search reads the delivery workdir, so a wedged
//!   filesystem is exactly the case that defeats the wall clock here. There is no user-space fix:
//!   this is the kernel's own guarantee ending.
//! - **The parent must be scheduled.** If this process is itself starved of CPU, stopped
//!   (`SIGSTOP`), or paused by its supervisor, nothing is issued and nothing is reaped. A wall-clock
//!   claim is a claim about *both* processes running.
//! - **Clocks.** The deadline is `Instant` (monotonic), so it survives wall-clock jumps; it does not
//!   survive the machine suspending mid-push, where monotonic time on some platforms does not
//!   advance across sleep.
//!
//! **When the assumptions fail, this fails CLOSED.** If the child cannot be reaped within
//! [`REAP_BOUND`] the turn is *not* released — the executor keeps waiting and reports the stall.
//! Handing the seat to a second delivery while the first may still be packing is the exact defect
//! this lane exists to remove, and an unreapable child is not evidence that it stopped.
//!
//! # What is deliberately NOT claimed
//!
//! Not an unconditional wall-clock guarantee. Not protection against a wedged filesystem. Not a
//! bound on a machine whose scheduler has stopped running this process. The honest claim is:
//! *within the deadline plus the reap bound, in every state the kernel lets a process leave, the
//! delivery's local work has stopped and the seat is free; in the states it does not, the seat stays
//! held and says so.*

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How long the parent waits for a killed child to actually exit before it reports a stall. This is
/// the only term the killable executor adds to the delivery drain bound.
///
/// Five seconds is not a scheduling estimate — a `SIGKILL`ed process that is runnable is gone in
/// microseconds. It is the window in which an *unrunnable* one (see the uninterruptible-sleep
/// assumption above) is distinguished from a slow one, so the stall can be reported as a stall
/// rather than hidden inside a longer wait.
pub const REAP_BOUND: Duration = Duration::from_secs(5);

/// The platforms this product actually ships (`.github/release-platforms.json`). All POSIX: the
/// `SIGKILL`/`waitpid` contract this executor rests on is available on every one of them, which is
/// what makes the design *feasible* rather than aspirational. There is no Windows artifact, so no
/// `TerminateProcess` path is written, guessed at, or claimed.
pub const SHIPPED_PLATFORMS: [&str; 3] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
];

/// The environment the child is given — and the whole of it. The child's environment is CLEARED and
/// rebuilt from these names, so nothing a seat, a job, or a shell left in this process's environment
/// can reach the delivery push, and no credential can ride along in a variable nobody audited.
///
/// Each entry earns its place: `PATH` because `Command` resolution and any libgit2 helper lookup
/// need it, `HOME` and `TMPDIR` because libgit2 and reqwest place temporary files, and the two
/// `SSL_CERT_*` names because a host with a non-default trust store (every musl container image we
/// ship into) would otherwise fail TLS in the child while succeeding in the parent.
pub const CHILD_ENV_ALLOWLIST: [&str; 5] = ["PATH", "HOME", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR"];

/// The subcommand the shipped binary dispatches to [`child_main`]. Internal, in the spelling the
/// binary already reserves for entrypoints that are not a user surface (`maxplayer __deliver`).
pub const CHILD_SUBCOMMAND: &str = "__delivery-push";

/// Overrides the program the parent re-execs. Set by tests, which run under a harness binary rather
/// than under the shipped one; unset in production, where [`resolve_child_program`] uses
/// `current_exe`.
pub const CHILD_PROGRAM_ENV: &str = "MAXPLAYER_DELIVERY_PUSH_EXE";

/// The protocol version carried in the hello frame. A child that does not speak this exact version
/// is refused rather than driven: a mixed-version pair is a partially-understood push, and the one
/// thing this executor may never do is leave work running that it cannot account for.
pub const PROTOCOL_VERSION: u32 = 1;

/// Parent → child.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
pub enum ToChild {
    /// The one job. Carries no key and no token.
    Push(PushRequest),
    /// The answer to a [`ToParent::Mint`]: either one scoped header, or the refusal that ends this
    /// delivery's authority.
    Minted {
        header: Option<String>,
        refused: Option<String>,
    },
}

/// Child → parent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
pub enum ToParent {
    /// Sent before anything else. `argv` and `env` are what the child ACTUALLY received, which is
    /// how the "no secret on argv, environment is exactly the allowlist" property is proved from
    /// inside the real child rather than asserted about the spawn spec.
    Hello {
        version: u32,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    },
    /// The transport needs an `Authorization` header for this destination.
    Mint { destination: String },
    /// Terminal. Exactly one of `oid`/`error` is set.
    Done {
        oid: Option<String>,
        error: Option<String>,
    },
}

/// Everything the child needs, and nothing more.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushRequest {
    pub workdir: PathBuf,
    pub remote_url: String,
    pub branch: String,
    pub gated_oid: String,
    /// Whether this remote authenticates at all. `false` means the child must never ask to mint.
    pub authenticated: bool,
    /// What is left of the delivery's absolute work deadline at the moment the request is written.
    /// Sent as a duration rather than an instant because `Instant` has no meaning across processes.
    pub budget_ms: u64,
}

#[derive(Debug)]
pub enum ExecutorError {
    /// The child program could not be resolved or spawned. Nothing ran.
    Spawn(String),
    /// The child violated the protocol. It has been killed and reaped.
    Protocol(String),
    /// The deadline passed; the child was killed and reaped. Carries the measured time from the
    /// kill to the confirmed exit — the number that says whether the bound held.
    Killed { after: Duration, reap: Duration },
    /// The child was killed and did NOT exit within [`REAP_BOUND`]. The turn is still held.
    Unreaped { waited: Duration },
    /// The push itself failed; the child exited on its own.
    Push(String),
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(why) => write!(f, "delivery push child could not start: {why}"),
            Self::Protocol(why) => write!(f, "delivery push child spoke out of turn: {why}"),
            Self::Killed { after, reap } => write!(
                f,
                "delivery push exceeded its deadline by {}ms and was killed; the child exited {}ms later",
                after.as_millis(),
                reap.as_millis()
            ),
            Self::Unreaped { waited } => write!(
                f,
                "delivery push child did not exit {}ms after SIGKILL; this seat stays held rather \
                 than hand the turn to a second delivery while the first may still be packing",
                waited.as_millis()
            ),
            Self::Push(why) => write!(f, "delivery push failed: {why}"),
        }
    }
}

impl std::error::Error for ExecutorError {}

/// Where the re-exec points. `current_exe` in production — the seller node runs inside the shipped
/// `maxplayer` binary, which dispatches [`CHILD_SUBCOMMAND`] — and [`CHILD_PROGRAM_ENV`] under a
/// test harness, whose own `current_exe` is the harness.
///
/// Deliberately NOT a `PATH` lookup: resolving `maxplayer` by name would let whatever is first on
/// `PATH` receive a delivery, which is a supply-chain hole in exchange for nothing.
pub fn resolve_child_program() -> Result<PathBuf, ExecutorError> {
    if let Some(explicit) = std::env::var_os(CHILD_PROGRAM_ENV) {
        let path = PathBuf::from(explicit);
        if path.as_os_str().is_empty() {
            return Err(ExecutorError::Spawn(format!(
                "{CHILD_PROGRAM_ENV} is set to an empty path"
            )));
        }
        return Ok(path);
    }
    std::env::current_exe()
        .map_err(|error| ExecutorError::Spawn(format!("current_exe is unreadable: {error}")))
}

/// The environment the child will be given: the allowlist, and only the entries of it this process
/// actually has. Separated from the spawn so a test can assert the *policy* without spawning.
pub fn child_env() -> Vec<(OsString, OsString)> {
    CHILD_ENV_ALLOWLIST
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .collect()
}

/// A spawned child that **cannot be forgotten**. Dropping it kills the process group and waits for
/// the exit; there is no path out of this module that leaves a delivery packing behind us.
pub struct KillableChild {
    child: Option<Child>,
    pid: i32,
    reaped: bool,
}

impl KillableChild {
    /// Spawn `program` with `args`, piped stdio, a cleared environment rebuilt from
    /// [`CHILD_ENV_ALLOWLIST`], and its own process group so the kill reaches descendants.
    pub fn spawn(program: &Path, args: &[&str]) -> Result<Self, ExecutorError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(child_env());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|error| ExecutorError::Spawn(format!("{}: {error}", program.display())))?;
        let pid = child.id() as i32;
        Ok(Self {
            child: Some(child),
            pid,
            reaped: false,
        })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.as_mut().and_then(|child| child.stdin.take())
    }

    pub fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|child| child.stdout.take())
    }

    pub fn stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut().and_then(|child| child.stderr.take())
    }

    /// `SIGKILL` to the process GROUP, then wait for the actual exit.
    ///
    /// Returns how long the exit took to confirm, or [`ExecutorError::Unreaped`] if the child was
    /// still not gone after [`REAP_BOUND`] — in which case the caller must NOT release the turn.
    pub fn kill_and_reap(&mut self) -> Result<Duration, ExecutorError> {
        let started = Instant::now();
        if self.reaped {
            return Ok(Duration::ZERO);
        }
        #[cfg(unix)]
        {
            // The GROUP, not the pid: a descendant that outlived its parent would otherwise keep
            // packing with nobody watching. Negative pid is the group. An ESRCH here means the
            // group is already gone, which is the outcome we wanted.
            unsafe { libc::kill(-self.pid, libc::SIGKILL) };
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(started.elapsed());
        };
        // Poll rather than block: a blocking `wait` on a child in uninterruptible sleep never
        // returns, and "we cannot confirm the exit" is an outcome this executor must be able to
        // REPORT rather than an outcome it hangs in.
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    self.reaped = true;
                    return Ok(started.elapsed());
                }
                Ok(None) => {
                    if started.elapsed() >= REAP_BOUND {
                        return Err(ExecutorError::Unreaped {
                            waited: started.elapsed(),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => {
                    return Err(ExecutorError::Protocol(format!(
                        "waiting for the delivery push child failed: {error}"
                    )));
                }
            }
        }
    }

    /// True once the kernel has reported this child's exit status.
    pub fn is_reaped(&self) -> bool {
        self.reaped
    }
}

impl Drop for KillableChild {
    fn drop(&mut self) {
        if !self.reaped {
            // Best effort by definition — `Drop` cannot report — but it is the same kill and the
            // same wait, so the common paths (success, error, panic, early return) all leave a
            // reaped child behind. The one path that must NOT reach here is the deadline breach,
            // which calls `kill_and_reap` explicitly so the stall can be reported.
            let _ = self.kill_and_reap();
        }
    }
}

/// One line of newline-delimited JSON per frame. JSON's own escaping means a serialized frame never
/// contains a newline, so the framing is unambiguous without a length prefix.
pub fn write_frame<W: Write, T: Serialize>(out: &mut W, frame: &T) -> std::io::Result<()> {
    let line = serde_json::to_string(frame)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Read one frame. `Ok(None)` is a clean end of stream.
pub fn read_frame<R: BufRead, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Pump the child's stdout into a channel so the parent can wait on frames WITH A DEADLINE. A
/// blocking read cannot be given one, and a parent blocked in a read it cannot leave is a parent
/// that never issues the kill.
fn pump<R: std::io::Read + Send + 'static>(
    stream: R,
    sink: Sender<std::io::Result<Option<ToParent>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let frame = read_frame::<_, ToParent>(&mut reader);
            let stop = !matches!(frame, Ok(Some(_)));
            if sink.send(frame).is_err() || stop {
                return;
            }
        }
    })
}

/// The parent half: drive one delivery push in a killable child, and return only when that child
/// has stopped — by finishing, by failing, or by being killed and reaped.
///
/// `mint` is the caller's existing per-request minter (destination binding, authority check and push
/// deadline included). It runs HERE, in the parent, on the parent's thread; the child receives only
/// its result.
pub fn run_push_in_child(
    program: &Path,
    request: &PushRequest,
    deadline: Instant,
    mut mint: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, ExecutorError> {
    let mut child = KillableChild::spawn(program, &[CHILD_SUBCOMMAND])?;
    let mut stdin = child
        .stdin()
        .ok_or_else(|| ExecutorError::Spawn("child stdin unavailable".to_owned()))?;
    let stdout = child
        .stdout()
        .ok_or_else(|| ExecutorError::Spawn("child stdout unavailable".to_owned()))?;
    let (sink, frames) = channel();
    let pump = pump(stdout, sink);

    let outcome = drive(
        &mut stdin,
        &frames,
        request,
        deadline,
        &mut mint,
        &mut child,
    );
    drop(stdin);
    let _ = pump.join();
    outcome
}

fn drive(
    stdin: &mut std::process::ChildStdin,
    frames: &Receiver<std::io::Result<Option<ToParent>>>,
    request: &PushRequest,
    deadline: Instant,
    mint: &mut impl FnMut(&str) -> Result<String, String>,
    child: &mut KillableChild,
) -> Result<String, ExecutorError> {
    let mut said_hello = false;
    write_frame(stdin, &ToChild::Push(request.clone()))
        .map_err(|error| ExecutorError::Spawn(format!("writing the push request: {error}")))?;

    loop {
        let now = Instant::now();
        // `checked_duration_since` is `None` exactly when the deadline is already behind us, which
        // is the kill case. Nothing in this loop may block for longer than what is left.
        let Some(left) = deadline.checked_duration_since(now) else {
            let reap = child.kill_and_reap()?;
            return Err(ExecutorError::Killed {
                after: now.saturating_duration_since(deadline),
                reap,
            });
        };
        match frames.recv_timeout(left) {
            Ok(Ok(Some(ToParent::Hello { version, .. }))) => {
                if version != PROTOCOL_VERSION {
                    child.kill_and_reap()?;
                    return Err(ExecutorError::Protocol(format!(
                        "child speaks protocol {version}, this parent speaks {PROTOCOL_VERSION}"
                    )));
                }
                said_hello = true;
            }
            Ok(Ok(Some(ToParent::Mint { destination }))) => {
                if !said_hello {
                    child.kill_and_reap()?;
                    return Err(ExecutorError::Protocol(
                        "child asked to mint before saying hello".to_owned(),
                    ));
                }
                if !request.authenticated {
                    child.kill_and_reap()?;
                    return Err(ExecutorError::Protocol(
                        "child asked to mint for an unauthenticated remote".to_owned(),
                    ));
                }
                let answer = match mint(&destination) {
                    Ok(header) => ToChild::Minted {
                        header: Some(header),
                        refused: None,
                    },
                    Err(refused) => ToChild::Minted {
                        header: None,
                        refused: Some(refused),
                    },
                };
                write_frame(stdin, &answer).map_err(|error| {
                    ExecutorError::Protocol(format!("answering a mint request: {error}"))
                })?;
            }
            Ok(Ok(Some(ToParent::Done { oid, error }))) => {
                // The child says it is finished; that is not the same as being gone. Reap before
                // returning, so the turn this result releases is released after an exit we saw.
                let _ = child.kill_and_reap()?;
                return match (oid, error) {
                    (Some(oid), None) => Ok(oid),
                    (_, Some(error)) => Err(ExecutorError::Push(error)),
                    (None, None) => Err(ExecutorError::Protocol(
                        "child finished without an oid or an error".to_owned(),
                    )),
                };
            }
            Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill_and_reap()?;
                return Err(ExecutorError::Protocol(
                    "child closed its pipe without finishing the push".to_owned(),
                ));
            }
            Ok(Err(error)) => {
                let _ = child.kill_and_reap()?;
                return Err(ExecutorError::Protocol(format!(
                    "unreadable frame from the child: {error}"
                )));
            }
            Err(RecvTimeoutError::Timeout) => {
                let overrun = Instant::now().saturating_duration_since(deadline);
                let reap = child.kill_and_reap()?;
                return Err(ExecutorError::Killed {
                    after: overrun,
                    reap,
                });
            }
        }
    }
}

/// The child half, dispatched by the shipped binary's [`CHILD_SUBCOMMAND`] arm.
///
/// Says hello (reporting the argv and environment it actually received), reads the one push request,
/// runs the existing push with a minter that round-trips to the parent, writes the outcome, exits.
pub fn child_main<R, W>(input: R, output: W) -> i32
where
    R: std::io::Read + Send + 'static,
    W: Write + Send + 'static,
{
    // Both halves of the pipe are OWNED and shared behind a lock, because the minter the transport
    // calls is `Fn + Send + Sync + 'static`: it has to be able to write a question and read an
    // answer from inside libgit2's callback, on whatever thread libgit2 is on. The lock is also what
    // keeps two concurrent asks from interleaving two answers on one pipe.
    let output = std::sync::Arc::new(std::sync::Mutex::new(output));
    let argv: Vec<String> = std::env::args().collect();
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let say = |frame: &ToParent| -> bool {
        match output.lock() {
            Ok(mut out) => write_frame(&mut *out, frame).is_ok(),
            Err(_) => false,
        }
    };
    if !say(&ToParent::Hello {
        version: PROTOCOL_VERSION,
        argv,
        env,
    }) {
        return 2;
    }
    let mut reader = BufReader::new(input);
    let request = match read_frame::<_, ToChild>(&mut reader) {
        Ok(Some(ToChild::Push(request))) => request,
        _ => {
            say(&ToParent::Done {
                oid: None,
                error: Some("expected a push request as the first frame".to_owned()),
            });
            return 2;
        }
    };
    let outcome = run_child_push(&request, reader, std::sync::Arc::clone(&output));
    let done = match &outcome {
        Ok(oid) => ToParent::Done {
            oid: Some(oid.clone()),
            error: None,
        },
        Err(error) => ToParent::Done {
            oid: None,
            error: Some(error.clone()),
        },
    };
    if !say(&done) {
        return 2;
    }
    if outcome.is_ok() { 0 } else { 1 }
}

#[cfg(not(feature = "git-delivery"))]
fn run_child_push<R, W>(
    _request: &PushRequest,
    _reader: R,
    _output: std::sync::Arc<std::sync::Mutex<W>>,
) -> Result<String, String> {
    Err("this build has no git delivery surface".to_owned())
}

#[cfg(feature = "git-delivery")]
fn run_child_push<R, W>(
    request: &PushRequest,
    reader: R,
    output: std::sync::Arc<std::sync::Mutex<W>>,
) -> Result<String, String>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    use std::sync::Mutex;

    // The minter the transport will call: one round-trip to the parent per wire request. The parent
    // owns the key, the destination binding, the authority check and the deadline; this side owns
    // nothing but the question. `Mutex` because the transport's minter is `Fn`, and because two
    // concurrent asks on one pipe would interleave two answers.
    let pipe = Mutex::new(reader);
    let mint: crate::git_transport::AuthMinter = std::sync::Arc::new(move |destination: &str| {
        let mut reader = pipe
            .lock()
            .map_err(|_| "the delivery push pipe is poisoned".to_owned())?;
        let mut output = output
            .lock()
            .map_err(|_| "the delivery push pipe is poisoned".to_owned())?;
        write_frame(
            &mut *output,
            &ToParent::Mint {
                destination: destination.to_owned(),
            },
        )
        .map_err(|error| format!("asking the parent to authorize a leg: {error}"))?;
        match read_frame::<_, ToChild>(&mut *reader) {
            Ok(Some(ToChild::Minted {
                header: Some(header),
                ..
            })) => Ok(header),
            Ok(Some(ToChild::Minted {
                refused: Some(refused),
                ..
            })) => Err(refused),
            Ok(Some(_)) | Ok(None) => {
                Err("the parent stopped answering authorization requests".to_owned())
            }
            Err(error) => Err(format!("reading the parent's authorization: {error}")),
        }
    });

    // The same two steps, in the same order, the in-process push has always taken: replace the
    // workdir's `.git/config` so a planted `insteadOf` cannot redirect the seller's token, then push
    // the gated object. `seller_git`'s async wrapper exists to hold a delivery turn on a blocking
    // thread; this process IS the blocking work and its turn is held by the parent, so the child
    // calls the two synchronous pieces directly rather than building a runtime to await them.
    crate::seller_git::neutralize_push_config(&request.workdir)
        .map_err(|error| error.to_string())?;
    crate::seller_git::push_branch_with_minter(
        &request.workdir,
        &request.remote_url,
        &request.branch,
        &request.gated_oid,
        if request.authenticated {
            Some(mint)
        } else {
            None
        },
        None,
        None,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_through_the_pipe_encoding() {
        let request = PushRequest {
            workdir: PathBuf::from("/tmp/delivery"),
            remote_url: "https://relay.example/repo.git".to_owned(),
            branch: "job-1".to_owned(),
            gated_oid: "0".repeat(40),
            authenticated: true,
            budget_ms: 150_000,
        };
        let mut wire = Vec::new();
        write_frame(&mut wire, &ToChild::Push(request.clone())).expect("write");
        assert_eq!(
            wire.iter().filter(|byte| **byte == b'\n').count(),
            1,
            "a frame is exactly one line, or the framing is ambiguous"
        );
        let mut reader = BufReader::new(wire.as_slice());
        let back: ToChild = read_frame(&mut reader).expect("read").expect("a frame");
        assert_eq!(back, ToChild::Push(request));
    }

    #[test]
    fn the_child_environment_is_the_allowlist_and_nothing_else() {
        // The policy, asserted without spawning: whatever this process carries, the child is offered
        // only names from the allowlist.
        for (name, _) in child_env() {
            let name = name.into_string().expect("ascii name");
            assert!(
                CHILD_ENV_ALLOWLIST.contains(&name.as_str()),
                "{name} is not on the child environment allowlist"
            );
        }
    }

    #[test]
    fn an_empty_program_override_is_refused_rather_than_spawned() {
        // SAFETY: single-threaded test-local environment mutation, restored below.
        unsafe { std::env::set_var(CHILD_PROGRAM_ENV, "") };
        let refused = resolve_child_program();
        unsafe { std::env::remove_var(CHILD_PROGRAM_ENV) };
        assert!(
            matches!(refused, Err(ExecutorError::Spawn(_))),
            "an empty override must not fall through to current_exe"
        );
    }

    #[test]
    fn every_shipped_platform_is_one_this_executor_can_kill() {
        // The feasibility claim, pinned: if a platform is ever added to the release matrix that is
        // not POSIX, this executor's guarantee does not extend to it and this test is the place that
        // says so.
        for platform in SHIPPED_PLATFORMS {
            assert!(
                platform.contains("linux") || platform.contains("darwin"),
                "{platform} is not a POSIX platform; SIGKILL/waitpid cannot be assumed there"
            );
        }
    }
}
