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
//! Two properties follow, and both are load-bearing. Stated as narrowly as they are true:
//!
//! - **The key never leaves the actor.** What crosses the pipe is a minted header, so the child's
//!   custody is over TOKENS, not over the key. Each token is short-lived and scoped to this job's
//!   ref and destination.
//! - **The parent's deadline bounds what the child can still be GIVEN.** A child past its deadline
//!   cannot obtain a new header, so it cannot begin an authenticated leg it has not already been
//!   authorized for. The kill ends the *work*; the minter refusal ends further *authority*.
//!
//! And, just as load-bearing, what those two do NOT say:
//!
//! - **Not "at most one token".** A push authenticates two legs and each can be challenged, so a
//!   child may hold more than one token at once; [`MAX_MINT_REQUESTS`] caps how many it can ever
//!   ask for, which is a bound, not a count of one.
//! - **A token already minted is not recalled.** The parent can refuse the NEXT header; it cannot
//!   reach into the child and invalidate one already handed over. Within that token's short life,
//!   a child that has it can use it. What bounds that window is the token's own scope and lifetime
//!   plus the kill — not a revocation that travels backwards.
//! - **The check-to-send window is bounded, not zero.** The parent answers a child's authority check
//!   with the truth at the moment it writes the answer; the child transmits some time after reading
//!   it. The parent re-asks the owner every [`CANCELLATION_POLL`] while the child runs and kills on
//!   a revocation, so that window is bounded by the poll interval instead of by the deadline. It is
//!   not an atomic fence at the wire, and this module does not claim one.
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
//! | 12 | cleanup: wait for the stdout reader to reach END OF FILE | parent | [`REAP_BOUND`], applied on every path through that loop |
//! | 13 | the turn is dropped, the lock is free | parent | — |
//!
//! Steps 10–11 run on **every** exit path, including success, error, panic and an early return,
//! because they are a `Drop` (see [`KillableChild`]). A kill that is merely *issued* releases
//! nothing: [`KillableChild::reap`] returns only when the kernel has reported the child's exit
//! status, which it does only once the process is gone.
//!
//! **Step 12 is a wait, not a join, and what it establishes is exactly one fact.** The reader thread
//! is never joined — joining a thread parked in a read that only ends when the last holder of the
//! write end lets go is the unbounded phase this module exists to remove. Instead the parent waits,
//! under [`REAP_BOUND`], for that reader to report [`PumpEnd::Eof`]: the kernel returning zero
//! bytes, which it does only once every holder of that descriptor has closed it. Anything else — the
//! bound expiring, the read failing, a malformed frame — is NOT that fact and is reported as its own
//! outcome ([`ExecutorError::CleanupUnbounded`], [`ExecutorError::CleanupUnobserved`]), both of
//! which retain the seat.
//!
//! EOF is evidence about a DESCRIPTOR, not a census of processes. A descendant that closes this one
//! descriptor and keeps running produces the same EOF, and nothing here detects it. The claim is
//! "the pipe this delivery wrote on has no holders left", not "every process this delivery started
//! is gone".
//!
//! Writer, minter and stderr-relay threads are likewise **detached, not joined**: each is bounded by
//! this deadline for the purpose of the parent's own progress, and an arbitrary minter that never
//! answers can outlive the delivery. The production minter carries its own deadline. What is
//! established is that the PARENT returns and the direct child is gone — not that every thread this
//! delivery started has ended.
//!
//! # What the bound guarantees, and under which assumptions
//!
//! The kill lands at the **caller's absolute deadline** — whatever the delivery arm passed in, which
//! for the production path is `DELIVERY_DRAIN_BOUND` (150s work deadline + 120s for one in-flight
//! leg) from when that delivery started. It is not a fresh 270s measured from the spawn, and a
//! delivery handed a shorter deadline is killed at the shorter one. On top of it: [`REAP_BOUND`]
//! for the reap, and a further [`REAP_BOUND`] for step 12.
//!
//! This is a **conditional** bound and is documented as one. What holds it up:
//!
//! - **`SIGKILL` cannot be caught, blocked or ignored** (POSIX, and all three platforms this product
//!   ships for are POSIX — see [`SHIPPED_PLATFORMS`]). No amount of libgit2 or C code in the child
//!   can decline it. This is the property in-process cancellation could not have at any price.
//! - **The kill goes to the process GROUP** (`kill(-pgid)`), and the child is made a group leader at
//!   spawn, so a descendant that is still IN that group is signalled with it — though libgit2 spawns
//!   none today. A descendant that left the group first (its own `setsid`/`setpgid`) is not reached
//!   by that signal, is not waited for, and is not claimed to be gone; step 12's EOF wait is what
//!   notices one still holding the stdout pipe, and even that only while it holds it.
//! - **The child's own budget is the parent's remaining time at the instant the request is
//!   written, minus the pipe transit.** The parent stamps both a remaining duration and the same
//!   deadline as an absolute wall-clock instant; the child subtracts its own `now` from the second
//!   and takes whichever of the two is smaller. The transit between the parent's write and the
//!   child's read is therefore charged to the child instead of granted to it, and a wall clock
//!   stepped backward cannot lift the child past the duration ceiling. What is NOT claimed: this is
//!   one host's clock, not a synchronised one, and the budget is still only what lets the child
//!   refuse work it cannot finish — the parent's kill is what actually bounds it.
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
//! bound on a machine whose scheduler has stopped running this process.
//!
//! And not `deadline + REAP_BOUND` for the SEAT. That sentence used to stand here and it was
//! wider than the code: releasing the seat needs two separate windows, not one. The kill and the
//! confirmed exit are bounded by [`REAP_BOUND`]; observing end of file on the child's stdout is a
//! SECOND window of up to [`REAP_BOUND`] which starts after the reap, because a descriptor that
//! something else inherited stays open after the process we killed is gone. Stating one bound for
//! two consecutive waits understated the worst case by a whole reap bound.
//!
//! The claims that hold, each said only as wide as it is. `S` below is the SYNCHRONOUS SUPERVISOR
//! TIME defined under "the phases no clock here interrupts"; it is an additive term, not a timer:
//!
//! * **The SIGNAL — and this one no longer carries `S` at all.** Within
//!   `deadline + WATCHDOG_TICK + w` the child's process group has been sent `SIGKILL`, where `w` is
//!   the time this OS takes to wake a sleeping thread and deliver a signal. The kill is issued by
//!   [`KillableChild::arm_deadline_watchdog`]'s thread, which is armed the instant a pid exists and
//!   whose entire body is sleep-wake-signal: it never encodes a frame, never decodes one, never
//!   calls the minter, never asks the owner anything, and is not behind the spawn. So the instant of
//!   the kill does not depend on where the supervisor thread is, which is what `S` measured.
//!
//!   ASSUMPTIONS, STATED RATHER THAN HIDDEN. This is a claim about a machine that is still
//!   scheduling this process: that a sleeping thread whose sleep has expired is eventually run, and
//!   that `SIGKILL` to a process group is delivered. No number is printed for `w` and no universal
//!   OS guarantee is claimed for it — on a machine that has stopped scheduling this process, or
//!   against a child in uninterruptible sleep (see above), this bound is late by that stall like
//!   every other bound here. What IS claimed, and could not be claimed before, is the SHAPE: the
//!   term is scheduler latency, and it does not grow with the size of a frame, the cost of the
//!   owner's check, or how long a signer takes to answer.
//! * **The child.** Within `deadline + WATCHDOG_TICK + w + REAP_BOUND + S` the child process has
//!   been killed and its exit confirmed, or the executor says it could not confirm it and the seat
//!   stays held. `S` survives HERE and honestly so: confirming an exit and reporting it is the
//!   supervisor's job, and the supervisor still has to reach it. What changed is that the child is
//!   no longer RUNNING during that `S` — it was signalled at the first bullet's bound.
//! * **The seat.** Within `deadline + WATCHDOG_TICK + w + 2 * REAP_BOUND + S` the turn has been
//!   handed on, or it is
//!   retained for the life of this process with the reason named
//!   ([`ExecutorError::Unreaped`], [`ExecutorError::CleanupUnbounded`],
//!   [`ExecutorError::CleanupUnobserved`], [`ExecutorError::WaitFailed`]).
//! * **The same two numbers on the EXCEPTION path.** A child that will not die is killed and waited
//!   for by `drive`, again by the cleanup that normalizes the outcome, and again by
//!   `KillableChild::drop`. Those retries used to start a fresh [`REAP_BOUND`] each, so the
//!   worst case was three reap windows plus the end-of-file window while the text above said two
//!   windows in total. [`REAP_BOUND`] is now a BUDGET PER CHILD: the time already spent waiting for
//!   that child is accumulated, later attempts re-signal, poll the exit once and return. Retrying
//!   changes how certain the outcome is, never the bound.
//!
//! # The phases no clock here interrupts — the term `S`
//!
//! The deadlines above are enforced at WAITS. Between two waits the supervisor thread runs work
//! that nothing in this module can cut short, and honesty requires it be added rather than assumed
//! away. `S` is the total of, per delivery:
//!
//! * **Encode.** Serializing each outbound frame. Bounded by construction at [`MAX_FRAME_BYTES`]:
//!   `encode_frame` serializes into a sink that REFUSES past the cap, so an oversized value is
//!   abandoned mid-encode. Before that the whole value was built and then measured, which made this
//!   phase as large as the value — unbounded work inside a module that claims bounded ones.
//! * **Decode.** Parsing one inbound frame, read under the same cap.
//! * **Spawn.** One `Command::spawn` on the first pass, before `drive` and therefore before any
//!   deadline check can reach it.
//! * **The authority call itself.** Whatever the owner's check costs, once per ask.
//!
//! `S` is NOT given a number here and no OS guarantee is claimed for it. It is the same class of
//! assumption as "this process is still being scheduled": if the machine stalls inside one of those
//! phases, every bound in this module is late by that stall, and the module says so instead of
//! printing a figure it cannot enforce. What IS claimed is that each contributor is either capped
//! by size ([`MAX_FRAME_BYTES`]) or is a single bounded-count operation, and that the number of
//! contributions is finite — mint frames are capped in count by [`MAX_MINT_REQUESTS`] and the
//! parent's queue by [`MAX_QUEUED_FRAMES`].
//! * **Not claimed at all:** that every process which inherited the child's stdout has stopped.
//!   The executor kills the child's process group and then asks whether the pipe closed; if it did
//!   not, that is reported as an unknown and the seat is kept, which is the whole of the answer.
//!   Anything that escaped the group is outside what this module can establish.
//!
//! Revocation is bounded separately and by the parent's own clock. While the child runs, every wait
//! in the delivery — the frame wait, a write the child has not acknowledged, and a mint whose reply
//! the signer is holding — is cut at ONE shared next-ask deadline held in [`PollClock`], and each
//! ask re-arms it. It does not depend on the child being quiet, and a child that floods the parent
//! with frames cannot postpone it.
//!
//! The exact claim, because the obvious stronger one is false: NO WAIT OUTLIVES THE SHARED NEXT
//! ASK. The wall-clock interval between two authority observations is [`CANCELLATION_POLL`] plus
//! the synchronous parent work between one wait returning and the next ask — frame decode, encode
//! and allocation of the next frame, and process spawn on the first pass. That work runs on this
//! thread and no clock inside this module can interrupt it; it is accounted for as the progressing
//! -phase allowance on [`CANCELLATION_POLL`], not hidden inside a flat "every 50 ms". Independent
//! per-wait slices — the previous design — allowed several full intervals to pass between
//! observations while the delivery was making progress; that is what the shared deadline removes.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel, sync_channel};
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

/// How often the deadline watchdog re-checks whether its child is still wanted.
///
/// This is the granularity of the watchdog's wake-up, and therefore the only term this module adds
/// to "the child is signalled at its deadline". It is not a timeout and it is not a retry interval:
/// the watchdog sleeps for whichever is shorter, this tick or the time actually left, so the final
/// sleep ends AT the deadline and this value only bounds how long a finished child leaves the
/// thread parked before it notices it can stop.
///
/// See [`KillableChild::arm_deadline_watchdog`] for the bound it participates in.
pub const WATCHDOG_TICK: Duration = Duration::from_millis(25);

/// The largest frame this protocol will read. A peer that writes without bound is backpressure the
/// reader would otherwise absorb into unbounded memory — and unbounded parent-side buffering is
/// itself a phase outside the drain bound. A frame over this cap is a protocol violation: the child
/// is killed and reaped, not read further.
///
/// 1 MiB because every frame in this protocol is a handful of short fields; the only variable-length
/// members are a workdir path, a remote URL and one NIP-98 header.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// How many frames the parent will hold from the child while it is busy elsewhere — minting, or
/// waiting on a write.
///
/// A per-frame size cap is not a memory bound: a child that writes a million small frames while the
/// supervisor is inside the signer costs the parent unbounded memory, and "the parent's own
/// buffering" is a phase nobody put a limit on. The queue is therefore SYNCHRONOUS and this small:
/// once it is full the pump stops reading, the child's own writes block on pipe backpressure, and
/// the stalled party is the one that can be killed rather than the one holding the kill.
pub const MAX_QUEUED_FRAMES: usize = 64;

/// How much of the child's stderr the parent will relay before it stops reading it. The child's
/// stderr is where the transport prints its overrun and refusal diagnostics; a parent that pipes it
/// and never drains it turns that diagnostic into a stalled child and prints nothing. Relayed, not
/// buffered — and capped, because a child that writes forever must not be able to make the parent
/// print forever.
pub const MAX_CHILD_STDERR_BYTES: u64 = 256 * 1024;

/// Children this process could NOT confirm dead. Incremented when a reap does not complete, and
/// **never decremented** — an unconfirmed child is a permanent fact about this process, not a
/// transient one.
///
/// This exists because `Drop` cannot report. Every other path returns [`ExecutorError::Unreaped`] to
/// a caller that must retain exclusion; a drop on a panic or an early return has nowhere to return
/// it to, and swallowing it silently would be exactly the defect this module exists to remove: a
/// seat handed on while work may still be running. A seat consults [`unconfirmed_children`] before
/// treating its delivery lane as free.
static UNCONFIRMED_CHILDREN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many delivery-push children this process started and could not confirm had exited. Non-zero
/// means at least one delivery lane must stay closed for the life of this process.
pub fn unconfirmed_children() -> usize {
    UNCONFIRMED_CHILDREN.load(std::sync::atomic::Ordering::SeqCst)
}

/// Record a child whose exit this process could not establish.
///
/// The one place the count moves, so the fact and its consumer can be exercised as a pair rather
/// than asserted about each other. Its production caller is [`KillableChild::drop`]; its production
/// consumer is the delivery-push dispatch in `seller_git`, which refuses to start new work while
/// this is non-zero.
pub fn record_unconfirmed_child() {
    UNCONFIRMED_CHILDREN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Whether this seat's delivery turn may be released, given what the reap actually established.
///
/// The whole fail-closed rule in one place, so it can be tested as a rule rather than inferred from
/// the paths that happen to call it. **Unknown exit is treated as still running.** A silent child is
/// not a dead child; a kill that was issued is not an exit that was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// The kernel reported the child's exit. The work has stopped; the turn may go.
    Release,
    /// Exit unknown or unconfirmed. The turn is RETAINED. This sacrifices liveness on this seat
    /// deliberately, and it is named as that rather than described as recovery.
    Retain,
}

/// The rule: release only on a confirmed exit.
pub fn exclusion_after_reap(reap: &Result<Duration, ExecutorError>) -> Exclusion {
    match reap {
        Ok(_) => Exclusion::Release,
        Err(_) => Exclusion::Retain,
    }
}

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

/// How long the parent will sit in ONE wait for a frame before it re-asks the owner whether this
/// delivery is still authorized.
///
/// The parent's answer to a child's authority check is true when it is written, and the child reads
/// it some unbounded time later: between those two moments the owner can go away, and nothing on
/// this side was looking. The kill is what ends that window, and the kill used to be driven only by
/// the DEADLINE — so a revocation with 140 seconds left on the clock was not acted on until the
/// clock ran out. Polling here does not make the check-to-send window zero-width, and nothing in
/// this module claims it does: it makes that window bounded by this interval instead of by the
/// deadline.
pub const CANCELLATION_POLL: Duration = Duration::from_millis(50);

/// How many authorizations one child may ask this parent to mint.
///
/// A push makes two authenticated legs — the advertisement `GET` and the pack `POST` — and the
/// transport can be challenged once on each, so four is the most the shipped child needs. The count
/// used to be unbounded, which is why "a compromised child holds at most one token" was not a
/// property of anything: nothing stopped it asking again. The headroom above four is for a
/// challenge the transport retries, not for a child that keeps asking.
pub const MAX_MINT_REQUESTS: u32 = 8;

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
    /// The answer to a [`ToParent::Check`]: the parent's LIVE answer, at the moment it was asked,
    /// to "may this delivery still transmit?". `refused` set is an end of authority, and the child
    /// must not transmit.
    Authority { refused: Option<String> },
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
    /// The child is about to transmit and is asking the parent, ACROSS THE PIPE, whether this
    /// delivery still owns its turn.
    ///
    /// This frame exists because the parent's own authority check is a check the parent makes about
    /// a moment the parent chooses. Between the parent approving a mint and the child reaching
    /// `send`, the owner can go away; a check the child never makes is not a boundary at the
    /// child's submission. The child asks here, immediately before the request leaves it.
    Check { phase: String },
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
    ///
    /// This is a CEILING, not the budget. On its own it hands the child the pipe transit for free:
    /// the child starts counting when it READS, and everything between the parent's write and that
    /// read is time the parent has already spent. [`Self::deadline_unix_ms`] is what removes that.
    pub budget_ms: u64,
    /// The same deadline as an absolute wall-clock instant — UNIX epoch milliseconds — stamped at
    /// the same moment `budget_ms` is measured.
    ///
    /// `Instant` has no meaning across processes, but parent and child are the same host and read
    /// the same clock, so this one does: the child subtracts its OWN `now` and the pipe transit is
    /// accounted for rather than granted. The child takes the MINIMUM of this and `budget_ms`, so a
    /// wall clock stepped BACKWARD between the two reads cannot extend the child past the ceiling;
    /// a forward step only shortens it. Neither replaces the parent's kill.
    pub deadline_unix_ms: u64,
}

/// Wall-clock `now` in UNIX epoch milliseconds, saturating rather than panicking on a clock before
/// the epoch. Used only for the cross-process deadline, never for measuring an interval.
pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// How long the CHILD may run, decided at the instant it reads the request.
///
/// The whole point is the `min`. `budget_ms` alone restarts the clock at the read, so the pipe
/// transit — the parent's write, the scheduler, the child's read — was time the parent had spent
/// and the child was handed anyway. The absolute deadline removes exactly that interval, because
/// both processes read one host clock. Keeping the duration as a ceiling is what makes a wall clock
/// stepped BACKWARD between the two reads unable to extend the child; a forward step only shortens
/// it, which fails safe. Neither is the real bound: the parent's kill is.
pub(crate) fn child_budget(request: &PushRequest, now_ms: u64) -> Duration {
    let by_ceiling = request.budget_ms;
    let by_clock = request.deadline_unix_ms.saturating_sub(now_ms);
    Duration::from_millis(by_ceiling.min(by_clock))
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
    /// The child was reaped, but the read end of its pipe was STILL HELD [`REAP_BOUND`] later —
    /// which means something that inherited it outlived the process group we killed. We cannot say
    /// the delivery's local phase is over, so the turn is still held.
    CleanupUnbounded { waited: Duration },
    /// The stdout pump stopped for a reason that is NOT end of file, so this parent never observed
    /// the write end of the child's stdout being released. A reader that stopped is not a pipe that
    /// closed: the descriptor's remaining owners are unaccounted for, which is the same unknown the
    /// other retaining variants describe. Kept separate from [`Self::CleanupUnbounded`] because the
    /// two are different facts — one is "still held after the bound", the other is "we stopped
    /// looking" — and a reader who has to act on them needs to know which happened.
    CleanupUnobserved { why: String },
    /// The owner revoked this delivery, or its lifetime ended, while the child was running. The
    /// child was killed and its exit was CONFIRMED, so this releases the turn — it is a stop, not
    /// an unknown. Separate from [`Self::Killed`] because a revocation is not a deadline breach and
    /// reporting it as one misstates why the work ended.
    Revoked { why: String, reap: Duration },
    /// The kernel refused to tell us whether the child exited (`waitpid` itself failed). This is an
    /// UNKNOWN exit, not a protocol fault: nothing about the child's behaviour is implicated, and
    /// nothing about its death is established. It is separate from [`Self::Protocol`] precisely so
    /// that the release rule can treat it as "still running" — a `waitpid` error folded into a
    /// protocol error is an unknown exit wearing a releasable name.
    WaitFailed { why: String },
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
            Self::CleanupUnbounded { waited } => write!(
                f,
                "delivery push child was reaped but its output pipe was still held {}ms later, so \
                 something outlived its process group; this seat stays held rather than hand the \
                 turn to a second delivery while the first may still be touching the workdir",
                waited.as_millis()
            ),
            Self::CleanupUnobserved { why } => write!(
                f,
                "delivery push child was reaped but this parent never observed end of file on its \
                 stdout ({why}), so the remaining owners of that pipe are unaccounted for; this \
                 seat stays held rather than hand the turn to a second delivery"
            ),
            Self::Revoked { why, reap } => write!(
                f,
                "delivery push was revoked while its child was running ({why}); the child was \
                 killed and the kernel confirmed the exit {}ms later",
                reap.as_millis()
            ),
            Self::WaitFailed { why } => write!(
                f,
                "delivery push child's exit could not be established ({why}); this seat stays held \
                 rather than hand the turn to a second delivery on an exit nobody observed"
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
///
/// **The override is honoured in every build, and that is a limitation, not a guarantee.** This is
/// not "`current_exe` only": a process whose environment carries [`CHILD_PROGRAM_ENV`] delivers with
/// the program named there. What is enforced is the weaker, checkable thing — the override must be
/// an ABSOLUTE path to a file that exists. A relative path resolved against a working directory this
/// process does not control is the `PATH` hole again in a different spelling, and it used to be
/// accepted. Anyone who can set this parent's environment can already do worse to it; the honest
/// claim is that a delivery cannot be redirected by the *ambient* filesystem, not that it cannot be
/// redirected at all.
pub fn resolve_child_program() -> Result<PathBuf, ExecutorError> {
    if let Some(explicit) = std::env::var_os(CHILD_PROGRAM_ENV) {
        return child_program_from_override(&explicit);
    }
    std::env::current_exe()
        .map_err(|error| ExecutorError::Spawn(format!("current_exe is unreadable: {error}")))
}

/// What [`CHILD_PROGRAM_ENV`] is allowed to name. Separated from the lookup so the POLICY can be
/// asserted directly, rather than through a test that has to mutate this process's environment
/// while other tests are reading it.
pub fn child_program_from_override(raw: &std::ffi::OsStr) -> Result<PathBuf, ExecutorError> {
    let path = PathBuf::from(raw);
    if path.as_os_str().is_empty() {
        return Err(ExecutorError::Spawn(format!(
            "{CHILD_PROGRAM_ENV} is set to an empty path"
        )));
    }
    if !path.is_absolute() {
        return Err(ExecutorError::Spawn(format!(
            "{CHILD_PROGRAM_ENV} is set to the relative path {}; a delivery child is resolved from \
             an absolute path or not at all",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(ExecutorError::Spawn(format!(
            "{CHILD_PROGRAM_ENV} names {}, which is not a file",
            path.display()
        )));
    }
    Ok(path)
}

/// The environment the child will be given: the allowlist, and only the entries of it this process
/// actually has. Separated from the spawn so a test can assert the *policy* without spawning.
pub fn child_env() -> Vec<(OsString, OsString)> {
    CHILD_ENV_ALLOWLIST
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .collect()
}

/// How much of [`REAP_BOUND`] is still available for a child that has already been waited for
/// `spent`.
///
/// The window belongs to the CHILD, not to the call. `drive` kills it, the cleanup that follows
/// normalizes the outcome, and `Drop` kills again behind every return: three callers, and when each
/// started a fresh [`REAP_BOUND`] an unconfirmed exit cost three full windows plus the end-of-file
/// window, against a module header advertising two windows in total. Charging every attempt against
/// one budget is what makes the advertised number the real one.
fn reap_window_left(spent: Duration) -> Duration {
    REAP_BOUND.saturating_sub(spent)
}

/// **This process observed the delivery's exit.** Handed to [`KillableChild`] by the seat, and
/// fired from whichever thread actually saw the kernel report the exit.
///
/// It is a callback rather than a direct call into the turn because the confirmation has to be
/// publishable from a thread that owns none of the delivery's state — the deadline watchdog — and
/// this module must not have to know what a turn is in order to let it.
///
/// FIRED ONLY FROM AN OBSERVED EXIT. A signal issued, a deadline passed, and a reap budget spent
/// without an answer all leave it unfired, because each of those is an UNKNOWN exit and the seat's
/// rule for an unknown exit is to retain.
pub type ExitConfirmation = std::sync::Arc<dyn Fn() + Send + Sync>;

/// A spawned child that **cannot be forgotten**. Dropping it kills the process group and waits for
/// the exit; there is no path out of this module that leaves a delivery packing behind us.
///
/// The child handle itself lives in [`ExitGuard`], not here: see that type for why the supervisor
/// is no longer the only thread that can confirm an exit.
pub struct KillableChild {
    pid: i32,
    /// Shared with the deadline watchdog, and the owner of the child handle. See [`ExitGuard`].
    guard: std::sync::Arc<std::sync::Mutex<ExitGuard>>,
    /// Set by the watchdog when IT issued the kill, for the operator line and for tests that need
    /// to know which side stopped the child.
    watchdog_fired: std::sync::Arc<AtomicBool>,
}

/// The interlock between the supervisor and the deadline watchdog — and the OWNER of the child.
///
/// A pid is only safe to signal until it has been reaped; afterwards the number can be reused by an
/// unrelated process, and a late `SIGKILL` would land on a stranger. Both sides therefore go
/// through this mutex: a `try_wait` only happens while holding it and sets `disarmed` in the same
/// critical section as a successful reap, and the watchdog only signals while holding it and only
/// when `disarmed` is still false. There is no window between "the kernel reaped the pid" and "the
/// watchdog knows", because the two are one locked section.
///
/// # Why the child handle moved in here
///
/// It used to live on [`KillableChild`], which is owned by the supervisor's stack. That made
/// `Child::try_wait` — the ONLY call that can turn a kill into a confirmed exit — reachable from
/// exactly one thread: the synchronous executor. The watchdog could stop a child on time and still
/// leave the seat blocked forever, because a supervisor stalled anywhere between arming and its
/// reap (cloning the request, calling the owner's authority check) never got to the `try_wait`, and
/// the seat's rule requires an OBSERVED exit before it hands on. The kill was independent of that
/// stall and the confirmation was not.
///
/// Behind this mutex the handle belongs to whichever thread reaches it first. The supervisor still
/// reaps on its normal path; the watchdog reaps when it had to kill. The same guard that already
/// made a late signal impossible is what makes two reapers safe, and `confirm` fires exactly once
/// because it is TAKEN by the observer.
struct ExitGuard {
    pid: i32,
    /// True once this pid has been reaped, or once the child is otherwise known finished. A
    /// disarmed guard never signals again.
    disarmed: bool,
    /// The spawned child. `None` once it has been taken for a wait that consumed it, or when this
    /// guard never had one.
    child: Option<Child>,
    /// True once the kernel has reported this child's exit to this process.
    reaped: bool,
    /// Time already spent waiting for THIS child's exit, across every reap attempt made on it by
    /// EITHER thread. [`REAP_BOUND`] is charged against this total rather than against one call, so
    /// the retries on the failing path cannot multiply the advertised window — and so the watchdog
    /// reaping cannot buy a second window the seat was never promised. See `kill_and_reap`.
    spent_reaping: Duration,
    /// The seat's confirmation sink, taken by whichever thread observes the exit.
    confirm: Option<ExitConfirmation>,
}

impl ExitGuard {
    /// `SIGKILL` the process GROUP and then the process, if this guard is still armed.
    ///
    /// Returns whether a signal was issued. `ESRCH` is not an error here: it means the group is
    /// already gone, which is the outcome being asked for.
    fn kill_if_armed(&self) -> bool {
        if self.disarmed {
            return false;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
            libc::kill(self.pid, libc::SIGKILL);
        }
        true
    }

    /// ONE poll of the child's exit, under this guard, from whichever thread holds it.
    ///
    /// Returns the poll's outcome and — only when the kernel actually reported an exit — the seat's
    /// confirmation sink, TAKEN so that it can fire exactly once no matter how many threads poll.
    ///
    /// The sink is returned rather than called here on purpose: firing it runs seat code that takes
    /// the turn's own locks, and this executor must never hold its child guard across a foreign
    /// callback. Every caller fires it after releasing this lock.
    fn observe_exit(
        &mut self,
    ) -> (
        std::io::Result<Option<std::process::ExitStatus>>,
        Option<ExitConfirmation>,
    ) {
        let Some(child) = self.child.as_mut() else {
            // No handle: nothing this guard can observe, and nothing it may claim. `disarmed` stops
            // the signalling, but the exit stays UNCONFIRMED and the sink stays unfired.
            self.disarmed = true;
            return (Ok(None), None);
        };
        let outcome = child.try_wait();
        if matches!(outcome, Ok(Some(_))) {
            // Reaped and disarmed in the same critical section, as before — plus the confirmation,
            // which is now published from here rather than from the supervisor's return path.
            self.disarmed = true;
            self.reaped = true;
            return (outcome, self.confirm.take());
        }
        (outcome, None)
    }
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
            pid,
            guard: std::sync::Arc::new(std::sync::Mutex::new(ExitGuard {
                pid,
                disarmed: false,
                child: Some(child),
                reaped: false,
                spent_reaping: Duration::ZERO,
                confirm: None,
            })),
            watchdog_fired: std::sync::Arc::new(AtomicBool::new(false)),
        })
    }

    /// Give this child's OBSERVED exit somewhere to go that is not the supervisor's return value.
    ///
    /// Install before arming. Whichever thread first sees the kernel report this child's exit fires
    /// `confirm` — the supervisor on its ordinary path, or the deadline watchdog when the
    /// supervisor never got there. It fires at most once.
    ///
    /// This is the seat's independence from a stalled synchronous executor, and it is deliberately
    /// narrow: it publishes an exit this process WATCHED happen. It is not reachable from a
    /// deadline, from a signal, or from a caller that gave up.
    pub fn publish_confirmed_exit_to(&self, confirm: ExitConfirmation) {
        let mut state = self
            .guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.confirm = Some(confirm);
    }

    /// Hand this child's absolute deadline to a thread of its own.
    ///
    /// THE KILL STOPS BEING SOMETHING THE SUPERVISOR HAS TO REACH. Before this existed, the signal
    /// was issued by the supervisor loop, so it was behind every synchronous section that loop runs
    /// between waits — the `S` term documented at the top of this module. `S` has no number: the
    /// spawn happens before `drive` is even entered, and the authority check is whatever the owner's
    /// closure costs. A stop that is late by `S` is a child still writing to the seat's workdir
    /// after its turn ended, which is the defect this module exists to remove.
    ///
    /// The watchdog holds the same absolute deadline and nothing else. It sleeps, wakes, and
    /// signals — it never encodes a frame, never decodes one, never calls the minter and never asks
    /// the owner anything. So the instant the child is signalled does not depend on where the
    /// supervisor is, only on this thread being scheduled.
    ///
    /// # The bound, and what it assumes
    ///
    /// Once armed, the child's process group is signalled within `deadline + WATCHDOG_TICK + w`,
    /// where `w` is the time the OS takes to wake a sleeping thread and deliver a signal on a
    /// machine that is still scheduling this process. `w` is NOT a guarantee this module can make —
    /// it is the same assumption as "this process still runs at all" — and no figure is printed for
    /// it. What IS claimed, and what the previous formulation could not claim, is that the term is
    /// scheduler latency rather than `S`: it does not grow with the size of a frame, the cost of the
    /// owner's check, or how long a signer takes to answer.
    ///
    /// Arming is deliberately at the EARLIEST point a pid exists. The supervisor's own kill stays
    /// exactly where it is: the watchdog bounds when the child is SIGNALLED, and the supervisor
    /// still owns confirming the exit, reporting it, and the custody decision when it cannot.
    pub fn arm_deadline_watchdog(&self, deadline: Instant) {
        let guard = std::sync::Arc::clone(&self.guard);
        let fired = std::sync::Arc::clone(&self.watchdog_fired);
        std::thread::spawn(move || {
            loop {
                // Sliced rather than one long sleep, so a child that finishes normally stops this
                // thread promptly instead of leaving it parked until a deadline nobody needs.
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(left.min(WATCHDOG_TICK));
                let Ok(state) = guard.lock() else { return };
                if state.disarmed {
                    return;
                }
            }
            // The deadline has passed. Signal under the lock, so this cannot race a reap that is
            // happening right now and land on a recycled pid.
            {
                let Ok(mut state) = guard.lock() else { return };
                if state.kill_if_armed() {
                    fired.store(true, Ordering::SeqCst);
                }
                // Poll once while we already hold the lock: a child that was killed before the
                // supervisor ever wrote to it is usually already gone by now.
                let (outcome, confirm) = state.observe_exit();
                drop(state);
                if let Some(confirm) = confirm {
                    confirm();
                    return;
                }
                if !matches!(outcome, Ok(None)) {
                    // Reaped by the other side, or an error that makes this exit UNKNOWN. Either
                    // way there is nothing further this thread may claim.
                    return;
                }
            }
            // THE KILL IS NOT THE CONFIRMATION, AND THIS THREAD NOW OWNS BOTH.
            //
            // Signalling on time never made the seat safe to hand on: the seat's rule is that this
            // process must have OBSERVED the exit, and the only call that observes it is a
            // `try_wait` on the child handle. While that handle lived on the supervisor's stack,
            // this thread could stop a delivery punctually and still leave the seat blocked for as
            // long as the supervisor stalled — in its request clone, in the owner's authority check,
            // anywhere between arming and its own reap. The kill was independent of the supervisor
            // and the confirmation was not, so the seat's bound was still the supervisor's latency.
            //
            // So this thread reaps what it killed. It charges the SAME [`REAP_BOUND`] budget the
            // supervisor charges, so confirming from here cannot buy a window the seat was never
            // promised, and it publishes ONLY on an actual reported exit. A budget that runs out
            // leaves the exit unknown and the seat retained, which is the outcome an unconfirmed
            // child is supposed to have.
            let started = Instant::now();
            loop {
                let (outcome, confirm, budget) = {
                    let Ok(mut state) = guard.lock() else { return };
                    let (outcome, confirm) = state.observe_exit();
                    let budget = reap_window_left(state.spent_reaping);
                    if matches!(outcome, Ok(Some(_))) {
                        state.spent_reaping += started.elapsed();
                    }
                    (outcome, confirm, budget)
                };
                if let Some(confirm) = confirm {
                    confirm();
                    return;
                }
                match outcome {
                    // Someone else observed it and has already published. Nothing owed here.
                    Ok(Some(_)) => return,
                    Ok(None) => {
                        if started.elapsed() >= budget {
                            // UNCONFIRMED. The seat keeps the turn; see `kill_and_reap`.
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    // An unknown exit must never be published as a confirmed one.
                    Err(_) => return,
                }
            }
        });
    }

    /// True when the deadline watchdog, rather than the supervisor, issued this child's kill.
    pub fn watchdog_fired(&self) -> bool {
        self.watchdog_fired.load(Ordering::SeqCst)
    }

    /// `try_wait`, performed in the same critical section that disarms the watchdog.
    ///
    /// Reaping and disarming must be indivisible: between them the pid is free for the kernel to
    /// reuse, and a watchdog that signalled in that window would kill an unrelated process.
    /// The seat's confirmation is fired AFTER this releases the guard, never under it.
    fn guarded_try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let (outcome, confirm) = {
            let mut state = self
                .guard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.observe_exit()
        };
        if let Some(confirm) = confirm {
            confirm();
        }
        outcome
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Take one of the child's pipe handles from under the guard.
    fn take_pipe<T>(&mut self, take: impl FnOnce(&mut Child) -> Option<T>) -> Option<T> {
        let mut state = self
            .guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.child.as_mut().and_then(take)
    }

    pub fn stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.take_pipe(|child| child.stdin.take())
    }

    pub fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.take_pipe(|child| child.stdout.take())
    }

    pub fn stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.take_pipe(|child| child.stderr.take())
    }

    /// `SIGKILL` to the process GROUP, then wait for the actual exit.
    ///
    /// Returns how long the exit took to confirm, or [`ExecutorError::Unreaped`] if the child was
    /// still not gone after [`REAP_BOUND`] — in which case the caller must NOT release the turn.
    ///
    /// [`REAP_BOUND`] IS A BUDGET FOR THE CHILD, NOT FOR ONE CALL. A child that does not exit is
    /// killed and waited for more than once on the failing path: `drive` kills it, the cleanup that
    /// follows normalizes the outcome, and [`Drop`] kills again behind every return, panic and
    /// early exit. When each of those calls started its own full window, an unconfirmed exit cost
    /// three consecutive [`REAP_BOUND`] waits, and the seat's advertised `deadline + 2 *
    /// REAP_BOUND` — which allows ONE reap window and ONE end-of-file window — was understated by
    /// the retries, on exactly the path where the numbers matter. The time already spent waiting
    /// for THIS child is therefore accumulated and charged against the same budget, so the second
    /// and third attempts re-send the signal, poll the exit ONCE, and return what they find.
    /// Repeated attempts change the certainty of the outcome, never the bound.
    pub fn kill_and_reap(&mut self) -> Result<Duration, ExecutorError> {
        let started = Instant::now();
        {
            // The GROUP, not the pid: a descendant that outlived its parent would otherwise keep
            // packing with nobody watching. Negative pid is the group. An ESRCH here means the
            // group is already gone, which is the outcome we wanted.
            //
            // Issued through the SAME interlock the watchdog uses, so the supervisor's kill and the
            // watchdog's kill cannot both be in flight around a reap. Whichever arrives first, the
            // guard makes sure neither signals a pid this process has already waited for.
            let state = self
                .guard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.reaped {
                return Ok(Duration::ZERO);
            }
            state.kill_if_armed();
            if state.child.is_none() {
                return Ok(started.elapsed());
            }
        }
        // Poll rather than block: a blocking `wait` on a child in uninterruptible sleep never
        // returns, and "we cannot confirm the exit" is an outcome this executor must be able to
        // REPORT rather than an outcome it hangs in.
        loop {
            match self.guarded_try_wait() {
                Ok(Some(_status)) => {
                    let mut state = self
                        .guard
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.spent_reaping += started.elapsed();
                    return Ok(started.elapsed());
                }
                Ok(None) => {
                    // Against the CHILD's budget, not this call's elapsed time. An exhausted budget
                    // means this attempt has already polled the exit once above and found it
                    // absent, which is the whole of what a further wait could add.
                    //
                    // The budget lives on the guard because the watchdog charges the same one.
                    let (waited, budget) = {
                        let state = self
                            .guard
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        (
                            state.spent_reaping + started.elapsed(),
                            reap_window_left(state.spent_reaping),
                        )
                    };
                    if started.elapsed() >= budget {
                        let mut state = self
                            .guard
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state.spent_reaping = waited;
                        return Err(ExecutorError::Unreaped { waited });
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => {
                    let mut state = self
                        .guard
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.spent_reaping += started.elapsed();
                    // NOT `Protocol`: an unknown exit must not be able to wear a name the release
                    // rule lets through. See [`ExecutorError::WaitFailed`].
                    return Err(ExecutorError::WaitFailed {
                        why: error.to_string(),
                    });
                }
            }
        }
    }

    /// True once the kernel has reported this child's exit status — to EITHER reaper.
    pub fn is_reaped(&self) -> bool {
        self.guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reaped
    }
}

impl Drop for KillableChild {
    fn drop(&mut self) {
        if self.is_reaped() {
            return;
        }
        // The same kill and the same wait as every other path, so success, error, panic and early
        // return all leave a reaped child behind. What `Drop` cannot do is REPORT, and an
        // unconfirmed exit that nobody hears about is a seat handed on while work may still be
        // running — the defect this module exists to remove. So a failure here is recorded in a
        // process-wide counter that a seat must consult before it treats its lane as free.
        if self.kill_and_reap().is_err() {
            record_unconfirmed_child();
        }
    }
}

/// One line of newline-delimited JSON per frame. JSON's own escaping means a serialized frame never
/// contains a newline, so the framing is unambiguous without a length prefix.
///
/// The line is produced and CAPPED before a byte is written. [`MAX_FRAME_BYTES`] used to bound only
/// what this protocol would read; a cap on one direction is not a cap on the protocol, so it now
/// bounds what either side will write as well. An over-cap frame is a bug on the writing side and is
/// refused there, where it can still be reported, rather than discovered by the reader after the
/// bytes are already in the pipe.
pub fn encode_frame<T: Serialize>(frame: &T) -> std::io::Result<String> {
    // CAPPED DURING SERIALIZATION, not after it. Checking `line.len()` against the cap once
    // `to_string` had returned meant the complete value was materialized first: the cap bounded
    // what this parent would WRITE, and bounded nothing about the work and the allocation it did to
    // find out. That matters here and not only in general — encoding runs SYNCHRONOUSLY on the
    // supervisor thread, between the waits the deadline is enforced in, so an oversized value would
    // have been an unbounded phase inside a module whose whole claim is bounded ones. Serializing
    // into a sink that stops at the cap makes the worst case a fixed [`MAX_FRAME_BYTES`] of work
    // regardless of how large the value is.
    let mut sink = CappedLine {
        bytes: Vec::new(),
        capped: false,
    };
    if let Err(error) = serde_json::to_writer(&mut sink, frame) {
        if sink.capped {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "refusing to write a frame over this protocol's {MAX_FRAME_BYTES}-byte cap; \
                     encoding was stopped AT the cap rather than completed and measured"
                ),
            ));
        }
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
    }
    let mut line = String::from_utf8(sink.bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if line.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "refusing to write a {}-byte frame; this protocol's cap is {MAX_FRAME_BYTES} bytes",
                line.len()
            ),
        ));
    }
    line.push('\n');
    Ok(line)
}

/// A `Write` sink that accepts at most [`MAX_FRAME_BYTES`] and then refuses, so an over-cap frame
/// is abandoned mid-encode instead of being built and measured.
struct CappedLine {
    /// Bytes, not a `String`: a single `write` may land inside a multi-byte character, and a lossy
    /// per-chunk conversion would change the length being measured against the cap.
    bytes: Vec<u8>,
    /// Set when a write was refused for the cap, so the caller can tell that stop apart from a
    /// serializer fault without inspecting an error string.
    capped: bool,
}

impl Write for CappedLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len() + buf.len() > MAX_FRAME_BYTES {
            self.capped = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame exceeds this protocol's cap",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn write_frame<W: Write, T: Serialize>(out: &mut W, frame: &T) -> std::io::Result<()> {
    let line = encode_frame(frame)?;
    out.write_all(line.as_bytes())?;
    out.flush()
}

/// Read one frame. **`Ok(None)` is end of file and NOTHING else**: the kernel returned zero bytes,
/// which happens only once every holder of the write end has closed it.
///
/// A blank line used to return `Ok(None)` too, which made a parser-level event indistinguishable
/// from a kernel-level one, and let a caller that reads "the stream ended" conclude "the writers are
/// gone". It is a malformed frame and it is reported as one.
pub fn read_frame<R: BufRead, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    // Bounded read: `read_line` on an unbounded writer is unbounded memory in this process, and a
    // buffer nobody capped is a phase nobody bounded.
    // Spelled as a free-function call so resolution picks `impl Read for &mut R` rather than moving
    // the caller's reader out from behind its reference.
    let mut limited = std::io::Read::take(&mut *reader, MAX_FRAME_BYTES as u64 + 1);
    let read = limited.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if read > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "blank line where a frame was expected; this is a malformed frame, not end of file",
        ));
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Why the stdout pump stopped. Three facts that used to arrive as one channel disconnection, and
/// a caller that cannot tell them apart cannot say what it observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpEnd {
    /// **Observed end of file.** `read` returned zero, which the kernel does only once every holder
    /// of the write end of that pipe has closed it. This is the only one of the three that says
    /// anything about who still holds the descriptor.
    Eof,
    /// The read itself failed. The pump is gone; the pipe's owners are not accounted for.
    ReadFailed(String),
    /// The parent stopped listening — the receiving end went away while the child was still
    /// writing. Says nothing about the child.
    ParentStopped,
}

/// Pump the child's stdout into a channel so the parent can wait on frames WITH A DEADLINE. A
/// blocking read cannot be given one, and a parent blocked in a read it cannot leave is a parent
/// that never issues the kill.
///
/// **A malformed frame does not end the pump.** It used to: any parse failure returned the thread,
/// the channel disconnected, and the cleanup below read that disconnection as a closed pipe — so a
/// single blank line or bad byte was enough to make this parent report that it had seen the child's
/// stdout close when it had seen no such thing. The parse failure is reported to the drive, which
/// still treats it as a protocol fault and kills; the pump keeps reading the descriptor until the
/// kernel actually ends it, because that read is the only thing that can establish [`PumpEnd::Eof`].
///
/// The reason it stopped is published in `end` BEFORE the sink is dropped, so a receiver that sees
/// the disconnection can always read why it happened.
fn pump<R: std::io::Read + Send + 'static>(
    stream: R,
    sink: SyncSender<std::io::Result<Option<ToParent>>>,
    end: std::sync::Arc<std::sync::Mutex<Option<PumpEnd>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let reason = loop {
            let frame = read_frame::<_, ToParent>(&mut reader);
            // Only two things end this thread from the child's side: the kernel says the pipe is
            // closed, or the read fails. A frame we could not parse is a message about the CHILD,
            // not about the descriptor, so it is forwarded and the reading continues.
            let stop = match &frame {
                Ok(Some(_)) => None,
                Ok(None) => Some(PumpEnd::Eof),
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => None,
                Err(error) => Some(PumpEnd::ReadFailed(error.to_string())),
            };
            if sink.send(frame).is_err() {
                break PumpEnd::ParentStopped;
            }
            if let Some(reason) = stop {
                break reason;
            }
        };
        if let Ok(mut slot) = end.lock() {
            *slot = Some(reason);
        }
        drop(sink);
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
    mint: crate::git_transport::AuthMinter,
    authority: crate::git_transport::AuthorityCheck,
) -> Result<String, ExecutorError> {
    run_push_in_child_confirming(program, request, deadline, mint, authority, None)
}

/// As [`run_push_in_child`], plus somewhere for the child's OBSERVED exit to go that does not
/// depend on this function returning.
///
/// THE CONFIRMATION IS THE POINT. Everything this function does between arming and its reap is
/// synchronous supervisor work — cloning the request, asking the owner's authority check, encoding
/// a frame — and a thread stalled in any of it never reaches the `try_wait` that turns a kill into
/// a confirmed exit. The deadline watchdog already made the KILL independent of that stall. Handing
/// it `on_confirmed_exit` makes the CONFIRMATION independent of it too, so a seat waiting on this
/// delivery is bounded by the child's deadline and reap rather than by where this thread happens to
/// be. See [`KillableChild::publish_confirmed_exit_to`].
pub fn run_push_in_child_confirming(
    program: &Path,
    request: &PushRequest,
    deadline: Instant,
    mint: crate::git_transport::AuthMinter,
    authority: crate::git_transport::AuthorityCheck,
    on_confirmed_exit: Option<ExitConfirmation>,
) -> Result<String, ExecutorError> {
    let mut child = KillableChild::spawn(program, &[CHILD_SUBCOMMAND])?;
    // INSTALLED BEFORE ARMING, so there is no instant at which the watchdog could reap this child
    // and find nowhere to report it.
    if let Some(confirm) = on_confirmed_exit {
        child.publish_confirmed_exit_to(confirm);
    }
    // ARMED HERE, AT THE EARLIEST INSTANT A PID EXISTS — before the pipes are taken, before the
    // pump and writer threads exist, and before `drive` is entered.
    //
    // Everything between this line and the first deadline check inside `drive` is synchronous
    // supervisor work (the `S` term at the top of this module): taking three pipe handles, spawning
    // the relay, the pump and the writer, and then the first encode. None of it is interruptible by
    // the loop that used to own the kill, so a child that went wrong during it was stopped late by
    // however long that work took. From here the signal is owned by a thread that does none of it.
    child.arm_deadline_watchdog(deadline);
    let stdin = child
        .stdin()
        .ok_or_else(|| ExecutorError::Spawn("child stdin unavailable".to_owned()))?;
    let stdout = child
        .stdout()
        .ok_or_else(|| ExecutorError::Spawn("child stdout unavailable".to_owned()))?;
    // Relayed and capped rather than piped-and-ignored: an undrained stderr pipe is a child that
    // stalls on its own diagnostic, and a diagnostic the operator never sees.
    if let Some(stderr) = child.stderr() {
        relay_stderr(stderr);
    }
    let (sink, frames) = sync_channel(MAX_QUEUED_FRAMES);
    let pump_end = std::sync::Arc::new(std::sync::Mutex::new(None));
    let pump = pump(stdout, sink, std::sync::Arc::clone(&pump_end));
    let mut writer = Writer::spawn(stdin);

    let outcome = drive(
        &mut writer,
        &frames,
        request,
        deadline,
        &mint,
        &authority,
        &mut child,
    );
    // Closing the parent's end is what the child reads as EOF. Dropping the handle drops the job
    // channel, which is what the writer thread is normally parked on; a writer still stuck inside a
    // `write_all` is NOT waited for, because waiting on it is the unbounded phase this type exists
    // to remove. Its write fails once the child is gone.
    drop(writer);

    // FAIL CLOSED BY CONSTRUCTION. Every arm of `drive` reaps before it returns — but "every arm"
    // is a property of a function that will be edited again, and the one thing this module may
    // never do is release a seat on an exit nobody observed. So the rule is also stated ONCE, at
    // the single point every return passes through: if this process has not seen the child exit,
    // the outcome that leaves here is an unconfirmed-exit outcome, whatever `drive` decided.
    let outcome = if child.is_reaped() {
        outcome
    } else {
        match child.kill_and_reap() {
            Ok(_) => outcome,
            Err(unconfirmed) => Err(unconfirmed),
        }
    };

    // CLEANUP, BOUNDED. Joining the pump is the obvious move and it is unbounded: the pump sits in
    // a blocking read that only ends at EOF, and EOF only arrives when the LAST holder of the write
    // end closes it. A grandchild that escaped the process group we killed still holds it, and then
    // the join never returns and this parent never comes back at all. So we do not join: we wait
    // for the channel to disconnect, which happens exactly when the pump returns, and we give that
    // the same REAP_BOUND we give the reap. Losing that race is not a delivery failure we can
    // shrug at — it says something from this delivery outlived the kill — so it fails closed.
    //
    // THE DEADLINE IS TESTED ON EVERY PATH THROUGH THIS LOOP, including the one that receives a
    // frame. `recv_timeout(REAP_BOUND - elapsed)` alone bounds a SINGLE wait, not the loop: once the
    // bound is spent the remaining timeout is zero, and a queue that keeps being refilled keeps
    // returning `Ok` from a zero-length wait, forever. A writer that escaped the kill is exactly the
    // thing that can refill it, and it is the case this cleanup exists for.
    let cleanup_started = Instant::now();
    let cleaned = drain_until_pipe_ends(&frames, REAP_BOUND, cleanup_started);
    if !cleaned {
        return Err(ExecutorError::CleanupUnbounded {
            waited: cleanup_started.elapsed(),
        });
    }
    // The channel disconnected — the pump returned. WHY it returned is the whole question. Only
    // [`PumpEnd::Eof`] is an observation about the pipe: the kernel ends a read with zero bytes only
    // once the last holder of the write end has let it go. A pump that stopped because its own read
    // failed, or because nobody was listening any more, says nothing at all about who still holds
    // that descriptor, and this parent may not call that a cleaned-up delivery. It used to: any
    // disconnection was success, so a malformed frame was reported as a closed pipe.
    //
    // What EOF does NOT establish is stated at the claim, not only here: a descendant that closes
    // this one descriptor and keeps running produces the same EOF. See the module header.
    let ended = pump_end.lock().ok().and_then(|slot| slot.clone());
    match ended {
        Some(PumpEnd::Eof) => {}
        Some(PumpEnd::ReadFailed(why)) => {
            return Err(ExecutorError::CleanupUnobserved {
                why: format!("the read on the child's stdout failed: {why}"),
            });
        }
        Some(PumpEnd::ParentStopped) => {
            return Err(ExecutorError::CleanupUnobserved {
                why: "this parent stopped reading the child's stdout before it ended".to_owned(),
            });
        }
        None => {
            return Err(ExecutorError::CleanupUnobserved {
                why: "the reader thread ended without recording why it stopped".to_owned(),
            });
        }
    }
    drop(pump);
    outcome
}

/// Drain what is left in the frame channel until the pump drops its end, and return whether that
/// happened inside `bound` measured from `started`.
///
/// Extracted so the loop that has to hold the bound can be driven directly by a test: the fault it
/// guards against — a queue refilled as fast as it is drained — cannot be reproduced through a real
/// child without an escaped descendant to do the refilling.
pub(crate) fn drain_until_pipe_ends<T>(
    frames: &Receiver<T>,
    bound: Duration,
    started: Instant,
) -> bool {
    loop {
        // Checked FIRST, on every iteration, receiving or not. This is the bound.
        let Some(left) = bound.checked_sub(started.elapsed()) else {
            return false;
        };
        match frames.recv_timeout(left) {
            // Frames still queued behind the outcome; drain them, the decision is already made.
            // Back to the top, where the deadline is applied again.
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return false,
        }
    }
}

/// The parent's writes, off the drive thread and therefore boundable.
///
/// A blocking `write_all` to a child that is not draining its stdin parks the calling thread until
/// the child reads — and the drive thread is the only thread that can issue the kill. Ordinary pipe
/// backpressure is not the exotic uninterruptible-sleep case; it is the ordinary case, and it was
/// outside the deadline. Here the write happens on its own thread and the drive waits for the
/// acknowledgement with the SAME absolute deadline as every other phase.
struct Writer {
    lines: Option<Sender<String>>,
    acks: Receiver<std::io::Result<()>>,
}

impl Writer {
    fn spawn(mut stdin: std::process::ChildStdin) -> Self {
        let (lines, jobs) = channel::<String>();
        let (done, acks) = channel();
        std::thread::spawn(move || {
            while let Ok(line) = jobs.recv() {
                let wrote = stdin
                    .write_all(line.as_bytes())
                    .and_then(|()| stdin.flush());
                let failed = wrote.is_err();
                if done.send(wrote).is_err() || failed {
                    return;
                }
            }
        });
        Self {
            lines: Some(lines),
            acks,
        }
    }

    /// Hand one frame to the writer thread. Encoding happens HERE, before any wait is sized, so the
    /// time it costs is the parent's and not silently added to what the child is allowed.
    ///
    /// Split from [`Self::await_ack`] on purpose: this call and the wait for the acknowledgement are
    /// two phases, and sizing the second from a duration measured before the first is exactly how a
    /// bound drifts. The caller measures again between them.
    fn send_frame(&mut self, frame: &ToChild) -> Result<(), WriteStall> {
        let line = encode_frame(frame).map_err(|error| WriteStall::Failed(error.to_string()))?;
        let Some(lines) = self.lines.as_ref() else {
            return Err(WriteStall::Failed("the writer is closed".to_owned()));
        };
        if lines.send(line).is_err() {
            return Err(WriteStall::Failed(
                "the delivery push child's stdin is closed".to_owned(),
            ));
        }
        Ok(())
    }

    /// Wait up to `slice` for the writer thread to say the frame is gone. [`WriteStall::TimedOut`]
    /// means only "not yet within this slice" — the caller decides whether that slice was a
    /// cancellation tick or the end of the deadline.
    fn await_ack(&mut self, slice: Duration) -> Result<(), WriteStall> {
        match self.acks.recv_timeout(slice) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(WriteStall::Failed(error.to_string())),
            Err(RecvTimeoutError::Disconnected) => Err(WriteStall::Failed(
                "the delivery push child's stdin writer stopped".to_owned(),
            )),
            Err(RecvTimeoutError::Timeout) => Err(WriteStall::TimedOut),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closes the job channel, which unparks the writer thread and drops the child's stdin with
        // it. Deliberately no join: see `run_push_in_child`.
        self.lines.take();
    }
}

enum WriteStall {
    /// The write did not complete within what was left of the deadline.
    TimedOut,
    Failed(String),
}

/// Relay the child's stderr to this process's stderr, bounded. Not joined, and not allowed to grow:
/// see [`MAX_CHILD_STDERR_BYTES`].
fn relay_stderr(stream: std::process::ChildStderr) {
    std::thread::spawn(move || {
        let mut capped = std::io::Read::take(stream, MAX_CHILD_STDERR_BYTES);
        let _ = std::io::copy(&mut capped, &mut std::io::stderr());
    });
}

/// ONE next-ask deadline, shared by every wait in a delivery.
///
/// Slicing each wait at [`CANCELLATION_POLL`] *independently* is not the bound it looks like. After
/// an ask at `t0`, a frame/mint/write sequence that finishes at `t0 + 49ms` is not a timeout, so no
/// arm re-asks; the next wait then starts a FULL fresh 50 ms slice and the owner is not observed
/// until `t0 + 99ms`. Nested mint and ACK waits made it worse: each began its own full slice with
/// no knowledge of when the last ask happened, so progressing execution — not a stall — could run
/// several slices between observations. The interval was a property of each individual wait, and
/// the claim was about the delivery.
///
/// This makes it one clock. Every wait is cut at `next_ask`, whoever is waiting, and every ask
/// re-arms `next_ask` from the moment of the ask. A wait that returns early does not earn its
/// successor a fresh slice.
///
/// WHAT THIS BOUNDS, EXACTLY: no wait in a delivery blocks past `next_ask`. The interval between
/// two authority observations is therefore [`CANCELLATION_POLL`] plus the SYNCHRONOUS parent work
/// that runs between one wait returning and the next ask — frame decode, the encode/allocate of the
/// next frame, and the spawn on the first pass. That work is not interruptible from this thread and
/// is NOT covered by this clock; it is bounded only by the progressing-phase allowance documented
/// on [`CANCELLATION_POLL`]. The honest statement is "no wait outlives the shared next ask", not
/// "the owner is observed every 50 ms of wall clock".
struct PollClock {
    next_ask: Instant,
}

impl PollClock {
    /// Arm the first interval from now. The delivery has just asked — the caller checked authority
    /// before spawning — so the first ask is due one full interval from here, not immediately.
    fn armed_now() -> Self {
        Self {
            next_ask: Instant::now() + CANCELLATION_POLL,
        }
    }

    /// Ask the owner if the shared deadline has arrived, and re-arm from the ask itself.
    ///
    /// Re-arming from `Instant::now()` AFTER the call, rather than from `next_ask`, means a slow
    /// authority backend cannot make the parent ask in a tight loop to "catch up" on intervals it
    /// spent inside the check.
    fn ask_if_due(&mut self, authority: &crate::git_transport::AuthorityCheck) -> Result<(), String> {
        if Instant::now() >= self.next_ask {
            authority()?;
            self.observed();
        }
        Ok(())
    }

    /// Record an authority observation made by the caller — the child's own `Check`, or an arm that
    /// asked directly. Answering the child and asking the owner are the same call, so it counts.
    fn observed(&mut self) {
        self.next_ask = Instant::now() + CANCELLATION_POLL;
    }

    /// The longest this wait may block: never past the shared next ask, never past `left`.
    ///
    /// A zero slice is deliberate rather than guarded against. If the ask is already due the wait
    /// returns immediately and the next `ask_if_due` performs it; a floor here would let a wait
    /// outlive the deadline it exists to enforce, which is the exact overshoot being fixed.
    fn slice(&self, left: Duration) -> Duration {
        left.min(self.next_ask.saturating_duration_since(Instant::now()))
    }
}

fn drive(
    writer: &mut Writer,
    frames: &Receiver<std::io::Result<Option<ToParent>>>,
    request: &PushRequest,
    deadline: Instant,
    mint: &crate::git_transport::AuthMinter,
    authority: &crate::git_transport::AuthorityCheck,
    child: &mut KillableChild,
) -> Result<String, ExecutorError> {
    let mut said_hello = false;
    let mut sent_request = false;
    let mut mints: u32 = 0;
    // When the owner was last asked. The caller checked authority immediately before the spawn, so
    // the interval starts there rather than at an epoch that would force a redundant first ask.
    let mut poll_clock = PollClock::armed_now();

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
        // THE POLL IS A CLOCK, NOT A CONSEQUENCE OF SILENCE.
        //
        // Revocation used to be acted on in exactly one place: the arm that runs when no frame
        // arrived within the slice. That made the whole "the owner is re-asked every
        // CANCELLATION_POLL" claim conditional on the child being QUIET. A child that kept the
        // parent busy — authority checks in a loop, or any other frame faster than the slice — was
        // never a timeout, so the owner was never re-asked, and the delivery ran to its deadline no
        // matter when authority ended. The bound held for well-behaved children and failed for
        // exactly the ones it exists for.
        //
        // Asking on ELAPSED TIME instead makes the interval a property of the parent's clock, which
        // is what was claimed. Traffic can no longer outrun it.
        //
        // The check is now against a SHARED next-ask deadline rather than this loop's own elapsed
        // time, so a wait that returned early somewhere below does not buy the next one a fresh
        // full interval.
        if let Err(why) = poll_clock.ask_if_due(authority) {
            let reap = child.kill_and_reap()?;
            return Err(ExecutorError::Revoked { why, reap });
        }
        // The one job, written INSIDE the deadline rather than before the first check of it. A
        // child that never reads its stdin used to park this thread here, before any phase this
        // loop bounds, with the kill unreachable behind it.
        if !sent_request {
            sent_request = true;
            // The budget is measured HERE, at the write, not when the request was built. It is the
            // parent's remaining time handed across as a duration, and every millisecond spent
            // between building the request and writing it — the spawn, the fork/exec, the
            // handshake — used to be given back to the child as budget it never had.
            //
            // Both fields are stamped from the SAME moment, and they are not redundant: the
            // duration is a ceiling the child can never exceed, and the absolute instant is what
            // makes the pipe transit the child's cost instead of a free extension. The child takes
            // whichever is smaller. See [`PushRequest::deadline_unix_ms`].
            //
            // THE CLONE HAPPENS FIRST, and the clock is read after it. It used to be the other way
            // round: `left` was measured at the top of the loop, the request was then cloned, and
            // the absolute stamp was computed as a FRESH wall-clock now plus that already-stale
            // duration. Copying the request is parent work, and adding a duration measured before
            // it to an instant measured after it handed the child exactly that copy time as extra
            // life. Both fields now come from one pair of readings taken here, with nothing but the
            // arithmetic between them.
            let mut request = request.clone();
            let at = Instant::now();
            let at_ms = now_unix_ms();
            let Some(remaining) = deadline.checked_duration_since(at) else {
                let reap = child.kill_and_reap()?;
                return Err(ExecutorError::Killed {
                    after: at.saturating_duration_since(deadline),
                    reap,
                });
            };
            request.budget_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
            request.deadline_unix_ms = at_ms.saturating_add(request.budget_ms);
            stalled_write(
                writer,
                &ToChild::Push(request),
                deadline,
                child,
                "writing the push request",
                authority,
                &mut poll_clock,
            )?;
            continue;
        }
        // Bounded by the cancellation poll, not only by the deadline: see [`CANCELLATION_POLL`].
        // Every wait in this loop is short enough that the owner is re-asked while the child works,
        // rather than only when the clock runs out.
        let poll = poll_clock.slice(left);
        match frames.recv_timeout(poll) {
            Ok(Ok(Some(ToParent::Hello { version, .. }))) => {
                if version != PROTOCOL_VERSION {
                    child.kill_and_reap()?;
                    return Err(ExecutorError::Protocol(format!(
                        "child speaks protocol {version}, this parent speaks {PROTOCOL_VERSION}"
                    )));
                }
                // A handshake happens ONCE. A second hello is a child saying something this
                // protocol has no meaning for, and "accepted it and carried on" is not a protocol
                // this parent can describe.
                if said_hello {
                    child.kill_and_reap()?;
                    return Err(ExecutorError::Protocol(
                        "child said hello twice".to_owned(),
                    ));
                }
                said_hello = true;
            }
            Ok(Ok(Some(ToParent::Mint { destination }))) => {
                mints += 1;
                if mints > MAX_MINT_REQUESTS {
                    child.kill_and_reap()?;
                    return Err(ExecutorError::Protocol(format!(
                        "child asked for {mints} authorizations; this delivery's legs need at most \
                         {MAX_MINT_REQUESTS}"
                    )));
                }
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
                // The mint is bounded by the SAME deadline as every other phase, and it runs OFF
                // this thread to make that true: a signer whose queue is full, or whose reply is
                // being held, must not be able to stop the parent from issuing the kill. The
                // abandoned thread carries no lock of ours and is bounded by the minter's own push
                // deadline; the private key never leaves the actor either way.
                let (answered, answer_rx) = channel();
                let minter = std::sync::Arc::clone(mint);
                let target = destination.clone();
                std::thread::spawn(move || {
                    let _ = answered.send(minter(&target));
                });
                // WAITED FOR IN SLICES, so a held signer reply is not also a hole in the revocation
                // bound. This used to be one `recv_timeout` for the whole remaining deadline: a
                // signer that answered slowly — the realistic case, since a mint is a round trip
                // into an actor that can be busy or saturated — meant the owner was not asked again
                // until the clock ran out, however long that was. The mint is the longest wait in
                // the protocol and it was the one wait nobody polled through.
                //
                // Revoked DURING the mint is the case that matters: the thread minting is
                // abandoned, and whatever it eventually produces is dropped on this side of the
                // pipe. A token for a delivery whose owner is gone is never written to the child,
                // so it never reaches the wire — which is the property the post-mint check states
                // and this is what makes it hold while the mint is still outstanding.
                let answer = loop {
                    let now = Instant::now();
                    let Some(left_for_mint) = deadline.checked_duration_since(now) else {
                        let after = now.saturating_duration_since(deadline);
                        let reap = child.kill_and_reap()?;
                        return Err(ExecutorError::Killed { after, reap });
                    };
                    // Cut at the SHARED next ask, not at a fresh full interval of this wait's own.
                    // A mint entered 40 ms after the last ask gets 10 ms, not 50.
                    if let Err(why) = poll_clock.ask_if_due(authority) {
                        let reap = child.kill_and_reap()?;
                        return Err(ExecutorError::Revoked { why, reap });
                    }
                    let slice = poll_clock.slice(left_for_mint);
                    match answer_rx.recv_timeout(slice) {
                        Ok(Ok(header)) => {
                            break ToChild::Minted {
                                header: Some(header),
                                refused: None,
                            }
                        }
                        Ok(Err(refused)) => {
                            break ToChild::Minted {
                                header: None,
                                refused: Some(refused),
                            }
                        }
                        Err(RecvTimeoutError::Timeout) if slice < left_for_mint => {
                            if let Err(why) = authority() {
                                let reap = child.kill_and_reap()?;
                                return Err(ExecutorError::Revoked { why, reap });
                            }
                            poll_clock.observed();
                        }
                        // The signer did not answer inside this delivery's own deadline (or died
                        // trying). The work is stopped the same way any other overrun is stopped.
                        Err(_) => {
                            let after = Instant::now().saturating_duration_since(deadline);
                            let reap = child.kill_and_reap()?;
                            return Err(ExecutorError::Killed { after, reap });
                        }
                    }
                };
                stalled_write(
                    writer,
                    &answer,
                    deadline,
                    child,
                    "answering a mint request",
                    authority,
                    &mut poll_clock,
                )?;
            }
            Ok(Ok(Some(ToParent::Check { phase }))) => {
                if !said_hello {
                    child.kill_and_reap()?;
                    return Err(ExecutorError::Protocol(
                        "child asked about its authority before saying hello".to_owned(),
                    ));
                }
                // The parent's LIVE answer, taken now rather than recalled from the mint. This is
                // the boundary the child enforces on its own side; what the parent owes it is a
                // current answer and a bounded one.
                let refused = authority()
                    .err()
                    .map(|ended| format!("{ended} (at {phase})"));
                // This IS an ask, so it restarts the interval. Answering the child's question and
                // asking the owner are the same call; counting it keeps a child that checks
                // frequently from making the parent ask more often than its own poll, while the
                // elapsed-time check above keeps one that checks constantly from making it ask
                // less.
                poll_clock.observed();
                stalled_write(
                    writer,
                    &ToChild::Authority {
                        refused: refused.clone(),
                    },
                    deadline,
                    child,
                    "answering an authority check",
                    authority,
                    &mut poll_clock,
                )?;
                // ANSWERED, THEN ENDED. Telling the child its leg is refused is not the same as
                // ending the delivery, and this arm used to do only the first: a child that kept
                // asking was told "no" every time and went on running until the deadline. The
                // refusal goes out first — the child is owed a current answer — and then this
                // delivery stops, with the child killed and its exit confirmed, because authority
                // ending is the end of the work and not a property of one leg.
                if let Some(why) = refused {
                    let reap = child.kill_and_reap()?;
                    return Err(ExecutorError::Revoked { why, reap });
                }
            }
            Ok(Ok(Some(ToParent::Done { oid, error }))) => {
                // The child says it is finished; that is not the same as being gone. Reap before
                // returning, so the turn this result releases is released after an exit we saw.
                let _ = child.kill_and_reap()?;
                if !said_hello {
                    return Err(ExecutorError::Protocol(
                        "child reported a result before saying hello".to_owned(),
                    ));
                }
                return match (oid, error) {
                    // The oid the child reports is the one the parent ASKED for, or this delivery
                    // did not deliver what it was told to. The parent held the gated oid the whole
                    // time and never compared it; a result that names a different object was
                    // returned to the caller as this delivery's result.
                    (Some(oid), None) if oid == request.gated_oid => Ok(oid),
                    (Some(oid), None) => Err(ExecutorError::Protocol(format!(
                        "child reported delivering {oid}, but this delivery's gated object is {}",
                        request.gated_oid
                    ))),
                    (_, Some(error)) => Err(ExecutorError::Push(error)),
                    (None, None) => Err(ExecutorError::Protocol(
                        "child finished without an oid or an error".to_owned(),
                    )),
                };
            }
            // END OF FILE, observed: the kernel reported zero bytes on the child's stdout.
            Ok(Ok(None)) => {
                let reap = child.kill_and_reap()?;
                return Err(attribute_vanished_child(
                    child,
                    deadline,
                    reap,
                    "child's stdout reached end of file without finishing the push",
                ));
            }
            // The READER stopped. Not the same fact: it means this parent has no further view of
            // that pipe, which is why the cleanup below asks the pump why it ended rather than
            // treating its disappearance as a closed descriptor.
            Err(RecvTimeoutError::Disconnected) => {
                let reap = child.kill_and_reap()?;
                return Err(attribute_vanished_child(
                    child,
                    deadline,
                    reap,
                    "the reader on the child's stdout stopped before the push finished",
                ));
            }
            Ok(Err(error)) => {
                let reap = child.kill_and_reap()?;
                return Err(attribute_vanished_child(
                    child,
                    deadline,
                    reap,
                    &format!("unreadable frame from the child: {error}"),
                ));
            }
            Err(RecvTimeoutError::Timeout) => {
                // A tick, not the clock running out: re-ask the owner. This covers the interval
                // between answering the child's authority check and the child reading that answer,
                // which is the interval this parent cannot otherwise see into.
                //
                // NO LONGER THE ONLY PLACE, and the comment here used to say it was. That sentence
                // described the defect: a revocation was acted on only when the child had gone
                // quiet. The owner is now also asked on elapsed time at the top of this loop, inside
                // an unacknowledged write, and inside a mint whose reply has not come back.
                if poll < left {
                    if let Err(why) = authority() {
                        let reap = child.kill_and_reap()?;
                        return Err(ExecutorError::Revoked { why, reap });
                    }
                    poll_clock.observed();
                    continue;
                }
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

/// Say WHY a child stopped speaking, when the deadline is one of the candidate reasons.
///
/// A child whose stdout reaches end of file has, from the supervisor's seat, done one of two very
/// different things: it violated the protocol, or it was stopped on purpose and the pipe closed
/// because the process is gone. Before the deadline watchdog existed the second case could not
/// arise here — the supervisor issued every kill itself, so it always knew — and so end of file was
/// reported as [`ExecutorError::Protocol`] unconditionally.
///
/// That is now a misattribution waiting to happen, and misattribution is not cosmetic: an operator
/// reading "the child spoke out of turn" goes looking for a protocol bug, and a caller matching on
/// [`ExecutorError::Killed`] to account for an overrun never sees it. A deadline stop must be
/// reported as a deadline stop by whichever side issued it.
///
/// Both conditions are checked, and the pair is deliberate. `watchdog_fired` is the precise fact but
/// it is published just after the signal, so the child's end of file can reach this loop first; the
/// deadline comparison closes that window. Either one means the same thing — this delivery was out
/// of time — and the overrun is measured from the deadline either way.
fn attribute_vanished_child(
    child: &KillableChild,
    deadline: Instant,
    reap: Duration,
    otherwise: &str,
) -> ExecutorError {
    let now = Instant::now();
    if child.watchdog_fired() || now >= deadline {
        return ExecutorError::Killed {
            after: now.saturating_duration_since(deadline),
            reap,
        };
    }
    ExecutorError::Protocol(otherwise.to_owned())
}

/// One parent write, with the deadline on it and the kill behind it. A write that does not complete
/// in time is the same overrun as any other, and is stopped the same way.
/// Write one frame to the child within the delivery's absolute deadline, re-asking the owner every
/// [`CANCELLATION_POLL`] while the write is outstanding.
///
/// Two things this fixes, and both were real holes rather than tidiness:
///
/// 1. The wait used to be sized by a duration measured at the top of the drive loop — before the
///    frame was encoded and handed over. Encoding is parent work; charging it to nobody meant the
///    acknowledgement could be waited for past the deadline it was supposed to sit inside. The
///    remaining time is now recomputed from `deadline` AFTER the frame is on its way, and again on
///    every slice, so no phase is paid for out of a duration measured before it started.
/// 2. The wait used to be one uninterrupted block of the whole remaining deadline. A child that
///    never drains its stdin parked the parent here for the entire budget, and a revocation that
///    arrived during it was not acted on until the clock ran out. The wait is now sliced, and the
///    owner is asked on every slice — so the write leg has the same revocation bound the frame loop
///    claims, instead of being the one place the claim did not hold.
/// 3. Those slices used to be a full [`CANCELLATION_POLL`] each, measured from the moment this
///    function was entered and unaware of when the owner was last asked. A write begun 45 ms after
///    an ask therefore waited until 95 ms past it. It now shares the caller's [`PollClock`], so the
///    first slice is only what is left of the current interval, and an ask made in here is visible
///    to the drive loop when the write returns.
fn stalled_write(
    writer: &mut Writer,
    frame: &ToChild,
    deadline: Instant,
    child: &mut KillableChild,
    what: &str,
    authority: &crate::git_transport::AuthorityCheck,
    poll_clock: &mut PollClock,
) -> Result<(), ExecutorError> {
    if let Err(WriteStall::Failed(why)) = writer.send_frame(frame) {
        // Reap BEFORE reporting. A write error used to return straight out of `drive` past a
        // still-live child, leaving the kill to a `Drop` whose failure nobody could return.
        child.kill_and_reap()?;
        return Err(ExecutorError::Protocol(format!("{what}: {why}")));
    }
    loop {
        let now = Instant::now();
        let Some(left) = deadline.checked_duration_since(now) else {
            let after = now.saturating_duration_since(deadline);
            let reap = child.kill_and_reap()?;
            return Err(ExecutorError::Killed { after, reap });
        };
        if let Err(why) = poll_clock.ask_if_due(authority) {
            let reap = child.kill_and_reap()?;
            return Err(ExecutorError::Revoked { why, reap });
        }
        let slice = poll_clock.slice(left);
        match writer.await_ack(slice) {
            Ok(()) => return Ok(()),
            Err(WriteStall::TimedOut) => {
                if slice < left {
                    if let Err(why) = authority() {
                        let reap = child.kill_and_reap()?;
                        return Err(ExecutorError::Revoked { why, reap });
                    }
                    poll_clock.observed();
                    continue;
                }
                let after = Instant::now().saturating_duration_since(deadline);
                let reap = child.kill_and_reap()?;
                return Err(ExecutorError::Killed { after, reap });
            }
            Err(WriteStall::Failed(why)) => {
                child.kill_and_reap()?;
                return Err(ExecutorError::Protocol(format!("{what}: {why}")));
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
    use std::sync::{Arc, Mutex};

    // This child's OWN deadline, derived from what the parent sent. The parent's `Instant` means
    // nothing here; without this the child had no clock at all and `budget_ms` was a field nobody
    // read. It is taken NOW, at the read, and [`child_budget`] subtracts the pipe transit from it
    // rather than granting it. It does not replace the parent's kill — the child is still not
    // trusted to bound itself — it is what makes the transport's own pre-wire gates real on this
    // side instead of `None`.
    let deadline = Instant::now() + child_budget(request, now_unix_ms());
    let lifetime: crate::git_transport::AuthorityCheck = Arc::new(move || {
        if Instant::now() >= deadline {
            return Err(
                "this delivery's work budget is spent; the child will not transmit".to_owned(),
            );
        }
        Ok(())
    });

    // The pipe is shared by BOTH gates below, in one lock order (reader, then writer), because both
    // are round trips on the one pipe and the transport may call either from a libgit2 thread.
    let pipe = Arc::new(Mutex::new(reader));

    // The minter the transport will call: one round-trip to the parent per wire request. The parent
    // owns the key, the destination binding, the authority check and the deadline; this side owns
    // nothing but the question.
    let ask_reader = Arc::clone(&pipe);
    let ask_writer = Arc::clone(&output);
    let mint: crate::git_transport::AuthMinter = Arc::new(move |destination: &str| {
        let mut reader = ask_reader
            .lock()
            .map_err(|_| "the delivery push pipe is poisoned".to_owned())?;
        let mut output = ask_writer
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
            // Both fields decided together, by the rule in [`minted_answer`]. Matching `header`
            // first meant a frame carrying a header AND a refusal was read as permission — the
            // child picking the answer it liked out of an answer that contradicted itself.
            Ok(Some(ToChild::Minted { header, refused })) => minted_answer(header, refused),
            Ok(Some(_)) | Ok(None) => {
                Err("the parent stopped answering authorization requests".to_owned())
            }
            Err(error) => Err(format!("reading the parent's authorization: {error}")),
        }
    });

    // The authority gate the transport asks IMMEDIATELY BEFORE it transmits. It is a round trip to
    // the parent, on the same pipe, for the same reason the mint is: the answer lives on the other
    // side of the process boundary, and an answer the child recalls from the mint is an answer
    // about a moment that has passed. Between the parent approving the mint and this point the
    // child can be descheduled, the owner can go away, and the parent's own checks — which all
    // happened before the header crossed the pipe — cannot see it.
    let check_reader = Arc::clone(&pipe);
    let check_writer = Arc::clone(&output);
    let authority: crate::git_transport::AuthorityCheck = Arc::new(move || {
        let mut reader = check_reader
            .lock()
            .map_err(|_| "the delivery push pipe is poisoned".to_owned())?;
        let mut output = check_writer
            .lock()
            .map_err(|_| "the delivery push pipe is poisoned".to_owned())?;
        write_frame(
            &mut *output,
            &ToParent::Check {
                phase: "before transmitting".to_owned(),
            },
        )
        .map_err(|error| format!("asking the parent whether this delivery still owns: {error}"))?;
        match read_frame::<_, ToChild>(&mut *reader) {
            Ok(Some(ToChild::Authority { refused: None })) => Ok(()),
            Ok(Some(ToChild::Authority {
                refused: Some(refused),
            })) => Err(refused),
            // FAIL CLOSED. No answer is not permission.
            Ok(Some(_)) | Ok(None) => {
                Err("the parent stopped answering authority checks; not transmitting".to_owned())
            }
            Err(error) => Err(format!("reading the parent's authority answer: {error}")),
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
        // Both gates are the CHILD's, enforced on this side of the pipe. They were `None`, which
        // meant the transport's pre-wire authority and lifetime checks did nothing at all in the
        // production child — the one process that actually transmits. An anonymous remote mints no
        // token and so asks the parent nothing about minting; it is still gated here.
        Some(authority),
        Some(lifetime),
    )
    .map_err(|error| error.to_string())
}

/// What a `Minted` reply MEANS, as a rule rather than a match arm the next edit can reorder.
///
/// Exactly one of the two fields carries the answer. A reply with both is not permission with a
/// note attached: it is a parent that contradicted itself, and the only safe reading of a
/// contradiction on an authorization channel is refusal. A reply with neither is not permission
/// either.
pub fn minted_answer(header: Option<String>, refused: Option<String>) -> Result<String, String> {
    match (header, refused) {
        (Some(header), None) => Ok(header),
        (None, Some(refused)) => Err(refused),
        (Some(_), Some(refused)) => Err(format!(
            "the parent's authorization both granted and refused this leg ({refused}); refusing to \
             use it"
        )),
        (None, None) => {
            Err("the parent's authorization was empty; refusing to transmit".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AN OVER-CAP FRAME IS ABANDONED MID-ENCODE, not built in full and then measured.
    ///
    /// `MAX_FRAME_BYTES` used to be checked against `line.len()` after `serde_json::to_string` had
    /// returned, so the cap bounded what the parent would WRITE and bounded nothing about the work
    /// and the allocation it did to find out. That matters in this module specifically: encoding
    /// runs synchronously on the supervisor thread, between the waits the deadline is enforced in,
    /// so an oversized value was an unbounded phase inside a module whose claim is bounded ones.
    ///
    /// The oracle is the SERIALIZER, not the error text. This value reports how many of its elements
    /// were actually serialized before the sink refused; a full materialization emits all of them,
    /// a capped encode stops shortly after the cap.
    #[test]
    fn an_oversized_frame_stops_at_the_cap_instead_of_being_materialized() {
        use serde::ser::SerializeSeq;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        const CHUNK: usize = 4 * 1024;
        // Four times the cap, so a materializing encoder does four times the work it is allowed to.
        const CHUNKS: usize = (4 * MAX_FRAME_BYTES) / CHUNK;

        struct Counted {
            chunk: String,
            emitted: Arc<AtomicUsize>,
        }

        impl Serialize for Counted {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut seq = serializer.serialize_seq(Some(CHUNKS))?;
                for _ in 0..CHUNKS {
                    seq.serialize_element(&self.chunk)?;
                    self.emitted.fetch_add(1, Ordering::SeqCst);
                }
                seq.end()
            }
        }

        let emitted = Arc::new(AtomicUsize::new(0));
        let value = Counted {
            chunk: "x".repeat(CHUNK),
            emitted: Arc::clone(&emitted),
        };

        let error = encode_frame(&value).expect_err("a frame four times the cap must be refused");
        let text = error.to_string();
        assert!(
            text.contains("cap"),
            "an over-cap frame must be refused as an over-cap frame: {text}"
        );

        let done = emitted.load(Ordering::SeqCst);
        assert!(
            done < CHUNKS / 2,
            "the encoder serialized {done} of {CHUNKS} elements before it was stopped; the value         was materialized in full and only then measured, which is the unbounded synchronous phase         this cap exists to remove"
        );
        assert!(
            done * CHUNK <= MAX_FRAME_BYTES + CHUNK,
            "the encoder produced {} bytes past a {MAX_FRAME_BYTES}-byte cap",
            done * CHUNK
        );

        // And an ordinary frame still encodes, newline and all.
        let line = encode_frame(&ToParent::Check {
            phase: "send-pack".to_owned(),
        })
        .expect("an in-cap frame must still encode");
        assert!(line.ends_with('\n') && line.len() < 128);
    }

    /// THE REAP WINDOW IS A BUDGET FOR THE CHILD, NOT FOR EACH CALLER.
    ///
    /// Stated as a rule rather than raced against a live process, and the reason is worth naming:
    /// making a real child survive `SIGKILL` long enough to force three consecutive full windows is
    /// not constructible in a test on this platform — a process only ignores `SIGKILL` while it is
    /// inside uninterruptible kernel work, which a test cannot arrange on demand. So the accounting
    /// is proved here, and what is NOT proved is that a genuinely unkillable child was observed.
    /// The behavioural half below is the ordinary path: a real child, killed, confirmed, and charged.
    #[test]
    fn repeated_reap_attempts_share_one_window_instead_of_multiplying_it() {
        assert_eq!(
            reap_window_left(Duration::ZERO),
            REAP_BOUND,
            "the first attempt must get the whole window"
        );

        // Three attempts, as the failing path really makes them: drive, then the cleanup, then Drop.
        let mut spent = Duration::ZERO;
        let mut attempts = 0;
        while reap_window_left(spent) > Duration::ZERO && attempts < 16 {
            // Each attempt uses whatever it is given, which is the worst case for the total.
            spent += reap_window_left(spent).min(REAP_BOUND / 3);
            attempts += 1;
        }
        assert!(
            spent <= REAP_BOUND,
            "three attempts spent {spent:?} against a {REAP_BOUND:?} window; a per-call window is         exactly the defect — the seat's advertised deadline + 2 * REAP_BOUND cannot survive it"
        );
        assert_eq!(
            reap_window_left(spent),
            Duration::ZERO,
            "an exhausted budget must leave nothing for a further attempt to wait on"
        );
        assert_eq!(
            reap_window_left(REAP_BOUND * 3),
            Duration::ZERO,
            "an overspent budget must saturate at zero rather than wrap"
        );

        // The ordinary path, against a real process: killed, confirmed, and the time charged.
        let mut child = KillableChild::spawn(Path::new("/bin/sh"), &["-c", "sleep 30"])
            .expect("spawn a child to reap");
        let pid = child.pid();
        let took = child.kill_and_reap().expect("a live child must be reapable");
        assert!(
            took <= REAP_BOUND,
            "reaping a shell took {took:?}, past the window it is allowed"
        );
        assert!(child.is_reaped(), "the child was not confirmed gone");
        assert!(
            child.spent_reaping <= REAP_BOUND,
            "the child was charged {:?} against a {REAP_BOUND:?} window",
            child.spent_reaping
        );
        // A second attempt on a reaped child costs nothing at all.
        assert_eq!(
            child.kill_and_reap().expect("already reaped"),
            Duration::ZERO
        );
        assert!(
            unsafe { libc::kill(pid, 0) } != 0,
            "the reaped child is still present"
        );
    }

    /// THE SIZING RULE, WITHOUT A CLOCK TO ARGUE WITH.
    ///
    /// The behavioural cadence gate in `tests/delivery_push_protocol_and_revocation.rs` can only
    /// bound the worst observed gap, and on a host whose timed wakeups are coalesced its floor is
    /// wider than the defect. The defect is a sizing rule, so it is proved here as one: every wait
    /// in a delivery is cut at ONE shared next-ask deadline, and a wait entered late in the current
    /// interval gets WHAT REMAINS of it rather than a fresh full interval of its own. No sleeping,
    /// nothing to be late, nothing a scheduler can make pass.
    #[test]
    fn poll_clock_sizes_every_wait_from_one_shared_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let plenty = Duration::from_secs(30);

        // A wait entered 40 ms into a 50 ms interval may block for the remaining 10, not for 50.
        // The old code computed `left.min(CANCELLATION_POLL)` here and got the full interval, which
        // is how a delivery making steady progress reached ~90 ms between observations.
        let mut clock = PollClock::armed_now();
        clock.next_ask = Instant::now() + Duration::from_millis(10);
        let slice = clock.slice(plenty);
        assert!(
            slice <= Duration::from_millis(10),
            "a wait entered late in the interval was sized {slice:?}; it must be cut at the shared             next ask, not given a slice of its own"
        );
        assert!(
            slice < CANCELLATION_POLL,
            "the wait was handed a full fresh interval ({slice:?}) despite most of the current one             already being spent"
        );

        // TWO CONSECUTIVE WAITS, NO ASK BETWEEN THEM. The second must not be refreshed by the first
        // having returned early: that is exactly the nested mint/ACK case, where an inner wait used
        // to start its own full slice with no knowledge of when the owner was last asked.
        let second = clock.slice(plenty);
        assert!(
            second <= slice,
            "a second wait with no ask between them was sized {second:?} after {slice:?}; the         deadline is shared, so it can only shrink"
        );

        // `left` still wins when the delivery's own deadline is nearer than the next ask.
        let nearly_over = Duration::from_millis(3);
        assert_eq!(
            clock.slice(nearly_over),
            nearly_over,
            "a wait must never be sized past the delivery's remaining time"
        );

        // Due means due: a zero slice, so the wait returns at once and the ask happens. A floor here
        // would let a wait outlive the deadline it exists to enforce.
        clock.next_ask = Instant::now() - Duration::from_millis(1);
        assert_eq!(
            clock.slice(plenty),
            Duration::ZERO,
            "an ask that is already due must not buy the next wait any time at all"
        );

        // An ask happens exactly once when due, and re-arms from the ask itself.
        let asked = Arc::new(AtomicUsize::new(0));
        let authority: crate::git_transport::AuthorityCheck = {
            let asked = Arc::clone(&asked);
            Arc::new(move || {
                asked.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };
        clock.ask_if_due(&authority).expect("still authorized");
        assert_eq!(asked.load(Ordering::SeqCst), 1, "a due ask was not made");
        clock.ask_if_due(&authority).expect("still authorized");
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "the owner was asked again immediately; re-arming must start a new interval, not         leave the ask due"
        );
        let after = clock.slice(plenty);
        assert!(
            after > CANCELLATION_POLL / 2 && after <= CANCELLATION_POLL,
            "the interval after an ask was {after:?}; it must be a full {CANCELLATION_POLL:?}"
        );

        // A revocation is reported to the caller rather than swallowed, and only when the ask is due.
        let ended: crate::git_transport::AuthorityCheck =
            Arc::new(|| Err("the owner of this delivery went away".to_owned()));
        let mut clock = PollClock::armed_now();
        clock.ask_if_due(&ended).expect("not due yet, so not asked");
        clock.next_ask = Instant::now() - Duration::from_millis(1);
        let why = clock
            .ask_if_due(&ended)
            .expect_err("a due ask must surface the owner's refusal");
        assert!(why.contains("went away"), "the refusal was rewritten: {why}");
    }

    /// The child's two bounds, and the rule that picks between them.
    ///
    /// The end-to-end consequence of the ABSOLUTE bound — a child that read its request too late
    /// opening no connection at all — is gated behaviourally against a real listener in
    /// `tests/delivery_push_transit_budget.rs`. What is gated here is the other half, which that
    /// harness cannot reach: a wall clock that steps BACKWARD between the parent's stamp and the
    /// child's read makes the absolute deadline the LARGER of the two, and the duration ceiling has
    /// to be what binds. This is the actual function the child calls, with the actual request type.
    #[test]
    fn the_child_takes_whichever_of_its_two_bounds_is_smaller() {
        let mut request = PushRequest {
            workdir: PathBuf::from("/tmp/delivery"),
            remote_url: "https://relay.example/repo.git".to_owned(),
            branch: "job-1".to_owned(),
            gated_oid: "0".repeat(40),
            authenticated: false,
            budget_ms: 0,
            deadline_unix_ms: 0,
        };
        let stamped = 1_000_000_000_000u64;

        // The ordinary case: stamped together, read instantly. The two agree.
        request.budget_ms = 30_000;
        request.deadline_unix_ms = stamped + 30_000;
        assert_eq!(
            child_budget(&request, stamped),
            Duration::from_millis(30_000),
            "a request read at the instant it was stamped gets what the parent measured"
        );

        // The transit: 5s passed between the write and the read. That is the parent's spend, and
        // the child must not be given it back.
        assert_eq!(
            child_budget(&request, stamped + 5_000),
            Duration::from_millis(25_000),
            "the pipe transit is charged to the child, not granted to it"
        );

        // Read after the deadline: nothing left, and nothing negative.
        assert_eq!(
            child_budget(&request, stamped + 31_000),
            Duration::ZERO,
            "a deadline already spent leaves zero budget, not a wrapped one"
        );

        // THE CEILING. A clock stepped an hour backward between stamp and read puts the absolute
        // deadline an hour away. The duration is what stops the child taking it.
        request.budget_ms = 500;
        request.deadline_unix_ms = stamped + 3_600_000;
        assert_eq!(
            child_budget(&request, stamped),
            Duration::from_millis(500),
            "a wall clock that steps backward must never buy a delivery more time than the parent \
             measured"
        );
    }

    #[test]
    fn a_frame_round_trips_through_the_pipe_encoding() {
        let request = PushRequest {
            workdir: PathBuf::from("/tmp/delivery"),
            remote_url: "https://relay.example/repo.git".to_owned(),
            branch: "job-1".to_owned(),
            gated_oid: "0".repeat(40),
            authenticated: true,
            budget_ms: 150_000,
            deadline_unix_ms: now_unix_ms().saturating_add(150_000),
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
    fn an_unknown_exit_retains_the_turn_rather_than_permitting_overlap() {
        // The rule the verdict names: issuing a kill is not proof of an exit. Only a reap the
        // kernel completed releases the seat; every other outcome — a stalled reap, a wait error,
        // anything at all — retains it, and that lost liveness is deliberate and named.
        assert_eq!(
            exclusion_after_reap(&Ok(Duration::from_millis(3))),
            Exclusion::Release
        );
        assert_eq!(
            exclusion_after_reap(&Err(ExecutorError::Unreaped {
                waited: REAP_BOUND
            })),
            Exclusion::Retain,
            "a child that could not be reaped must keep its seat closed"
        );
        assert_eq!(
            exclusion_after_reap(&Err(ExecutorError::Protocol("wait failed".to_owned()))),
            Exclusion::Retain,
            "an unreadable exit status is an UNKNOWN exit, and unknown fails closed"
        );
    }

    #[test]
    fn a_frame_larger_than_the_cap_is_refused_rather_than_buffered() {
        // Unbounded parent-side buffering is a phase outside the drain bound, so the reader caps
        // what one frame may cost before it costs it.
        let mut oversized = vec![b'x'; MAX_FRAME_BYTES + 16];
        oversized.push(b'\n');
        let mut reader = BufReader::new(oversized.as_slice());
        let refused = read_frame::<_, ToChild>(&mut reader);
        assert!(
            refused.is_err(),
            "a frame past the cap must be refused, not read into memory this process never bounded"
        );
    }

    /// The cleanup drain must be bounded by the LOOP's deadline, not by one wait's timeout. A queue
    /// refilled as fast as it is drained is the case that separates the two, and it is the case
    /// this cleanup exists for: something that escaped the kill is what does the refilling.
    #[test]
    fn the_cleanup_drain_returns_at_its_bound_while_frames_keep_arriving() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::{channel, sync_channel};

        let (tx, rx) = sync_channel::<u8>(4);
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let feeder_stop = std::sync::Arc::clone(&stop);
        // Holds its end of the channel and keeps it non-empty: the channel never disconnects and a
        // receive with any timeout, including a zero one, keeps succeeding.
        std::thread::spawn(move || {
            while !feeder_stop.load(Ordering::SeqCst) {
                if tx.send(1).is_err() {
                    return;
                }
            }
        });

        let bound = Duration::from_millis(300);
        let started = Instant::now();
        let (done, finished) = channel();
        std::thread::spawn(move || {
            let cleaned = drain_until_pipe_ends(&rx, bound, started);
            let _ = done.send((cleaned, started.elapsed()));
        });

        // A DEADLINE THAT IS NOT APPLIED ON THIS PATH NEVER RETURNS, so the failure this gate has to
        // produce is a failure and not a hang: the drain is run on its own thread and waited for.
        let (cleaned, took) = finished
            .recv_timeout(bound * 10)
            .expect("the cleanup drain never returned while frames kept arriving: its bound is not applied on the path that receives one");
        stop.store(true, Ordering::SeqCst);

        assert!(
            !cleaned,
            "a drain that never saw the channel disconnect must not report a cleaned-up pipe"
        );
        assert!(
            took < bound * 3,
            "the drain overran its bound by too much to call it bounded: {took:?}"
        );
    }

    /// **The false-cleanup counterexample, as behaviour.** A malformed frame used to end the reader
    /// thread; the channel then disconnected; and the cleanup read that disconnection as "the pipe
    /// closed". So one blank line was enough to make this parent report an observation it had never
    /// made — while the write end of that stdout was still held.
    ///
    /// The holder here is a reader that does not reach end of file until the test lets it, which is
    /// what the kernel does while any process still holds the write end. It is a model of an
    /// escaped descendant's effect on this parent, not a real escaped process: the group-kill gate
    /// in `delivery_push_custody.rs` owns that half.
    #[test]
    fn a_malformed_frame_is_not_end_of_file_and_does_not_end_the_reader() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::sync_channel;

        /// Yields the scripted bytes, then BLOCKS — no end of file — until `closed` is set.
        struct HeldPipe {
            script: Vec<u8>,
            at: usize,
            closed: std::sync::Arc<AtomicBool>,
        }

        impl std::io::Read for HeldPipe {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.at < self.script.len() {
                    let take = (self.script.len() - self.at).min(buf.len());
                    buf[..take].copy_from_slice(&self.script[self.at..self.at + take]);
                    self.at += take;
                    return Ok(take);
                }
                while !self.closed.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(0)
            }
        }

        let closed = std::sync::Arc::new(AtomicBool::new(false));
        let pipe = HeldPipe {
            script: b"\n".to_vec(),
            at: 0,
            closed: std::sync::Arc::clone(&closed),
        };
        let (sink, frames) = sync_channel(MAX_QUEUED_FRAMES);
        let end = std::sync::Arc::new(std::sync::Mutex::new(None));
        let reader = pump(pipe, sink, std::sync::Arc::clone(&end));

        let malformed = frames
            .recv_timeout(Duration::from_secs(5))
            .expect("the malformed frame must reach the parent");
        assert!(
            malformed.is_err(),
            "a blank line reached the parent as a clean end of stream: {malformed:?}"
        );

        // THE FACT UNDER TEST: with the write end still held, this parent must NOT be able to
        // conclude the pipe ended. The drain has to time out.
        assert!(
            !drain_until_pipe_ends(&frames, Duration::from_millis(400), Instant::now()),
            "the parent concluded its child's pipe had closed while that pipe was still held; a \
             reader that stopped is not a descriptor that closed"
        );
        assert_eq!(
            end.lock().expect("pump end").clone(),
            None,
            "the reader ended on a malformed frame instead of reading on to the actual end"
        );

        // And when the holder really does let go, the same parent observes the real thing.
        closed.store(true, Ordering::SeqCst);
        assert!(
            drain_until_pipe_ends(&frames, Duration::from_secs(5), Instant::now()),
            "a pipe whose holders let go must drain to a clean end"
        );
        reader.join().expect("reader thread");
        assert_eq!(
            end.lock().expect("pump end").clone(),
            Some(PumpEnd::Eof),
            "the only thing that may be recorded as end of file is a read that returned zero bytes"
        );
    }

    /// The positive half of the same rule: when the writers really do let go, the drain says so.
    #[test]
    fn the_cleanup_drain_reports_a_pipe_whose_writers_let_go() {
        use std::sync::mpsc::sync_channel;

        let (tx, rx) = sync_channel::<u8>(4);
        tx.send(7).expect("queue one frame behind the outcome");
        drop(tx);
        assert!(
            drain_until_pipe_ends(&rx, Duration::from_millis(500), Instant::now()),
            "a channel whose sender is gone must drain to a clean end"
        );
    }

    /// Three different facts, three different answers. A blank line is a MALFORMED FRAME; only a
    /// read that returns zero bytes is end of file.
    #[test]
    fn a_blank_line_is_a_malformed_frame_and_only_a_closed_pipe_is_end_of_file() {
        let mut blank = BufReader::new(&b"\n"[..]);
        let parsed = read_frame::<_, ToChild>(&mut blank);
        assert!(
            parsed.is_err(),
            "a blank line must be reported as a malformed frame, not as the stream ending"
        );

        let mut empty = BufReader::new(&b""[..]);
        assert!(
            matches!(read_frame::<_, ToChild>(&mut empty), Ok(None)),
            "a read that returns zero bytes is end of file, and is the only thing that is"
        );
    }

    /// An authorization that both grants and refuses is a contradiction, and a contradiction on this
    /// channel is a refusal. The child used to take the header and transmit.
    #[test]
    fn an_authorization_that_grants_and_refuses_is_refused() {
        assert_eq!(
            minted_answer(Some("Nostr abc".to_owned()), None),
            Ok("Nostr abc".to_owned())
        );
        assert_eq!(
            minted_answer(None, Some("revoked".to_owned())),
            Err("revoked".to_owned())
        );
        let ambiguous = minted_answer(Some("Nostr abc".to_owned()), Some("revoked".to_owned()));
        assert!(
            ambiguous
                .as_ref()
                .err()
                .is_some_and(|why| why.contains("both granted and refused")),
            "a reply carrying a header AND a refusal must not be read as permission: {ambiguous:?}"
        );
        assert!(
            minted_answer(None, None).is_err(),
            "an empty authorization is not permission"
        );
    }

    /// The override policy itself. A relative path is resolved against a working directory this
    /// process does not control, which is the `PATH` hole in a different spelling; it used to be
    /// accepted as given.
    #[test]
    fn a_child_program_override_must_be_an_absolute_path_to_a_file() {
        use std::ffi::OsStr;

        let relative = child_program_from_override(OsStr::new("maxplayer"));
        assert!(
            relative
                .as_ref()
                .err()
                .is_some_and(|why| why.to_string().contains("relative path")),
            "a relative override must be refused: {relative:?}"
        );
        assert!(
            child_program_from_override(OsStr::new("")).is_err(),
            "an empty override must be refused"
        );
        assert!(
            child_program_from_override(OsStr::new("/nonexistent/maxplayer-delivery-child"))
                .is_err(),
            "an override naming nothing on disk must be refused"
        );
        // And the case production depends on: this test binary's own path, which is what a harness
        // sets, is accepted unchanged.
        let me = std::env::current_exe().expect("current_exe");
        assert_eq!(
            child_program_from_override(me.as_os_str()).expect("an absolute existing file"),
            me
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
