//! Establishing egress containment inside the job's own network namespace (#797).
//!
//! [`crate::sandbox_net`] renders the policy; this module puts it in force. Three containers, in one
//! order that is not negotiable:
//!
//! 1. a **holder** — a trivial container that exists only to own a network namespace,
//! 2. a **sidecar** — joins that namespace, applies the rendered rules, exits,
//! 3. the **job** — joins the same namespace, and so starts with the rules already in place.
//!
//! The holder is what closes the race. A sidecar cannot apply rules to a namespace that does not
//! exist yet, and a job that creates its own namespace is already running before anything can be
//! installed into it — measured at 236 ms of uncontained execution. By making a third container own
//! the namespace, the rules are in force *before the job process exists at all*, so the window is not
//! narrowed, it is absent.
//!
//! ## Why the job's argv changes shape here
//!
//! `--network=container:<holder>` puts the job in the holder's namespace, and the daemon then refuses
//! several networking flags outright — `--add-host` among them:
//!
//! ```text
//! docker: Error response from daemon: conflicting options: custom host-to-IP mapping and the network mode
//! ```
//!
//! So the job cannot be given the `host.docker.internal` alias it used to reach the credential proxy,
//! and putting `--add-host` on the *holder* would be theatre: `/etc/hosts` is per-mount-namespace and
//! these containers share only the network one. The job therefore receives a **literal address**, and
//! [`host_gateway_probe_argv`] measures it rather than computing it — see the warning there, because
//! the obvious computation is wrong in a way no rendering test can see.
//!
//! Name resolution needs its own answer. A container joining the namespace still gets its own
//! `/etc/resolv.conf` pointing at docker's embedded resolver on `127.0.0.11`, which is why
//! `sandbox_net`'s "loopback is never denied" test is load-bearing rather than decorative for a
//! runc seat. Under gVisor that resolver never answers at all — the sandbox terminates loopback in
//! its own network stack, so the packet never reaches the daemon's socket — so a contained job is
//! handed a real resolver file instead and this module carries the addresses inside it through to
//! the policy, one port-53 exception per resolver. See [`crate::sandbox_dns`].

use crate::sandbox_net::{Family, NetPolicy};

/// The containment sidecar image, pinned to this build's version exactly as
/// [`crate::seller_exec::DEFAULT_SANDBOX_IMAGE`] is. Both images are published by the same workflow
/// job on the same tag: a version that shipped one but not the other cannot start a contained job at
/// all, so they are deliberately impossible to skew.
pub const DEFAULT_NETFILTER_IMAGE: &str =
    concat!("ghcr.io/makeprisms/maxplayer-netfilter:v", env!("CARGO_PKG_VERSION"));

/// The docker label every holder carries, so an orphan left by a crashed daemon can be found and
/// reaped by something that never saw the job that created it.
pub const HOLDER_LABEL: &str = "ai.maxplayer.netns-holder";

/// The docker label carrying the **owning seat** of a holder — the seller public key hex, which is
/// stable across restarts, unique per seat, and not secret.
///
/// **Why ownership is carried and not inferred.** A holder is unattached twice in every job's life:
/// between [`establish`] creating it and the job joining it, and again after the job exits. So "no
/// job attached" is a normal state, not evidence of abandonment, and no measurement of liveness or
/// age can recover *whose* holder it is — age lowers the odds of a collision without ever
/// establishing ownership. This label is the answer, and it is why the reaper can run on a host
/// where several seller daemons share a docker socket.
///
/// A holder carrying no seat label belongs to nobody this build can name, so it is **never reaped**.
/// That leaks a container rather than destroying another seat's running job, which is the direction
/// this whole module chooses whenever it has to choose.
pub const HOLDER_SEAT_LABEL: &str = "ai.maxplayer.netns-holder-seat";

/// The docker label carrying the absolute unix second after which this seat may remove the
/// container **without consulting anything in this process**.
///
/// **Why the expiry is written into the container instead of remembered.** Everything this module
/// used to rely on to finish a cleanup — a retained owner, a supervisor, a watch on the daemon's
/// event stream — lives in the process that created the container, and so dies with it. A stamp on
/// the container itself is the one record that survives a `SIGKILL`, a crash mid-create, and a
/// container the daemon only materialises after this process is gone. The sweep that reads it needs
/// no memory of the job at all: it asks docker what exists, and the answer carries its own verdict.
///
/// **Why `deadline + grace` and not a fixed cap.** A seat's jobs do not share a lifetime — the
/// deadline is `--job-timeout-secs`, else the offer's own deadline, else the default, chosen per job
/// by [`crate::seller::job_deadline_unix`]. A single global age would either strangle a long job
/// that was legitimately awarded a long deadline, or leave a short one lying around for hours. This
/// label carries the job's OWN effective deadline plus [`CLEANUP_GRACE_SECS`], so each container is
/// judged against the lifetime its own job was actually granted.
///
/// A container carrying no expiry label, or one that does not parse, is **never** swept on this
/// path: an unreadable stamp is not an expired one, and the boot reaper remains the backstop for
/// anything older than this build.
pub const HOLDER_CLEANUP_AFTER_LABEL: &str = "ai.maxplayer.netns-cleanup-after";

/// The docker label naming what the container was for: the namespace holder, or one of the
/// short-lived helpers that join its namespace.
///
/// Carried so the sweep can report what it removed in terms an operator can act on, and so a future
/// role can be excluded without having to guess from a container name.
pub const HOLDER_ROLE_LABEL: &str = "ai.maxplayer.netns-role";

/// The holder that owns the job's network namespace for the whole run.
pub const ROLE_HOLDER: &str = "holder";

/// A short-lived helper that joins the holder's namespace (plan applier, readback probe).
pub const ROLE_HELPER: &str = "helper";

/// How long after a job's own effective deadline its containers become sweepable.
///
/// **One hour, and the size is the point.** The deadline is when the job must be finished, not when
/// its containers stop being legitimately in use: delivery, evidence capture and the teardown that
/// normally removes these containers all happen after it. A grace shorter than that work would have
/// the sweep racing the ordinary cleanup path for a container still in use — the one outcome worse
/// than the leak it exists to fix. An hour is far past any of it, and the cost of the margin is a
/// dead container occupying a name and no policy for at most that long.
pub const CLEANUP_GRACE_SECS: u64 = 3_600;

/// The value for [`HOLDER_CLEANUP_AFTER_LABEL`]: this job's effective deadline plus the grace.
///
/// Saturating, so a deadline near `u64::MAX` yields `u64::MAX` — a container that is never swept on
/// this path — rather than wrapping to zero and becoming instantly removable while its job runs.
/// Of the two failures available to arithmetic here, leaking is the one that does not destroy live
/// work.
#[must_use]
pub fn cleanup_after_unix(effective_deadline_unix: u64) -> u64 {
    effective_deadline_unix.saturating_add(CLEANUP_GRACE_SECS)
}

/// How long any one `docker` invocation in this module may take before it is killed. A create or a
/// sidecar that never returns would otherwise hold the launch open indefinitely, and an unbounded
/// wait is the state in which cancellation leaves work nobody owns.
pub const DOCKER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// How long a deadline-killed command waits for its plan writer to notice the closed pipe.
///
/// Killing the child closes the read end, so a blocked `write_all` fails with `EPIPE` almost at
/// once. This grace exists so that the common case is JOINED rather than abandoned; a writer still
/// running after it is reported as outstanding, never waited on indefinitely. It is a bound on how
/// long this process will wait for that thread -- not a claim about how quickly any particular
/// writer unblocks.
const WRITER_EPIPE_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

/// The docker client one containment lifecycle spawns, carried **explicitly** by the code that uses
/// it.
///
/// There is no environment variable, no global, and no configuration field behind this. The only
/// constructor a shipped build can reach is [`DockerCli::system`], which is the constant `docker` on
/// `PATH`; a test that needs a stand-in passes one in as an argument to the single call under test,
/// so two tests running in parallel cannot see or disturb each other's client.
///
/// The earlier shapes of this seam were both wrong, in instructive ways. An environment variable let
/// anyone able to set a variable on the process redirect every containment command — create,
/// inspect, remove — at a binary of their choosing, in a shipped build. Moving it to a
/// `cfg(test)` process-global removed the shipped exposure but not the interference: a global is
/// still shared, still needs a lock every reader must remember to take, and any helper that forgot
/// — cleanup running from `Drop`, for instance — read whatever another test had installed. An
/// argument has neither failure mode.
#[derive(Clone, Debug)]
pub struct DockerCli {
    program: std::sync::Arc<str>,
}

impl DockerCli {
    /// The production client: `docker`, resolved on `PATH`. Nothing selects another.
    #[must_use]
    pub fn system() -> Self {
        Self { program: std::sync::Arc::from("docker") }
    }

    /// Test-only: an explicit stand-in, handed to one call. Not reachable from a shipped build.
    #[cfg(test)]
    fn stand_in(path: &std::path::Path) -> Self {
        Self { program: std::sync::Arc::from(path.to_string_lossy().as_ref()) }
    }

    fn program(&self) -> &str {
        &self.program
    }
}

/// How long cleanup will wait for an in-flight create, and how long an owner keeps trying after it.
///
/// A parameter rather than a constant so the delayed path can be exercised in under a second. The
/// production values are [`FenceBounds::production`]; nothing else constructs one outside tests.
#[derive(Clone, Copy, Debug)]
struct FenceBounds {
    /// How long `Drop` itself blocks before handing off to an owner.
    fast: std::time::Duration,
    /// The outer bound on the handed-off owner, counted from when it takes over.
    max: std::time::Duration,
    /// How long the owner keeps asking the daemon to confirm the removal it issued.
    confirm: std::time::Duration,
    /// How much longer the owner KEEPS the job after `max` expires with the create still running.
    ///
    /// `max` is where an owner used to stop being an owner: it swept, printed a leak, and returned
    /// while the create was still in flight, so a container landing one millisecond later had
    /// nobody responsible for it. This is the window in which that container is still SOMEBODY'S —
    /// the owner stays on the create's own schedule, removes what lands, and confirms it gone.
    retain: std::time::Duration,
    /// The base interval at which the [`CleanupSupervisor`] re-attempts an obligation it holds.
    ///
    /// Doubled per failed attempt up to [`CleanupSupervisor::BACKOFF_CAP_MULTIPLE`] times this.
    /// This is a SCHEDULE, not a window: it decides when the next attempt runs, never whether there
    /// is one. Nothing the supervisor holds is released by the passage of time.
    reschedule: std::time::Duration,
}

impl FenceBounds {
    /// The bound that matters is the create client's own: [`DOCKER_DEADLINE`] kills it at 120s, so a
    /// blocking create closure cannot outlive that, and an owner waiting a margin past it waits for
    /// an event that is guaranteed to have happened rather than for a guessed duration.
    fn production() -> Self {
        Self {
            fast: NetnsHolder::CREATE_SETTLE_DEADLINE,
            max: DOCKER_DEADLINE + std::time::Duration::from_secs(15),
            confirm: std::time::Duration::from_secs(10),
            // A create that has not settled by `max` is past its own client's kill, so this covers
            // a daemon still working after the client it answered is gone — the case where the
            // container appears with no client left to attribute it to.
            retain: DOCKER_DEADLINE,
            reschedule: std::time::Duration::from_secs(5),
        }
    }
}

/// A running holder container, and the guarantee that it goes away.
///
/// Constructed **before** the container does, so that every `?` — and every cancellation — after
/// that point tears it down on the way out. The holder is a resource with a lifetime, not a step in
/// a procedure.
///
/// The guard also owns the **temporary containers joined to the namespace**. A sidecar is a joiner:
/// while it lives the namespace cannot go away, so removing the holder while an applier or a
/// readback is still running leaves the namespace pinned by a process nobody is tracking. Every
/// sidecar is therefore named, registered here for its lifetime, and force-removed before the holder
/// is.
/// Counts creates that may still be in flight **after** the future awaiting them is gone.
///
/// Adoption alone was never a fence. It supplies a NAME to remove; it says nothing about WHEN the
/// container under that name comes into existence. A create runs on a blocking pool thread, and
/// cancelling the future above it does not stop that thread: the create can still be queued inside
/// the daemon, or half-finished, at the instant cleanup runs. Cleanup then asks docker to remove a
/// container that does not exist YET, is told "No such container" — which this module correctly
/// treats as benign — and returns satisfied. Moments later the create lands. The result is a
/// running container with a name nobody holds, which is the exact orphan the registry exists to
/// prevent, manufactured by the cleanup path.
///
/// So a single early remove is not enough, and no ordering of removes fixes it: the remove has to
/// happen on the far side of the create SETTLING. This fence is that far side. Every create takes a
/// ticket before it is issued, the ticket is moved into the blocking closure, and it is released
/// when that closure ends — whether it succeeded, failed, was killed on the deadline, or ran on
/// past a cancelled future. Cleanup waits for the count to reach zero before it removes anything.
#[derive(Debug)]
struct CreationFence {
    in_flight: std::sync::Mutex<usize>,
    settled: std::sync::Condvar,
    /// How many tickets this fence has EVER issued, which only ever grows.
    ///
    /// `in_flight` answers "is work outstanding now" and is therefore blind to work that started
    /// and finished between two observations. This answers the different question "was anything
    /// ever started under this fence at all", which is the only way to tell a create that was
    /// refused before it began from one that was launched and instantly killed — those two look
    /// identical from the outside, and exactly one of them leaves a container behind.
    issued: std::sync::Mutex<usize>,
    /// The owners this fence is RETAINING for jobs whose bounded owner ran out of wait.
    ///
    /// A bounded owner that reaches its limit with the create still in flight has exactly two
    /// honest options: keep waiting (which only moves the edge), or hand the job to something that
    /// outlives it. This is the something. It holds EVERY job handed to it — two owners that arrive
    /// on the same fence are two obligations, and the second does not replace the first — and it is
    /// drained the moment the create settles. It is still not a registry of containers: each entry
    /// is one job for the one holder this fence exists to count.
    retained: std::sync::Mutex<Vec<RetainedOwner>>,
    /// Creates issued under this fence whose client NEVER GOT THE DAEMON'S ANSWER.
    ///
    /// A client killed on its deadline, or one that lost its connection mid-request, has sent a
    /// request the daemon may still be applying. For such a name, "absent now" is not "will never
    /// exist": the ticket's release says only that THIS PROCESS is done, and the daemon was never
    /// heard from. These are recorded here by the closure that killed the client and are turned into
    /// [`Owed::WatchForLanding`] obligations at the moment this fence settles.
    unanswered: std::sync::Mutex<Vec<UnansweredCreate>>,
    /// Where an obligation goes when this fence can no longer hold it.
    ///
    /// A fence lives exactly as long as its holder and its tickets. The obligations it holds do not
    /// have that lifetime: a removal the daemon will not confirm is owed for as long as the process
    /// runs. So the fence is never the LAST owner — when it is destroyed with work still owed, that
    /// work moves here rather than dying with it.
    supervisor: std::sync::Arc<CleanupSupervisor>,
    /// The bounds any obligation this fence hands on will run under.
    bounds: FenceBounds,
}

impl Default for CreationFence {
    /// Production fences all report to the one process-lifetime supervisor.
    fn default() -> Self {
        Self::supervised_by(CleanupSupervisor::process(), FenceBounds::production())
    }
}

/// How many times a fence being DESTROYED re-runs an owner it still holds before handing it on.
///
/// A removal that could not be confirmed puts itself back, so the runner — not the job — is what
/// bounds the attempts. Destruction is the last moment THIS FENCE can act, so it spends a few
/// attempts here and then transfers what is still owed to the [`CleanupSupervisor`], which has no
/// such last moment. It is a bound on this fence's work, not on the obligation.
const RETAINED_FINAL_RUNS: usize = 3;

/// One create the daemon was never heard to answer, and when its request was sent.
#[derive(Clone, Debug)]
struct UnansweredCreate {
    name: String,
    issued: std::time::SystemTime,
    /// The client that issued the request, so the watch asks the same daemon.
    client: DockerCli,
}

/// WHAT an owner owes on its names, which decides what observation discharges it.
#[derive(Clone, Copy, Debug)]
enum Owed {
    /// Remove the names and CONFIRM each one absent. The create for these names was ANSWERED by the
    /// daemon (the client returned, or was refused before it asked), so a confirmed absence is the
    /// end of the story: nothing is left that could still land.
    RemoveAndConfirm,
    /// The daemon never answered the create for these names, so absence proves nothing. What
    /// discharges one of these is a DAEMON observation that the create ran its course: the container
    /// is seen present (then removed and confirmed gone), or the daemon's own event log since
    /// `issued` shows a container under exactly this name (it landed, and is already gone). No clock
    /// discharges it. If neither observation ever arrives, the name stays watched for as long as the
    /// process lives, because the API has no way to say "that request will never be applied".
    WatchForLanding { issued: std::time::SystemTime },
}

impl Owed {
    fn label(self) -> &'static str {
        match self {
            Owed::RemoveAndConfirm => "remove-and-confirm",
            Owed::WatchForLanding { .. } => "watch-for-landing",
        }
    }
}

/// Cleanup somebody holds on a holder's behalf after its bounded owner is gone.
///
/// Data, not a closure. A closure that is dropped does nothing and leaves no trace, and it cannot
/// be reported on, merged, or re-scheduled by anyone but the code that built it. This carries the
/// names it is responsible for and everything needed to act on them, so any owner — the fence, the
/// supervisor, a test — can run one attempt and see exactly what is still outstanding afterwards.
#[derive(Clone)]
struct RetainedOwner {
    holder: String,
    names: Vec<String>,
    client: DockerCli,
    bounds: FenceBounds,
    owed: Owed,
}

/// What a retained owner reports after one attempt.
///
/// The point of returning this rather than `()` is that a cleanup which issued a removal and could
/// not confirm absence has NOT finished, and must not be able to end by returning quietly. It hands
/// back the job that is still owed, and the runner decides when it runs next — never whether.
enum Custody {
    /// Every name is discharged on a daemon observation. Nothing is owed.
    Discharged,
    /// Some names are still owed. This is the job that owes them, narrowed to exactly those names.
    StillOwed(RetainedOwner),
}

/// What happened when a job was handed to a fence.
///
/// A registration that silently does nothing is the failure this type exists to make impossible:
/// the caller cannot ignore the settled case, because the job comes back and must be run.
#[must_use = "an already-settled fence hands the job back, and it must be run or it is lost"]
enum Registration {
    /// The fence took it; the create is still in flight, so a future last-ticket drop will run it.
    Retained,
    /// NOTHING was in flight at the instant of registration, so no future drop exists to run it.
    /// The job is handed back rather than parked where it would never fire.
    AlreadySettled(RetainedOwner),
}

impl std::fmt::Debug for RetainedOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "RetainedOwner({}, still owns {})",
            self.owed.label(),
            self.names.join(", ")
        )
    }
}

impl RetainedOwner {
    /// One bounded attempt at what this owner owes. Nothing here waits on a clock of its own: each
    /// step is a removal, an inspect or an event query, all bounded by the existing deadlines.
    fn attempt(self) -> Custody {
        match self.owed {
            Owed::RemoveAndConfirm => self.remove_and_confirm(),
            Owed::WatchForLanding { issued } => self.watch_for_landing(issued),
        }
    }

    /// Remove and confirm. A failed confirmation returns THIS owner again over exactly the names
    /// that could not be confirmed, so responsibility narrows to what is actually outstanding
    /// instead of being discarded at the first disappointment.
    fn remove_and_confirm(self) -> Custody {
        // A fresh fence deliberately: the create this would have waited for is the one that already
        // ended, so there is nothing left to wait for and the owner goes straight to removal and
        // confirmation. It keeps no `Arc` back to the fence that holds it — that would be a cycle.
        let owner = HolderCleanup {
            name: self.holder.clone(),
            joiners: self.names.iter().filter(|name| **name != self.holder).cloned().collect(),
            creation: std::sync::Arc::new(CreationFence::default()),
            client: self.client.clone(),
            bounds: self.bounds,
        };
        owner.sweep();
        match owner.confirm_all_absent() {
            Ok(()) => Custody::Discharged,
            Err(mut pending) => {
                pending.sort();
                pending.dedup();
                eprintln!(
                    "sandbox: a retained owner removed {} but could not confirm absence within {:?} \
                     — it is NOT releasing them: the job is still owed",
                    pending.join(", "),
                    self.bounds.confirm
                );
                Custody::StillOwed(RetainedOwner { names: pending, ..self })
            }
        }
    }

    /// Look for a create the daemon was never heard to answer.
    ///
    /// Per name, exactly one of three daemon observations, and a query that fails is none of them:
    ///  * `inspect` finds it: it LANDED. Remove it and confirm it gone; only then is it discharged.
    ///  * `inspect` says absent AND the daemon's event log since the request shows a COMPLETED
    ///    lifecycle under this exact name — a `create` and a `destroy` for the SAME container id,
    ///    and no id created under the name that lacks its `destroy` — AND a second `inspect`, taken
    ///    AFTER the event query, still says absent. Then it landed and is already gone. Discharged.
    ///  * Anything else: still owed. Absence alone is what the first version mistook for proof, and
    ///    "any event under the name" is what the second did: an inspect that says absent, a landing
    ///    a moment later and an event log that then shows that landing's `create` is a LIVE
    ///    container, and the previous version discharged it on exactly that evidence. A `create`
    ///    without its `destroy` now keeps the name owed; the next attempt finds it present and
    ///    removes it.
    fn watch_for_landing(self, issued: std::time::SystemTime) -> Custody {
        let mut still_owed = Vec::new();
        for name in &self.names {
            match container_is_absent(&self.client, name) {
                Some(false) => {
                    eprintln!(
                        "sandbox: {name} LANDED after its create client was never answered — the \
                         watching owner is removing it now"
                    );
                    if let Err(error) = NetnsHolder::force_remove(&self.client, name) {
                        eprintln!("sandbox: could not remove late-landing {name}: {error} — still owed");
                        still_owed.push(name.clone());
                        continue;
                    }
                    if container_is_absent(&self.client, name) != Some(true) {
                        still_owed.push(name.clone());
                    }
                }
                Some(true) => {
                    let lifecycle = lifecycle_since(&self.client, name, issued, self.bounds.confirm);
                    // The order of these three observations is the evidence: absent, then the
                    // daemon's record that what was created under this name has ALSO been destroyed,
                    // then absent AGAIN after that record was read. A landing between the first
                    // inspect and the event query shows up as a `create` with no `destroy` and is
                    // retained; a landing after the event query shows up in the second inspect.
                    let completed_and_gone = lifecycle == Some(Lifecycle::Completed)
                        && container_is_absent(&self.client, name) == Some(true);
                    if completed_and_gone {
                        eprintln!(
                            "sandbox: the daemon's event log shows the container created under \
                             {name} after its unanswered request was also destroyed, and it is \
                             absent again after that record — discharged on that observation"
                        );
                    } else {
                        if lifecycle == Some(Lifecycle::Landed) {
                            eprintln!(
                                "sandbox: {name} was created after its unanswered request and the \
                                 daemon has NOT recorded its destruction — it is live or its end is \
                                 unknown, so it stays owed and the next attempt removes it"
                            );
                        }
                        still_owed.push(name.clone());
                    }
                }
                None => still_owed.push(name.clone()),
            }
        }
        if still_owed.is_empty() {
            Custody::Discharged
        } else {
            Custody::StillOwed(RetainedOwner { names: still_owed, ..self })
        }
    }
}

/// Build the owner that removes `names` under holder `name` and confirms each one absent.
fn retained_removal(
    name: String,
    names: Vec<String>,
    client: DockerCli,
    bounds: FenceBounds,
) -> RetainedOwner {
    RetainedOwner { holder: name, names, client, bounds, owed: Owed::RemoveAndConfirm }
}

/// Build the owner that watches for one unanswered create to land.
fn retained_watch(create: UnansweredCreate, bounds: FenceBounds) -> RetainedOwner {
    RetainedOwner {
        holder: create.name.clone(),
        names: vec![create.name],
        client: create.client,
        bounds,
        owed: Owed::WatchForLanding { issued: create.issued },
    }
}

/// The owner of last resort for this process: cleanup that no fence can hold any longer.
///
/// Every other owner in this module has an end — a bounded wait, a settlement event, a destructor.
/// Each of those ends used to be where responsibility quietly stopped. This has no such end short of
/// the process itself: an obligation adopted here is retried on a schedule, with bounded work per
/// attempt and a backoff between attempts, until a daemon observation discharges it. It is an
/// in-memory queue and one thread. It is deliberately NOT a journal, a registry of every container,
/// or anything that survives the process — the boot reaper remains the backstop across a restart.
///
/// What wakes it: its own schedule (the earliest `due` among what it holds), and every adoption.
/// Nothing else has to remember it exists.
///
/// Its thread is started when the first fence reports to it — at ESTABLISH time, while the process
/// is creating a holder, not at the exhaustion moment when a destructor hands work over — and it
/// parks when idle rather than exiting, so the spawn happens once. When there is no thread anyway
/// (the spawn failed), progress does not wait for a future adoption: every cleanup event in the
/// process — a create settling, a fence or a holder being destroyed, another adoption — retries the
/// spawn and, failing that, runs one due attempt inline on the thread that raised the event.
struct CleanupSupervisor {
    state: std::sync::Mutex<SupervisorState>,
    changed: std::sync::Condvar,
    /// Test-only: make every thread spawn fail, so the no-thread path can be driven deterministically
    /// rather than by exhausting the process's thread limit.
    #[cfg(test)]
    refuse_threads: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for CleanupSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        write!(
            formatter,
            "CleanupSupervisor(owes {} job(s), {} attempt(s) run)",
            state.queued.len() + usize::from(!state.running.is_empty()),
            state.attempts
        )
    }
}

#[derive(Default)]
struct SupervisorState {
    queued: Vec<ScheduledOwner>,
    /// The names of the owner whose attempt is running right now. It is out of the queue while it
    /// runs, and it is still owned; this is what keeps "outstanding" truthful across that moment.
    running: Vec<String>,
    worker_alive: bool,
    /// Attempts this supervisor has run, ever. Lets a test assert that scheduling HAPPENED rather
    /// than that a flag says it would.
    attempts: usize,
}

struct ScheduledOwner {
    owner: RetainedOwner,
    due: std::time::Instant,
    failures: u32,
}

/// A snapshot of one obligation the supervisor holds, for reporting and for assertion.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Outstanding {
    holder: String,
    names: Vec<String>,
    kind: &'static str,
}

impl CleanupSupervisor {
    /// The backoff stops doubling at this multiple of `bounds.reschedule`.
    const BACKOFF_CAP_MULTIPLE: u32 = 8;

    /// The one supervisor every production fence reports to, created on first use.
    fn process() -> &'static std::sync::Arc<Self> {
        static PROCESS: std::sync::OnceLock<std::sync::Arc<CleanupSupervisor>> =
            std::sync::OnceLock::new();
        PROCESS.get_or_init(Self::new)
    }

    /// A supervisor of its own, so a test can own and inspect exactly what it hands over.
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            state: std::sync::Mutex::new(SupervisorState::default()),
            changed: std::sync::Condvar::new(),
            #[cfg(test)]
            refuse_threads: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SupervisorState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take an obligation, permanently. Its first attempt is due now.
    fn adopt(self: &std::sync::Arc<Self>, owner: RetainedOwner) {
        let names = owner.names.join(", ");
        let kind = owner.owed.label();
        {
            let mut state = self.lock();
            state.queued.push(ScheduledOwner {
                owner,
                due: std::time::Instant::now(),
                failures: 0,
            });
        }
        self.changed.notify_all();
        eprintln!(
            "sandbox: the cleanup supervisor now owns {names} ({kind}) and will retry on a schedule \
             until the daemon confirms it settled"
        );
        self.poke();
    }

    /// Make sure a worker thread exists, if one can. Called when a fence is created — at establish
    /// time — so the spawn happens while the process is building, not while it is tearing down.
    fn ensure_worker(self: &std::sync::Arc<Self>) {
        let claimed = {
            let mut state = self.lock();
            if state.worker_alive {
                false
            } else {
                state.worker_alive = true;
                true
            }
        };
        if !claimed {
            return;
        }
        if let Err(error) = self.spawn_worker() {
            self.lock().worker_alive = false;
            eprintln!(
                "sandbox: could not start the cleanup supervisor thread ({error}) — scheduled \
                 cleanup will be driven inline from cleanup events until a thread can be started"
            );
        }
    }

    /// Drive owed work forward from ANY cleanup event, without depending on a future adoption.
    ///
    /// With a worker alive this is a wake, which is free. Without one — the spawn failed at every
    /// earlier opportunity — this retries the spawn, and if that fails too it runs ONE due attempt
    /// inline on the calling thread, bounded like every attempt is. The supervisor's queue therefore
    /// makes progress on the process's own cleanup activity: every create that settles, every fence
    /// and holder destroyed, every adoption. What it does NOT promise, and this is named: with no
    /// thread ever available and no further cleanup activity in the process, the next attempt waits
    /// for the next such event. That is the residual, and it is bounded by the process's own life.
    fn poke(self: &std::sync::Arc<Self>) {
        let needs_worker = {
            let mut state = self.lock();
            if state.queued.is_empty() || state.worker_alive {
                false
            } else {
                state.worker_alive = true;
                true
            }
        };
        self.changed.notify_all();
        if !needs_worker {
            return;
        }
        if let Err(error) = self.spawn_worker() {
            // No thread means nothing is scheduled, and saying "adopted" would be a lie. One attempt
            // runs inline right now so the obligation is at least acted on; it stays queued, and
            // EVERY later cleanup event retries the thread and runs the next due attempt.
            self.lock().worker_alive = false;
            eprintln!(
                "sandbox: could not start the cleanup supervisor thread ({error}) — running one due \
                 attempt inline; the queue is kept and every later cleanup event drives it"
            );
            self.run_one_due_inline();
        }
    }

    fn spawn_worker(self: &std::sync::Arc<Self>) -> std::io::Result<()> {
        #[cfg(test)]
        if self.refuse_threads.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::other("thread spawn refused by the test"));
        }
        let serving = std::sync::Arc::clone(self);
        std::thread::Builder::new()
            .name("mx-cleanup-supervisor".to_owned())
            .spawn(move || serving.serve())
            .map(|_| ())
    }

    /// The worker: run whatever is due, sleep until the next due time, park while nothing is owed.
    ///
    /// It does not exit when the queue empties. Exiting made every later adoption a fresh spawn —
    /// at exactly the moment the process is tearing something down — and a spawn that fails there
    /// is what leaves work with no thread. One thread for the process's life is the cheaper trade.
    fn serve(self: std::sync::Arc<Self>) {
        loop {
            let next = {
                let mut state = self.lock();
                loop {
                    if state.queued.is_empty() {
                        state = self
                            .changed
                            .wait(state)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        continue;
                    }
                    let now = std::time::Instant::now();
                    let (index, due) = state
                        .queued
                        .iter()
                        .enumerate()
                        .map(|(index, scheduled)| (index, scheduled.due))
                        .min_by_key(|(_, due)| *due)
                        .expect("non-empty");
                    if due <= now {
                        let scheduled = state.queued.swap_remove(index);
                        state.running = scheduled.owner.names.clone();
                        break scheduled;
                    }
                    state = self
                        .changed
                        .wait_timeout(state, due - now)
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .0;
                }
            };
            // An attempt STARTING is a state change too: whoever is waiting on this supervisor's
            // condition (a test asserting an owner is mid-attempt, or anything that reports what is
            // outstanding) has to be woken for it, not only for the attempt ending. Without this the
            // running window is observable only by a poll that happens to land inside it.
            self.changed.notify_all();
            self.run(next);
        }
    }

    fn run_one_due_inline(&self) {
        let taken = {
            let mut state = self.lock();
            let due_now = state
                .queued
                .iter()
                .position(|scheduled| scheduled.due <= std::time::Instant::now());
            due_now.map(|index| {
                let scheduled = state.queued.swap_remove(index);
                state.running = scheduled.owner.names.clone();
                scheduled
            })
        };
        if let Some(scheduled) = taken {
            self.changed.notify_all();
            self.run(scheduled);
        }
    }

    /// One attempt, outside the lock, then re-queue what is still owed with a backoff.
    fn run(&self, scheduled: ScheduledOwner) {
        let ScheduledOwner { owner, failures, .. } = scheduled;
        let bounds = owner.bounds;
        let outcome = owner.attempt();
        {
            let mut state = self.lock();
            state.attempts += 1;
            state.running.clear();
            if let Custody::StillOwed(owner) = outcome {
                let failures = failures.saturating_add(1);
                let multiple = 2u32.saturating_pow(failures).min(Self::BACKOFF_CAP_MULTIPLE);
                state.queued.push(ScheduledOwner {
                    owner,
                    due: std::time::Instant::now() + bounds.reschedule * multiple,
                    failures,
                });
            }
        }
        self.changed.notify_all();
    }

    /// Everything this supervisor currently owns, including the one mid-attempt.
    #[cfg(test)]
    fn outstanding(&self) -> Vec<Outstanding> {
        let state = self.lock();
        let mut all: Vec<Outstanding> = state
            .queued
            .iter()
            .map(|scheduled| Outstanding {
                holder: scheduled.owner.holder.clone(),
                names: scheduled.owner.names.clone(),
                kind: scheduled.owner.owed.label(),
            })
            .collect();
        if !state.running.is_empty() {
            all.push(Outstanding {
                holder: String::new(),
                names: state.running.clone(),
                kind: "running",
            });
        }
        all
    }

    #[cfg(test)]
    fn owns(&self, name: &str) -> bool {
        self.outstanding().iter().any(|owed| owed.names.iter().any(|owned| owned == name))
    }

    /// Test-only: whether a worker thread is believed alive.
    #[cfg(test)]
    fn has_worker(&self) -> bool {
        self.lock().worker_alive
    }

    /// Test-only: make every later thread spawn fail.
    #[cfg(test)]
    fn refuse_threads(&self) {
        self.refuse_threads.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test-only: block until the earliest queued attempt is due, or the bound expires.
    #[cfg(test)]
    fn wait_until_something_is_due(&self, bound: std::time::Duration) -> bool {
        let started = std::time::Instant::now();
        loop {
            let earliest = self.lock().queued.iter().map(|scheduled| scheduled.due).min();
            match earliest {
                None => return false,
                Some(due) if due <= std::time::Instant::now() => return true,
                Some(due) => {
                    if started.elapsed() >= bound {
                        return false;
                    }
                    std::thread::sleep((due - std::time::Instant::now()).min(bound));
                }
            }
        }
    }

    /// Test-only: block until `name` is owned (queued or mid-attempt), or the bound expires.
    #[cfg(test)]
    fn wait_until_owns(&self, name: &str, bound: std::time::Duration) -> bool {
        self.wait_for(bound, |state| {
            state.running.iter().any(|owned| owned == name)
                || state.queued.iter().any(|scheduled| scheduled.owner.names.iter().any(|owned| owned == name))
        })
    }

    /// Test-only: block until an attempt over `name` is RUNNING, or the bound expires.
    #[cfg(test)]
    fn wait_until_running(&self, name: &str, bound: std::time::Duration) -> bool {
        self.wait_for(bound, |state| state.running.iter().any(|owned| owned == name))
    }

    /// Block until nothing is owed, or the bound expires. Returns whether it is idle.
    ///
    /// Synchronised on the supervisor's own state changes, so a test waits for the fact rather than
    /// sleeping until it has probably happened.
    #[cfg(test)]
    fn wait_until_idle(&self, bound: std::time::Duration) -> bool {
        self.wait_for(bound, |state| state.queued.is_empty() && state.running.is_empty())
    }

    /// Block until at least `count` attempts have run, or the bound expires.
    #[cfg(test)]
    fn wait_until_attempts_at_least(&self, count: usize, bound: std::time::Duration) -> bool {
        self.wait_for(bound, |state| state.attempts >= count)
    }

    #[cfg(test)]
    fn wait_for(
        &self,
        bound: std::time::Duration,
        satisfied: impl Fn(&SupervisorState) -> bool,
    ) -> bool {
        let started = std::time::Instant::now();
        let mut state = self.lock();
        while !satisfied(&state) {
            let Some(left) = bound.checked_sub(started.elapsed()) else {
                return false;
            };
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        true
    }
}

impl CreationFence {
    /// A fence that hands what it cannot hold to `supervisor`.
    fn supervised_by(supervisor: &std::sync::Arc<CleanupSupervisor>, bounds: FenceBounds) -> Self {
        // The supervisor's thread is started HERE, while a holder is being established, so that the
        // one spawn this process needs happens at build time rather than at a destructor.
        supervisor.ensure_worker();
        Self {
            in_flight: std::sync::Mutex::new(0),
            settled: std::sync::Condvar::new(),
            issued: std::sync::Mutex::new(0),
            retained: std::sync::Mutex::new(Vec::new()),
            unanswered: std::sync::Mutex::new(Vec::new()),
            supervisor: std::sync::Arc::clone(supervisor),
            bounds,
        }
    }

    /// Take custody of one create that is about to be issued.
    fn begin(self: &std::sync::Arc<Self>) -> CreationTicket {
        {
            let mut in_flight =
                self.in_flight.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *in_flight += 1;
        }
        {
            let mut issued = self.issued.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *issued += 1;
        }
        CreationTicket { fence: std::sync::Arc::clone(self) }
    }

    /// Record that the client for `name`'s create ended WITHOUT the daemon's answer.
    ///
    /// Called by the closure that killed the client, while it still holds its ticket, so the record
    /// is always in place before the fence can settle.
    fn note_unanswered(&self, name: String, issued: std::time::SystemTime, client: &DockerCli) {
        eprintln!(
            "sandbox: the create client for {name} ended without the daemon's answer — its request \
             may still be applied, so absence will NOT be taken as proof for this name; it will be \
             watched once this fence settles"
        );
        self.unanswered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(UnansweredCreate { name, issued, client: client.clone() });
    }

    /// Install a job, or hand it back because there is nothing left to run it.
    ///
    /// ATOMIC WITH THE ZERO TRANSITION, and that is the entire point of the shape. The check and
    /// the installation happen under ONE `in_flight` guard, and [`CreationTicket::drop`] takes the
    /// slot while still holding that same guard. Without this, a real ordering loses the job
    /// outright: the bounded owner's wait times out, the last ticket drops and finds the slot
    /// empty, and only then does cleanup install a callback that no further drop will ever run.
    /// Now the two cases are exhaustive — either a drop is still coming and the fence keeps the
    /// job, or none is, and the caller is handed it back and must run it.
    ///
    /// EVERY job is kept. A second registration on an occupied fence is a second obligation, and it
    /// is pushed alongside the first — never assigned over it.
    fn register(&self, owner: RetainedOwner) -> Registration {
        let in_flight = self.in_flight.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *in_flight == 0 {
            return Registration::AlreadySettled(owner);
        }
        self.retained.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(owner);
        Registration::Retained
    }

    /// Take custody of a job: retained for the settlement that is coming, or RUN NOW if none is.
    ///
    /// Registering on an already-idle fence used to mean parking a closure that nothing would ever
    /// fire, which reads exactly like ownership and behaves exactly like dropping it on the floor.
    fn take_custody(&self, owner: RetainedOwner) {
        match self.register(owner) {
            Registration::Retained => {}
            Registration::AlreadySettled(owner) => self.run_and_keep_if_still_owed(vec![owner]),
        }
    }

    /// Run each owner once and PUT BACK every one that is still owed.
    ///
    /// A removal whose absence could not be confirmed has not finished, so it returns to the fence:
    /// a later settlement runs it again, and if no later settlement ever comes, this fence's own
    /// destruction does — and hands it on from there. Nothing is dropped and nothing is overwritten:
    /// the still-owed job is pushed next to whatever else the fence holds.
    fn run_and_keep_if_still_owed(&self, owners: Vec<RetainedOwner>) {
        for owner in owners {
            if let Custody::StillOwed(still_owed) = owner.attempt() {
                self.retained
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(still_owed);
            }
        }
    }

    /// Hand every unanswered create to the supervisor as a watch. Called at settlement.
    fn watch_unanswered(&self, unanswered: Vec<UnansweredCreate>) {
        for create in unanswered {
            self.supervisor.adopt(retained_watch(create, self.bounds));
        }
    }

    /// How much work has ever been started under this fence.
    #[cfg(test)]
    fn tickets_issued(&self) -> usize {
        *self.issued.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether this fence is still holding somebody's cleanup.
    ///
    /// Exists so ownership can be ASSERTED rather than read out of a log line: a test can ask the
    /// fence whether a job is still owned, which a message about ownership cannot answer.
    #[cfg(test)]
    fn holds_retained_owner(&self) -> bool {
        self.retained
            .lock()
            .map(|retained| !retained.is_empty())
            .unwrap_or(false)
    }

    /// The names of every job this fence holds, one entry per job. For assertion, not for logs.
    #[cfg(test)]
    fn retained_names(&self) -> Vec<Vec<String>> {
        self.retained
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|owner| owner.names.clone())
            .collect()
    }

    /// Block until every in-flight create has ended, or the bound expires.
    ///
    /// Returns whether it settled. A timeout is reported by the caller rather than swallowed: a
    /// create still running after this bound is a create whose container this process may never see,
    /// and saying so is the difference between a known leak and a silent one.
    fn wait_until_settled(&self, bound: std::time::Duration) -> bool {
        let started = std::time::Instant::now();
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *in_flight > 0 {
            let Some(left) = bound.checked_sub(started.elapsed()) else {
                return false;
            };
            let (guard, timeout) = self
                .settled
                .wait_timeout(in_flight, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            in_flight = guard;
            if timeout.timed_out() && *in_flight > 0 {
                return false;
            }
        }
        true
    }
}

/// One in-flight create. Releasing it is what "the create has settled" means.
///
/// Held by the blocking closure itself, never by the future awaiting it — that is the whole point.
/// A cancelled future drops its side and the closure keeps this one until it genuinely ends.
#[derive(Debug)]
struct CreationTicket {
    fence: std::sync::Arc<CreationFence>,
}

impl CreationTicket {
    /// The fence this ticket belongs to, so work spawned underneath it can take its OWN ticket.
    ///
    /// Detached IO threads use this. A reader or writer holding a pipe endpoint is work this
    /// process is still doing, and until it takes a ticket of its own it is work nobody is counted
    /// for — the closure could return, release the only ticket, and let cleanup conclude while the
    /// thread was still reading.
    fn fence(&self) -> &std::sync::Arc<CreationFence> {
        &self.fence
    }
}

impl Drop for CreationTicket {
    fn drop(&mut self) {
        // The decrement and the claim on the retained jobs are ONE critical section, taken in the
        // same order as `register`: `in_flight` first, then `retained`. Releasing the count before
        // looking at the slot is what opened the missed-handoff window -- a registration could slip
        // in after this drop had already decided there was nothing to run.
        let (owners, unanswered) = {
            let mut in_flight =
                self.fence.in_flight.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *in_flight = in_flight.saturating_sub(1);
            if *in_flight == 0 {
                let owners = std::mem::take(
                    &mut *self.fence.retained.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                );
                let unanswered = std::mem::take(
                    &mut *self
                        .fence
                        .unanswered
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()),
                );
                (owners, unanswered)
            } else {
                (Vec::new(), Vec::new())
            }
        };
        self.fence.settled.notify_all();
        // THE HANDOFF LANDS HERE, on the thread that actually ended the create.
        //
        // A create the daemon never answered goes to the supervisor as a WATCH first: for that name
        // the settlement below proves only that this process stopped asking, so the removal and
        // confirmation the retained owners are about to do cannot be its discharge.
        if !unanswered.is_empty() {
            self.fence.watch_unanswered(unanswered);
        }
        // A bounded owner that gave up earlier left its job with the fence instead of dropping it.
        // This is the event it was waiting for -- not a clock, the create's own end -- so the job
        // runs now, however long "now" took to arrive. The lock is released before it runs: the
        // job removes and confirms, and both talk to the daemon.
        if !owners.is_empty() {
            self.fence.run_and_keep_if_still_owed(owners);
        }
        // A create settling is a cleanup event: if the supervisor holds work and has no thread, this
        // is one of the moments that drives it — so its queue never waits on a future adoption.
        self.fence.supervisor.poke();
    }
}

impl Drop for CreationFence {
    /// THE LAST MOMENT THIS FENCE CAN ACT — and not the last moment this process can.
    ///
    /// The jobs still held run here, up to [`RETAINED_FINAL_RUNS`] rounds. What is STILL owed after
    /// that is not dropped and not merely named: it is TRANSFERRED to the [`CleanupSupervisor`],
    /// whose lifetime is the process's. The previous version printed LEAKED here and let the owner
    /// die, reasoning that nothing in the process outlived this point. That inference was wrong:
    /// the last `Arc` to one completed job's fence disappears while the seller goes on serving
    /// other work, and the daemon that refused three removals may well accept the fourth.
    ///
    /// The owners deliberately keep no `Arc` back to this fence (that would be a cycle, and the
    /// fence would never be destroyed at all); this is what makes that safe.
    fn drop(&mut self) {
        for _ in 0..RETAINED_FINAL_RUNS {
            let owners = std::mem::take(
                &mut *self.retained.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
            if owners.is_empty() {
                break;
            }
            for owner in owners {
                if let Custody::StillOwed(still_owed) = owner.attempt() {
                    self.retained
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(still_owed);
                }
            }
        }
        // Out of attempts HERE. Not out of owners.
        let still_owed = std::mem::take(
            &mut *self.retained.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for owner in still_owed {
            eprintln!(
                "sandbox: {} could not be confirmed absent in {} attempts by a fence that is being \
                 destroyed — custody is TRANSFERRED, not released: the cleanup supervisor owns these \
                 names from here and keeps retrying while this process lives",
                owner.names.join(", "),
                RETAINED_FINAL_RUNS
            );
            self.supervisor.adopt(owner);
        }
        // A ticket holds an `Arc` to its fence, so a fence cannot be destroyed with a create still
        // in flight and its unanswered list is normally drained at settlement. Exhaustive anyway:
        // whatever is recorded here goes to the supervisor rather than out of existence.
        let unanswered = std::mem::take(
            &mut *self.unanswered.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if !unanswered.is_empty() {
            self.watch_unanswered(unanswered);
        }
        // A fence being destroyed is a cleanup event too. See `CleanupSupervisor::poke`.
        self.supervisor.poke();
    }
}

#[derive(Debug)]
pub struct NetnsHolder {
    name: String,
    sidecars: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    creation: std::sync::Arc<CreationFence>,
    client: DockerCli,
    bounds: FenceBounds,
}

impl NetnsHolder {
    /// Adopt a container name as the holder, whether or not the container exists yet.
    ///
    /// Private on purpose. Adoption happens **before** the create command is issued: the create is
    /// an await, an await is a cancellation point, and a cancelled create can still complete inside
    /// the blocking pool after the future is gone. Adopting afterwards left exactly that container
    /// with no guard — running, joined to nothing, and invisible to this process.
    ///
    /// Adoption gives cleanup a name. [`CreationFence`] gives it a TIME. Both are required.
    #[cfg(test)]
    fn adopt(name: String, client: DockerCli) -> Self {
        Self::adopt_bounded(name, client, FenceBounds::production())
    }

    /// As [`Self::adopt`], with the cleanup bounds named by the caller so the delayed path can be
    /// exercised without waiting out the production ones.
    fn adopt_bounded(name: String, client: DockerCli, bounds: FenceBounds) -> Self {
        Self {
            name,
            sidecars: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            creation: std::sync::Arc::new(CreationFence::default()),
            client,
            bounds,
        }
    }

    /// Test-only: the SAME holder as [`Self::adopt_bounded`] — same fields, same `Drop` — whose fence
    /// reports to a supervisor the test owns, so what the production destructor hands over can be
    /// asserted on rather than read out of the process-wide supervisor's log.
    #[cfg(test)]
    fn adopt_supervised(
        name: String,
        client: DockerCli,
        bounds: FenceBounds,
        supervisor: &std::sync::Arc<CleanupSupervisor>,
    ) -> Self {
        Self {
            name,
            sidecars: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            creation: std::sync::Arc::new(CreationFence::supervised_by(supervisor, bounds)),
            client,
            bounds,
        }
    }

    /// The docker client this holder was built with. Cleanup uses it too, so a stand-in cannot be
    /// half-applied: whatever created the container is what removes and confirms it.
    fn client(&self) -> &DockerCli {
        &self.client
    }

    /// Take a ticket for a create about to be issued against this holder.
    fn fence_creation(&self) -> CreationTicket {
        self.creation.begin()
    }

    /// Register a sidecar container name for the duration of one command.
    fn watch_sidecar(&self, name: String) -> SidecarGuard {
        if let Ok(mut names) = self.sidecars.lock() {
            names.push(name.clone());
        }
        SidecarGuard { name, registry: std::sync::Arc::clone(&self.sidecars), completed: false }
    }

    /// Whether a failed `docker rm` says "there was nothing here" rather than "I could not do it".
    ///
    /// The only benign failure. Because the holder is adopted **before** its create is issued, a run
    /// cancelled in that window tears down a container that never existed, and docker rightly
    /// objects. Every other message is a container this process could not remove — a leak, which the
    /// caller reports as a leak. An empty stderr is not benign: a removal that failed without saying
    /// why is the one case where assuming success would be a silent orphan.
    fn force_remove_stderr_is_benign(stderr: &str) -> bool {
        stderr.contains("No such container")
    }

    /// How long one `docker rm` may run before it is abandoned and reported as a leak.
    ///
    /// This runs inside `Drop`, on the thread that is tearing the job down, so it is a hard cap on
    /// how long a wedged daemon can hold that thread. Long enough that an ordinary removal under
    /// load is never cut short -- removals finish in well under a second -- and short enough that a
    /// daemon which has stopped answering ends the job instead of pinning the caller forever.
    const REMOVE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

    /// How long cleanup waits for an in-flight create to settle before removing regardless.
    ///
    /// Bounded by the same reasoning as [`Self::REMOVE_DEADLINE`], and deliberately longer than it:
    /// a create that has reached the daemon finishes in well under a second, while the thing this
    /// guards against — removing BEFORE the container exists — is unrecoverable once it happens.
    /// The create's own [`DOCKER_DEADLINE`] kills the client at 120s, so this can never wait for a
    /// hung client indefinitely; it waits for the blocking closure to end, which that kill forces.
    const CREATE_SETTLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

    /// Wait for one child, bounded. On the deadline the child is killed and reaped, and the wait is
    /// reported as a failure rather than as a removal that succeeded.
    ///
    /// `Child::wait`, and the `output()` this replaced, have no timeout at all: a docker client
    /// talking to a daemon that never answers blocks forever, which in `Drop` means teardown never
    /// returns. The word "bounded" was in the comment above this function long before anything in
    /// it bounded anything.
    fn wait_bounded(
        child: &mut std::process::Child,
        deadline: std::time::Duration,
    ) -> Result<std::process::ExitStatus, String> {
        let expires = std::time::Instant::now() + deadline;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {
                    if std::time::Instant::now() >= expires {
                        // Killed AND reaped: leaving a zombie behind would be its own small leak,
                        // and the kill is what makes the bound real rather than advisory.
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "docker rm did not finish within {}s and was abandoned -- the container \
                             may still exist",
                            deadline.as_secs()
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => return Err(format!("could not wait for docker rm: {error}")),
            }
        }
    }

    /// Force-remove one container by name, bounded, and say what actually happened.
    ///
    /// `Ok(())` means docker reported the removal, or reported that there was nothing to remove.
    fn force_remove(client: &DockerCli, name: &str) -> Result<(), String> {
        let mut child = std::process::Command::new(client.program())
            .args(["rm", "--force", "--volumes", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run docker rm: {error}"))?;
        let status = Self::wait_bounded(&mut child, Self::REMOVE_DEADLINE)?;
        // Read after the wait returns. `docker rm` writes one short line at most, so this cannot
        // deadlock on a full pipe the way a chatty child could.
        let mut stderr_bytes = Vec::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read as _;
            let _ = pipe.read_to_end(&mut stderr_bytes);
        }
        if status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_owned();
        // Removing something that was never created is the expected path when a create was
        // cancelled before it started, and it is not a cleanup failure.
        if Self::force_remove_stderr_is_benign(&stderr) {
            Ok(())
        } else {
            Err(if stderr.is_empty() {
                "docker rm failed and said nothing".to_owned()
            } else {
                stderr
            })
        }
    }

    /// The container name, for `docker` commands that address it directly.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What to pass to `docker run --network` so a container joins this namespace.
    /// The `--network` value that joins a container to the namespace `name` owns.
    ///
    /// An associated function as well as a method because the readback needs it for a holder it must
    /// not own: taking a `&NetnsHolder` there would mean handing out a guard whose `Drop` destroys a
    /// namespace the caller is only reading.
    pub fn network_mode_for(name: &str) -> String {
        format!("container:{name}")
    }

    pub fn network_mode(&self) -> String {
        Self::network_mode_for(&self.name)
    }
}

/// One sidecar's registration, dropped when its command finishes however it finishes.
///
/// On a normal return the container is already gone (`--rm`) and this only deregisters. On
/// cancellation the future is dropped mid-command, the name stays with the holder, and the holder's
/// own `Drop` force-removes it — which is the case that used to leave a joiner pinning a namespace
/// whose holder had just been removed.
#[derive(Debug)]
struct SidecarGuard {
    name: String,
    registry: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Set only when the command returned. A guard dropped without this is a cancelled command.
    completed: bool,
}

impl SidecarGuard {
    /// The command's client was **reaped with an exit status**. `docker run --rm` removes the
    /// container when its client exits -- including on a nonzero exit -- so from here the name is no
    /// longer a cleanup target, and keeping it would make the holder report a leak for something
    /// already gone.
    ///
    /// Deliberately NOT called for a result that merely returned: a deadline kill, a signal, or a
    /// failure before the wait leaves a container this process never saw finish.
    fn completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for SidecarGuard {
    /// Deregisters ONLY a command that finished.
    ///
    /// This used to deregister unconditionally, which quietly inverted the custody it was written
    /// for. Cancellation drops the future mid-command, which drops this guard, which struck the
    /// name from the registry -- and the holder's own `Drop`, reading that registry moments later,
    /// then saw nothing to remove. The container the blocking docker client had already created
    /// stayed joined to the namespace with no guard, no record and no remover: exactly the orphan
    /// the registry exists to prevent, produced by the cleanup path itself.
    ///
    /// So a cancelled command leaves its name behind deliberately. The cost of keeping a name whose
    /// container never got created is one `docker rm` answering "No such container", which
    /// [`NetnsHolder::force_remove_stderr_is_benign`] already treats as success. The cost of
    /// dropping a name whose container does exist is a pinned namespace nothing will ever clean up.
    fn drop(&mut self) {
        if !self.completed {
            return;
        }
        if let Ok(mut names) = self.registry.lock() {
            names.retain(|name| name != &self.name);
        }
    }
}

impl Drop for NetnsHolder {
    /// Destroy the holder, **synchronously**, and everything joined to it first.
    ///
    /// Deliberately a blocking `std::process::Command` and not a spawned task: a task spawned from
    /// `Drop` can be discarded when the runtime shuts down, and runtime shutdown is exactly the path a
    /// panicking or aborted job takes. A leaked holder is a container pinned to a namespace nothing
    /// will ever clean up, so the ~100 ms block is the cheaper end of that trade.
    ///
    /// **Sidecars go first.** A joiner still running when the holder is removed keeps the namespace
    /// alive, and is precisely what a cancelled applier or readback leaves behind. Only names this
    /// run registered are removed; nothing is matched by pattern, so a sibling job's containers are
    /// never in scope.
    ///
    /// Failure is reported, never propagated and never implied away: `Drop` cannot return, so each
    /// failure is printed as a failure — "could not remove", not "destroyed" — and
    /// [`reap_orphans`] is the backstop. A cleanup that failed is a leak that is now on the record.
    fn drop(&mut self) {
        // FIRST, before a single remove is issued: let any in-flight create finish.
        //
        // Removing ahead of the create is worse than not removing at all, because "No such
        // container" reads as success and closes the case on a container that is about to exist.
        // Waiting here costs nothing in the ordinary path (nothing is in flight, the count is
        // already zero) and is the only thing that makes the removes below meaningful in the
        // cancelled path.
        let cleanup = HolderCleanup {
            name: self.name.clone(),
            joiners: self.sidecars.lock().map(|names| names.clone()).unwrap_or_default(),
            creation: std::sync::Arc::clone(&self.creation),
            client: self.client.clone(),
            bounds: self.bounds,
        };
        if self.creation.wait_until_settled(self.bounds.fast) {
            // Ordinary path: nothing was in flight, or it finished while we waited. `docker rm`
            // returning success here IS the daemon's answer, so no second question is asked.
            //
            // A removal the daemon REFUSED is not answered by a log line. This path used to sweep,
            // print "LEAKED" for whatever refused, and return — the one path every completed job
            // takes, and the one path that bypassed the supervisor entirely: with nothing in flight
            // there was no retained owner and no unanswered create, so the fence that followed this
            // holder to destruction adopted nothing. A refused holder or joiner was lost while the
            // process went on living. What refuses here now goes INTO live ownership: the supervisor
            // removes and confirms it on a schedule, for as long as the process runs. Non-blocking,
            // so `Drop` stays the ~100 ms it always was.
            let refused = cleanup.sweep();
            if !refused.is_empty() {
                eprintln!(
                    "sandbox: {} refused removal in netns holder {}'s ordinary teardown — NOT \
                     released: the cleanup supervisor owns these names from here and keeps retrying \
                     while this process lives",
                    refused.join(", "),
                    self.name
                );
                self.creation.supervisor.adopt(retained_removal(
                    self.name.clone(),
                    refused,
                    self.client.clone(),
                    self.bounds,
                ));
            }
            // A holder being destroyed is a cleanup event. See `CleanupSupervisor::poke`.
            self.creation.supervisor.poke();
            return;
        }
        // Delayed path. The create is STILL running, and this is the case the previous version got
        // wrong: it removed anyway, printed LEAKED, and returned — leaving nobody responsible for
        // the container that was still on its way. "No such container" then read as success for an
        // object about to exist.
        //
        // Removing now cannot be made safe by waiting longer, so cleanup is not removed — it is
        // HANDED OVER. The owner below outlives this `Drop` and finishes the job on the create's own
        // schedule: it waits for the ticket to actually settle, then removes, then keeps asking the
        // daemon until absence is CONFIRMED. The wait is bounded by the create client's own
        // `DOCKER_DEADLINE` kill plus a margin, so it waits for an event guaranteed to occur rather
        // than for a duration someone guessed.
        let name = self.name.clone();
        let spawned = std::thread::Builder::new()
            .name("mx-holder-cleanup".to_owned())
            .spawn(move || cleanup.own_until_settled_or_confirmed());
        match spawned {
            Ok(_owner) => eprintln!(
                "sandbox: a create against netns holder {name} is still in flight after {:?} — \
                 cleanup is NOT removing ahead of it; an owner has been retained and will remove \
                 and confirm once the create settles",
                self.bounds.fast
            ),
            // No thread to hand it to: finish the job here rather than remove early. Blocking is
            // the lesser harm; removing ahead of a live create is the one outcome with no recovery.
            Err(error) => {
                eprintln!(
                    "sandbox: could not retain a cleanup owner for netns holder {name} ({error}) — \
                     completing the wait inline instead"
                );
                let inline = HolderCleanup {
                    name: self.name.clone(),
                    joiners: self.sidecars.lock().map(|names| names.clone()).unwrap_or_default(),
                    creation: std::sync::Arc::clone(&self.creation),
                    client: self.client.clone(),
                    bounds: self.bounds,
                };
                inline.own_until_settled_or_confirmed();
            }
        }
    }
}

/// The cleanup that owns a holder's name once the holder itself is gone.
///
/// Split out of `Drop` for one reason: `Drop` must not be the last thing that cares about the
/// container. When a create is still in flight, this outlives the holder and stays responsible until
/// the create settles or the daemon confirms the name is gone.
#[cfg(feature = "acp")]
struct HolderCleanup {
    name: String,
    joiners: Vec<String>,
    creation: std::sync::Arc<CreationFence>,
    client: DockerCli,
    bounds: FenceBounds,
}

#[cfg(feature = "acp")]
impl HolderCleanup {
    /// Remove the joiners, then the holder. Sidecars first: a joiner still running pins the
    /// namespace the holder is being torn down to release.
    ///
    /// Returns every name whose removal the daemon REFUSED (or did not answer), in removal order, so
    /// the caller can keep owning them. Logging a refusal was never the same as owning it.
    fn sweep(&self) -> Vec<String> {
        let mut refused = Vec::new();
        for joiner in &self.joiners {
            if let Err(error) = NetnsHolder::force_remove(&self.client, joiner) {
                eprintln!(
                    "sandbox: could not remove sidecar {joiner} joined to netns holder {}: {error} \
                     — the namespace may still be pinned by it",
                    self.name
                );
                refused.push(joiner.clone());
            }
        }
        if let Err(error) = NetnsHolder::force_remove(&self.client, &self.name) {
            eprintln!(
                "sandbox: could not remove netns holder {}: {error} — not destroyed; it stays owed \
                 to whoever called this sweep",
                self.name
            );
            refused.push(self.name.clone());
        }
        refused
    }

    /// Ask the daemon, repeatedly, whether EVERY container this owner is responsible for is gone —
    /// each joiner as well as the holder.
    ///
    /// A removal issued is not a removal observed. `Some(true)` is the only answer that retires a
    /// name; "could not tell" is treated exactly like "still there", because the cost of asking
    /// again is a bounded retry and the cost of believing it is an orphan nobody is looking for.
    ///
    /// Confirming the holder ALONE was not enough, and that was a real hole: [`Self::sweep`] only
    /// LOGS a failed joiner removal, so a sidecar that refused to go on still pins the namespace
    /// the holder was torn down to release. An owner ending on holder-absence announced a clean
    /// release directly over the top of a container it owns and never asked about.
    ///
    /// Returns the names that could not be confirmed gone, so the caller can name them.
    fn confirm_all_absent(&self) -> Result<(), Vec<String>> {
        let give_up = std::time::Instant::now() + self.bounds.confirm;
        let mut pause = std::time::Duration::from_millis(20);
        // Joiners first: the holder's namespace is not actually released while one of them pins it.
        let mut pending: Vec<String> =
            self.joiners.iter().cloned().chain(std::iter::once(self.name.clone())).collect();
        loop {
            pending.retain(|name| container_is_absent(&self.client, name) != Some(true));
            if pending.is_empty() {
                return Ok(());
            }
            if std::time::Instant::now() >= give_up {
                return Err(pending);
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(std::time::Duration::from_millis(500));
        }
    }

    /// Wait for the create to genuinely settle, then remove, then confirm.
    ///
    /// Ends on ACTUAL settlement followed by CONFIRMED absence. If the create never settles within
    /// the bound, the sweep still runs and the confirmation still decides the verdict: a container
    /// that never landed is confirmed absent and the case closes honestly; one that cannot be
    /// confirmed gone is reported as leaked, with the reason, rather than silently written off.
    fn own_until_settled_or_confirmed(self) {
        let mut settled = self.creation.wait_until_settled(self.bounds.max);
        // Best effort either way: whatever HAS landed should go now.
        self.sweep();
        if !settled {
            // `max` expired with the create STILL RUNNING. This is where ownership used to end: it
            // swept, printed a leak and returned, which handed the container that was still on its
            // way to nobody. The sweep above cannot cover it — you cannot remove what has not
            // appeared — so the only thing that keeps it owned is staying.
            //
            // So the job is KEPT for `retain` longer. If the create lands in that window it is
            // swept again, by an owner that is still responsible for it, and then confirmed gone.
            eprintln!(
                "sandbox: a create against netns holder {} is STILL IN FLIGHT after {:?} — this \
                 owner is NOT releasing it: custody is retained for a further {:?}, and anything \
                 that lands in that window will be removed and confirmed by this owner",
                self.name, self.bounds.max, self.bounds.retain
            );
            settled = self.creation.wait_until_settled(self.bounds.retain);
            if settled {
                // It landed late, and it is still this owner's to remove.
                self.sweep();
            }
        }
        if !settled {
            // The wait is over and the create is STILL in flight. Ownership does NOT end here.
            //
            // Waiting longer was never the answer. Whatever the bound, the case that breaks is a
            // container landing one millisecond past it, so a bigger window makes the orphan rarer
            // without making it impossible -- it moves the edge, it does not remove it. The job is
            // TRANSFERRED instead, to an owner the create's own fence retains. The fence is the one
            // object that knows when this create genuinely ends, because the create's ticket is
            // what releases it, so the handoff is to the settlement event itself rather than to
            // another clock.
            //
            // This claims nothing about whether the daemon will finish. It is the narrower true
            // thing: if the container ever lands, somebody still owns it.
            self.creation.take_custody(retained_removal(
                self.name.clone(),
                self.joiners.clone(),
                self.client.clone(),
                self.bounds,
            ));
            eprintln!(
                "sandbox: a create against netns holder {} was STILL IN FLIGHT after {:?} and did \
                 not land within the further {:?} this owner retained it — custody is NOT being \
                 released: it has been TRANSFERRED to an owner retained by the create's own fence, \
                 which removes and confirms this holder and its {} joiner(s) when that create \
                 settles, whenever that is",
                self.name,
                self.bounds.max,
                self.bounds.retain,
                self.joiners.len()
            );
            return;
        }
        if let Err(pending) = self.confirm_all_absent() {
            // A removal was ISSUED and the daemon never confirmed absence. Responsibility used to
            // end on the log line below -- the names were named, and then let go, which is
            // indistinguishable downstream from a clean release. An unconfirmed name is kept
            // instead: the fence retains an owner holding exactly those names, so they remain OWNED
            // rather than merely mentioned, and a later create settling on this holder runs them
            // again.
            // THIS FENCE IS ALREADY IDLE. The create settled -- that is why confirmation ran at
            // all -- so there is no future ticket drop here to fire a parked callback. Handing the
            // job over therefore has to mean RUN IT, which `take_custody` does, and if it still
            // cannot confirm absence it goes back into the slot where this fence's destruction
            // will run it again rather than discard it.
            self.creation.take_custody(retained_removal(
                self.name.clone(),
                pending.clone(),
                self.client.clone(),
                self.bounds,
            ));
            eprintln!(
                "sandbox: could not confirm {} absent within {:?} after the create settled — these \
                 are NOT released: an owner for them was run again on this holder's fence and is \
                 kept there until it confirms, and the boot reaper remains the backstop",
                pending.join(", "),
                self.bounds.confirm
            );
        }
    }
}

/// Containment established for one job: the namespace, and the address the job must use to reach its
/// credential proxy. Both come from the same measurement, so the firewall pinhole and the base URL
/// cannot disagree.
#[derive(Debug)]
pub struct Containment {
    pub holder: NetnsHolder,
    pub proxy_host: String,
    /// The link inside the namespace the egress filters were installed on, as measured — never a
    /// guess like `eth0`. Carried so a caller, a log line or a test can name the interface that is
    /// actually filtered rather than the one everybody assumes.
    pub egress_dev: String,
}

/// The holder's container name for `job_id`.
///
/// Derived from the job id rather than random, so a stale holder can be attributed to the job that
/// leaked it, and a second attempt for the same job collides loudly instead of quietly leaking the
/// first one.
pub fn holder_name(job_id: &str) -> String {
    format!("maxplayer-netns-{job_id}")
}

/// `docker run` argv for the holder.
///
/// It runs `sleep infinity` in exec form — no shell — and that emptiness is the point: `docker run -d`
/// returns only *after* the entrypoint has begun executing, so whatever the holder runs is the one
/// thing that runs in the namespace before the rules land. `sleep` is the smallest possible answer.
///
/// `--read-only`, `--cap-drop ALL`, non-root and `no-new-privileges` because a container that exists
/// to hold a namespace needs nothing else, and it shares that namespace with a stranger's job.
///
/// `seat` is the owning seller's public key hex and goes on as a second label. It is what lets the
/// boot reaper tell this seat's holders from another daemon's on a shared host; see
/// [`HOLDER_SEAT_LABEL`].
///
/// `cleanup_after` is the absolute unix second from [`cleanup_after_unix`] — this job's own
/// effective deadline plus the grace. It goes on as a third label so the periodic sweep can judge
/// this container **without knowing anything about the job**, including after the process that
/// created it is gone. It is derived by the seller from the deadline it is itself enforcing; no part
/// of it comes from the buyer's payload, which is why a request cannot ask for a container that
/// never expires.
pub fn holder_argv(
    name: &str,
    network: &str,
    image: &str,
    uid: u32,
    gid: u32,
    job_id: &str,
    seat: &str,
    cleanup_after: u64,
) -> Vec<String> {
    [
        "docker",
        "run",
        "--detach",
        "--name",
        name,
        "--network",
        network,
        "--label",
        &format!("{HOLDER_LABEL}={job_id}"),
        "--label",
        &format!("{HOLDER_SEAT_LABEL}={seat}"),
        "--label",
        &format!("{HOLDER_CLEANUP_AFTER_LABEL}={cleanup_after}"),
        "--label",
        &format!("{HOLDER_ROLE_LABEL}={ROLE_HOLDER}"),
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        &format!("{uid}:{gid}"),
        "--entrypoint",
        "sleep",
        image,
        "infinity",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker run` argv for the sidecar that applies the plan.
///
/// `NET_ADMIN` is the whole reason this is a separate container: it is the one capability the design
/// hands out, it is scoped to a throwaway namespace, and it is gone before the job starts. The sidecar
/// runs as root *inside its own container* because capabilities attach to root without file
/// capabilities — acceptable only because the image is our own 4 MB one, holds no policy of its own,
/// and exits immediately.
///
/// `--rm` is safe here specifically because the caller captures stdout and stderr before the container
/// is removed; the evidence is in hand before the container is gone.
pub fn sidecar_argv(holder: &NetnsHolder, image: &str) -> Vec<String> {
    [
        "docker",
        "run",
        "--rm",
        "--interactive",
        "--network",
        &holder.network_mode(),
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_ADMIN",
        "--security-opt",
        "no-new-privileges",
        image,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker run` argv that reads the installed rules back out of the holder's namespace, for one
/// address family.
///
/// A **separate container** from the one that installed them, running a **different verb** (`-S`, not
/// `-A`), because the question is what the kernel holds and not whether the installer believes it
/// succeeded. `--entrypoint` replaces the applier, so this container is handed no plan and cannot
/// modify anything even though it must carry `NET_ADMIN` to list rules at all.
///
/// The output is parsed and judged in Rust by [`crate::sandbox_net::NetPolicy::verify_readback`]. The
/// sidecar image is reused rather than adding a third image: it already carries both binaries, and a
/// separate image would grow the supply-chain surface to run one read-only command.
pub fn readback_argv(holder_name: &str, image: &str, family: Family) -> Vec<String> {
    [
        "docker",
        "run",
        "--rm",
        "--network",
        &NetnsHolder::network_mode_for(holder_name),
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_ADMIN",
        "--security-opt",
        "no-new-privileges",
        "--entrypoint",
        family.binary(),
        image,
        "-S",
        crate::sandbox_net::OUTPUT_CHAIN,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// The plan as the sidecar reads it: one `<binary> <args…>` line per rule, plus the count, so the
/// caller can cross-check the sidecar's echoed total against what was actually rendered.
///
/// A mismatch between those two numbers is the only way to detect a truncated stdin — no exit code
/// reveals it, because a short plan applies perfectly.
pub fn plan_stdin(policy: &NetPolicy) -> (String, usize) {
    let plan = policy.install_plan();
    let mut out = String::new();
    for (binary, args) in &plan {
        out.push_str(binary);
        for arg in args {
            out.push(' ');
            out.push_str(arg);
        }
        out.push('\n');
    }
    (out, plan.len())
}

/// `docker run` argv that asks **docker** what `host-gateway` means on this platform, by resolving
/// `alias` inside a throwaway container that is allowed to carry `--add-host`.
///
/// Deliberately a measurement and not a computation, and this is the trap it exists to avoid:
/// `docker network inspect <net>` reports the **joined network's** gateway, while `host-gateway`
/// resolves to a daemon-wide address — measured on one box in one run as `172.21.0.1` and
/// `172.17.0.1` respectively. Computing the pinhole from the former puts the ACCEPT on an address
/// nothing listens on, the range denies eat the real one, and every job silently loses its model
/// while every rendering test stays green (they assert order and shape, never the address).
///
/// `alias` is a parameter rather than a reference to `credential_proxy::PROXY_HOST_ALIAS` so that
/// this module compiles on default features: the proxy lives behind `wallet`, and the argv deciding
/// what a stranger's job can reach must be built and tested on every build.
pub fn host_gateway_probe_argv(image: &str, alias: &str) -> Vec<String> {
    [
        "docker",
        "run",
        "--rm",
        "--add-host",
        &format!("{alias}:host-gateway"),
        "--entrypoint",
        "getent",
        image,
        "ahostsv4",
        alias,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// The first IPv4 address in `getent ahostsv4` output (`<ip>\t<STREAM|DGRAM> <name>` lines).
pub fn parse_getent_ipv4(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .find(|field| {
            let mut octets = field.split('.');
            let parsed = (&mut octets).take(4).filter(|o| o.parse::<u8>().is_ok()).count();
            parsed == 4 && octets.next().is_none()
        })
        .map(str::to_owned)
}

/// `docker` argv listing `seat`'s holder containers by **full** id and owning seat.
///
/// Full ids rather than docker's truncated default, because a joined job's `NetworkMode` names its
/// holder by full id and orphan detection compares the two directly.
///
/// **Two barriers on purpose, and only one of them is load-bearing.** The `label=<seat>` filter asks
/// docker to hand back this seat's holders alone, so a foreign id is never even a candidate for
/// removal. But the decision is not left there: the seat label is also *printed*, and
/// [`reapable_holders`] re-checks it in Rust with an exact string comparison. That comparison is the
/// guard. The filter is narrowing — worth having because it shrinks what a later bug could reach,
/// and safe to have because its only failure that matters is matching too little, which leaks a
/// holder instead of destroying someone's job.
pub fn list_holders_argv(seat: &str) -> Vec<String> {
    [
        "docker",
        "ps",
        "--all",
        "--no-trunc",
        "--filter",
        &format!("label={HOLDER_LABEL}"),
        "--filter",
        &format!("label={HOLDER_SEAT_LABEL}={seat}"),
        "--format",
        &format!("{{{{.ID}}}}\t{{{{.Label \"{HOLDER_SEAT_LABEL}\"}}}}"),
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// One holder as the reaper sees it: its full container id, and the seat that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderRecord {
    pub id: String,
    /// The owning seat from [`HOLDER_SEAT_LABEL`]. `None` for a holder created by a build older than
    /// that label — unattributable, and so never a removal candidate.
    pub seat: Option<String>,
}

/// Parse `docker ps --format '{{.ID}}\t{{.Label …}}'` output into one record per holder.
///
/// An absent label arrives as an **empty field**, not a missing one, so emptiness is what maps to
/// `None`. Reading it as a seat named "" would make every legacy holder look like it belonged to a
/// seat whose id is the empty string, and one caller passing an empty seat would then reap the lot.
pub fn parse_holder_listing(stdout: &str) -> Vec<HolderRecord> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next().unwrap_or_default().trim().to_owned();
            let seat = fields.next().map(str::trim).filter(|seat| !seat.is_empty()).map(str::to_owned);
            HolderRecord { id, seat }
        })
        .filter(|holder| !holder.id.is_empty())
        .collect()
}

/// `docker` argv listing the containers **this seat owns**, with the metadata the expiry sweep
/// judges them by: full id, owning seat, cleanup-after stamp, and role.
///
/// The `label=<seat>` filter is narrowing, exactly as in [`list_holders_argv`], and exactly as
/// there it is **not** the guard: [`expired_owned`] re-checks the seat in Rust with an exact string
/// comparison, because a filter that silently matched too much would be indistinguishable from one
/// that worked. The filter's only failure that matters is matching too little, which leaks a
/// container instead of removing a stranger's.
///
/// `--all` because an expired container is usually not running: a holder whose job died is
/// `Exited`, and a helper that finished is `Exited` too. Listing only running containers would miss
/// precisely the leftovers this sweep exists to remove.
pub fn list_owned_argv(seat: &str) -> Vec<String> {
    [
        "docker",
        "ps",
        "--all",
        "--no-trunc",
        "--filter",
        &format!("label={HOLDER_SEAT_LABEL}={seat}"),
        "--format",
        &format!(
            "{{{{.ID}}}}\t{{{{.Label \"{HOLDER_SEAT_LABEL}\"}}}}\t{{{{.Label \"{HOLDER_CLEANUP_AFTER_LABEL}\"}}}}\t{{{{.Label \"{HOLDER_ROLE_LABEL}\"}}}}"
        ),
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// One container as the expiry sweep sees it.
///
/// Every field after `id` is an `Option` because every one of them can be absent on a real host: a
/// container from a build older than these labels, a container whose labels were not applied
/// because the create died between docker accepting the argv and recording it, or simply a
/// container belonging to something else that happens to carry the seat label. Absence is never
/// read as a permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedContainer {
    /// Full container id, as the sweep will name it to `docker rm`.
    pub id: String,
    /// Owning seat from [`HOLDER_SEAT_LABEL`]; `None` when the label is absent or empty.
    pub seat: Option<String>,
    /// Parsed [`HOLDER_CLEANUP_AFTER_LABEL`]; `None` when absent, empty, or not a unix second.
    pub cleanup_after: Option<u64>,
    /// Parsed [`HOLDER_ROLE_LABEL`]; reported, never a removal criterion on its own.
    pub role: Option<String>,
}

/// Parse `docker ps --format '{{.ID}}\t{{.Label …}}…'` output into one record per container.
///
/// **A malformed stamp parses to `None`, not to zero.** An absent label arrives from docker as an
/// empty field, and a corrupted one as arbitrary text; reading either as the number 0 would date the
/// container to 1970 and make it instantly sweepable. Every unreadable stamp therefore becomes
/// `None`, which [`expired_owned`] refuses to act on. The failure mode is a leak the operator can
/// see, never a removal nobody authorised.
pub fn parse_owned_listing(stdout: &str) -> Vec<OwnedContainer> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next().unwrap_or_default().trim().to_owned();
            let field = |fields: &mut std::str::Split<'_, char>| {
                fields.next().map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
            };
            let seat = field(&mut fields);
            let cleanup_after = field(&mut fields).and_then(|value| value.parse::<u64>().ok());
            let role = field(&mut fields);
            OwnedContainer { id, seat, cleanup_after, role }
        })
        .filter(|container| !container.id.is_empty())
        .collect()
}

/// The containers `seat` may remove right now: **owned by `seat`** and carrying a **readable**
/// cleanup stamp that `now_unix` has passed.
///
/// **Both legs are required and neither is sufficient**, for the same reason the boot reaper needs
/// two. Ownership alone would remove a container whose job is still inside its deadline. Expiry
/// alone would remove a co-tenant seat's container on a shared docker socket — the exact accident
/// [`HOLDER_SEAT_LABEL`] was added to prevent.
///
/// **Attachment is deliberately NOT consulted here**, and that is the one place this predicate
/// differs from [`reapable_holders`]. The boot reaper must not touch an attached holder, because a
/// live job is joined to it and `unattached` is its only evidence the job is gone. This sweep has
/// better evidence: the container's own stamp says its job's deadline passed more than
/// [`CLEANUP_GRACE_SECS`] ago. A container still attached at that point is attached to something
/// that outlived its own deadline by an hour, which is the leak — refusing to remove it would leave
/// precisely the case this exists for. The grace is sized so that ordinary post-deadline work has
/// long finished.
///
/// An empty `seat` selects nothing: a caller that cannot name itself owns nothing to remove.
#[must_use]
pub fn expired_owned(containers: &[OwnedContainer], seat: &str, now_unix: u64) -> Vec<String> {
    if seat.trim().is_empty() {
        return Vec::new();
    }
    containers
        .iter()
        .filter(|container| container.seat.as_deref() == Some(seat))
        .filter(|container| container.cleanup_after.is_some_and(|after| now_unix >= after))
        .map(|container| container.id.clone())
        .collect()
}

/// `docker` argv listing every container on the host by full id.
pub fn list_all_containers_argv() -> Vec<String> {
    ["docker", "ps", "--all", "--no-trunc", "--quiet"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// `docker` argv printing one `<network-mode>` line per container in `ids`, in order.
pub fn network_modes_argv(ids: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = ["docker", "inspect", "--format", "{{.HostConfig.NetworkMode}}"]
        .into_iter()
        .map(String::from)
        .collect();
    argv.extend(ids.iter().cloned());
    argv
}

/// The holders `seat` may remove: **owned by `seat`** and with no container joined to them.
///
/// **Both legs are required, and neither is sufficient.** Ownership alone would remove a holder this
/// seat is mid-way through attaching a job to. Attachment alone is the bug this function exists to
/// forbid: a holder is unattached twice in every job's life, so "nothing attached" says nothing
/// whatever about whether the holder is in use, let alone whose it is. A removal needs an explicit
/// ownership match *and* an idle namespace.
///
/// **Why attachment is a comparison and not a docker filter.** A live job joins its holder with
/// `--network container:<id>`, which docker records as a `NetworkMode` of `container:<holder-full-id>`.
/// Docker cannot select on that: measured on this host, `docker ps --filter network=<holder>` matches
/// **nothing** for such a container, and `docker ps --format '{{.Networks}}'` prints an **empty** field
/// for it. So the only way to see the join is to read the modes and compare.
///
/// **What the ownership leg closed.** This function used to take every labelled holder on the host and
/// keep the unattached ones, which made a boot on a shared host able to strip the namespace out from
/// under another daemon's job — either one already running, or one in its pre-attach window. The
/// comment here recorded that race and judged a per-daemon label not worth the complexity "until a
/// host actually runs two seller daemons". That condition is now met: VM1854 runs two earning seats
/// and Server One runs three. [`HOLDER_SEAT_LABEL`] is that label.
///
/// **What remains, stated rather than papered over.** A seat cannot clean up after a *different*
/// seat, and a holder from a build predating the seat label has no owner to match, so both leak until
/// something removes them by hand. A leaked holder costs a container and holds no policy; the job
/// that could have used it is already gone. That is the trade this module takes every time.
pub fn reapable_holders(holders: &[HolderRecord], seat: &str, modes: &str) -> Vec<String> {
    let attached: Vec<&str> = modes
        .lines()
        .map(str::trim)
        .filter_map(|mode| mode.strip_prefix("container:"))
        .collect();
    holders
        .iter()
        .filter(|holder| holder.seat.as_deref() == Some(seat))
        .filter(|holder| !attached.iter().any(|target| target == &holder.id.as_str()))
        .map(|holder| holder.id.clone())
        .collect()
}

/// Ask docker which of `seat`'s holders are reapable right now: the three reads, then
/// [`reapable_holders`] on what came back. Selects; removes nothing.
///
/// Split out of [`reap_orphans`] so that a caller which wants to SHOW an operator what a reap would
/// touch — `maxplayer sandbox-reap --seat <hex> --dry-run` (#905) — shares this selection instead of
/// carrying a second copy of it. A second copy is exactly how the host-wide predicate #876 removed
/// would come back: it would start as a listing, and nothing would hold it to both legs. There is one
/// reap predicate and this is the one place it is measured.
///
/// The empty-seat refusal lives here rather than in `reap_orphans` for the same reason: it guards the
/// SELECTION, so it guards every caller of it, including one that only intends to print.
#[cfg(feature = "acp")]
pub async fn reapable_holders_live(seat: &str) -> Result<Vec<String>, String> {
    // An empty seat would match every holder whose label failed to parse, so refuse to run at all
    // rather than reap on an identity we do not have. A caller that cannot name itself has nothing to
    // clean up.
    if seat.trim().is_empty() {
        return Err("refusing to reap: no owning seat was named".to_owned());
    }
    // The production client, named here and passed down. Nothing in this path reads an environment
    // variable, a global, or a configuration field to decide what to spawn.
    let client = DockerCli::system();
    let (listing, _) = run_docker(&client, list_holders_argv(seat), None)
        .await
        .map_err(|error| format!("could not list containment holders — {error}"))?;
    let holders = parse_holder_listing(&listing);
    if holders.is_empty() {
        return Ok(Vec::new());
    }

    let (all, _) = run_docker(&client, list_all_containers_argv(), None)
        .await
        .map_err(|error| format!("could not list containers — {error}"))?;
    let all: Vec<String> = all.lines().map(str::trim).filter(|id| !id.is_empty()).map(str::to_owned).collect();
    let (modes, _) = run_docker(&client, network_modes_argv(&all), None)
        .await
        .map_err(|error| format!("could not read container network modes — {error}"))?;

    Ok(reapable_holders(&holders, seat, &modes))
}

/// What one reap did: the holders it removed, and the selected holders it could not remove.
///
/// Two lists rather than one, because the two callers need opposite answers from the same run and
/// either list folded into the other destroys one of them:
///
///   * The boot reaper in `seller_node::run` must never be blocked by a stuck holder. It reads
///     `removed` for its log line and treats `failed` as information, not as a gate.
///   * `maxplayer sandbox-reap` is an operator asking whether a retired seat's leak is gone. For it,
///     "selected three, removed none" is a runtime error. Reporting that as an empty result would
///     print "there were none" — a false statement about the host, on the line a script reads.
///
/// Carrying the failures back rather than printing them is what lets both hold at once. An earlier
/// version wrote them with `eprintln!` from this crate and returned only the removals: the boot path
/// could not route them through its own operator log, the CLI's injected error writer never saw them
/// at all, and — because they were not returned — a TOTAL failure arrived at the caller as an empty
/// list, indistinguishable from nothing to do. Whether a failure is fatal is the caller's decision,
/// and a caller cannot make it without being told.
#[cfg(feature = "acp")]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReapReport {
    /// The holders `docker rm` accepted. Only these were actually removed.
    pub removed: Vec<String>,
    /// The holders the selection chose and `docker rm` refused, each with docker's own reason.
    pub failed: Vec<(String, String)>,
}

#[cfg(feature = "acp")]
impl ReapReport {
    /// How many holders the selection chose, removed or not.
    ///
    /// `removed.len()` alone understates a failing run: the leak the operator asked about is the
    /// selected count, not the successful one.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.removed.len() + self.failed.len()
    }
}

/// Remove `seat`'s own holders that no job is attached to, and report what happened to each.
///
/// `seat` is the caller's seller public key hex. It is the *only* thing that makes this safe to run on
/// a host shared with other seller daemons: see [`reapable_holders`] for why ownership has to be
/// carried and why attachment state cannot stand in for it.
///
/// Best-effort **at the boot call site**, and that is a property of the caller, not of this function.
/// A leaked holder is a resource leak, not an open door — it owns a namespace and holds no policy, and
/// the job that could have used it is already gone — so a failure must never block a boot, whereas a
/// failure to *establish* containment refuses the job outright. The two are deliberately not
/// symmetrical. What changed with #905 is only WHERE that decision is taken: this function reports
/// every failure in [`ReapReport::failed`] and gates on nothing, and each caller chooses. Swallowing
/// the failure here would have forced best-effort on the operator command too, which needs the
/// opposite.
#[cfg(feature = "acp")]
pub async fn reap_orphans(seat: &str) -> Result<ReapReport, String> {
    let mut report = ReapReport::default();
    let client = DockerCli::system();
    for holder in reapable_holders_live(seat).await? {
        match run_docker(
            &client,
            ["docker", "rm", "--force", "--volumes", holder.as_str()]
                .into_iter()
                .map(String::from)
                .collect(),
            None,
        )
        .await
        {
            Ok(_) => report.removed.push(holder),
            // One stuck holder must not stop the others being cleaned up, so this collects and
            // carries on. What it must not do is DROP the failure: the loop continuing is a
            // scheduling decision, not a verdict that the removal was unimportant.
            Err(error) => report.failed.push((holder, error)),
        }
    }
    Ok(report)
}

/// Per-command bound for the periodic sweep's docker calls.
///
/// Shorter than [`DOCKER_DEADLINE`] on purpose. That bound sizes the calls a *job launch* depends
/// on, where waiting two minutes beats failing the job. The sweep depends on nothing and is retried
/// every tick, so a docker daemon that has stopped answering should cost this loop twenty seconds
/// and be tried again later, not hold the seller's cadence for two minutes to reach the same
/// conclusion.
#[cfg(feature = "acp")]
pub const SWEEP_DOCKER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How many expired containers a single sweep will remove before leaving the rest to the next one.
///
/// **Bounded work, so the leak cannot become the outage.** A host that accumulated hundreds of
/// leftovers — a crash loop, a docker daemon down for a day — would otherwise hand this loop an
/// unbounded queue of removals on one tick, and the seller stops answering offers while it drains.
/// The remainder is not lost: it is still expired on the next tick, and the sweep is periodic. A
/// backlog clears across several minutes instead of blocking one.
#[cfg(feature = "acp")]
pub const MAX_SWEEP_REMOVALS: usize = 32;

/// Remove this seat's containers whose own cleanup stamp `now_unix` has passed, and report what
/// happened to each.
///
/// This is the replacement for keeping an owner alive in memory until every container is confirmed
/// gone. Nothing here remembers a job: the stamp written at create time is the whole record, so a
/// container that appeared **after** the process that asked for it had exited is discovered by the
/// next sweep exactly like any other, and a seller that was `SIGKILL`ed mid-job cleans up after its
/// own restart.
///
/// **A failed listing is never an empty one.** Both reads return `Err` rather than an empty
/// selection, because "docker did not answer" and "nothing is expired" are the same value to a
/// caller that only counts removals — and treating the first as the second is how a sweep reports
/// success for a host it never looked at.
///
/// **A failed removal is retried by the NEXT sweep, not here.** The container stays expired, so the
/// following tick selects it again. Retrying in place would spend this tick's bounded budget on a
/// container docker has already refused once.
#[cfg(feature = "acp")]
pub async fn sweep_expired(seat: &str, now_unix: u64) -> Result<ReapReport, String> {
    sweep_expired_with(&DockerCli::system(), seat, now_unix).await
}

/// [`sweep_expired`], with the docker client supplied by the caller.
///
/// Private for the same reason [`establish_with`] is: a test hands in a stand-in as an ARGUMENT, so
/// the substitution is confined to the call under test and two such tests can run in parallel
/// without sharing any global.
#[cfg(feature = "acp")]
async fn sweep_expired_with(
    client: &DockerCli,
    seat: &str,
    now_unix: u64,
) -> Result<ReapReport, String> {
    // Refused rather than run on an identity we do not have: an empty seat would match every
    // container whose seat label failed to parse. Same refusal, same reason, as
    // `reapable_holders_live`.
    if seat.trim().is_empty() {
        return Err("refusing to sweep: no owning seat was named".to_owned());
    }
    let mut report = ReapReport::default();
    let (listing, _) = run_bounded(client, list_owned_argv(seat), None, SWEEP_DOCKER_DEADLINE)
        .await
        .map_err(|error| format!("could not list this seat's containers — {error}"))?;
    let expired = expired_owned(&parse_owned_listing(&listing), seat, now_unix);
    for id in expired.into_iter().take(MAX_SWEEP_REMOVALS) {
        match run_bounded(
            client,
            ["docker", "rm", "--force", "--volumes", id.as_str()]
                .into_iter()
                .map(String::from)
                .collect(),
            None,
            SWEEP_DOCKER_DEADLINE,
        )
        .await
        {
            Ok(_) => report.removed.push(id),
            // Collected and carried past, exactly as the boot reaper does: one container docker
            // refuses must not stop the rest, and the failure is returned rather than dropped so
            // the caller can say so in its log.
            Err(error) => report.failed.push((id, error)),
        }
    }
    Ok(report)
}

/// Run a `docker` argv to completion, optionally feeding `stdin`, and return `(stdout, stderr)`.
///
/// `std::process::Command` on a blocking pool thread, not `tokio::process`: this crate's tokio is
/// built without the `process` feature, and reaching for it would widen the dependency of every
/// default build to enable three calls that happen once per job.
#[cfg(feature = "acp")]
async fn run_docker(
    client: &DockerCli,
    argv: Vec<String>,
    stdin: Option<String>,
) -> Result<(String, String), String> {
    run_bounded(client, argv, stdin, DOCKER_DEADLINE).await
}

/// As [`run_docker`], but the create it issues is **fenced**: the ticket lives inside the blocking
/// closure, so cleanup cannot remove ahead of a create that outlived the future awaiting it.
///
/// The ticket is deliberately not held by this future. Holding it here would release it on
/// cancellation — at precisely the moment the create is still running — which is the bug.
#[cfg(feature = "acp")]
async fn run_docker_fenced(
    client: &DockerCli,
    argv: Vec<String>,
    stdin: Option<String>,
    ticket: CreationTicket,
) -> Result<(String, String), String> {
    let client = client.clone();
    // Taken HERE, on the caller's side of the queue. `spawn_blocking` hands work to a pool that can
    // be saturated, and a clock started inside the closure cannot see the time spent waiting for a
    // thread -- so a create could sit queued for longer than its own deadline and still be handed a
    // full budget on arrival. The bound is measured from the moment the work was ASKED for.
    let queued_at = std::time::Instant::now();
    let joined = tokio::task::spawn_blocking(move || {
        // Moved in, and dropped only when this closure ends: killed on the deadline, failed, or
        // finished. That drop is what "settled" means to `CreationFence::wait_until_settled`.
        let _ticket = ticket;
        let mut child_exited = false;
        run_bounded_blocking(
            &client,
            argv,
            stdin,
            DOCKER_DEADLINE,
            queued_at,
            Some(_ticket.fence()),
            &mut child_exited,
        )
    })
    .await;
    match joined {
        Ok(outcome) => outcome,
        Err(error) => Err(format!("docker task panicked: {error}")),
    }
}

/// Run an argv to completion with a **wall-clock bound**, optionally feeding `stdin`.
///
/// The bound is the cancellation ownership this module was missing. A `docker` client that never
/// returns holds the launch open for as long as it likes, and while it is blocked in the pool the
/// future above it can be cancelled — leaving a command nobody is waiting for and a container nobody
/// is tracking. Past the deadline the child is killed and the caller gets a failure that names the
/// deadline rather than a hang that names nothing.
#[cfg(feature = "acp")]
async fn run_bounded(
    client: &DockerCli,
    argv: Vec<String>,
    stdin: Option<String>,
    deadline: std::time::Duration,
) -> Result<(String, String), String> {
    run_bounded_tracked(client, argv, stdin, deadline).await.0
}

/// As [`run_bounded`], and also says whether the docker CLIENT was reaped with an exit status.
///
/// **That flag is not a removal receipt, and nothing downstream may read it as one.** It says one
/// narrow thing: this process waited for the client and got a status back. It is `true` for a clean
/// exit, a nonzero exit, AND a signal-terminated client — every case where `try_wait` yields a
/// status — because all of them mean the same thing here, that the client is no longer running.
///
/// What it deliberately does NOT mean is that the container is gone. `docker run --rm` asks the
/// daemon to remove the container on the container's own lifecycle; it is not discharged by this
/// process reaping a local client, and on an error path the removal may never have been reached.
/// Promoting "reaped" to "removed" here is what struck live containers off the registry that exists
/// to remove them. The only thing entitled to end custody is a daemon-side absence check — see
/// [`run_sidecar_confirmed`] and [`container_is_absent`].
///
/// A client that was never reaped at all (deadline kill before a status, a panicked task) yields
/// `false`, which is weaker still: not even worth asking the daemon about yet.
#[cfg(feature = "acp")]
async fn run_bounded_tracked(
    client: &DockerCli,
    argv: Vec<String>,
    stdin: Option<String>,
    deadline: std::time::Duration,
) -> (Result<(String, String), String>, bool) {
    run_bounded_tracked_fenced(client, argv, stdin, deadline, None).await
}

/// As [`run_bounded_tracked`], optionally holding a [`CreationTicket`] for the duration of the
/// blocking work.
///
/// The ticket exists because registering a name is not the same as fencing a create. Registration
/// tells cleanup WHAT to remove; it says nothing about WHEN the container appears. A sidecar create
/// still in flight when the holder drops would be removed by name, answered "No such container"
/// because it does not exist yet, marked done — and would then land as an orphan pinning the very
/// namespace the holder was trying to tear down.
///
/// As in [`run_docker_fenced`], the ticket is moved INTO the closure and never held by this future,
/// so cancelling the future cannot release it while the create is still running.
#[cfg(feature = "acp")]
async fn run_bounded_tracked_fenced(
    client: &DockerCli,
    argv: Vec<String>,
    stdin: Option<String>,
    deadline: std::time::Duration,
    ticket: Option<CreationTicket>,
) -> (Result<(String, String), String>, bool) {
    let client = client.clone();
    // As in [`run_docker_fenced`]: the clock starts before the queue, not after it.
    let queued_at = std::time::Instant::now();
    let joined = tokio::task::spawn_blocking(move || {
        let _ticket = ticket;
        let mut child_exited = false;
        let outcome = run_bounded_blocking(
            &client,
            argv,
            stdin,
            deadline,
            queued_at,
            _ticket.as_ref().map(CreationTicket::fence),
            &mut child_exited,
        );
        (outcome, child_exited)
    })
    .await;
    match joined {
        Ok(pair) => pair,
        // A panicked task establishes nothing about the container either.
        Err(error) => (Err(format!("docker task panicked: {error}")), false),
    }
}

/// The blocking half of [`run_bounded_tracked`]. Sets `child_exited` the moment the child is reaped.
#[cfg(feature = "acp")]
fn run_bounded_blocking(
    client: &DockerCli,
    argv: Vec<String>,
    stdin: Option<String>,
    deadline: std::time::Duration,
    queued_at: std::time::Instant,
    fence: Option<&std::sync::Arc<CreationFence>>,
    child_exited: &mut bool,
) -> Result<(String, String), String> {
    {
        use std::io::{Read, Write};
        use std::process::{Command, Stdio};

        let (program, args) = argv.split_first().expect("an argv is never empty");
        // Substituted at the SPAWN site, not in the argv builders: every rendered argv still reads
        // `docker ...`, so what the plan tests assert is what production runs.
        let program =
            if program == "docker" { client.program().to_owned() } else { program.clone() };
        let program = program.as_str();
        // The clock was started by the CALLER, before this work was queued, and every wait below is
        // measured against it: queue time, spawn, plan write, child wait, output drain and writer
        // join all spend the same budget.
        //
        // Anchoring it after the stdin write left that write outside the bound entirely, and
        // anchoring it inside this closure left the queue wait outside it. What is bounded here is
        // exactly this process's flow; it is NOT a statement about when the daemon finishes creating
        // a container, which only a daemon-side absence check can settle.
        let started = queued_at;
        // REFUSED BEFORE IT IS ISSUED, not bounded after it.
        //
        // The budget can already be gone before this closure runs at all: it sat in the blocking
        // pool's queue, and queue time spends the same clock as every wait below. Spawning anyway
        // starts a create whose caller is ALREADY past its bound -- the container can land with
        // nothing waiting on it, which is the orphan this module exists to prevent, issued
        // knowingly. Bounding the wait afterwards cannot help: by then the create exists. The only
        // correct answer at this point is to not start it.
        if started.elapsed() >= deadline {
            return Err(format!(
                "`{program}` was NOT started: its {}s budget was already spent while the work sat \
                 queued for a blocking thread, so no create was issued — starting one here would \
                 launch a container whose caller is already past its bound",
                deadline.as_secs(),
            ));
        }
        // The name this command creates, if it creates one, and the instant its request was issued.
        // Both are needed by the paths below where the client ends WITHOUT the daemon's answer: a
        // killed client, a signalled client, or one that lost the connection mid-request has sent a
        // create the daemon may still apply, and for that name a later "absent" is not "never".
        // Recorded on the fence while this closure still holds its ticket, so the record is in place
        // before the fence can settle.
        let creates = fence.and_then(|fence| container_named_by(args).map(|name| (fence, name)));
        let issued = std::time::SystemTime::now();
        let note_unanswered = |creates: &Option<(&std::sync::Arc<CreationFence>, String)>| {
            if let Some((fence, name)) = creates {
                fence.note_unanswered(name.clone(), issued, client);
            }
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run `{program}`: {error}"))?;
        // How much of the budget is left, measured from the caller's pre-queue clock. Every wait
        // below asks this rather than starting a fresh one, so no step can quietly extend the bound.
        let remaining = || deadline.saturating_sub(started.elapsed());

        // Written on its own thread so a blocked write cannot outrun the deadline, and its result
        // comes back through a CHANNEL rather than a `JoinHandle`.
        //
        // `JoinHandle::join` has no timeout. The old code joined it unconditionally, reasoning that
        // reaping the child closes the read end -- but a descendant started by the client inherits
        // that end and can hold it open, so the join could block after the bounded wait had already
        // returned. A channel can be waited on WITH the remaining budget; the thread itself cannot
        // be killed (Rust has no such thing), so when it outlives the bound it is NAMED instead of
        // being silently dropped.
        let (wrote_tx, wrote_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let writing = match stdin {
            Some(plan) => {
                let mut pipe =
                    child.stdin.take().ok_or_else(|| "docker stdin was not piped".to_string())?;
                // The writer takes a ticket of its OWN, and holds it until the write ends.
                //
                // Reporting that a writer is still running was never the same as owning it. The
                // channel timeout below lets this CALL end; the thread keeps the pipe endpoint
                // either way, and while it was ticketless the closure could return, release the
                // only ticket, and let the fence read as settled with a write still in progress.
                // Cleanup would then be free to remove against a create that had not finished
                // being written to. Now the fence cannot reach zero while this thread exists.
                let ticket = fence.map(|fence| fence.begin());
                std::thread::spawn(move || {
                    let _ticket = ticket;
                    let outcome = pipe.write_all(plan.as_bytes()).map_err(|error| {
                        format!("could not write the plan to the sidecar: {error}")
                    });
                    let _ = wrote_tx.send(outcome);
                });
                true
            }
            None => false,
        };

        // Poll rather than `wait_with_output`, so the deadline is enforceable at all.
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    note_unanswered(&creates);
                    return Err(format!("could not wait for `{program}`: {error}"));
                }
            }
            if started.elapsed() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                // The daemon's answer to this create was never read. Whether the request was applied
                // is now unknown to this process, and it is recorded as exactly that -- not as
                // absent, not as failed -- so cleanup watches the name instead of trusting one
                // empty inspect.
                note_unanswered(&creates);
                // The writer is settled HERE too, not abandoned. Killing the child closes the read
                // end, so a blocked `write_all` fails with `EPIPE` and the thread ends on its own;
                // this waits a short, explicit grace for exactly that and reports the writer as
                // still running when it does not arrive. Dropping the handle instead is how this
                // flow used to end "complete" while a write was still in progress.
                // Whichever way it goes, the writer's disposition is STATED. The failure this
                // replaces was silence: the handle was dropped on the way out, so a caller could
                // not tell a writer that had finished from one still pushing a plan into a pipe.
                // Both answers are legitimate; not having asked is not.
                let writer = if !writing {
                    "; no plan was being written"
                } else if wrote_rx.recv_timeout(WRITER_EPIPE_GRACE).is_ok() {
                    "; the thread writing its plan was settled after the kill"
                } else {
                    "; the thread writing its plan is STILL RUNNING in this process and could not \
                     be joined within the grace after the kill"
                };
                return Err(format!(
                    "`{program}` did not finish within {}s and was killed — a command with no bound \
                     is a launch that can hang and a container nobody is waiting for{writer}",
                    deadline.as_secs(),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // Reaped with a status — ANY status, including a signal termination, which `code()` reports
        // as `None` below. This flag means only "the client is no longer running", never "the
        // container is gone": `--rm` is discharged by the daemon on the container's lifecycle, not
        // by this process waiting on a client. The caller must still confirm absence with the
        // daemon before ending custody.
        *child_exited = true;
        // Reaping the child does NOT close its pipes. A descendant it started inherits the write
        // ends and can hold them open indefinitely, and `read_to_end` returns at EOF -- precisely
        // what such a descendant withholds. Draining on this thread therefore put an UNBOUNDED wait
        // directly after the bounded one, which is the hole this replaces: the drains run on their
        // own threads and are collected against the same budget as everything above.
        // Each reader sends its RESULT, not a buffer: a read that failed is an output this process
        // did not get, and it is recorded as unknown rather than as "the client said nothing".
        let (drained_tx, drained_rx) =
            std::sync::mpsc::channel::<(&'static str, std::io::Result<Vec<u8>>)>();
        let mut pending: Vec<&'static str> = Vec::new();
        // Each drain takes its OWN ticket, for the same reason the writer does: a descendant can
        // hold these endpoints open long past the channel timeout below, and a reader still blocked
        // on a pipe is work this process is still doing. Ticketless, it was work nobody was counted
        // for -- the closure returned, the last ticket went with it, and the fence said settled
        // while two threads still held the create's output. The ticket is released when the read
        // ends, not when this call does.
        if let Some(mut pipe) = child.stdout.take() {
            let tx = drained_tx.clone();
            pending.push("stdout");
            let ticket = fence.map(|fence| fence.begin());
            std::thread::spawn(move || {
                let _ticket = ticket;
                let mut buffer = Vec::new();
                let outcome = pipe.read_to_end(&mut buffer).map(|_| buffer);
                let _ = tx.send(("stdout", outcome));
            });
        }
        if let Some(mut pipe) = child.stderr.take() {
            let tx = drained_tx.clone();
            pending.push("stderr");
            let ticket = fence.map(|fence| fence.begin());
            std::thread::spawn(move || {
                let _ticket = ticket;
                let mut buffer = Vec::new();
                let outcome = pipe.read_to_end(&mut buffer).map(|_| buffer);
                let _ = tx.send(("stderr", outcome));
            });
        }
        drop(drained_tx);
        let mut stdout = Vec::new();
        // `None` until stderr is read TO EOF WITHOUT ERROR. A stream still held by a descendant, or
        // a read that failed, leaves this unknown — and an unknown stderr is never read as "the
        // daemon refused".
        let mut stderr_read: Option<Vec<u8>> = None;
        while !pending.is_empty() {
            match drained_rx.recv_timeout(remaining()) {
                Ok((which, outcome)) => {
                    pending.retain(|name| *name != which);
                    match (which, outcome) {
                        ("stdout", Ok(buffer)) => stdout = buffer,
                        ("stdout", Err(error)) => {
                            eprintln!("sandbox: could not read `{program}` stdout: {error}");
                        }
                        (_, Ok(buffer)) => stderr_read = Some(buffer),
                        (_, Err(error)) => {
                            eprintln!(
                                "sandbox: could not read `{program}` stderr: {error} — its answer, \
                                 if it gave one, is unknown to this process"
                            );
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let stderr_known = stderr_read
            .as_deref()
            .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned());
        let done = std::process::Output {
            status,
            stdout,
            stderr: stderr_read.clone().unwrap_or_default(),
        };
        let stdout = String::from_utf8_lossy(&done.stdout).trim().to_owned();
        let stderr = stderr_known.clone().unwrap_or_default();
        // CLASSIFIED HERE, before any early return below, and once. The client has ended; whether
        // the daemon ANSWERED it — accepted, or refused — is decided from positive proof only: exit
        // 0, the daemon's own refusal text in a stderr this process read to EOF, or a `run` whose
        // exit code is the contained command's. Everything else — a signal, a client-side exit with
        // no daemon text, a stderr still held by a descendant, a failed read — is an outcome this
        // process does not know, and the name is recorded as unanswered so absence is not taken as
        // proof for it. The previous flow returned on a pending drain BEFORE reaching its
        // classification, so a reaped client whose pipe a descendant held was never recorded at all,
        // and it recognised only eight stderr substrings as "lost the daemon", reading every other
        // text as a refusal.
        let answered = daemon_answered(
            args.first().map(String::as_str),
            done.status.code(),
            stderr_known.as_deref(),
        );
        if !answered {
            note_unanswered(&creates);
        }
        // A half-written plan is a sidecar that acted on a truncated instruction, so the write's own
        // failure is reported -- but only when the child itself did not already fail, because the
        // child's exit code names the refusal more precisely than a broken pipe does. Waited on with
        // what is left of the budget, never unconditionally.
        let mut writer_outstanding = false;
        let wrote = if writing {
            match wrote_rx.recv_timeout(remaining()) {
                Ok(result) => result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    writer_outstanding = true;
                    Err(format!(
                        "the plan was still being written to `{program}` when the {}s bound expired \
                         -- the writing thread is still running in this process, so this call ends \
                         on its bound rather than reporting a completed write",
                        deadline.as_secs()
                    ))
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    Err("the thread writing the plan to the sidecar panicked".to_string())
                }
            }
        } else {
            Ok(())
        };
        // An output this process never finished reading is not an output it may report. Naming the
        // stream and the still-running reader is the honest end; inventing a truncated success is
        // how a caller comes to believe a create said something it never said.
        if !pending.is_empty() {
            return Err(format!(
                "`{program}` was reaped, but its {} did not reach EOF within the {}s bound — a \
                 descendant is holding the pipe open, so this call ends on its bound; the reading \
                 thread(s) remain outstanding in this process{}",
                pending.join(" and "),
                deadline.as_secs(),
                if writer_outstanding {
                    ", as does the plan writer"
                } else {
                    ""
                }
            ));
        }
        match done.status.code() {
            Some(0) => match wrote {
                Ok(()) => Ok((stdout, stderr)),
                Err(error) => Err(error),
            },
            // The sidecar's codes are an interface; pass them through in the message so the caller's
            // error names WHICH refusal happened rather than "it failed". Whether the daemon
            // answered was decided above, before the drain check, from positive proof.
            Some(code) => {
                Err(format!("exit {code}: {}", if stderr.is_empty() { &stdout } else { &stderr }))
            }
            None => Err("killed by a signal".to_string()),
        }
    }
}

/// Whether a docker client that has ENDED was, on positive evidence, ANSWERED by the daemon —
/// accepted or refused — so that its request is settled and absence afterwards means absence.
///
/// Positive proof only, and exactly these three:
///  * exit 0: the daemon accepted; for `run --detach` it answered with the id.
///  * a stderr this process read to EOF that carries the daemon's own refusal text
///    (`Error response from daemon`): the request reached the daemon and was refused, or ran and
///    left something the ordinary remove-and-confirm path owns.
///  * a `run` whose exit code is not the CLI's own 125: the contained command ran (126/127 are
///    "cannot invoke"/"not found" for a container that WAS created and the rest are the command's
///    own codes), so the container existed and `--rm` or the holder's cleanup owns it.
///
/// Everything else is UNKNOWN and returns `false`: a signal, a client-side 125 with no daemon text,
/// a stderr not read to EOF (`None`), an empty stderr, or any error text at all that is not the
/// daemon's. The version this replaces recognised eight client-side substrings as "lost the daemon"
/// and treated every other text as a refusal — an inference from a list, in the direction that
/// releases custody. The cost of the positive rule is named: a client-side argument error (exit 125,
/// `docker: invalid reference format`) is now watched like an unanswered create, one bounded inspect
/// per scheduled attempt for the life of the process, because the event log cannot say "no request
/// was ever made" any more than it can say "that request will never be applied".
#[cfg(feature = "acp")]
fn daemon_answered(verb: Option<&str>, code: Option<i32>, stderr: Option<&str>) -> bool {
    match code {
        Some(0) => true,
        Some(code) => {
            let daemon_spoke =
                stderr.is_some_and(|text| text.contains("Error response from daemon"));
            let command_ran = verb == Some("run") && code != 125;
            daemon_spoke || command_ran
        }
        None => false,
    }
}

/// The container name a docker argv would create, if it would create one.
///
/// Only `run` and `create` make containers, and only `--name` gives one a name this module owns.
/// Anything else has nothing to watch.
#[cfg(feature = "acp")]
fn container_named_by(args: &[String]) -> Option<String> {
    let creates = matches!(args.first().map(String::as_str), Some("run" | "create"));
    if !creates {
        return None;
    }
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--name" {
            return args.next().cloned();
        }
        if let Some(name) = arg.strip_prefix("--name=") {
            return Some(name.to_owned());
        }
    }
    None
}

/// A unique name for one temporary container joined to `holder`'s namespace.
///
/// Unique per process and per call, so nothing here can address — or remove — a container belonging
/// to another run.
pub fn sidecar_name(holder: &str, verb: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{holder}-{verb}-{}-{serial}", std::process::id())
}

/// Give a `docker run` argv an explicit container name.
///
/// An unnamed sidecar cannot be cleaned up after a cancellation: docker assigns it a random name
/// this process never learns, so the one container capable of pinning the namespace open is the one
/// container nothing can address.
pub fn with_container_name(mut argv: Vec<String>, name: &str) -> Result<Vec<String>, String> {
    match argv.get(1).map(String::as_str) {
        Some("run") => {
            argv.splice(2..2, ["--name".to_owned(), name.to_owned()]);
            Ok(argv)
        }
        other => Err(format!(
            "refusing to name {other:?} as a container: this is not a `docker run` argv, and naming \
             the wrong command would register a cleanup target that does not exist"
        )),
    }
}

/// Run one sidecar joined to the holder's namespace: named, registered for its lifetime, bounded.
#[cfg(feature = "acp")]
async fn run_sidecar(
    holder: &NetnsHolder,
    verb: &str,
    argv: Vec<String>,
    stdin: Option<String>,
) -> Result<(String, String), String> {
    run_sidecar_with_deadline(holder, verb, argv, stdin, DOCKER_DEADLINE).await
}

/// As [`run_sidecar`], with the bound named by the caller.
///
/// The deadline is a parameter solely so the custody rule below can be measured offline. A test
/// cannot wait out the production bound, and a rule about what happens when the client is killed is
/// worth nothing if the only thing measured is the flag feeding it.
#[cfg(feature = "acp")]
async fn run_sidecar_with_deadline(
    holder: &NetnsHolder,
    verb: &str,
    argv: Vec<String>,
    stdin: Option<String>,
    deadline: std::time::Duration,
) -> Result<(String, String), String> {
    run_sidecar_confirmed(holder, verb, argv, stdin, deadline, container_is_absent).await
}


/// How custody asks whether a container is gone. `Some(true)` = confirmed absent, `Some(false)` =
/// confirmed present, `None` = could not be established.
///
/// Injected so the rule below is measurable without a daemon. Only `Some(true)` releases custody, so
/// a confirmer that cannot tell is treated exactly like one that says "still there".
#[cfg(feature = "acp")]
type ConfirmAbsent = fn(&DockerCli, &str) -> Option<bool>;

/// As [`run_sidecar_with_deadline`], with the absence check injected.
///
/// **Custody ends on confirmed absence, and on nothing else.**
///
/// The previous rule ended it on a reaped client, reasoning that `docker run --rm` removes the
/// container when its client exits. That reasoning describes the happy path and quietly covers the
/// failure paths with it. A client reaped with a nonzero status, a stdin write that failed before
/// the wait, a client killed on the deadline — each returns from the same call, and none of them is
/// the DAEMON confirming the container is gone. `--rm` is a request to the daemon, not a receipt
/// from it: removal can still be queued, in progress, or refused, and on an error path it may never
/// have been reached at all. Deregistering on the client's say-so struck live containers off the
/// registry that exists to remove them.
///
/// So the client's exit is now only a reason to ASK. The answer comes from docker, and a confirmer
/// that cannot answer keeps the name a cleanup target — the cost of which is one `docker rm`
/// replying "No such container", which cleanup already treats as success.
#[cfg(feature = "acp")]
async fn run_sidecar_confirmed(
    holder: &NetnsHolder,
    verb: &str,
    argv: Vec<String>,
    stdin: Option<String>,
    deadline: std::time::Duration,
    confirm_absent: ConfirmAbsent,
) -> Result<(String, String), String> {
    let name = sidecar_name(holder.name(), verb);
    let argv = with_container_name(argv, &name)?;
    // Registered BEFORE the command starts: a cancellation between these two lines must still leave
    // a cleanup target behind, and registering afterwards would not.
    let mut registration = holder.watch_sidecar(name.clone());
    // Registration says WHAT to remove; the ticket says WHEN it is safe to. Without it, a holder
    // dropped while this create is in flight removes the name, is told "No such container" because
    // the container does not exist YET, treats that as done — and the create then lands as an
    // orphan pinning the namespace. Moved into the blocking closure, never held by this future.
    let (outcome, child_exited) = run_bounded_tracked_fenced(
        holder.client(),
        argv,
        stdin,
        deadline,
        Some(holder.fence_creation()),
    )
    .await;
    // Reaching this line at all proves the command is no longer in flight: a cancellation drops the
    // future before it, so a cancelled command's name stays a cleanup target.
    //
    // A client that was never reaped is not even worth asking about — the daemon may still be
    // creating or running that container — so custody is simply kept.
    if child_exited {
        let asked = name.clone();
        let client = holder.client().clone();
        let absent = tokio::task::spawn_blocking(move || confirm_absent(&client, &asked))
            .await
            .unwrap_or(None);
        if absent == Some(true) {
            registration.completed();
        }
    }
    drop(registration);
    outcome
}

/// Ask docker whether a container name is gone.
///
/// `Some(true)` only for docker saying the object does not exist. A successful inspect is
/// `Some(false)`: the container is still there. Anything else — docker missing, the daemon not
/// answering, an unrecognised error — is `None`, which keeps custody.
#[cfg(feature = "acp")]
fn container_is_absent(client: &DockerCli, name: &str) -> Option<bool> {
    let mut child = std::process::Command::new(client.program())
        .args(["inspect", "--type", "container", "--format", "{{.Id}}", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let status = NetnsHolder::wait_bounded(&mut child, NetnsHolder::REMOVE_DEADLINE).ok()?;
    let mut stderr_bytes = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read as _;
        let _ = pipe.read_to_end(&mut stderr_bytes);
    }
    if status.success() {
        return Some(false);
    }
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    if NetnsHolder::force_remove_stderr_is_benign(&stderr) || stderr.contains("No such object") {
        Some(true)
    } else {
        None
    }
}

/// What the daemon's event log says happened under one exact name since a request was issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    /// The daemon answered and recorded no container created under the name since the request.
    NoRecord,
    /// At least one container was created under the name since the request and the daemon has NOT
    /// recorded a `destroy` for that same container id. It is live, or its end is unknown. Either
    /// way it is not evidence that the name is finished.
    Landed,
    /// Every container created under the name since the request has a `destroy` recorded for the
    /// same id, and there was at least one. The request ran its course and what it made is gone.
    Completed,
}

/// Read the daemon's own event log for containers under EXACTLY `name` since `issued`, and say
/// whether what was created there has ALSO been destroyed.
///
/// This is the observation that lets a watched name be discharged when it is absent NOW: absence
/// alone is what a delayed create looks like before it lands. The first version of this asked only
/// "did ANY event under this name happen since the request", which a `create` alone answers yes to
/// — so a container that landed between the caller's inspect and this query was read as finished
/// while it was running. Lifecycle evidence is now paired BY CONTAINER ID: a `create` counts as
/// finished only when a `destroy` for the same id follows it, and a `create` with no `destroy` under
/// the name makes the whole answer [`Lifecycle::Landed`] whatever else the log shows. Lines are
/// matched on the exact name field — docker's `container=` filter matches prefixes, so the output is
/// checked rather than trusted. `None` when the daemon did not answer, or did not finish answering
/// within `bound`, which keeps custody.
///
/// Bounded twice over. The child is waited on with [`NetnsHolder::REMOVE_DEADLINE`]; its stdout is
/// read on a thread whose result is waited for with `bound`, never joined without one. A reaped
/// client does not close a pipe a descendant inherited, and an unbounded join here ran on the ONE
/// supervisor thread — so one held pipe stalled every name the supervisor owed, not just this one.
/// A reader that outlives `bound` is named in the log and its answer is discarded as uncertain.
///
/// Limitation, stated: the daemon's event buffer is finite, and there is no API that says "that
/// request will never be applied". A name whose create was never delivered at all is therefore
/// never discharged by this observation and stays watched for the life of the process, at the cost
/// of one bounded inspect per scheduled attempt. Identity is by exact name plus container id, not by
/// request: a client whose answer was never read has no request id to correlate, so a container
/// another actor created and destroyed under this exact name inside the window would read as this
/// request's completion. Holder names are unique per job id, so that actor would have to reuse this
/// job's name deliberately.
#[cfg(feature = "acp")]
fn lifecycle_since(
    client: &DockerCli,
    name: &str,
    issued: std::time::SystemTime,
    bound: std::time::Duration,
) -> Option<Lifecycle> {
    let since = issued.duration_since(std::time::UNIX_EPOCH).ok()?;
    let until = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?;
    let stamp = |at: std::time::Duration| format!("{}.{:09}", at.as_secs(), at.subsec_nanos());
    let started = std::time::Instant::now();
    let mut child = std::process::Command::new(client.program())
        .args([
            "events",
            "--since",
            &stamp(since),
            "--until",
            &stamp(until),
            "--filter",
            "type=container",
            "--filter",
            &format!("container={name}"),
            "--format",
            "{{.Actor.ID}}\t{{.Actor.Attributes.name}}\t{{.Action}}",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (read_tx, read_rx) = std::sync::mpsc::channel::<std::io::Result<Vec<u8>>>();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        let outcome = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = read_tx.send(outcome);
    });
    let status = NetnsHolder::wait_bounded(&mut child, bound.min(NetnsHolder::REMOVE_DEADLINE)).ok()?;
    let bytes = match read_rx.recv_timeout(bound.saturating_sub(started.elapsed())) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            eprintln!("sandbox: could not read the daemon's event log for {name}: {error} — uncertain");
            return None;
        }
        Err(_) => {
            eprintln!(
                "sandbox: the daemon's event log for {name} did not reach EOF within {bound:?} — a \
                 descendant of the client is holding the pipe; the reading thread remains \
                 outstanding in this process and its partial answer is DISCARDED as uncertain, so \
                 the name stays owed and the owner moves on to its other names"
            );
            return None;
        }
    };
    if !status.success() {
        eprintln!(
            "sandbox: the daemon's event log for {name} could not be read ({status}) — uncertain, \
             the name stays owed"
        );
        return None;
    }
    Some(lifecycle_from_events(&String::from_utf8_lossy(&bytes), name))
}

/// Pair `create`/`destroy` events by container id under exactly `name`. Pure, so it is unit-tested
/// on its own against the daemon's line format.
#[cfg(feature = "acp")]
fn lifecycle_from_events(output: &str, name: &str) -> Lifecycle {
    // id -> (created since the request, destroyed since the request)
    let mut by_id: Vec<(String, bool, bool)> = Vec::new();
    for line in output.lines() {
        let mut fields = line.split('\t');
        let (Some(id), Some(actor), Some(action)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if actor != name {
            continue;
        }
        let entry = match by_id.iter_mut().find(|(known, _, _)| known == id) {
            Some(entry) => entry,
            None => {
                by_id.push((id.to_owned(), false, false));
                by_id.last_mut().expect("just pushed")
            }
        };
        match action {
            "create" => entry.1 = true,
            "destroy" => entry.2 = true,
            _ => {}
        }
    }
    // A `destroy` alone is a container created BEFORE the request, which was never this request's.
    let created: Vec<&(String, bool, bool)> = by_id.iter().filter(|(_, created, _)| *created).collect();
    if created.is_empty() {
        Lifecycle::NoRecord
    } else if created.iter().all(|(_, _, destroyed)| *destroyed) {
        Lifecycle::Completed
    } else {
        Lifecycle::Landed
    }
}

/// Establish containment for one job: measure the proxy address, create the namespace holder, install
/// the rendered policy into it.
///
/// On success the caller launches the job with `--network` = [`NetnsHolder::network_mode`] and points
/// its base URL at [`Containment::proxy_host`]. On **any** failure the holder is destroyed on the way
/// out — the guard exists from the moment the container does, so a `?` cannot leave a half-configured
/// namespace behind.
///
/// There is no partial success and no retry. A sidecar that failed mid-plan leaves rules already
/// applied, and re-running appends the whole plan on top of them: the second attempt then reports
/// success over a duplicated, half-ordered ruleset. Destroying the namespace is the only sound
/// recovery, which is why the sidecar's exit 3 says so explicitly.
#[cfg(feature = "acp")]
#[allow(clippy::too_many_arguments)]
pub async fn establish(
    network: &str,
    holder_image: &str,
    sidecar_image: &str,
    proxy_alias: &str,
    job_id: &str,
    seat: &str,
    uid: u32,
    gid: u32,
    proxy_ports: Option<crate::sandbox_net::PortRange>,
    log_connections: bool,
    dns_resolvers: Vec<String>,
    cleanup_after: u64,
) -> Result<Containment, String> {
    // The production client is named here, once, and threaded down. This is the ONLY constructor a
    // shipped build can reach, and it takes no input: no environment variable, no config field, no
    // global. A test that needs a stand-in calls `establish_with` and hands one in.
    establish_with(
        &DockerCli::system(),
        FenceBounds::production(),
        network,
        holder_image,
        sidecar_image,
        proxy_alias,
        job_id,
        seat,
        uid,
        gid,
        proxy_ports,
        log_connections,
        dns_resolvers,
        cleanup_after,
    )
    .await
}

/// [`establish`], with the docker client and the cleanup bounds supplied by the caller.
///
/// Private, and the only way to supply either. Tests pass a stand-in here as an ARGUMENT, so the
/// substitution is confined to the one call under test: nothing is installed anywhere another test
/// could read it, no lock has to be remembered, and two such tests can run in parallel without
/// seeing each other.
#[cfg(feature = "acp")]
#[allow(clippy::too_many_arguments)]
async fn establish_with(
    client: &DockerCli,
    bounds: FenceBounds,
    network: &str,
    holder_image: &str,
    sidecar_image: &str,
    proxy_alias: &str,
    job_id: &str,
    seat: &str,
    uid: u32,
    gid: u32,
    proxy_ports: Option<crate::sandbox_net::PortRange>,
    log_connections: bool,
    dns_resolvers: Vec<String>,
    cleanup_after: u64,
) -> Result<Containment, String> {
    // Measured BEFORE the holder exists, so a probe failure needs no cleanup.
    let (probe_stdout, _) =
        run_docker(client, host_gateway_probe_argv(sidecar_image, proxy_alias), None)
            .await
            .map_err(|error| format!("could not resolve {proxy_alias} for the pinhole — {error}"))?;
    let proxy_host = parse_getent_ipv4(&probe_stdout).ok_or_else(|| {
        format!("resolving {proxy_alias} produced no IPv4 address (got {probe_stdout:?})")
    })?;

    let name = holder_name(job_id);
    // Adopted BEFORE the create is issued, not after it returns. `run_docker` awaits, an await is a
    // cancellation point, and the blocking create can complete after the future above it is gone:
    // adopting afterwards left exactly that container running with no guard and no record. The guard
    // costs one `docker rm` that reports "No such container" when the create never happened.
    let holder = NetnsHolder::adopt_bounded(name.clone(), client.clone(), bounds);
    // Fenced, not merely adopted. The ticket is taken before the create is issued and travels into
    // the blocking closure, so a cancellation here leaves cleanup waiting for the create to settle
    // instead of racing it to a "No such container" that means "not yet".
    let ticket = holder.fence_creation();
    run_docker_fenced(
        client,
        holder_argv(&name, network, holder_image, uid, gid, job_id, seat, cleanup_after),
        None,
        ticket,
    )
    .await
    .map_err(|error| format!("could not start the netns holder {name} — {error}"))?;

    // The resolvers arrive from the caller rather than being discovered here, and that is the one
    // property that keeps the job's `/etc/resolv.conf` and this policy in agreement: the caller
    // resolves once, writes that file from the result, and hands the same addresses here. Two
    // discoveries could disagree and the job would be pointed at a resolver its own firewall drops.
    let policy = NetPolicy {
        gateway: proxy_host.clone(),
        proxy_ports,
        log_connections,
        dns_resolvers,
    };
    let (plan, expected) = plan_stdin(&policy);
    let (applied, _) =
        run_sidecar(&holder, "iptables", sidecar_argv(&holder, sidecar_image), Some(plan))
            .await
            .map_err(|error| format!("containment was not installed — {error}"))?;

    // The count cross-check. A truncated stdin applies cleanly and exits 0, so no exit code reveals
    // it; only comparing the sidecar's own total against what was rendered does.
    let applied: usize = applied
        .parse()
        .map_err(|_| format!("the sidecar reported {applied:?} rules applied, not a number"))?;
    if applied != expected {
        return Err(format!(
            "containment is incomplete: {applied} of {expected} rules applied (the plan was truncated in transit)"
        ));
    }

    // The readback (#797 R1). Everything above this point is the installer's own account of its work:
    // an exit code and a number it chose to print. Neither can distinguish a namespace whose rules are
    // in force from one where a runtime accepted `--cap-add NET_ADMIN` and quietly did nothing. So the
    // kernel is asked directly, per family, and the job is refused unless the answer holds.
    //
    // Both families are checked, and a v6 failure is as fatal as a v4 one: an unfiltered address family
    // is the cheapest bypass there is.
    for family in [Family::V4, Family::V6] {
        let (readback, _) = run_sidecar(
            &holder,
            "iptables-readback",
            readback_argv(holder.name(), sidecar_image, family),
            None,
        )
        .await
        .map_err(|error| {
            format!("could not read {} rules back from the namespace — {error}", family.binary())
        })?;
        policy.verify_readback(family, &readback).map_err(|error| {
            format!("containment did not verify after installation — {error}")
        })?;
    }

    // ── The interface the packets actually leave by ───────────────────────────────────────────
    //
    // Everything above installs and verifies rules on the host kernel's `OUTPUT` chain, and a gVisor
    // payload never traverses it: `runsc` runs its own netstack and hands finished packets straight
    // to the namespace's veth. The readback above is entirely honest and the job is still uncontained
    // — measured on this repo's fixtures, both families, over TCP.
    //
    // So the same rendered policy is translated onto the veth itself, and unconditionally rather than
    // only for a `runsc` job: `establish` is not told which runtime the caller will launch under, and
    // "contained under one runtime" is exactly the state being closed here. Under `runc` the filters
    // are redundant with the chain above, which costs one qdisc and a handful of filters per job.
    //
    // Same failure discipline as the chain above: no partial success, no retry. Every `?` from here
    // leaves through the holder guard, which destroys the namespace on the way out.
    let dev = egress_device(&holder, sidecar_image).await?;
    let iface = crate::sandbox_iface::IfacePlan::derive(&dev, &policy)
        .map_err(|error| format!("the egress filter plan for {dev} could not be rendered — {error}"))?;
    let (iface_plan, iface_expected) = crate::sandbox_iface::plan_stdin(&iface);
    let (iface_applied, _) = run_sidecar(
        &holder,
        "iface",
        crate::sandbox_iface::iface_sidecar_argv(holder.name(), sidecar_image),
        Some(iface_plan),
    )
    .await
    .map_err(|error| {
        format!(
            "egress filters were not installed on {dev} — {error} (the applier's exit 6 means this \
             sidecar image shipped without iproute2, so no job can be contained by this build; its \
             exit 3 means the interface is PARTIALLY filtered and the namespace is being destroyed \
             rather than retried)"
        )
    })?;

    // The same count cross-check the chain above does, for the same reason: a truncated stdin applies
    // perfectly and exits 0, and only comparing the applier's own total against what was rendered
    // reveals it.
    let iface_applied: usize = iface_applied.parse().map_err(|_| {
        format!("the interface applier reported {iface_applied:?} filters applied, not a number")
    })?;
    if iface_applied != iface_expected {
        return Err(format!(
            "egress filtering is incomplete: {iface_applied} of {iface_expected} steps applied on \
             {dev} (the plan was truncated in transit)"
        ));
    }

    // The readback, from a different container running a different verb, because everything above is
    // still the installer's own account of its work. `verify_readback` checks presence, order, both
    // families, the exceptions' width and that no drop carries a protocol match — the TCP-only drop
    // is the bug this closes, not the fix.
    let (iface_readback, _) = run_sidecar(
        &holder,
        "iface-readback",
        crate::sandbox_iface::filter_readback_argv(holder.name(), sidecar_image, &dev),
        None,
    )
    .await
    .map_err(|error| format!("could not read the egress filters back from {dev} — {error}"))?;
    iface.verify_readback(&iface_readback).map_err(|error| {
        format!("egress filtering did not verify on {dev} after installation — {error}")
    })?;

    Ok(Containment { holder, proxy_host, egress_dev: dev })
}

/// Which link inside the holder's namespace the job's packets leave by — measured from the
/// namespace's own link list, never assumed to be `eth0`.
///
/// The probe is an **unprivileged** container (`--cap-drop ALL`, no `NET_ADMIN`): enumerating links
/// is a read, and the one container in this design that can change an interface must not also be the
/// thing that chooses which interface to change.
///
/// [`crate::sandbox_iface::select_egress_link`] refuses anything that is not a job's own namespace —
/// a bridge among the links, no loopback, or more than one candidate — so a mis-aimed `--network`
/// fails the launch here instead of installing drops on something shared.
#[cfg(feature = "acp")]
async fn egress_device(holder: &NetnsHolder, sidecar_image: &str) -> Result<String, String> {
    let (links, _) = run_sidecar(
        holder,
        "link-probe",
        crate::sandbox_iface::link_probe_argv(holder.name(), sidecar_image),
        None,
    )
    .await
    .map_err(|error| format!("could not enumerate the links in the job's namespace — {error}"))?;
    let parsed = crate::sandbox_iface::parse_links(&links).map_err(|error| {
        format!("the job's namespace listed a link this build cannot read — {error}")
    })?;
    let link = crate::sandbox_iface::select_egress_link(&parsed)
        .map_err(|error| format!("the job's egress interface could not be identified — {error}"))?;
    Ok(link.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_net::{Family, PortRange};

    fn policy() -> NetPolicy {
        NetPolicy {
            gateway: "172.17.0.1".into(),
            proxy_ports: Some(PortRange::new(9000, 9002).expect("valid range")),
            log_connections: true,
            dns_resolvers: Vec::new(),
        }
    }

    /// Two seller daemons sharing one host. `SEAT_A` is a co-tenant; `SEAT_B` is the one booting.
    fn seat_a() -> String {
        "a1".repeat(32)
    }
    fn seat_b() -> String {
        "b2".repeat(32)
    }

    /// Seat A's holder, in its **pre-attach window**: created, its job has not joined yet.
    fn seat_a_holder() -> String {
        "a".repeat(64)
    }
    /// Seat B's own holder, genuinely stale — left by a crash before its guard ran.
    fn seat_b_holder() -> String {
        "b".repeat(64)
    }

    /// The whole point of the pair below: **nothing is attached to either holder.** Attachment state
    /// therefore cannot tell the two apart, and ownership is the only discriminator that exists.
    const NOTHING_ATTACHED: &str = "bridge\nhost\nmx-sandbox-net\n";

    fn two_seats_one_host() -> Vec<HolderRecord> {
        vec![
            HolderRecord { id: seat_a_holder(), seat: Some(seat_a()) },
            HolderRecord { id: seat_b_holder(), seat: Some(seat_b()) },
        ]
    }

    /// LEG 1 — seat B must not remove seat A's holder, which is unattached but very much in use.
    ///
    /// The pair with [`seat_b_does_select_its_own_stale_holder`] is deliberate and neither half stands
    /// alone: this one passes for a reaper that removes nothing at all, and that one passes for the
    /// host-wide reaper this replaced. They are separate `#[test]`s rather than two asserts in one
    /// body so that a failure names which leg went red — an early assert would silence the other.
    #[test]
    fn seat_b_does_not_select_seat_as_live_but_unattached_holder() {
        let selected = reapable_holders(&two_seats_one_host(), &seat_b(), NOTHING_ATTACHED);
        assert!(
            !selected.contains(&seat_a_holder()),
            "LEG 1: seat B selected seat A's live-but-unattached holder for removal: {selected:?}"
        );
    }

    /// LEG 2 — the anti-vacuity half: seat B must still remove its own stale holder.
    #[test]
    fn seat_b_does_select_its_own_stale_holder() {
        let selected = reapable_holders(&two_seats_one_host(), &seat_b(), NOTHING_ATTACHED);
        assert!(
            selected.contains(&seat_b_holder()),
            "LEG 2: seat B failed to select its OWN stale holder — a reaper that reaps nothing: {selected:?}"
        );
    }

    /// A holder from a build older than the seat label has no owner to match, so nobody removes it.
    /// Unattributable must mean left alone: the alternative is a seat destroying a stranger's job.
    #[test]
    fn an_unlabelled_holder_belongs_to_nobody_and_is_never_reaped() {
        let legacy = vec![HolderRecord { id: seat_b_holder(), seat: None }];
        assert!(
            reapable_holders(&legacy, &seat_b(), NOTHING_ATTACHED).is_empty(),
            "a holder with no seat label must never be selected"
        );
        // …and an empty seat must not become the key that matches it.
        assert!(reapable_holders(&legacy, "", NOTHING_ATTACHED).is_empty());
    }

    /// An absent label arrives as an empty FIELD. Read as a seat named "", every legacy holder would
    /// look owned, and one caller passing an empty seat would take the host.
    #[test]
    fn a_missing_seat_label_parses_as_no_owner_not_as_an_empty_owner() {
        let listing = format!("{}\t{}\n{}\t\n{}\n", seat_a_holder(), seat_a(), seat_b_holder(), "c".repeat(64));
        let parsed = parse_holder_listing(&listing);
        assert_eq!(parsed.len(), 3, "{parsed:?}");
        assert_eq!(parsed[0], HolderRecord { id: seat_a_holder(), seat: Some(seat_a()) });
        assert_eq!(parsed[1], HolderRecord { id: seat_b_holder(), seat: None });
        assert_eq!(parsed[2], HolderRecord { id: "c".repeat(64), seat: None });
        // Blank lines are not a holder with no id.
        assert!(parse_holder_listing("\n  \n").is_empty());
    }

    /// Ownership does not license removing a holder a job is attached to — the seat's own job, mid
    /// pre-attach window, is the case that must survive its own daemon's boot.
    #[test]
    fn a_seats_own_holder_with_a_job_attached_survives() {
        let modes = format!("bridge\ncontainer:{}\n", seat_b_holder());
        assert!(
            reapable_holders(&two_seats_one_host(), &seat_b(), &modes).is_empty(),
            "an attached holder must survive even for the seat that owns it"
        );
    }

    #[test]
    fn the_job_joins_the_holders_namespace_and_never_names_a_network() {
        let holder = NetnsHolder::adopt("maxplayer-netns-abc".into(), DockerCli::system());
        assert_eq!(holder.network_mode(), "container:maxplayer-netns-abc");
    }

    #[test]
    fn the_holder_runs_sleep_in_exec_form_with_no_shell() {
        let argv = holder_argv("h", "net", "img", 1000, 1000, "abc", &seat_b(), 2_000_000_000);
        let tail = &argv[argv.len() - 4..];
        assert_eq!(tail, ["--entrypoint", "sleep", "img", "infinity"]);
        // A shell anywhere in the argv would mean the holder runs something that parses a string.
        assert!(!argv.iter().any(|a| a == "sh" || a == "bash" || a == "-c"), "{argv:?}");
    }

    #[test]
    fn the_holder_is_locked_down_and_labelled_for_reaping() {
        let argv = holder_argv("h", "net", "img", 1000, 1000, "abc", &seat_b(), 2_000_000_000);
        for expected in ["--read-only", "--cap-drop", "ALL", "no-new-privileges"] {
            assert!(argv.iter().any(|a| a == expected), "missing {expected} in {argv:?}");
        }
        assert!(argv.iter().any(|a| a == "ai.maxplayer.netns-holder=abc"), "{argv:?}");
        // The reaper must be able to find what the holder was labelled with, and to tell whose it is.
        let filter = list_holders_argv(&seat_b());
        assert!(filter.iter().any(|a| a == "label=ai.maxplayer.netns-holder"), "{filter:?}");
    }

    /// The holder is stamped with its owning seat at creation. Without this the reap filter has
    /// nothing to match and every holder is unattributable — a reaper that correctly reaps nothing.
    #[test]
    fn the_holder_carries_the_seat_that_created_it() {
        let argv = holder_argv("h", "net", "img", 1000, 1000, "abc", &seat_b(), 2_000_000_000);
        assert!(
            argv.iter().any(|a| a == &format!("{HOLDER_SEAT_LABEL}={}", seat_b())),
            "{argv:?}"
        );
        // The value the creator stamps is the value the reaper filters on — one string, two sites.
        let stamped = format!("{HOLDER_SEAT_LABEL}={}", seat_b());
        assert!(
            list_holders_argv(&seat_b()).iter().any(|a| a == &format!("label={stamped}")),
            "creation label and reap filter must name the same seat"
        );
    }

    #[test]
    fn only_the_sidecar_is_granted_net_admin() {
        let holder = NetnsHolder::adopt("h".into(), DockerCli::system());
        let sidecar = sidecar_argv(&holder, "netfilter");
        assert!(sidecar.windows(2).any(|w| w == ["--cap-add", "NET_ADMIN"]), "{sidecar:?}");
        // …and it still drops everything else first, so the grant is exactly one capability.
        assert!(sidecar.windows(2).any(|w| w == ["--cap-drop", "ALL"]), "{sidecar:?}");
        // The holder must never carry it: it shares its namespace with the job.
        let holder_argv = holder_argv("h", "net", "img", 1000, 1000, "abc", &seat_b(), 2_000_000_000);
        assert!(!holder_argv.iter().any(|a| a == "NET_ADMIN"), "{holder_argv:?}");
    }

    #[test]
    fn the_sidecar_takes_the_plan_on_stdin_and_is_told_nothing_else() {
        let holder = NetnsHolder::adopt("h".into(), DockerCli::system());
        let sidecar = sidecar_argv(&holder, "netfilter");
        assert!(sidecar.iter().any(|a| a == "--interactive"), "no stdin: {sidecar:?}");
        // The image is the last word — no policy is passed as an argument.
        assert_eq!(sidecar.last().map(String::as_str), Some("netfilter"));
    }

    #[test]
    fn every_rendered_rule_becomes_exactly_one_stdin_line() {
        let (stdin, count) = plan_stdin(&policy());
        let lines: Vec<&str> = stdin.lines().collect();
        assert_eq!(lines.len(), count, "the count must be the number of lines the sidecar reads");
        assert!(count > 0, "an empty plan is a refusal, never a pass");
        for line in &lines {
            let binary = line.split_whitespace().next().expect("a rule names its binary");
            assert!(
                binary == Family::V4.binary() || binary == Family::V6.binary(),
                "the sidecar refuses anything else (exit 5): {line}"
            );
            assert!(line.contains("-A OUTPUT"), "in-netns rules append to OUTPUT: {line}");
        }
    }

    #[test]
    fn both_families_reach_the_sidecar_in_one_plan() {
        let (stdin, _) = plan_stdin(&policy());
        assert!(stdin.lines().any(|l| l.starts_with("iptables ")), "no v4 rules");
        assert!(stdin.lines().any(|l| l.starts_with("ip6tables ")), "no v6 rules");
    }

    #[test]
    fn the_gateway_is_asked_of_docker_never_computed() {
        let argv = host_gateway_probe_argv("img", "host.docker.internal");
        // The probe must ask about the alias via host-gateway; a `network inspect` gateway is a
        // DIFFERENT address (measured: 172.21.0.1 for the joined network vs 172.17.0.1 for
        // host-gateway on the same box), and using it would put the pinhole where nothing listens.
        assert!(argv.iter().any(|a| a == "host.docker.internal:host-gateway"), "{argv:?}");
        assert!(!argv.iter().any(|a| a.contains("inspect")), "{argv:?}");
    }

    #[test]
    fn the_probe_output_yields_the_address() {
        let out = "172.17.0.1      STREAM host.docker.internal\n172.17.0.1      DGRAM  host.docker.internal\n";
        assert_eq!(parse_getent_ipv4(out).as_deref(), Some("172.17.0.1"));
        // Negative controls: nothing to parse must not invent an address.
        assert_eq!(parse_getent_ipv4("").as_deref(), None);
        assert_eq!(parse_getent_ipv4("host.docker.internal not found\n").as_deref(), None);
        assert_eq!(parse_getent_ipv4("1.2.3\n").as_deref(), None);
        assert_eq!(parse_getent_ipv4("1.2.3.4.5\n").as_deref(), None);
        assert_eq!(parse_getent_ipv4("999.1.1.1\n").as_deref(), None);
    }

    /// A holder with a job attached is in use; one without may be stale. Within one seat's own
    /// holders, attachment is what separates the two.
    #[test]
    fn only_holders_with_no_job_attached_are_reapable() {
        let busy = "a".repeat(64);
        let idle = "b".repeat(64);
        let mine = vec![
            HolderRecord { id: busy.clone(), seat: Some(seat_b()) },
            HolderRecord { id: idle.clone(), seat: Some(seat_b()) },
        ];
        // One job joined to `busy`, plus containers on ordinary networks that name no holder.
        let modes = format!("bridge\ncontainer:{busy}\nhost\nmx-sandbox-net\n");
        assert_eq!(
            reapable_holders(&mine, &seat_b(), &modes),
            vec![idle],
            "the holder with a job attached must survive"
        );
    }

    /// The positive control: this seat's own holders, nothing attached, all reapable. Without this a
    /// predicate that never matches would look like a careful one.
    #[test]
    fn holders_are_reaped_when_nothing_is_attached() {
        let one = "c".repeat(64);
        let two = "d".repeat(64);
        let mine = vec![
            HolderRecord { id: one.clone(), seat: Some(seat_b()) },
            HolderRecord { id: two.clone(), seat: Some(seat_b()) },
        ];
        assert_eq!(reapable_holders(&mine, &seat_b(), "bridge\nhost\n"), vec![one, two]);
    }

    /// A `container:` mode naming a *different* holder must not protect this one — the comparison is on
    /// the id, and a prefix match or a contains() would confuse the two.
    #[test]
    fn an_attachment_to_another_holder_does_not_protect_this_one() {
        let holder = "e".repeat(64);
        let other = "f".repeat(64);
        let mine = vec![HolderRecord { id: holder.clone(), seat: Some(seat_b()) }];
        let modes = format!("container:{other}\n");
        assert_eq!(reapable_holders(&mine, &seat_b(), &modes), vec![holder]);
    }

    /// The reaper asks for full ids, because a job's network mode names its holder by full id. Comparing
    /// a truncated id against that would never match and would reap every holder, including busy ones.
    #[test]
    fn the_holder_listing_asks_for_untruncated_ids() {
        let argv = list_holders_argv(&seat_b());
        assert!(argv.contains(&"--no-trunc".to_owned()), "{argv:?}");
        assert!(argv.iter().any(|arg| arg == &format!("label={HOLDER_LABEL}")), "{argv:?}");
        // `--quiet` would suppress the seat column the ownership check reads.
        assert!(!argv.contains(&"--quiet".to_owned()), "{argv:?}");
    }

    /// The listing must both narrow to this seat and print the seat back for the Rust-side check.
    /// Asking docker without reading the answer would leave the guard resting on a filter alone.
    #[test]
    fn the_holder_listing_narrows_to_the_seat_and_prints_it_back() {
        let argv = list_holders_argv(&seat_b());
        assert!(
            argv.iter().any(|arg| arg == &format!("label={HOLDER_SEAT_LABEL}={}", seat_b())),
            "the listing must filter to the booting seat: {argv:?}"
        );
        // Written out by hand rather than rebuilt with the same `format!` escaping the code uses: an
        // expectation that borrows the idiom under test agrees with it even when both are wrong. These
        // are the bytes docker must receive as a Go template, read back off a failing run.
        let format = argv.last().expect("a --format template");
        assert_eq!(format, "{{.ID}}\t{{.Label \"ai.maxplayer.netns-holder-seat\"}}");
        // Round-trip: what that template produces is what the parser reads.
        let parsed = parse_holder_listing(&format!("{}\t{}\n", seat_b_holder(), seat_b()));
        assert_eq!(parsed, vec![HolderRecord { id: seat_b_holder(), seat: Some(seat_b()) }]);
    }

    #[test]
    fn the_mode_query_names_every_container_it_was_given() {
        let ids = vec!["one".to_owned(), "two".to_owned()];
        let argv = network_modes_argv(&ids);
        assert_eq!(&argv[argv.len() - 2..], ["one", "two"]);
        assert!(argv.contains(&"{{.HostConfig.NetworkMode}}".to_owned()), "{argv:?}");
    }

    #[test]
    fn the_measured_address_is_what_the_pinhole_names() {
        // The single-source property: whatever `resolve_proxy_host` measures is the string handed to
        // NetPolicy.gateway, so the ACCEPT and the job's base URL cannot drift apart.
        let measured = parse_getent_ipv4("172.17.0.1      STREAM host.docker.internal\n")
            .expect("probe output parses");
        let policy = NetPolicy {
            gateway: measured.clone(),
            proxy_ports: Some(PortRange::new(9000, 9000).expect("valid range")),
            log_connections: false,
            dns_resolvers: Vec::new(),
        };
        let (stdin, _) = plan_stdin(&policy);
        // The pinhole is v4 — the proxy is reached at the namespace's v4 gateway. The v6 plan also
        // carries ACCEPTs, and they are deliberately not pinholes: they are the two neighbour
        // discovery exceptions, which name no host and open no port. Matching on "ACCEPT" alone
        // would count them here and the assertion would be about arithmetic, not about the pinhole.
        let accepts: Vec<&str> = stdin
            .lines()
            .filter(|l| l.starts_with("iptables ") && l.contains("ACCEPT"))
            .collect();
        assert_eq!(accepts.len(), 1, "exactly one v4 pinhole: {accepts:?}");
        assert!(accepts[0].contains(&measured), "the pinhole must name the measured host: {accepts:?}");
        let v6_accepts = stdin
            .lines()
            .filter(|l| l.starts_with("ip6tables ") && l.contains("ACCEPT"))
            .count();
        assert_eq!(v6_accepts, 2, "v6 permits neighbour discovery and nothing else");
    }

    // ── Cancellation custody (F4) ─────────────────────────────────────────────────────────────
    //
    // A cancelled establish must leave nothing running that this process cannot name. These tests
    // check the three properties that make that true without a daemon: the sidecar is addressable,
    // its name is unique to this run, and the holder tracks it for exactly as long as it is alive.

    /// Every container joined to the namespace is named by us. An unnamed sidecar gets a random name
    /// this process never learns, so a cancellation mid-command leaves the one container capable of
    /// pinning the namespace open as the one container nothing can address.
    #[test]
    fn every_sidecar_is_named_so_a_cancelled_one_can_still_be_removed() {
        let holder = NetnsHolder::adopt("maxplayer-netns-abc".into(), DockerCli::system());
        let name = sidecar_name(holder.name(), "iface");
        for argv in [
            sidecar_argv(&holder, "netfilter"),
            readback_argv(holder.name(), "netfilter", Family::V4),
            crate::sandbox_iface::iface_sidecar_argv(holder.name(), "netfilter"),
            crate::sandbox_iface::filter_readback_argv(holder.name(), "netfilter", "eth0"),
            crate::sandbox_iface::link_probe_argv(holder.name(), "netfilter"),
        ] {
            let named = with_container_name(argv, &name).expect("a docker run argv");
            assert!(
                named.windows(2).any(|w| w == ["--name", name.as_str()]),
                "an unnamed joiner cannot be cleaned up: {named:?}"
            );
            // The name goes to the docker client, before the image and its command: appended at the
            // end it would become an argument to the sidecar instead of a flag to `run`.
            let at = named.iter().position(|a| a == "--name").expect("named");
            let image = named.iter().position(|a| a == "netfilter").expect("the image");
            assert!(at < image, "--name must precede the image: {named:?}");
        }
    }

    /// The name must be unique per call. A deterministic sidecar name is a name two concurrent jobs
    /// share, and cleaning up "the" sidecar would then remove a sibling's live container.
    #[test]
    fn sidecar_names_are_unique_per_call_so_cleanup_cannot_hit_a_sibling() {
        let first = sidecar_name("maxplayer-netns-abc", "iface");
        let second = sidecar_name("maxplayer-netns-abc", "iface");
        assert_ne!(first, second, "two joiners of the same holder must not share a name");
        // Each is still attributable to its holder and its purpose, which is what makes an orphan
        // readable to an operator rather than merely unique.
        for name in [&first, &second] {
            assert!(name.starts_with("maxplayer-netns-abc-iface-"), "{name}");
        }
        // Different holders never collide either.
        assert_ne!(
            sidecar_name("maxplayer-netns-abc", "iface"),
            sidecar_name("maxplayer-netns-def", "iface")
        );
    }

    /// Naming is refused rather than misapplied. Splicing `--name` into something that is not a
    /// `docker run` would register a cleanup target that does not exist, and a cleanup target that
    /// does not exist reports success for a container still running.
    #[test]
    fn naming_a_non_run_argv_is_refused() {
        let err = with_container_name(list_all_containers_argv(), "x")
            .expect_err("`docker ps` takes no --name");
        assert!(err.contains("not a `docker run` argv"), "{err}");
        assert!(with_container_name(network_modes_argv(&["a".into()]), "x").is_err());
        // The positive control, so the refusal is not simply "always refuse".
        let holder = NetnsHolder::adopt("h".into(), DockerCli::system());
        assert!(with_container_name(sidecar_argv(&holder, "img"), "x").is_ok());
    }

    /// A joiner is tracked for exactly its command's lifetime: registered before it starts (a
    /// cancellation between registration and start must still leave a cleanup target) and dropped
    /// when it finishes, so a completed sidecar is not removed twice or reported as an orphan.
    #[test]
    fn a_joiner_is_tracked_while_it_runs_and_forgotten_when_it_finishes() {
        let holder = NetnsHolder::adopt("maxplayer-netns-abc".into(), DockerCli::system());
        let tracked = |holder: &NetnsHolder| -> Vec<String> {
            holder.sidecars.lock().expect("registry").clone()
        };
        assert!(tracked(&holder).is_empty(), "nothing is joined before anything runs");

        let mut first = holder.watch_sidecar(sidecar_name(holder.name(), "iface"));
        let mut second = holder.watch_sidecar(sidecar_name(holder.name(), "iface-readback"));
        assert_eq!(tracked(&holder).len(), 2, "both live joiners are cleanup targets");

        // Finishing one deregisters only that one: the other is still running and still owned.
        // `completed` is what makes this finishing rather than cancellation, and only a command
        // that returned may claim it.
        let second_name = second.name.clone();
        second.completed();
        drop(second);
        assert_eq!(tracked(&holder), vec![first.name.clone()], "{second_name} must be forgotten");

        first.completed();
        drop(first);
        assert!(tracked(&holder).is_empty(), "a finished joiner is not an orphan");
    }

    /// A removal that never returns is abandoned on its deadline, killed, and reported as a
    /// failure -- not waited on forever and not reported as a removal that worked.
    ///
    /// This is the property the word "bounded" claimed while the code called `output()`, which has
    /// no timeout: a docker client talking to a wedged daemon blocked the teardown thread for as
    /// long as the daemon stayed wedged. Exercised on a child that is guaranteed not to exit, so
    /// the deadline is the only thing that can end the wait.
    #[test]
    fn a_removal_that_never_returns_is_abandoned_on_its_deadline() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let started = std::time::Instant::now();
        let error = NetnsHolder::wait_bounded(&mut child, std::time::Duration::from_millis(250))
            .expect_err("a child that never exits must hit the deadline");
        let waited = started.elapsed();

        assert!(error.contains("did not finish"), "{error}");
        assert!(
            error.contains("may still exist"),
            "an abandoned removal must be reported as a possible leak, not as success: {error}"
        );
        assert!(waited < std::time::Duration::from_secs(10), "waited {waited:?}");
        // Killed AND reaped, so the bound is real rather than advisory: the child is already gone
        // and this returns its status immediately rather than blocking for the remaining ~59s.
        assert!(
            child.try_wait().expect("reap").is_some(),
            "the abandoned child must be killed, not left running"
        );
    }

    /// A removal that answers promptly is NOT abandoned -- the control that keeps the test above
    /// from passing on a deadline that fires unconditionally.
    #[test]
    fn a_removal_that_returns_is_not_abandoned() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn true");
        let status = NetnsHolder::wait_bounded(&mut child, std::time::Duration::from_secs(10))
            .expect("a child that exits at once must be waited on normally");
        assert!(status.success(), "{status:?}");
    }

    /// A **cancelled** sidecar command leaves its name with the holder.
    ///
    /// The sibling above covers the finishing path. This one covers the path that produced the
    /// defect: the guard was struck from the registry by cancellation itself, so the holder's `Drop`
    /// found an empty list and removed nothing, while the container the blocking docker client had
    /// already created stayed joined to the namespace.
    ///
    /// Cancellation is performed here the way tokio performs it -- the future is polled once, so the
    /// registration exists and the command is in flight, and then the future is dropped. No runtime
    /// and no docker are involved, so this measures the custody rule itself.
    #[test]
    fn a_cancelled_joiner_stays_a_cleanup_target() {
        use std::future::Future as _;

        let holder = NetnsHolder::adopt("maxplayer-netns-cancelled".into(), DockerCli::system());
        let name = sidecar_name(holder.name(), "iface");
        {
            let mut command = Box::pin(async {
                let mut registration = holder.watch_sidecar(name.clone());
                // Stands in for the docker command that never returns before the cancellation.
                std::future::pending::<()>().await;
                registration.completed();
            });
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            assert!(
                command.as_mut().poll(&mut cx).is_pending(),
                "the command must still be in flight when it is cancelled"
            );
            assert_eq!(
                holder.sidecars.lock().expect("registry").len(),
                1,
                "the joiner is registered before its command starts"
            );
        }

        assert_eq!(
            holder.sidecars.lock().expect("registry").clone(),
            vec![name],
            "a cancelled command must leave its container as a cleanup target -- deregistering here \
             is what left an orphan pinning the namespace"
        );
    }

    /// Cleanup reports what happened. "No such container" after a cancelled create is the expected
    /// path and not a failure; anything else is a leak, and must be reported as one rather than
    /// swallowed into a teardown that claims to have destroyed the namespace.
    #[test]
    fn removing_something_that_was_never_created_is_not_a_cleanup_failure() {
        // The holder is adopted before the create is issued precisely so this case exists.
        let name = holder_name("a-job-whose-create-was-cancelled");
        assert!(name.starts_with("maxplayer-netns-"), "{name}");
        // No daemon is touched here; the classification under test is the string one, and it is the
        // only place a "nothing to remove" result is allowed to pass as success.
        assert!(
            NetnsHolder::force_remove_stderr_is_benign("Error: No such container: x"),
            "a container that never existed is not a leak"
        );
        for real in [
            "Error response from daemon: cannot remove a running container",
            "permission denied while trying to connect to the Docker daemon socket",
            "",
        ] {
            assert!(
                !NetnsHolder::force_remove_stderr_is_benign(real),
                "a failed removal must be reported as a leak, not as a teardown: {real:?}"
            );
        }
    }

    /// F4: **the bound itself, exercised.** Every other cancellation test in this module inspects
    /// argv or drives `Drop` by hand; none of them ever let a command run long enough to be
    /// stopped, so the deadline that owns cancellation was asserted only by reading it.
    ///
    /// `sleep 30` under a one-second bound needs no daemon and no docker: the property is that a
    /// command which does not finish is **killed** and the caller gets a failure naming the
    /// deadline — not a hang, and not a success. The elapsed-time assertion is the real one; an
    /// implementation that returned the right error after waiting out the full thirty seconds would
    /// satisfy the string check and still be the bug.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_command_that_outlives_its_deadline_is_killed_and_says_so() {
        let started = std::time::Instant::now();
        let outcome = run_bounded(
            &DockerCli::system(),
            vec!["sleep".to_owned(), "30".to_owned()],
            None,
            std::time::Duration::from_secs(1),
        )
        .await;
        let elapsed = started.elapsed();

        let error = outcome.expect_err("a command past its deadline must not report success");
        assert!(
            error.contains("did not finish within 1s"),
            "the failure must name the deadline it broke: {error}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the bound returned after {elapsed:?} — a deadline that is only reported once the \
             command finishes on its own is not a bound at all"
        );
    }

    /// F4: a program that cannot be started fails **by name**, immediately.
    ///
    /// The path that matters is the one where docker is absent or unexecutable: that must surface as
    /// a named failure rather than as a deadline timeout thirty seconds later, and it must never be
    /// confused with a container that was created.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_program_that_cannot_be_started_fails_by_name() {
        let missing = "maxplayer-no-such-program-exists";
        let error = run_bounded(
            &DockerCli::system(),
            vec![missing.to_owned()],
            None,
            std::time::Duration::from_secs(30),
        )
        .await
        .expect_err("a program that cannot be run must not report success");
        assert!(
            error.contains("could not run") && error.contains(missing),
            "the failure must name the program it could not run: {error}"
        );
    }

    /// F4 residual: the client returning is not the container being gone.
    ///
    /// `run_sidecar` used to call `registration.completed()` after **every** returned result, on the
    /// stated grounds that `docker run --rm` has removed the container by then. That holds for a
    /// client which was reaped with a status — including a nonzero one — and not otherwise. A client
    /// killed on our own deadline, or one that failed before the wait, leaves a container the daemon
    /// may still be creating or running; deregistering it struck the one cleanup target for a
    /// container that outlived its client.
    ///
    /// Both halves are asserted here, because only the pair distinguishes the fix from "never
    /// deregister", which would make every sidecar report a phantom leak.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "current_thread")]
    async fn only_a_reaped_client_may_end_a_sidecars_custody() {
        let (outcome, child_exited) = run_bounded_tracked(
            &DockerCli::system(),
            vec!["sh".to_owned(), "-c".to_owned(), "exit 7".to_owned()],
            None,
            std::time::Duration::from_secs(10),
        )
        .await;
        let error = outcome.expect_err("a nonzero exit is still a failure to the caller");
        assert!(error.contains("exit 7"), "the caller's error must name the code: {error}");
        assert!(
            child_exited,
            "a nonzero exit is a REAPED client: that is a reason to ASK docker whether the \
             container is gone. It is not itself an answer, and custody no longer ends on it \
             alone — see `a_reaped_client_whose_container_is_still_there_keeps_custody`"
        );

        let started = std::time::Instant::now();
        let (outcome, child_exited) = run_bounded_tracked(
            &DockerCli::system(),
            vec!["sleep".to_owned(), "30".to_owned()],
            None,
            std::time::Duration::from_millis(400),
        )
        .await;
        let error = outcome.expect_err("a command past its deadline must not report success");
        assert!(error.contains("did not finish within"), "{error}");
        assert!(
            !child_exited,
            "a client killed on the deadline has shown nothing about its container, so the name \
             must stay a cleanup target"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the deadline must be the thing that returned, not the command finishing"
        );
    }

    /// The custody rule at the site that applies it.
    ///
    /// The sibling above measures the flag; this one measures what `run_sidecar` DOES with it, which
    /// is the part a reviewer cannot take on trust. Written after a negative control showed the
    /// flag test alone stayed green while the decision was reverted to the defective one.
    ///
    /// No docker and no daemon: the argv names a script that ignores its arguments, which is all
    /// `with_container_name` needs (it requires `argv[1] == "run"` and splices the name after it).
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_sidecar_whose_client_was_killed_stays_a_cleanup_target() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("mx-custody-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let slow = dir.join("slow");
        let quick = dir.join("quick");
        std::fs::write(&slow, "#!/bin/sh\nsleep 30\n").expect("write slow");
        std::fs::write(&quick, "#!/bin/sh\nexit 0\n").expect("write quick");
        for path in [&slow, &quick] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let holder = NetnsHolder::adopt("maxplayer-netns-custody".into(), DockerCli::system());

        // A client killed on the deadline: the daemon may still be creating or running the
        // container, so the name has to survive as a cleanup target.
        let killed = run_sidecar_with_deadline(
            &holder,
            "iface",
            vec![slow.to_string_lossy().into_owned(), "run".to_owned()],
            None,
            std::time::Duration::from_millis(400),
        )
        .await;
        assert!(killed.is_err(), "a command past its deadline must not report success");
        assert_eq!(
            holder.sidecars.lock().expect("registry").len(),
            1,
            "a client killed on the deadline never showed its container gone, so its name must \
             stay a cleanup target"
        );

        // A client reaped normally AND docker confirming the container gone: only then may the name
        // be struck. Without this half, "never deregister" would pass the assertion above.
        //
        // The confirmer is injected rather than real. This used to call the production path, which
        // reached a live `docker inspect` from inside an offline unit test: the test passed only
        // because a daemon happened to answer, which is a dependency an offline suite must not have.
        fn confirmed_gone(_client: &DockerCli, _name: &str) -> Option<bool> {
            Some(true)
        }
        run_sidecar_confirmed(
            &holder,
            "iface",
            vec![quick.to_string_lossy().into_owned(), "run".to_owned()],
            None,
            std::time::Duration::from_secs(10),
            confirmed_gone,
        )
        .await
        .expect("a script that exits 0 must succeed");
        assert_eq!(
            holder.sidecars.lock().expect("registry").len(),
            1,
            "a reaped client whose container docker confirms gone must be struck, leaving only the \
             killed one"
        );

        let _ = std::fs::remove_dir_all(&dir);
        std::mem::forget(holder);
    }

    /// The `Err`-path custody failure, reproduced.
    ///
    /// This is the defect the verdict names: the client is reaped — `child_exited` is true, with a
    /// NONZERO exit, exactly the shape a deadline-killed, I/O-failed or refused `docker run` returns
    /// — and the container it named is **still there**. The old rule ended custody on the client's
    /// exit alone and struck the only cleanup target for a live container.
    ///
    /// Hermetic: the confirmer is a stub, so this asserts the DECISION, not a daemon's mood. Revert
    /// the rule to `if child_exited { registration.completed(); }` and this test fails.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_reaped_client_whose_container_is_still_there_keeps_custody() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("mx-err-custody-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let failing = dir.join("failing");
        std::fs::write(&failing, "#!/bin/sh\nexit 7\n").expect("write failing");
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        /// Docker answering "that container is still here".
        fn still_present(_client: &DockerCli, _name: &str) -> Option<bool> {
            Some(false)
        }
        /// Docker unable to answer at all — must be treated exactly like "still here".
        fn cannot_tell(_client: &DockerCli, _name: &str) -> Option<bool> {
            None
        }

        for (confirm, label) in [
            (still_present as ConfirmAbsent, "docker says the container is still there"),
            (cannot_tell as ConfirmAbsent, "docker cannot say whether it is there"),
        ] {
            let holder = NetnsHolder::adopt("maxplayer-netns-err-custody".into(), DockerCli::system());
            let outcome = run_sidecar_confirmed(
                &holder,
                "iface",
                vec![failing.to_string_lossy().into_owned(), "run".to_owned()],
                None,
                std::time::Duration::from_secs(10),
                confirm,
            )
            .await;

            let error = outcome.expect_err("exit 7 is a failure");
            assert!(error.contains("exit 7"), "the caller still sees the real error: {error}");
            assert_eq!(
                holder.sidecars.lock().expect("registry").len(),
                1,
                "the client was REAPED with a nonzero exit, but {label}: custody must be held \
                 until the container is CONFIRMED GONE, or cleanup has no target for a container \
                 that outlived its client"
            );
            std::mem::forget(holder);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A SIGNAL-terminated client is reaped too, and must still not end custody on its own.
    ///
    /// The R3 verdict named this case precisely: `try_wait` yields a status for a signalled child
    /// just as it does for an ordinary exit, so `child_exited` is `true` here, while `code()`
    /// returns `None` and the call reports "killed by a signal". The old comments promised that
    /// signal failures retain custody; the old code did not deliver it, because the flag alone was
    /// allowed to release the name.
    ///
    /// This is the sharpest form of "reaped is not removed": a client killed mid-flight tells us
    /// nothing whatever about whether the daemon created, is running, or removed that container.
    /// Custody is kept unless the daemon itself says the container is gone.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_signal_killed_client_does_not_end_custody_by_itself() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("mx-signal-custody-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let suicide = dir.join("suicide");
        // Kills ITSELF with SIGKILL: reaped with a status, but `code()` is None.
        std::fs::write(&suicide, "#!/bin/sh\nkill -9 $$\n").expect("write suicide");
        std::fs::set_permissions(&suicide, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        fn still_present(_client: &DockerCli, _name: &str) -> Option<bool> {
            Some(false)
        }

        let holder = NetnsHolder::adopt("maxplayer-netns-signal-custody".into(), DockerCli::system());
        let outcome = run_sidecar_confirmed(
            &holder,
            "iface",
            vec![suicide.to_string_lossy().into_owned(), "run".to_owned()],
            None,
            std::time::Duration::from_secs(10),
            still_present,
        )
        .await;

        let error = outcome.expect_err("a signalled client is a failure");
        assert!(
            error.contains("killed by a signal"),
            "this must exercise the signal path, not an ordinary nonzero exit: {error}"
        );
        assert_eq!(
            holder.sidecars.lock().expect("registry").len(),
            1,
            "a signal-killed client proves nothing about the container; custody must be kept"
        );

        std::mem::forget(holder);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── The production path itself ────────────────────────────────────────────────────────────
    //
    // Everything above tests a helper. These two drive `establish` — the function production calls,
    // with its real ordering of adopt, fence, create, apply and cleanup — against a stand-in docker
    // client, because the fault these close is precisely that a fixture was standing in for the
    // production path and could agree with a bug the production path does not survive.

    // The stand-in client is passed to `establish_with` as an ARGUMENT. There is deliberately no
    // lock and no shared cell here: the previous shape installed the client in a process-global,
    // which meant every test that touched this path had to remember to take a mutex, any helper
    // that ran outside one (cleanup from `Drop`, notably) read whatever another test had installed,
    // and the tests below could not run in parallel. Passing it in removes the interference rather
    // than serialising around it.

    /// A stand-in `docker` that answers `establish`'s sequence and records what it was asked.
    ///
    /// Writes the applier's stdin to `stdin.txt` and every removed name to `rm.log`, so a test can
    /// assert on what production actually sent rather than on what it believes production sends.
    #[cfg(feature = "acp")]
    fn stand_in_docker(work: &std::path::Path, create_delay: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let script = work.join("docker");
        let body = r#"#!/bin/sh
WORK="__WORK__"
# The daemon is unreachable: every query fails, and none of them may be read as "absent".
if [ -f "$WORK/daemon-down" ]; then
  echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?" >&2
  exit 1
fi
case "$*" in
  *"--entrypoint getent"*)
    echo "203.0.113.77   STREAM host.docker.internal"
    exit 0
    ;;
  *"inspect --type container"*)
    for a in "$@"; do last="$a"; done
    echo "inspect $last" >> "$WORK/events.log"
    if [ -f "$WORK/present-$last" ]; then
      echo "sha256:deadbeefcafe"
      exit 0
    fi
    # THE RACE, made deterministic: this inspect answers ABSENT, and the container lands the instant
    # after that answer — before any event query the caller makes next. `race-NAME` is consumed so
    # it fires exactly once; `raced-NAME` records that it fired.
    if [ -f "$WORK/race-$last" ]; then
      mv "$WORK/race-$last" "$WORK/present-$last"
      : > "$WORK/raced-$last"
    fi
    echo "Error response from daemon: No such container" >&2
    exit 1
    ;;
  *"rm --force --volumes"*)
    for a in "$@"; do last="$a"; done
    echo "$last" >> "$WORK/rm.log"
    echo "rm $last" >> "$WORK/events.log"
    if [ -f "$WORK/rmfail-$last" ]; then
      echo "Error response from daemon: cannot remove container $last" >&2
      exit 1
    fi
    # A removal that succeeds makes the container ABSENT, exactly as the daemon would: the presence
    # marker is what `inspect` answers from, so a test can assert the container really went away
    # instead of asserting that a removal was merely attempted.
    rm -f "$WORK/present-$last"
    exit 0
    ;;
  *"events --since"*)
    # The daemon's event log, in the production format `ID<TAB>name<TAB>action`: a container under
    # NAME that is present now has a `create` and no `destroy`; one a test recorded as landed and
    # since gone (`landed-NAME`) has both, for the same id. `evhang-NAME` leaves a descendant holding
    # this query's stdout open after the client exits, as a client's child process can, for 20s;
    # when it lets go it records `released-NAME`, so a test can assert ORDER against the hold.
    for a in "$@"; do
      case "$a" in
        container=*)
          n="${a#container=}"
          echo "events $n" >> "$WORK/events.log"
          if [ -f "$WORK/evhang-$n" ]; then
            ( sleep 20; : > "$WORK/released-$n" ) &
            exit 0
          fi
          if [ -f "$WORK/present-$n" ]; then
            printf 'deadbeefcafe\t%s\tcreate\n' "$n"
          fi
          if [ -f "$WORK/landed-$n" ]; then
            printf 'feedfacef00d\t%s\tcreate\nfeedfacef00d\t%s\tdestroy\n' "$n" "$n"
          fi
          # THE OTHER RACE, made deterministic: a container lands the instant after this event query
          # answered — before the caller's next inspect. Consumed so it fires once; `raced-after-
          # events-NAME` records that it did.
          if [ -f "$WORK/land-after-events-$n" ]; then
            mv "$WORK/land-after-events-$n" "$WORK/present-$n"
            : > "$WORK/raced-after-events-$n"
          fi
          ;;
      esac
    done
    exit 0
    ;;
  *--detach*)
    : > "$WORK/creating"
    echo "create-start" >> "$WORK/events.log"
    __DELAY__
    echo "create-end" >> "$WORK/events.log"
    echo deadbeefcafe
    exit 0
    ;;
esac
cat > "$WORK/stdin.txt"
echo 0
exit 0
"#
        .replace("__WORK__", &work.to_string_lossy())
        .replace("__DELAY__", create_delay);
        std::fs::write(&script, body).expect("write stand-in docker");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        script
    }

    #[cfg(feature = "acp")]
    fn stand_in_work_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mx-establish-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("work dir");
        dir
    }

    #[cfg(feature = "acp")]
    fn quick_bounds() -> FenceBounds {
        FenceBounds {
            fast: std::time::Duration::from_millis(10),
            max: std::time::Duration::from_millis(60),
            confirm: std::time::Duration::from_millis(300),
            retain: std::time::Duration::from_millis(400),
            reschedule: std::time::Duration::from_millis(10),
        }
    }

    /// Cleanup owns the JOINERS too, and must confirm each one is really gone.
    ///
    /// `sweep` only LOGS a failed sidecar removal, and confirmation inspected the holder alone. A
    /// sidecar that refused removal and is still running pins the very namespace the holder was
    /// torn down to release — so an owner that ends on holder-absence alone reports a clean release
    /// on top of a container it owns and never looked at. The daemon has to be asked about every
    /// owned name, not just the convenient one.
    #[cfg(feature = "acp")]
    #[test]
    fn cleanup_confirms_every_owned_joiner_is_absent_not_only_the_holder() {
        let work = stand_in_work_dir("joiner-confirm");
        let script = stand_in_docker(&work, "");
        // This sidecar refuses removal AND keeps answering "present": precisely the case that
        // holder-only confirmation reports as clean.
        std::fs::write(work.join("rmfail-side-1"), "").expect("marker");
        std::fs::write(work.join("present-side-1"), "").expect("marker");

        let cleanup = HolderCleanup {
            name: "holder-joiner-confirm".to_owned(),
            joiners: vec!["side-1".to_owned()],
            // Nothing in flight, so settlement is immediate and this test is only about custody.
            creation: std::sync::Arc::new(CreationFence::default()),
            client: DockerCli::stand_in(&script),
            bounds: quick_bounds(),
        };
        cleanup.own_until_settled_or_confirmed();

        let log = std::fs::read_to_string(work.join("events.log")).unwrap_or_default();
        assert!(
            log.contains("inspect side-1"),
            "cleanup ended custody without ever asking the daemon whether the sidecar it owns is \
             gone. Its removal failed and it is still running, pinning the namespace, and this \
             owner reported a clean release anyway. Event log:\n{log}"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// An absence observed while a create is STILL IN FLIGHT is not proof of anything.
    ///
    /// This is success-shaped emptiness: "No such container" reads identically whether the create
    /// never happened or has simply not landed yet. The previous owner waited out its bound, swept,
    /// asked once, got "absent", and returned announcing that *nothing landed* — while the create
    /// it was waiting on was still running and could land immediately afterwards, unowned.
    #[cfg(feature = "acp")]
    #[test]
    fn cleanup_does_not_take_absence_as_proof_while_a_create_is_still_in_flight() {
        let work = stand_in_work_dir("unsettled-confirm");
        let script = stand_in_docker(&work, "");
        let fence = std::sync::Arc::new(CreationFence::default());
        // Held for the whole test and never released: this create NEVER settles.
        let _ticket = fence.begin();

        let cleanup = HolderCleanup {
            name: "holder-unsettled".to_owned(),
            joiners: Vec::new(),
            creation: std::sync::Arc::clone(&fence),
            client: DockerCli::stand_in(&script),
            bounds: quick_bounds(),
        };
        cleanup.own_until_settled_or_confirmed();

        let log = std::fs::read_to_string(work.join("events.log")).unwrap_or_default();
        assert!(
            !log.contains("inspect holder-unsettled"),
            "the create never settled, yet cleanup asked the daemon for an absence answer and ended \
             on it. That answer cannot distinguish \"nothing landed\" from \"has not landed yet\", \
             so resting a clean verdict on it is exactly the orphan this fence exists to prevent. \
             Event log:\n{log}"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// The deadline must bound the WHOLE create flow, stdin included.
    ///
    /// The timer used to start after the plan had already been written to the child. A client that
    /// never reads its stdin fills the pipe and blocks that write forever, so the bound was never
    /// armed and the launch hung with no deadline at all — the precise state in which cancellation
    /// leaves work nobody owns.
    #[cfg(feature = "acp")]
    #[test]
    fn the_deadline_bounds_the_whole_create_flow_including_the_stdin_write() {
        use std::os::unix::fs::PermissionsExt as _;

        let work = stand_in_work_dir("stdin-bound");
        let script = work.join("docker");
        // Never reads stdin, so a large plan fills the pipe and the write blocks.
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").expect("write stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let deadline = std::time::Duration::from_millis(300);
        let mut child_exited = false;
        let started = std::time::Instant::now();
        let outcome = run_bounded_blocking(
            &DockerCli::stand_in(&script),
            vec!["docker".to_owned(), "create".to_owned()],
            Some("x".repeat(4 * 1024 * 1024)),
            deadline,
            started,
            None,
            &mut child_exited,
        );
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the bound never armed: a client that refuses to read its stdin blocked the write for \
             {elapsed:?} against a {deadline:?} deadline. A create with no enforceable bound is a \
             launch that can hang and a container nobody is waiting for."
        );
        assert!(outcome.is_err(), "a client killed on its deadline cannot report success");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A container that lands AFTER the bound is still removed — by an owner that never left.
    ///
    /// This is the ownership hole, and it is not a reporting one: when `max` expired with the
    /// create still running, the owner swept what it could see, printed a leak and RETURNED. The
    /// sweep cannot touch a container that has not appeared yet, so the one case the fence exists
    /// for — a create landing late — ended with no owner at all, and the container stayed up until
    /// a boot reaper happened to find it.
    ///
    /// The assertion is therefore about the CONTAINER, not the log: the stand-in daemon answers
    /// `inspect` from a presence marker and drops that marker when a removal succeeds, so this
    /// passes only if the thing that landed late was actually removed, and only if the removal came
    /// after it landed.
    #[cfg(feature = "acp")]
    #[test]
    fn a_create_that_lands_after_the_bound_is_still_removed_by_its_retained_owner() {
        use std::io::Write as _;

        let work = stand_in_work_dir("late-custody");
        let script = stand_in_docker(&work, "");
        let fence = std::sync::Arc::new(CreationFence::default());
        let ticket = fence.begin();

        // The create lands strictly AFTER the owner's first sweep, and only then settles.
        //
        // Ordered on the observed sweep rather than on a sleep, deliberately: a wall-clock delay
        // makes this test a race, and a lucky schedule where the pre-landing sweep happens to run
        // late lets a dropped-custody build pass. Waiting for the removal to appear in the stand-in
        // daemon's log pins the one ordering that matters — the owner has already swept, and the
        // container arrives afterwards, which is exactly the case a sweep cannot cover.
        let landing = work.clone();
        let lander = std::thread::spawn(move || {
            let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < give_up {
                let swept = std::fs::read_to_string(landing.join("rm.log"))
                    .map(|log| log.contains("holder-late"))
                    .unwrap_or(false);
                if swept {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::fs::write(landing.join("present-holder-late"), "").expect("presence marker");
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(landing.join("events.log"))
                .expect("events log");
            writeln!(log, "landed holder-late").expect("events log");
            drop(ticket);
        });

        let cleanup = HolderCleanup {
            name: "holder-late".to_owned(),
            joiners: Vec::new(),
            creation: std::sync::Arc::clone(&fence),
            client: DockerCli::stand_in(&script),
            bounds: FenceBounds {
                retain: std::time::Duration::from_secs(5),
                ..quick_bounds()
            },
        };
        cleanup.own_until_settled_or_confirmed();
        lander.join().expect("lander");

        assert!(
            !work.join("present-holder-late").exists(),
            "the container landed after the owner's bound and is STILL RUNNING: custody was \
             dropped at `max` while the create was in flight, so nothing removed what arrived \
             afterwards. An owner that stops owning at a timeout is how this fence manufactures the \
             orphan it exists to prevent."
        );
        let log = std::fs::read_to_string(work.join("events.log")).unwrap_or_default();
        let landed = log
            .lines()
            .position(|line| line.contains("landed holder-late"))
            .expect("the stand-in create never landed, so this test proved nothing");
        assert!(
            log.lines().skip(landed + 1).any(|line| line.contains("rm holder-late")),
            "the only removal issued for this holder happened BEFORE it existed — a removal aimed \
             at a container that had not landed yet, which the daemon answers 'No such container' \
             and which proves nothing. Event log:\n{log}"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A create still in flight when every bound expires is TRANSFERRED, not released.
    ///
    /// This is the termination path, and it is the one a longer wait cannot reach. The bounded
    /// owner waited `max`, then kept the job a further `retain`, and then — with the create still
    /// running — printed a leak and RETURNED. A container landing after that had no owner at all,
    /// which is the same hole the retained window was added to close, one window further out.
    ///
    /// Ownership is asserted here as a FACT ABOUT THE FENCE, not as a sentence in a log: the fence
    /// is holding somebody's cleanup, and when the create finally settles that owner removes the
    /// container that landed.
    #[cfg(feature = "acp")]
    #[test]
    fn a_create_still_in_flight_at_every_bound_is_transferred_to_a_retained_owner_that_removes_it() {
        let work = stand_in_work_dir("retained-transfer");
        let script = stand_in_docker(&work, "");
        let fence = std::sync::Arc::new(CreationFence::default());
        // Taken and NOT released: this create is still in flight through every bound below.
        let ticket = fence.begin();

        let cleanup = HolderCleanup {
            name: "holder-transfer".to_owned(),
            joiners: Vec::new(),
            creation: std::sync::Arc::clone(&fence),
            client: DockerCli::stand_in(&script),
            bounds: quick_bounds(),
        };
        cleanup.own_until_settled_or_confirmed();

        // The bounded owner is finished. Responsibility must NOT have ended with it.
        assert!(
            fence.holds_retained_owner(),
            "the owner reached its last bound with the create still in flight and simply returned, \
             so nothing is responsible for a container that has not landed yet. Another window \
             would only move this same edge; the job has to belong to somebody once the wait is \
             over."
        );

        // The create lands LONG after every bound expired, and only now settles.
        std::fs::write(work.join("present-holder-transfer"), "").expect("presence marker");
        drop(ticket);

        assert!(
            !work.join("present-holder-transfer").exists(),
            "the container landed after every bound expired and is STILL RUNNING. The retained \
             owner either never ran or never removed it — either way this is the orphan the fence \
             exists to prevent, arriving exactly where the bounded owner stopped looking."
        );
        assert!(
            !fence.holds_retained_owner(),
            "the retained owner was never consumed, so the handoff did not actually run on \
             settlement"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A name the daemon never confirmed absent stays OWNED.
    ///
    /// The removal was issued and the daemon would not say it was gone. That used to end in a log
    /// line and a return: the names were named, then let go, which downstream is indistinguishable
    /// from a clean release. An unconfirmed name is a container that may well still be running, so
    /// ownership of it has to survive the failure to confirm it.
    #[cfg(feature = "acp")]
    #[test]
    fn names_that_could_not_be_confirmed_absent_stay_owned_rather_than_released() {
        let work = stand_in_work_dir("confirm-failed");
        let script = stand_in_docker(&work, "");
        // The container is present, and every removal against it FAILS, so the daemon keeps
        // answering that it is still there and confirmation cannot succeed.
        std::fs::write(work.join("present-holder-unconfirmed"), "").expect("presence marker");
        std::fs::write(work.join("rmfail-holder-unconfirmed"), "").expect("rm failure marker");
        let fence = std::sync::Arc::new(CreationFence::default());

        let cleanup = HolderCleanup {
            name: "holder-unconfirmed".to_owned(),
            joiners: Vec::new(),
            creation: std::sync::Arc::clone(&fence),
            client: DockerCli::stand_in(&script),
            bounds: quick_bounds(),
        };
        cleanup.own_until_settled_or_confirmed();

        assert!(
            work.join("present-holder-unconfirmed").exists(),
            "the fixture removed the container after all, so this test proved nothing about \
             unconfirmed names"
        );
        assert!(
            fence.holds_retained_owner(),
            "the daemon never confirmed this name absent and the owner RELEASED it anyway. A \
             container that could not be confirmed gone is one that may still be running, and \
             naming it in a log is not the same as still owning it."
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A CREATE THAT SETTLED BEFORE CLEANUP REGISTERED still gets its container removed.
    ///
    /// This is the ordering the previous version lost outright: the bounded wait times out, the
    /// last ticket drops and finds an empty slot, and only THEN does cleanup install its callback.
    /// No later drop exists to run it, so the job sat in the slot until the fence died. The earlier
    /// test could not catch it because it deliberately held the last ticket alive across
    /// registration — the one ordering in which the bug cannot occur.
    ///
    /// What is asserted is the CONTAINER, not the slot: the presence marker the stand-in answers
    /// `inspect` from is gone, and the removal is in the daemon's own log.
    #[cfg(feature = "acp")]
    #[test]
    fn a_create_that_settled_before_cleanup_registered_is_still_removed() {
        let work = stand_in_work_dir("settle-before-register");
        let script = stand_in_docker(&work, "");
        std::fs::write(work.join("present-holder-raced"), "").expect("presence marker");
        let fence = std::sync::Arc::new(CreationFence::default());

        // The create ENDS FIRST: this is the last ticket, and it drops while the slot is empty.
        drop(fence.begin());

        // Only now does the bounded owner hand its job over — to a fence with nothing left in
        // flight that could ever fire it.
        fence.take_custody(retained_removal(
            "holder-raced".to_owned(),
            Vec::new(),
            DockerCli::stand_in(&script),
            quick_bounds(),
        ));

        assert!(
            !work.join("present-holder-raced").exists(),
            "the create settled BEFORE cleanup registered, nothing ever ran the handed-over job, \
             and the container is still there. A registration that lands after the last ticket \
             drop has to run the job, not park it where no event can reach it."
        );
        let removals = std::fs::read_to_string(work.join("rm.log")).unwrap_or_default();
        assert!(
            removals.contains("holder-raced"),
            "no removal was ever issued for a job handed to an already-settled fence: {removals:?}"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// THE LAST `Arc` TO THE FENCE GOING AWAY RUNS THE JOB — it does not discard it.
    ///
    /// The retained job is a `FnOnce`, and a `FnOnce` that is merely dropped does nothing at all.
    /// The closure deliberately keeps no `Arc` back to its own fence (that would be a cycle, and
    /// the fence would never be destroyed at all), so nothing held the slot alive: once the holder
    /// and every ticket were gone, destruction threw the cleanup away in silence — the one outcome
    /// downstream cannot tell apart from never having owned the container.
    ///
    /// The container is what is inspected, after every owner the test holds is gone.
    #[cfg(feature = "acp")]
    #[test]
    fn cleanup_survives_the_loss_of_every_arc_to_the_fence_and_still_removes() {
        let work = stand_in_work_dir("last-arc");
        let script = stand_in_docker(&work, "");
        std::fs::write(work.join("present-holder-lastarc"), "").expect("presence marker");
        // Removal FAILS at first, so the job cannot discharge and stays owed in the slot — the only
        // state in which a fence can be destroyed while still holding cleanup.
        std::fs::write(work.join("rmfail-holder-lastarc"), "").expect("rm failure marker");
        let fence = std::sync::Arc::new(CreationFence::default());

        let cleanup = HolderCleanup {
            name: "holder-lastarc".to_owned(),
            joiners: Vec::new(),
            creation: std::sync::Arc::clone(&fence),
            client: DockerCli::stand_in(&script),
            bounds: quick_bounds(),
        };
        cleanup.own_until_settled_or_confirmed();
        assert!(
            work.join("present-holder-lastarc").exists(),
            "the fixture removed the container while removals were supposed to fail, so this test \
             proved nothing about destruction"
        );

        // The daemon stops refusing: from here a removal would genuinely succeed.
        std::fs::remove_file(work.join("rmfail-holder-lastarc")).expect("clear the rm failure");
        // EVERY other owner is gone — the cleanup consumed itself — so this is the last `Arc`.
        assert_eq!(
            std::sync::Arc::strong_count(&fence),
            1,
            "this test is only meaningful while it holds the LAST Arc to the fence"
        );
        drop(fence);

        assert!(
            !work.join("present-holder-lastarc").exists(),
            "the last Arc to the fence was dropped while it still held cleanup, and the job went \
             with it: the container is STILL PRESENT. Destruction is the final moment this process \
             can act on a container it owns, so it has to act rather than drop a live obligation."
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A NAME THAT FAILED CONFIRMATION IS DISCHARGED LATER, BY THE OWNER THAT KEPT IT.
    ///
    /// Not "the slot is occupied" — that is the code's opinion of itself, and it reads the same
    /// whether the owner is alive or inert. The claim under test is that the retained owner is
    /// LIVE: when the container genuinely goes away later, this owner is what notices, and it stops
    /// owing only then. Nothing notifies it; it has to look.
    #[cfg(feature = "acp")]
    #[test]
    fn a_name_that_failed_confirmation_is_discharged_by_its_owner_on_the_later_real_removal() {
        let work = stand_in_work_dir("confirm-later");
        let script = stand_in_docker(&work, "");
        std::fs::write(work.join("present-holder-later"), "").expect("presence marker");
        std::fs::write(work.join("rmfail-holder-later"), "").expect("rm failure marker");
        let fence = std::sync::Arc::new(CreationFence::default());

        let cleanup = HolderCleanup {
            name: "holder-later".to_owned(),
            joiners: Vec::new(),
            creation: std::sync::Arc::clone(&fence),
            client: DockerCli::stand_in(&script),
            bounds: quick_bounds(),
        };
        cleanup.own_until_settled_or_confirmed();
        let looked_before = std::fs::read_to_string(work.join("events.log"))
            .unwrap_or_default()
            .lines()
            .count();
        assert!(
            work.join("present-holder-later").exists(),
            "the fixture confirmed the name absent after all, so nothing was left owed"
        );

        // LATER, the container genuinely goes away: the daemon finally reaps what those failed
        // removals were about, and removals start working again.
        std::fs::remove_file(work.join("present-holder-later")).expect("the container goes away");
        std::fs::remove_file(work.join("rmfail-holder-later")).expect("removals work again");

        // A later create settles on this holder's fence — the event the owner was kept for.
        drop(fence.begin());

        let looked_after = std::fs::read_to_string(work.join("events.log"))
            .unwrap_or_default()
            .lines()
            .count();
        assert!(
            looked_after > looked_before,
            "the retained owner never ran again when a later create settled: it was not a live \
             owner, only a flag recording that something had once gone wrong"
        );
        assert!(
            !fence.holds_retained_owner(),
            "the name is genuinely absent now and its owner did look, yet the job is still owed — \
             an owner that cannot discharge on the real removal never ends"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    // ---- Bob-renewal round 1: custody that does not end while the process lives ----------------
    //
    // Every gate below asserts on OWNERSHIP STRUCTURE (what the supervisor holds), on SCHEDULING
    // (attempts that actually ran, counted by the supervisor and by the daemon's `rm.log`) and on
    // CONTAINER STATE (the presence marker the stand-in daemon answers `inspect` from). None of them
    // reads a log string or a retained-slot boolean as its claim. Each waits on the supervisor's own
    // condition variable — a fact, not a sleep.

    fn rm_log_count(work: &std::path::Path, name: &str) -> usize {
        std::fs::read_to_string(work.join("rm.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == name)
            .count()
    }

    /// Kill a real create client at its bound so the daemon's answer is never read, exactly as
    /// production does it — through `run_bounded_blocking`, not by calling the recording method.
    #[cfg(feature = "acp")]
    fn issue_unanswered_create(client: &DockerCli, fence: &std::sync::Arc<CreationFence>, name: &str) {
        let mut child_exited = false;
        let outcome = run_bounded_blocking(
            client,
            vec![
                "docker".to_owned(),
                "run".to_owned(),
                "--detach".to_owned(),
                "--name".to_owned(),
                name.to_owned(),
                "alpine".to_owned(),
            ],
            None,
            std::time::Duration::from_millis(50),
            std::time::Instant::now(),
            Some(fence),
            &mut child_exited,
        );
        assert!(outcome.is_err(), "the fixture's create finished inside the bound: {outcome:?}");
        assert!(!child_exited, "the client was reaped with a status, so its answer WAS read");
    }

    /// MORE THAN THREE FAILURES, EVERY ORIGINAL REFERENCE GONE, THEN THE DAEMON RECOVERS.
    ///
    /// The R3 fence ran three destructor attempts and then printed LEAKED and let the owner die.
    /// Here the daemon refuses removal through the sweep, the settled run, all three destructor
    /// rounds and at least two more scheduled attempts after the fence no longer exists — and the
    /// names are still owned, by the supervisor, with attempts still being made. When the daemon
    /// finally accepts, the holder AND its joiner are removed and confirmed, and nothing is owed.
    #[cfg(feature = "acp")]
    #[test]
    fn custody_survives_more_than_three_failed_attempts_after_every_original_reference_is_gone() {
        let work = stand_in_work_dir("supervisor-handoff");
        let script = stand_in_docker(&work, "");
        for name in ["holder-sup", "joiner-sup"] {
            std::fs::write(work.join(format!("present-{name}")), "").expect("presence marker");
            std::fs::write(work.join(format!("rmfail-{name}")), "").expect("rm failure marker");
        }
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        let cleanup = HolderCleanup {
            name: "holder-sup".to_owned(),
            joiners: vec!["joiner-sup".to_owned()],
            creation: std::sync::Arc::clone(&fence),
            client: DockerCli::stand_in(&script),
            bounds: quick_bounds(),
        };
        cleanup.own_until_settled_or_confirmed();
        let mut left = fence.retained_names();
        left.iter_mut().for_each(|names| names.sort());
        assert_eq!(
            left,
            vec![vec!["holder-sup".to_owned(), "joiner-sup".to_owned()]],
            "the bounded owner did not leave its job with the fence"
        );

        // EVERY original reference goes: the cleanup consumed itself, and this is the last Arc.
        assert_eq!(std::sync::Arc::strong_count(&fence), 1);
        drop(fence);

        // BEFORE RECOVERY: ownership is live, in the supervisor, over both names.
        assert!(
            supervisor.owns("holder-sup") && supervisor.owns("joiner-sup"),
            "after the fence was destroyed nothing owned the names: {:?}",
            supervisor.outstanding()
        );
        // ...and it is SCHEDULED: attempts keep running with no Arc left anywhere, and they fail.
        assert!(
            supervisor.wait_until_attempts_at_least(2, std::time::Duration::from_secs(10)),
            "the supervisor never ran a second attempt: it holds the names but nothing wakes it"
        );
        let refused = rm_log_count(&work, "holder-sup");
        assert!(
            refused > 3,
            "only {refused} removal attempts reached the daemon; this gate requires more than \
             three failures before recovery"
        );
        assert!(supervisor.owns("holder-sup") && supervisor.owns("joiner-sup"));
        assert!(
            work.join("present-holder-sup").exists() && work.join("present-joiner-sup").exists(),
            "the fixture removed a container while removals were supposed to be refused"
        );

        // THE DAEMON RECOVERS.
        for name in ["holder-sup", "joiner-sup"] {
            std::fs::remove_file(work.join(format!("rmfail-{name}"))).expect("clear refusal");
        }
        assert!(
            supervisor.wait_until_idle(std::time::Duration::from_secs(10)),
            "the daemon accepts removals again but the supervisor never discharged: {:?}",
            supervisor.outstanding()
        );
        assert!(!work.join("present-holder-sup").exists(), "the holder is STILL PRESENT");
        assert!(!work.join("present-joiner-sup").exists(), "the joiner is STILL PRESENT");
        assert!(!supervisor.owns("holder-sup") && !supervisor.owns("joiner-sup"));
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A CREATE THE DAEMON NEVER ANSWERED IS WATCHED, AND CAUGHT WHEN IT LANDS LATE.
    ///
    /// Local completion in full: the create clients for the holder AND a joiner are killed at their
    /// bound, every ticket is released, the fence settles, and an inspect says both names are absent.
    /// R3 would have released custody on that absence. Here both names are owned by the supervisor,
    /// which looks and keeps them while they are absent, and when the daemon lands them AFTER all of
    /// that, both are removed and confirmed gone.
    #[cfg(feature = "acp")]
    #[test]
    fn a_create_the_daemon_never_answered_is_watched_and_removed_when_it_lands_late() {
        let work = stand_in_work_dir("unanswered");
        // The stand-in's create takes far longer than the bound, so the client is killed mid-request.
        let script = stand_in_docker(&work, "sleep 5");
        let client = DockerCli::stand_in(&script);
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        let tickets: Vec<CreationTicket> = ["holder-unans", "joiner-unans"]
            .into_iter()
            .map(|name| {
                let ticket = fence.begin();
                issue_unanswered_create(&client, &fence, name);
                ticket
            })
            .collect();
        // LOCAL COMPLETION: every ticket released, the fence settled.
        drop(tickets);
        assert!(fence.wait_until_settled(std::time::Duration::ZERO));
        // INITIAL ABSENCE: the daemon has not applied either create yet.
        assert_eq!(container_is_absent(&client, "holder-unans"), Some(true));
        assert_eq!(container_is_absent(&client, "joiner-unans"), Some(true));

        // Neither local completion nor absence released custody.
        assert!(
            supervisor.owns("holder-unans") && supervisor.owns("joiner-unans"),
            "an unanswered create was released on local completion: {:?}",
            supervisor.outstanding()
        );
        // The watch LOOKS while they are absent, and keeps them: absence is not an answer.
        assert!(supervisor.wait_until_attempts_at_least(2, std::time::Duration::from_secs(10)));
        assert!(
            supervisor.owns("holder-unans") && supervisor.owns("joiner-unans"),
            "an absent inspect was taken as proof the create will never land"
        );

        // THE DAEMON LANDS BOTH — after local completion, after initial absence.
        std::fs::write(work.join("present-holder-unans"), "").expect("the holder lands");
        std::fs::write(work.join("present-joiner-unans"), "").expect("the joiner lands");

        assert!(
            supervisor.wait_until_idle(std::time::Duration::from_secs(10)),
            "late-landing containers were never reconciled: {:?}",
            supervisor.outstanding()
        );
        assert!(!work.join("present-holder-unans").exists(), "the late holder is STILL PRESENT");
        assert!(!work.join("present-joiner-unans").exists(), "the late joiner is STILL PRESENT");
        assert_eq!(rm_log_count(&work, "holder-unans"), 1);
        assert_eq!(rm_log_count(&work, "joiner-unans"), 1);
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A DAEMON THAT CANNOT BE QUERIED KEEPS EVERY NAME OWNED. Query failure is never absence.
    ///
    /// Both kinds of obligation, with the daemon unreachable: a watched name whose inspect and event
    /// queries fail, and a removal whose `rm` and confirming inspect fail. Neither is discharged. When
    /// the daemon comes back — with the watched create having landed meanwhile — both are removed.
    #[cfg(feature = "acp")]
    #[test]
    fn a_daemon_that_cannot_be_queried_keeps_every_name_owned() {
        let work = stand_in_work_dir("daemon-down");
        let script = stand_in_docker(&work, "sleep 5");
        let client = DockerCli::stand_in(&script);
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        // An unanswered create, and a present container whose removal is owed.
        let ticket = fence.begin();
        issue_unanswered_create(&client, &fence, "holder-down");
        std::fs::write(work.join("present-holder-owed"), "").expect("presence marker");

        // The daemon goes away before any of it is looked at.
        std::fs::write(work.join("daemon-down"), "").expect("daemon down");
        drop(ticket);
        let cleanup = HolderCleanup {
            name: "holder-owed".to_owned(),
            joiners: Vec::new(),
            creation: std::sync::Arc::clone(&fence),
            client: client.clone(),
            bounds: quick_bounds(),
        };
        cleanup.own_until_settled_or_confirmed();
        assert_eq!(std::sync::Arc::strong_count(&fence), 1);
        drop(fence);

        assert!(supervisor.wait_until_attempts_at_least(3, std::time::Duration::from_secs(10)));
        assert!(
            supervisor.owns("holder-down") && supervisor.owns("holder-owed"),
            "a failed query was counted as absence: {:?}",
            supervisor.outstanding()
        );
        assert!(work.join("present-holder-owed").exists());

        // The daemon returns, and the unanswered create landed while it was unreachable.
        std::fs::write(work.join("present-holder-down"), "").expect("the create landed");
        std::fs::remove_file(work.join("daemon-down")).expect("daemon back");

        assert!(
            supervisor.wait_until_idle(std::time::Duration::from_secs(10)),
            "the daemon is back but names are still owed: {:?}",
            supervisor.outstanding()
        );
        assert!(!work.join("present-holder-down").exists(), "the landed create is STILL PRESENT");
        assert!(!work.join("present-holder-owed").exists(), "the owed removal is STILL PRESENT");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// TWO JOBS ON ONE FENCE ARE TWO OBLIGATIONS. Neither replaces, neither is dropped.
    ///
    /// R3's slot held one job: `register` assigned over an occupant and `run_and_keep_if_still_owed`
    /// named and dropped a still-owed job when the slot was taken. Here two jobs are registered while
    /// a create is in flight, both fail confirmation at settlement, both are still held, and both are
    /// discharged when the daemon accepts.
    #[cfg(feature = "acp")]
    #[test]
    fn two_jobs_owed_on_one_fence_are_both_kept_and_both_discharged() {
        let work = stand_in_work_dir("collision");
        let script = stand_in_docker(&work, "");
        let client = DockerCli::stand_in(&script);
        for name in ["holder-a", "holder-b"] {
            std::fs::write(work.join(format!("present-{name}")), "").expect("presence marker");
            std::fs::write(work.join(format!("rmfail-{name}")), "").expect("rm failure marker");
        }
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        // A create is in flight, so both registrations are RETAINED for the settlement.
        let ticket = fence.begin();
        let first = fence.register(retained_removal(
            "holder-a".to_owned(),
            vec!["holder-a".to_owned()],
            client.clone(),
            quick_bounds(),
        ));
        let second = fence.register(retained_removal(
            "holder-b".to_owned(),
            vec!["holder-b".to_owned()],
            client.clone(),
            quick_bounds(),
        ));
        assert!(matches!(first, Registration::Retained) && matches!(second, Registration::Retained));
        assert_eq!(
            fence.retained_names(),
            vec![vec!["holder-a".to_owned()], vec!["holder-b".to_owned()]],
            "the second registration replaced the first"
        );

        // Settlement runs both; the daemon refuses both; both must STILL be held.
        drop(ticket);
        let mut held = fence.retained_names();
        held.sort();
        assert_eq!(
            held,
            vec![vec!["holder-a".to_owned()], vec!["holder-b".to_owned()]],
            "a still-owed job was dropped or overwritten at settlement"
        );
        assert!(work.join("present-holder-a").exists() && work.join("present-holder-b").exists());

        // The daemon accepts; a later settlement runs both; both are gone and nothing is owed.
        for name in ["holder-a", "holder-b"] {
            std::fs::remove_file(work.join(format!("rmfail-{name}"))).expect("clear refusal");
        }
        drop(fence.begin());
        assert!(!work.join("present-holder-a").exists(), "holder-a is STILL PRESENT");
        assert!(!work.join("present-holder-b").exists(), "holder-b is STILL PRESENT");
        assert!(!fence.holds_retained_owner());
        assert!(supervisor.outstanding().is_empty());
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A WATCHED NAME THAT LANDED AND IS ALREADY GONE IS DISCHARGED ON THE DAEMON'S EVENT LOG.
    ///
    /// This is the one observation that lets an ABSENT watched name end: the daemon's own record that
    /// a container under exactly that name existed since the request. Until that record appears the
    /// name is kept; when it appears the name is discharged without any removal being issued.
    #[cfg(feature = "acp")]
    #[test]
    fn a_watched_name_that_landed_and_was_already_removed_is_discharged_on_the_event_log() {
        let work = stand_in_work_dir("landed-gone");
        let script = stand_in_docker(&work, "sleep 5");
        let client = DockerCli::stand_in(&script);
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        let ticket = fence.begin();
        issue_unanswered_create(&client, &fence, "holder-lg");
        drop(ticket);
        assert!(supervisor.wait_until_attempts_at_least(2, std::time::Duration::from_secs(10)));
        assert!(supervisor.owns("holder-lg"), "kept while absent with no daemon record of it");

        // The daemon's event log now shows the create ran and the container has since gone.
        std::fs::write(work.join("landed-holder-lg"), "").expect("event record");

        assert!(
            supervisor.wait_until_idle(std::time::Duration::from_secs(10)),
            "the daemon recorded the container's life, yet the name is still owed: {:?}",
            supervisor.outstanding()
        );
        assert_eq!(rm_log_count(&work, "holder-lg"), 0, "nothing was there to remove");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[cfg(feature = "acp")]
    #[test]
    fn only_a_named_create_is_something_to_watch() {
        let owned = |argv: &[&str]| container_named_by(&argv.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        assert_eq!(owned(&["run", "--detach", "--name", "x", "alpine"]), Some("x".to_owned()));
        assert_eq!(owned(&["create", "--name=y", "alpine"]), Some("y".to_owned()));
        assert_eq!(owned(&["run", "--rm", "alpine"]), None);
        assert_eq!(owned(&["rm", "--force", "--volumes", "--name"]), None);
    }

    /// F2: A CLIENT THAT ENDED IS "ANSWERED" ON POSITIVE PROOF ONLY. No list of lost-connection
    /// strings decides it, and text nobody listed is NOT a refusal.
    ///
    /// The version this replaces recognised eight substrings as "lost the daemon" and read every
    /// other nonzero stderr as the daemon refusing. A new client version's wording, a proxy's error,
    /// a truncated line — anything off the list — released custody over a request that may have been
    /// applied. Every row below that is not one of the three proofs must come back `false`.
    #[cfg(feature = "acp")]
    #[test]
    fn a_client_that_ended_is_answered_on_positive_proof_only_never_by_whitelist_inference() {
        let run = Some("run");
        let create = Some("create");
        let daemon = "docker: Error response from daemon: Conflict. The container name is already in use";
        let lost = "error during connect: Post \"http://%2Fvar%2Frun%2Fdocker.sock/v1.47/containers/create\": EOF";
        let unlisted = "docker: dial unix /var/run/docker.sock: connect: the client wrote this in a wording nobody listed";

        // The three proofs.
        assert!(daemon_answered(run, Some(0), None), "exit 0 is the daemon's acceptance");
        assert!(daemon_answered(run, Some(0), Some("")), "exit 0 with an empty stderr too");
        assert!(daemon_answered(run, Some(125), Some(daemon)), "the daemon's own refusal text");
        assert!(daemon_answered(create, Some(125), Some(daemon)), "for `create` as well");
        assert!(daemon_answered(run, Some(1), None), "the contained command ran and exited 1");
        assert!(daemon_answered(run, Some(127), Some("")), "126/127: the container WAS created");

        // Everything else is unknown — including text that is not on any list.
        assert!(!daemon_answered(run, Some(125), Some(lost)), "a lost connection is unknown");
        assert!(
            !daemon_answered(run, Some(125), Some(unlisted)),
            "text that matches no known signature was read as a REFUSAL: that is inference from a \
             whitelist, in the direction that releases custody"
        );
        assert!(!daemon_answered(run, Some(125), Some("")), "exit 125 that said nothing");
        assert!(!daemon_answered(run, Some(125), None), "exit 125 with stderr never read to EOF");
        assert!(!daemon_answered(create, Some(1), None), "`create` has no contained command to exit 1");
        assert!(!daemon_answered(create, Some(1), Some(unlisted)));
        assert!(!daemon_answered(run, None, Some(daemon)), "a signal ends the client, not the request");
        assert!(!daemon_answered(create, None, None));
    }

    /// F1: LIFECYCLE EVIDENCE IS PAIRED BY CONTAINER ID under the exact name. A `create` without its
    /// `destroy` is a live container, whatever else the log shows.
    #[cfg(feature = "acp")]
    #[test]
    fn lifecycle_evidence_pairs_create_and_destroy_by_container_id_under_the_exact_name() {
        use Lifecycle::{Completed, Landed, NoRecord};
        let name = "mx-netns-job";
        assert_eq!(lifecycle_from_events("", name), NoRecord);
        assert_eq!(lifecycle_from_events("aaa\tmx-netns-job\tcreate\n", name), Landed);
        assert_eq!(
            lifecycle_from_events("aaa\tmx-netns-job\tcreate\naaa\tmx-netns-job\tstart\n", name),
            Landed,
            "start is not an end"
        );
        assert_eq!(
            lifecycle_from_events(
                "aaa\tmx-netns-job\tcreate\naaa\tmx-netns-job\tdie\naaa\tmx-netns-job\tdestroy\n",
                name
            ),
            Completed
        );
        assert_eq!(
            lifecycle_from_events(
                "aaa\tmx-netns-job\tcreate\naaa\tmx-netns-job\tdestroy\nbbb\tmx-netns-job\tcreate\n",
                name
            ),
            Landed,
            "one finished lifecycle does not excuse a second container still live under the name"
        );
        assert_eq!(
            lifecycle_from_events("aaa\tmx-netns-job\tcreate\nbbb\tmx-netns-job\tdestroy\n", name),
            Landed,
            "a destroy of a DIFFERENT id does not end this one"
        );
        assert_eq!(
            lifecycle_from_events("ccc\tmx-netns-job\tdestroy\n", name),
            NoRecord,
            "a destroy alone is a container created before the request, never this request's"
        );
        assert_eq!(
            lifecycle_from_events("aaa\tmx-netns-job-2\tcreate\naaa\tmx-netns-job-2\tdestroy\n", name),
            NoRecord,
            "the filter matches prefixes; the name field is checked exactly"
        );
    }

    // ---- Round-2 gates: F1..F4 ------------------------------------------------------------------

    /// F1: ABSENT INSPECT, THEN A LANDING, THEN AN EVENT — and the live container is NOT discharged.
    ///
    /// The stand-in makes the race deterministic: the watch's inspect answers absent and the container
    /// lands the instant after (`race-NAME` becomes `present-NAME` inside that inspect). The event
    /// query the watch makes next therefore shows a `create` under the name — exactly the evidence
    /// the previous version discharged on, over a running container. Here the name must stay owed
    /// through that attempt, and the NEXT attempt must find the container present, remove it and
    /// confirm it gone. The claim is on container state and the fixture's own record of the race
    /// having fired, not on a log line.
    #[cfg(feature = "acp")]
    #[test]
    fn an_absent_inspect_then_a_landing_then_an_event_does_not_discharge_a_live_container() {
        let work = stand_in_work_dir("landing-race");
        let script = stand_in_docker(&work, "sleep 5");
        let client = DockerCli::stand_in(&script);
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        // Armed BEFORE the watch can run: the very first inspect is the one that races.
        std::fs::write(work.join("race-holder-race"), "").expect("race marker");
        let ticket = fence.begin();
        issue_unanswered_create(&client, &fence, "holder-race");
        drop(ticket);

        // Attempt 1 has run: absent → landing → event. The race fired, and the container is present.
        assert!(supervisor.wait_until_attempts_at_least(1, std::time::Duration::from_secs(10)));
        assert!(work.join("raced-holder-race").exists(), "the fixture's race never fired");
        let discharged_live = !supervisor.owns("holder-race") && work.join("present-holder-race").exists();
        assert!(
            !discharged_live,
            "the watch DISCHARGED holder-race on an event that showed a create with no destroy: the \
             container is present, and nothing owns it. This is the landing race the watch exists for."
        );
        assert!(supervisor.owns("holder-race"), "kept owed after the ambiguous event");

        // Attempt 2 finds it present, removes it and confirms it gone.
        assert!(
            supervisor.wait_until_idle(std::time::Duration::from_secs(10)),
            "the landed container was never reconciled: {:?}",
            supervisor.outstanding()
        );
        assert!(!work.join("present-holder-race").exists(), "the landed container is STILL PRESENT");
        assert_eq!(rm_log_count(&work, "holder-race"), 1, "removed exactly once, by the watch");
        // ORDER, from the fixture's own record: the racing inspect preceded the event query.
        let log = std::fs::read_to_string(work.join("events.log")).unwrap_or_default();
        let first_inspect = log.find("inspect holder-race").expect("an inspect ran");
        let first_events = log.find("events holder-race").expect("an event query ran");
        assert!(first_inspect < first_events, "the inspect did not precede the event query:\n{log}");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// F1: A LANDING BETWEEN THE EVENT QUERY AND THE CONFIRMING INSPECT does not discharge either.
    ///
    /// The mirror of the race above. The event log reads COMPLETE — an earlier container under the
    /// name was created and destroyed — and a new one lands the instant after that answer. Discharge
    /// requires a fresh absent inspect AFTER the completed record; that inspect finds the container,
    /// the name stays owed, and the next attempt removes it. Without the ordered final absence, a
    /// complete-looking record over a live container is a discharge.
    #[cfg(feature = "acp")]
    #[test]
    fn a_landing_between_the_event_query_and_the_confirming_inspect_does_not_discharge() {
        let work = stand_in_work_dir("landing-after-events");
        let script = stand_in_docker(&work, "sleep 5");
        let client = DockerCli::stand_in(&script);
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        // An earlier container under the name came and went; the new one lands right after the
        // event query answers.
        std::fs::write(work.join("landed-holder-late"), "").expect("event record");
        std::fs::write(work.join("land-after-events-holder-late"), "").expect("race marker");
        let ticket = fence.begin();
        issue_unanswered_create(&client, &fence, "holder-late");
        drop(ticket);

        assert!(supervisor.wait_until_attempts_at_least(1, std::time::Duration::from_secs(10)));
        assert!(work.join("raced-after-events-holder-late").exists(), "the fixture's race never fired");
        let discharged_live = !supervisor.owns("holder-late") && work.join("present-holder-late").exists();
        assert!(
            !discharged_live,
            "the watch DISCHARGED holder-late on a complete-looking record without a fresh absent \
             inspect after it: the container is present, and nothing owns it."
        );
        assert!(supervisor.owns("holder-late"), "kept owed after the record");
        assert!(
            supervisor.wait_until_idle(std::time::Duration::from_secs(10)),
            "the landed container was never reconciled: {:?}",
            supervisor.outstanding()
        );
        assert!(!work.join("present-holder-late").exists(), "the landed container is STILL PRESENT");
        assert_eq!(rm_log_count(&work, "holder-late"), 1, "removed exactly once, by the watch");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// F2: A REAPED CLIENT WHOSE STDERR A DESCENDANT HOLDS IS RECORDED AS UNANSWERED before release.
    ///
    /// The client exits 125 at once, having started a descendant that keeps its stderr open past
    /// the bound. The flow ends on the pending drain — and the previous version returned there,
    /// BEFORE its exit-code classification, so this create was never recorded and an absent inspect
    /// ended the story. Here the name must be owned by the supervisor once the drain's own ticket
    /// releases and the fence settles.
    #[cfg(feature = "acp")]
    #[test]
    fn a_reaped_client_whose_stderr_a_descendant_holds_is_recorded_as_unanswered() {
        use std::os::unix::fs::PermissionsExt as _;
        let work = stand_in_work_dir("held-stderr");
        let script = work.join("docker");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n( : > \"{}/holding\"; sleep 5 ) >/dev/null &\nexit 125\n",
                work.to_string_lossy()
            ),
        )
        .expect("write stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let client = DockerCli::stand_in(&script);
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        let ticket = fence.begin();
        let mut child_exited = false;
        let outcome = run_bounded_blocking(
            &client,
            vec![
                "docker".to_owned(),
                "run".to_owned(),
                "--detach".to_owned(),
                "--name".to_owned(),
                "holder-held".to_owned(),
                "alpine".to_owned(),
            ],
            None,
            std::time::Duration::from_secs(2),
            std::time::Instant::now(),
            Some(&fence),
            &mut child_exited,
        );
        assert!(child_exited, "the client was killed rather than reaped: the fixture did not reach the drain branch");
        assert!(work.join("holding").exists(), "the descendant never announced it held the pipe");
        let error = outcome.expect_err("an output never read to EOF is not a result");
        assert!(error.contains("did not reach EOF"), "ended on a different branch: {error}");
        drop(ticket);

        // The drain's ticket holds the fence until the descendant lets go; THEN it settles and the
        // unanswered record becomes a watch. If no record was made, nothing is ever owned.
        assert!(
            supervisor.wait_until_owns("holder-held", std::time::Duration::from_secs(10)),
            "a client reaped with exit 125 and a stderr this process never read was NOT recorded \
             as unanswered — the drain-pending return came before classification: {:?}",
            supervisor.outstanding()
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// F3a: A FAILED SUPERVISOR-THREAD SPAWN STILL MAKES SCHEDULED PROGRESS, without a future adoption.
    ///
    /// Every spawn is refused. The obligation is adopted (one inline attempt, refused by the daemon),
    /// and then NOTHING is adopted again. Progress must come from the process's own cleanup events:
    /// here a create settling on a fence that reports to this supervisor. Each such event runs the
    /// next due attempt inline; when the daemon accepts, the name is removed and confirmed.
    #[cfg(feature = "acp")]
    #[test]
    fn a_failed_supervisor_thread_spawn_still_makes_scheduled_progress_without_another_adoption() {
        let work = stand_in_work_dir("no-thread");
        let script = stand_in_docker(&work, "");
        std::fs::write(work.join("present-holder-nt"), "").expect("marker");
        std::fs::write(work.join("rmfail-holder-nt"), "").expect("marker");
        let client = DockerCli::stand_in(&script);
        let supervisor = CleanupSupervisor::new();
        supervisor.refuse_threads();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));
        assert!(!supervisor.has_worker(), "the refused spawn was recorded as a live worker");

        supervisor.adopt(retained_removal(
            "holder-nt".to_owned(),
            vec!["holder-nt".to_owned()],
            client.clone(),
            quick_bounds(),
        ));
        assert!(!supervisor.has_worker());
        assert!(supervisor.wait_until_attempts_at_least(1, std::time::Duration::ZERO), "no inline attempt ran on adoption");
        assert!(supervisor.owns("holder-nt"), "the refused removal was not kept");
        assert_eq!(rm_log_count(&work, "holder-nt"), 1);

        // NO FURTHER ADOPTION. A create settles on a fence reporting here — a cleanup event.
        assert!(supervisor.wait_until_something_is_due(std::time::Duration::from_secs(5)));
        drop(fence.begin());
        assert!(
            supervisor.wait_until_attempts_at_least(2, std::time::Duration::ZERO),
            "with no thread, the queued attempt did not run on a cleanup event: the queue sat \
             waiting for a future adoption that never came"
        );
        assert_eq!(rm_log_count(&work, "holder-nt"), 2);
        assert!(supervisor.owns("holder-nt"));

        // The daemon accepts. The next cleanup event discharges it.
        std::fs::remove_file(work.join("rmfail-holder-nt")).expect("clear refusal");
        assert!(supervisor.wait_until_something_is_due(std::time::Duration::from_secs(5)));
        drop(fence.begin());
        assert!(supervisor.wait_until_idle(std::time::Duration::ZERO), "still owed after acceptance: {:?}", supervisor.outstanding());
        assert!(!work.join("present-holder-nt").exists(), "STILL PRESENT");
        assert!(!supervisor.has_worker(), "a thread appeared although every spawn was refused");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// F3b: A DESCENDANT HOLDING THE EVENT PIPE DOES NOT STALL ANOTHER OWED NAME.
    ///
    /// A watched name's event query leaves a descendant holding stdout for 20s. The supervisor also
    /// owes a plain removal of another name. The event reader used to be joined without a bound on
    /// the ONE supervisor thread, so the other name waited out the descendant. Now the watch gives up
    /// on the reader at the owner's confirm bound (300ms here), keeps its name as uncertain, and the
    /// other name is removed and confirmed BEFORE the descendant lets go — asserted on the stand-in's
    /// release marker, an ordering, not on a wall-clock figure this host's spawn latency can break.
    #[cfg(feature = "acp")]
    #[test]
    fn a_descendant_holding_the_event_pipe_does_not_stall_another_owed_name() {
        let work = stand_in_work_dir("event-pipe-held");
        let script = stand_in_docker(&work, "sleep 5");
        let client = DockerCli::stand_in(&script);
        std::fs::write(work.join("evhang-holder-eh"), "").expect("marker");
        std::fs::write(work.join("present-other-eh"), "").expect("marker");
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));

        let ticket = fence.begin();
        issue_unanswered_create(&client, &fence, "holder-eh");
        drop(ticket);
        // The watch is mid-attempt when the other name arrives. (The fence settles only when the
        // create's own drain lets go of its ticket, so this wait covers that settlement too.)
        assert!(
            supervisor.wait_until_running("holder-eh", std::time::Duration::from_secs(30)),
            "the watch over holder-eh never ran an attempt: {:?}",
            supervisor.outstanding()
        );
        let adopted_at = std::time::Instant::now();
        supervisor.adopt(retained_removal(
            "other-eh".to_owned(),
            vec!["other-eh".to_owned()],
            client.clone(),
            quick_bounds(),
        ));

        // The fact under test is an ORDERING, not a duration: other-eh is discharged BEFORE the
        // descendant lets go of holder-eh's event pipe. The stand-in records that release as
        // `released-holder-eh` after a 20s hold, so a worker that waited the hold out is caught by
        // the marker regardless of how slowly this host spawns processes; the wall-clock bound
        // below is only there so a stalled worker fails the test instead of hanging it.
        let discharged = supervisor.wait_for(std::time::Duration::from_secs(10), |state| {
            !state.running.iter().any(|name| name == "other-eh")
                && !state.queued.iter().any(|s| s.owner.names.iter().any(|name| name == "other-eh"))
        });
        let took = adopted_at.elapsed();
        assert!(
            discharged,
            "other-eh was still owed {took:?} after adoption: the single worker was stalled by a \
             descendant holding another name's event pipe ({:?})",
            supervisor.outstanding()
        );
        assert!(
            !work.join("released-holder-eh").exists(),
            "other-eh was discharged only after the descendant released holder-eh's event pipe \
             ({took:?}): the worker waited the hold out instead of giving up on the reader at the \
             confirm bound"
        );
        assert!(!work.join("present-other-eh").exists(), "other-eh is STILL PRESENT");
        assert!(supervisor.owns("holder-eh"), "the uncertain event answer released the watched name");

        // The pipe is released and the daemon's log shows the lifecycle complete: the watch ends.
        std::fs::remove_file(work.join("evhang-holder-eh")).expect("clear hang");
        std::fs::write(work.join("landed-holder-eh"), "").expect("event record");
        assert!(supervisor.wait_until_idle(std::time::Duration::from_secs(20)), "{:?}", supervisor.outstanding());
        let _ = std::fs::remove_dir_all(&work);
    }

    /// F4: THE ORDINARY, SETTLED `NetnsHolder::drop` ROUTES REFUSED REMOVALS INTO LIVE OWNERSHIP.
    ///
    /// The actual production destructor — not a helper — on a holder with nothing in flight, whose
    /// holder AND joiner refuse removal. The previous fast path swept, printed LEAKED and returned;
    /// the fence destroyed a moment later held nothing to hand over. Here the supervisor must own both
    /// names the instant `drop` returns, keep attempting, and discharge them when the daemon accepts.
    #[cfg(feature = "acp")]
    #[test]
    fn a_refused_removal_in_the_ordinary_settled_holder_drop_is_owned_by_the_supervisor() {
        let work = stand_in_work_dir("fast-drop-refused");
        let script = stand_in_docker(&work, "");
        for name in ["holder-fd", "joiner-fd"] {
            std::fs::write(work.join(format!("present-{name}")), "").expect("marker");
            std::fs::write(work.join(format!("rmfail-{name}")), "").expect("marker");
        }
        let supervisor = CleanupSupervisor::new();
        let holder = NetnsHolder::adopt_supervised(
            "holder-fd".to_owned(),
            DockerCli::stand_in(&script),
            quick_bounds(),
            &supervisor,
        );
        holder.sidecars.lock().expect("registry").push("joiner-fd".to_owned());
        assert!(holder.creation.wait_until_settled(std::time::Duration::ZERO), "nothing is in flight");

        drop(holder); // THE PRODUCTION DESTRUCTOR, ordinary path.

        assert!(
            supervisor.owns("holder-fd") && supervisor.owns("joiner-fd"),
            "the settled fast path swept, logged and returned: nobody owns the refused names {:?}",
            supervisor.outstanding()
        );
        assert!(supervisor.wait_until_attempts_at_least(2, std::time::Duration::from_secs(10)));
        assert!(rm_log_count(&work, "holder-fd") >= 2 && rm_log_count(&work, "joiner-fd") >= 2);
        assert!(work.join("present-holder-fd").exists() && work.join("present-joiner-fd").exists());

        for name in ["holder-fd", "joiner-fd"] {
            std::fs::remove_file(work.join(format!("rmfail-{name}"))).expect("clear refusal");
        }
        assert!(supervisor.wait_until_idle(std::time::Duration::from_secs(10)), "{:?}", supervisor.outstanding());
        assert!(!work.join("present-holder-fd").exists(), "the holder is STILL PRESENT");
        assert!(!work.join("present-joiner-fd").exists(), "the joiner is STILL PRESENT");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// F4: the holder `establish` builds reports to the PROCESS supervisor — the one the test above
    /// drives by substitution is the same object in production, not a test-only route.
    #[test]
    fn a_production_holders_fence_reports_to_the_process_supervisor() {
        let holder = NetnsHolder::adopt_bounded(
            "holder-process-sup".to_owned(),
            DockerCli::stand_in(std::path::Path::new("/nonexistent/docker-never-run")),
            quick_bounds(),
        );
        assert!(std::sync::Arc::ptr_eq(&holder.creation.supervisor, CleanupSupervisor::process()));
        // Not dropped: its destructor would run `docker rm` against a program that does not exist,
        // and the refusal would then be owed by the PROCESS supervisor for the rest of this test
        // binary's life — a real obligation this test has no daemon to settle.
        std::mem::forget(holder);
    }

    // ---- Live gates: the same paths against the real daemon on the approved VM -----------------
    //
    // Run with `cargo test --features acp,wallet -p maxplayer-core --lib -- --ignored live_`.
    // The client is real `docker`; the containers are real. Where a fault is injected it is injected
    // at the CLIENT (a wrapper that refuses `rm` while a marker exists) and is labelled as such — the
    // daemon's answers to inspect, create, events and the eventual removal are the real daemon's.

    #[cfg(feature = "acp")]
    fn live_image() -> String {
        std::env::var("MAXPLAYER_HOLDER_IMAGE").unwrap_or_else(|_| "alpine".to_owned())
    }

    #[cfg(feature = "acp")]
    fn live_rm(name: &str) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "--force", "--volumes", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// LIVE: a create whose client the real daemon never answered is caught when it lands.
    ///
    /// The client is killed 20ms in, long before it has connected, so its request is never applied
    /// by the daemon on its own — the API offers no way to make the daemon DEFER a create, so the
    /// late landing is produced by the test issuing the same create after the watch has already seen
    /// the name absent. What is real: the kill path, the absent inspect, the landing, the supervisor's
    /// detection and removal, and the daemon confirming absence afterwards.
    #[cfg(feature = "acp")]
    #[test]
    #[ignore = "needs a real docker daemon"]
    fn live_a_create_the_daemon_never_answered_is_caught_when_it_lands() {
        let client = DockerCli::system();
        let name = format!("mx-live-unanswered-{}", std::process::id());
        live_rm(&name);
        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(
            &supervisor,
            FenceBounds { reschedule: std::time::Duration::from_millis(200), ..quick_bounds() },
        ));

        let ticket = fence.begin();
        let mut child_exited = false;
        let outcome = run_bounded_blocking(
            &client,
            vec![
                "docker".to_owned(),
                "run".to_owned(),
                "--detach".to_owned(),
                "--name".to_owned(),
                name.clone(),
                live_image(),
                "sleep".to_owned(),
                "300".to_owned(),
            ],
            None,
            std::time::Duration::from_millis(20),
            std::time::Instant::now(),
            Some(&fence),
            &mut child_exited,
        );
        assert!(outcome.is_err() && !child_exited, "the client was not killed unanswered: {outcome:?}");
        drop(ticket);
        assert!(supervisor.owns(&name), "the unanswered create was not watched");
        assert!(supervisor.wait_until_attempts_at_least(1, std::time::Duration::from_secs(60)));

        if supervisor.owns(&name) {
            assert_eq!(container_is_absent(&client, &name), Some(true));
            // THE LANDING, after local completion and after the watch has seen absence.
            let landed = std::process::Command::new("docker")
                .args(["run", "--detach", "--name", &name, &live_image(), "sleep", "300"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .status()
                .expect("docker run");
            assert!(landed.success(), "the test could not land the container");
            assert_eq!(container_is_absent(&client, &name), Some(false), "it did not land");
        } else {
            // The daemon applied the request after all and the watch already removed it: that is the
            // other legitimate branch, and the event log must show the container existed and is gone.
            assert_eq!(
                lifecycle_since(
                    &client,
                    &name,
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1),
                    std::time::Duration::from_secs(10),
                ),
                Some(Lifecycle::Completed)
            );
        }

        let idle = supervisor.wait_until_idle(std::time::Duration::from_secs(120));
        let absent = container_is_absent(&client, &name);
        live_rm(&name);
        assert!(idle, "the late-landing container was never reconciled: {:?}", supervisor.outstanding());
        assert_eq!(absent, Some(true), "the real daemon still has {name}");
    }

    /// LIVE: custody survives repeated refused removals and discharges when the real daemon accepts.
    ///
    /// The refusal is injected at the client (a wrapper that fails `rm` while `rmfail` exists and
    /// otherwise runs the real docker); the container, every inspect and the final removal are real.
    #[cfg(feature = "acp")]
    #[test]
    #[ignore = "needs a real docker daemon"]
    fn live_custody_survives_refused_removals_and_discharges_when_the_daemon_accepts() {
        use std::os::unix::fs::PermissionsExt as _;
        let work = stand_in_work_dir("live-refused");
        let wrapper = work.join("docker");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ \"$1\" = rm ] && [ -f \"{0}/rmfail\" ]; then\n  echo \"Error response \
                 from daemon: cannot remove container (injected at the client)\" >&2\n  exit 1\nfi\n\
                 exec docker \"$@\"\n",
                work.to_string_lossy()
            ),
        )
        .expect("wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::write(work.join("rmfail"), "").expect("refusal marker");
        let client = DockerCli::stand_in(&wrapper);
        let name = format!("mx-live-refused-{}", std::process::id());
        live_rm(&name);
        let created = std::process::Command::new("docker")
            .args(["run", "--detach", "--name", &name, &live_image(), "sleep", "300"])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("docker run");
        assert!(created.success());

        let supervisor = CleanupSupervisor::new();
        let fence = std::sync::Arc::new(CreationFence::supervised_by(&supervisor, quick_bounds()));
        let cleanup = HolderCleanup {
            name: name.clone(),
            joiners: Vec::new(),
            creation: std::sync::Arc::clone(&fence),
            client: client.clone(),
            bounds: FenceBounds {
                confirm: std::time::Duration::from_secs(1),
                retain: std::time::Duration::from_secs(1),
                reschedule: std::time::Duration::from_millis(200),
                ..quick_bounds()
            },
        };
        cleanup.own_until_settled_or_confirmed();
        assert_eq!(std::sync::Arc::strong_count(&fence), 1);
        drop(fence);

        assert!(supervisor.owns(&name), "after the fence died nobody owned {name}");
        assert!(supervisor.wait_until_attempts_at_least(2, std::time::Duration::from_secs(60)));
        assert!(supervisor.owns(&name));
        assert_eq!(container_is_absent(&client, &name), Some(false), "the real container is gone early");

        std::fs::remove_file(work.join("rmfail")).expect("the daemon accepts again");
        let idle = supervisor.wait_until_idle(std::time::Duration::from_secs(120));
        let absent = container_is_absent(&client, &name);
        live_rm(&name);
        let _ = std::fs::remove_dir_all(&work);
        assert!(idle, "still owed after removals were accepted: {:?}", supervisor.outstanding());
        assert_eq!(absent, Some(true), "the real daemon still has {name}");
    }

    /// LIVE (F1): the real daemon's event log, read in the production format, tells a container that
    /// LANDED and is still there from one whose lifecycle COMPLETED — and a name with no record.
    ///
    /// This is the evidence the watch discharges on. Against the real daemon: no record before the
    /// create; `Landed` (a create with no destroy under the exact name) while the container runs —
    /// the state in which the previous version discharged; `Completed` only after the daemon
    /// destroyed it. The ids and actions are the daemon's, not a fixture's.
    #[cfg(feature = "acp")]
    #[test]
    #[ignore = "needs a real docker daemon"]
    fn live_lifecycle_evidence_tells_a_landed_container_from_a_completed_one() {
        let client = DockerCli::system();
        let name = format!("mx-live-lifecycle-{}", std::process::id());
        live_rm(&name);
        let issued = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
        let bound = std::time::Duration::from_secs(10);
        assert_eq!(lifecycle_since(&client, &name, issued, bound), Some(Lifecycle::NoRecord));

        let created = std::process::Command::new("docker")
            .args(["run", "--detach", "--name", &name, &live_image(), "sleep", "300"])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("docker run");
        assert!(created.success());
        assert_eq!(container_is_absent(&client, &name), Some(false));
        let while_running = lifecycle_since(&client, &name, issued, bound);
        live_rm(&name);
        assert_eq!(
            while_running,
            Some(Lifecycle::Landed),
            "a running container's record must read as LANDED, never as complete"
        );
        // Removed. `docker rm --force` destroys asynchronously from the client's return, so the
        // record is polled for a bounded time before the claim is made.
        let started = std::time::Instant::now();
        let after_removal = loop {
            let lifecycle = lifecycle_since(&client, &name, issued, bound);
            if lifecycle == Some(Lifecycle::Completed) || started.elapsed() > std::time::Duration::from_secs(30) {
                break lifecycle;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        assert_eq!(container_is_absent(&client, &name), Some(true));
        assert_eq!(after_removal, Some(Lifecycle::Completed), "destroyed, yet the record does not read complete");
    }

    /// LIVE (F4): the ORDINARY, SETTLED `NetnsHolder::drop` on a real container whose removal the
    /// client refuses hands the name to the supervisor, which removes it when the daemon accepts.
    ///
    /// The production destructor, not a helper: nothing in flight, so the fast path runs. The refusal
    /// is injected at the client (wrapper fails `rm` while `rmfail` exists); the container, every
    /// inspect and the final removal are the real daemon's.
    #[cfg(feature = "acp")]
    #[test]
    #[ignore = "needs a real docker daemon"]
    fn live_the_ordinary_settled_holder_drop_hands_a_refused_removal_to_the_supervisor() {
        use std::os::unix::fs::PermissionsExt as _;
        let work = stand_in_work_dir("live-fast-drop");
        let wrapper = work.join("docker");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ \"$1\" = rm ] && [ -f \"{0}/rmfail\" ]; then\n  echo \"Error response \
                 from daemon: cannot remove container (injected at the client)\" >&2\n  exit 1\nfi\n\
                 exec docker \"$@\"\n",
                work.to_string_lossy()
            ),
        )
        .expect("wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::write(work.join("rmfail"), "").expect("refusal marker");
        let client = DockerCli::stand_in(&wrapper);
        let name = format!("mx-live-fastdrop-{}", std::process::id());
        live_rm(&name);
        let created = std::process::Command::new("docker")
            .args(["run", "--detach", "--name", &name, &live_image(), "sleep", "300"])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("docker run");
        assert!(created.success());

        let supervisor = CleanupSupervisor::new();
        let holder = NetnsHolder::adopt_supervised(
            name.clone(),
            client.clone(),
            FenceBounds {
                confirm: std::time::Duration::from_secs(1),
                retain: std::time::Duration::from_secs(1),
                reschedule: std::time::Duration::from_millis(200),
                ..quick_bounds()
            },
            &supervisor,
        );
        assert!(holder.creation.wait_until_settled(std::time::Duration::ZERO), "nothing is in flight");

        drop(holder); // THE PRODUCTION DESTRUCTOR, ordinary settled path.

        assert!(supervisor.owns(&name), "the settled drop swept, logged and returned: nobody owns {name}");
        assert!(supervisor.wait_until_attempts_at_least(2, std::time::Duration::from_secs(60)));
        assert!(supervisor.owns(&name));
        assert_eq!(container_is_absent(&client, &name), Some(false), "the real container is gone early");

        std::fs::remove_file(work.join("rmfail")).expect("the daemon accepts again");
        let idle = supervisor.wait_until_idle(std::time::Duration::from_secs(120));
        let absent = container_is_absent(&client, &name);
        live_rm(&name);
        let _ = std::fs::remove_dir_all(&work);
        assert!(idle, "still owed after removals were accepted: {:?}", supervisor.outstanding());
        assert_eq!(absent, Some(true), "the real daemon still has {name}");
    }

    /// Work whose budget expired IN THE QUEUE never starts a create at all.
    ///
    /// The clock starts before the work is queued, so a saturated blocking pool can consume the
    /// entire budget before the closure runs. The previous flow spawned anyway and then bounded the
    /// wait — but by then the create exists, and a container can land with its caller already past
    /// its bound. That is an orphan issued knowingly. The only correct answer at that point is to
    /// not start it, and what this asserts is that nothing was started.
    #[cfg(feature = "acp")]
    #[test]
    fn work_whose_budget_expired_in_the_queue_is_refused_before_anything_is_spawned() {
        let work = stand_in_work_dir("queue-expired");
        let script = stand_in_docker(&work, "");
        let deadline = std::time::Duration::from_millis(200);
        // The caller's clock, taken before the work was queued. It waited three budgets for a
        // thread.
        let queued_at = std::time::Instant::now() - std::time::Duration::from_millis(600);
        let fence = std::sync::Arc::new(CreationFence::default());
        let mut child_exited = false;

        let outcome = run_bounded_blocking(
            &DockerCli::stand_in(&script),
            vec!["docker".to_owned(), "run".to_owned(), "--detach".to_owned()],
            Some("plan".to_owned()),
            deadline,
            queued_at,
            Some(&fence),
            &mut child_exited,
        );

        outcome.expect_err("work whose budget was spent before it began cannot report success");
        // THE ASSERTION IS THAT NOTHING WAS EVER STARTED UNDER THIS FENCE.
        //
        // Deliberately not the stand-in's side effects: a client that IS spawned with no budget
        // left is killed within microseconds, long before a shell can reach its first line, so the
        // markers it would have written are absent either way and prove nothing. (That is not a
        // guess — with the pre-spawn refusal removed, the marker form of this gate stayed green.)
        //
        // Ticket issuance is the fact that survives that race. Spawning takes a ticket for the plan
        // writer synchronously, on this thread, before any wait — so a fence that never issued one
        // is a fence under which nothing was launched, whatever happened afterwards.
        assert_eq!(
            fence.tickets_issued(),
            0,
            "work whose budget had already expired in the queue was STARTED anyway: a ticket was \
             issued under this fence, so a client was launched with nothing left to wait for it. \
             The container it creates can land with its caller already past its bound — an orphan \
             this module exists to prevent, issued knowingly. Bounding the wait afterwards is too \
             late, because by then the create exists."
        );
        assert!(
            !work.join("creating").exists(),
            "a create against the stand-in daemon completed for work with no budget left"
        );
        assert!(!child_exited, "no child can have been reaped when none was ever spawned");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Reaping the client does not close its pipes: a DESCENDANT can hold them open.
    ///
    /// The bounded wait covered the child and stopped there. After the status came back the flow
    /// ran `read_to_end` on stdout and stderr on this very thread, with no bound at all, on the
    /// reasoning that a reaped child leaves closed pipes. It does not. Anything the client started
    /// inherits the write ends, and `read_to_end` waits for an EOF that a living descendant never
    /// sends — so the whole flow could block indefinitely immediately AFTER its deadline had been
    /// satisfied. The client here exits at once and leaves a descendant holding stdout.
    #[cfg(feature = "acp")]
    #[test]
    fn a_descendant_holding_the_output_pipe_cannot_outlast_the_bound() {
        use std::os::unix::fs::PermissionsExt as _;

        let work = stand_in_work_dir("descendant-pipe");
        let script = work.join("docker");
        // SYNCHRONIZED TO THE BRANCH THIS GATE EXISTS FOR.
        //
        // The branch under test is the drain that runs AFTER the client has been reaped. The
        // previous fixture gave the client a 400ms budget and assumed it would exit inside it; on
        // an independent loaded machine it did not, the deadline killed the client before it ever
        // exited, and the run took the deadline-kill path instead. The gate failed having never
        // reached the code it was written to exercise, which proves nothing either way — a fixture
        // that dies before its branch tests nothing.
        //
        // So the client exits AT ONCE with no work in front of it, the descendant announces that it
        // is holding the pipe, and the budget is wide enough that arriving at the drain is not a
        // race. `child_exited` is then checked FIRST, because it is the fact that says which branch
        // actually ran.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n( echo holding > \"{}/holding\"; sleep 6 ) &\nexit 0\n",
                work.to_string_lossy()
            ),
        )
        .expect("write stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let fence = std::sync::Arc::new(CreationFence::default());
        let deadline = std::time::Duration::from_secs(2);
        let mut child_exited = false;
        let started = std::time::Instant::now();
        let outcome = run_bounded_blocking(
            &DockerCli::stand_in(&script),
            vec!["docker".to_owned(), "create".to_owned()],
            None,
            deadline,
            started,
            Some(&fence),
            &mut child_exited,
        );
        let elapsed = started.elapsed();

        assert!(
            child_exited,
            "the client was killed on its deadline before it ever exited, so the post-exit drain \
             this gate exists for never ran. That is the FIXTURE failing, not the production code: \
             it has to reach the branch under test before it can say anything about it."
        );
        assert!(
            work.join("holding").exists(),
            "the descendant never announced that it had the pipe, so nothing was holding stdout \
             open and the drain had nothing to be blocked by"
        );
        assert!(
            elapsed < deadline * 4,
            "the flow ran unbounded AFTER the child was reaped: a descendant held stdout open and \
             the drain waited {elapsed:?} against a {deadline:?} bound. A create whose tail is \
             unbounded is a create nobody is waiting on."
        );
        outcome.expect_err(
            "an output this process never finished reading is not a result it may report",
        );
        // OWNERSHIP, not wording. The call is over and a reader thread still holds the create's
        // stdout. While it does, the fence must NOT read as settled: a settled fence is cleanup's
        // permission to start removing, and that permission cannot be granted while this process is
        // still doing the create's IO. The drain threads used to be ticketless, so the fence fell
        // silent the instant this function returned and the reader became work nobody was counted
        // for.
        assert!(
            !fence.wait_until_settled(std::time::Duration::from_millis(300)),
            "the fence reported this create SETTLED while a reader thread still held its stdout \
             open. Cleanup is entitled to remove on that answer, so unowned IO work outlived the \
             ticket that was supposed to cover it."
        );
        // And it is not owned forever. When the descendant lets go, the read ends and the ticket
        // goes with it: retained ownership means the lifecycle closes on the real event, not that
        // it never closes.
        assert!(
            fence.wait_until_settled(std::time::Duration::from_secs(20)),
            "the reader's ticket was never released even after the descendant exited and stdout \
             reached EOF, so this fence can never settle and cleanup could never run"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A writer that outlives the deadline is NAMED, never silently dropped.
    ///
    /// On the deadline path the flow killed the child, returned, and dropped the writer handle on
    /// the way out. Killing the direct client normally closes the read end and the blocked write
    /// fails with `EPIPE` — but a descendant holding that end open defeats exactly that, leaving a
    /// thread still writing a plan into a pipe while this call reports the command finished. The
    /// returned error has to carry that outstanding custody instead of implying a settled flow.
    #[cfg(feature = "acp")]
    #[test]
    fn a_plan_writer_still_running_after_the_deadline_is_reported_not_abandoned() {
        use std::os::unix::fs::PermissionsExt as _;

        let work = stand_in_work_dir("writer-outstanding");
        let script = work.join("docker");
        // Never reads stdin, so a large plan fills the pipe and the write blocks; the descendant
        // keeps the READ end open, so killing the client does NOT deliver `EPIPE` to the writer and
        // the outstanding-writer case is reached every time rather than by luck.
        //
        // The read end is parked on fd 3 on purpose. A background job in a non-interactive shell
        // has its STDIN redirected to /dev/null by POSIX, so the obvious `sleep 30 &` holds nothing
        // — and `sleep 30 <&0 &` does not help either, because that default is applied before the
        // redirection resolves, leaving it duplicating /dev/null. An unrelated descriptor is
        // inherited untouched, so fd 3 keeps the pipe genuinely open. Written the obvious way this
        // test raced: the writer took `EPIPE` instead, and whether it arrived inside the grace
        // decided the result — it passed single-threaded and failed under parallel load.
        std::fs::write(&script, "#!/bin/sh\nexec 3<&0\nsleep 30 &\nsleep 30\n")
            .expect("write stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let deadline = std::time::Duration::from_millis(300);
        let mut child_exited = false;
        let started = std::time::Instant::now();
        let outcome = run_bounded_blocking(
            &DockerCli::stand_in(&script),
            vec!["docker".to_owned(), "create".to_owned()],
            Some("x".repeat(4 * 1024 * 1024)),
            deadline,
            started,
            None,
            &mut child_exited,
        );
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the bound never armed: {elapsed:?} against {deadline:?}."
        );
        let error = outcome.expect_err("a client killed on its deadline cannot report success");
        // The assertion is that the writer was ACCOUNTED FOR, not which way it went.
        //
        // Both dispositions are correct: the kill normally closes the read end and the blocked
        // write ends on `EPIPE`, while a descendant holding that end leaves the thread running and
        // it has to be named. Which one happens depends on whether the stand-in reached its
        // backgrounded holder before the kill, and under parallel load it sometimes does not — an
        // earlier version of this test asserted the still-running branch and failed for that reason
        // alone. What must never happen, and is what the production defect did, is ending the
        // deadline path having said nothing about the writer at all.
        assert!(
            error.contains("the thread writing its plan") || error.contains("no plan was being written"),
            "the deadline path ended without accounting for the thread writing the plan — that \
             handle was dropped, so a write into a descendant-held pipe can continue while this \
             call reads as finished. Got:\n{error}"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Time spent WAITING FOR A THREAD is time spent against the bound.
    ///
    /// `spawn_blocking` hands work to a pool that can be saturated, and the clock used to start
    /// inside the closure — after the queue. A create could therefore sit queued for longer than
    /// its entire deadline and still be handed a full fresh budget when a thread finally freed up,
    /// which is not a bound on the flow at all. The clock is now taken on the caller's side and
    /// passed in; this hands in a budget already mostly spent and requires the remainder to be
    /// honoured rather than restarted.
    #[cfg(feature = "acp")]
    #[test]
    fn the_bound_counts_the_time_the_work_spent_queued_for_a_thread() {
        use std::os::unix::fs::PermissionsExt as _;

        let work = stand_in_work_dir("queue-time");
        let script = work.join("docker");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").expect("write stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let deadline = std::time::Duration::from_millis(600);
        let queued_for = std::time::Duration::from_millis(500);
        // Stood for a call that waited `queued_for` on the pool before a thread took it.
        let queued_at = std::time::Instant::now() - queued_for;
        let mut child_exited = false;
        let entered = std::time::Instant::now();
        let outcome = run_bounded_blocking(
            &DockerCli::stand_in(&script),
            vec!["docker".to_owned(), "create".to_owned()],
            None,
            deadline,
            queued_at,
            None,
            &mut child_exited,
        );
        let spent_here = entered.elapsed();

        assert!(outcome.is_err(), "a client killed on its deadline cannot report success");
        // What is left of the budget, plus slack for a loaded machine. A flow that restarts its
        // clock on arrival instead spends the WHOLE deadline here and lands well outside this.
        let remainder = deadline - queued_for + std::time::Duration::from_millis(250);
        assert!(
            spent_here < remainder,
            "the queue wait was not counted: this call had {queued_for:?} of a {deadline:?} budget \
             already spent before it started, so at most {remainder:?} remained — yet it ran a \
             further {spent_here:?}, a full fresh deadline granted on arrival. Work that waits \
             longer than its bound for a thread would never be cut off."
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// The address `establish` MEASURES is the address its rendered policy pinholes.
    ///
    /// The single-source property was only ever asserted against a hand-built `NetPolicy`. That
    /// cannot catch the failure that matters: `establish` measuring one address and rendering the
    /// plan from another, which produces a job whose firewall permits a proxy it is not pointed at,
    /// or points at a proxy its firewall drops. Here the measurement comes from the stand-in client
    /// and the assertion is made on the bytes production actually sent to the applier.
    ///
    /// The run ends at the applier's count cross-check, which is the point of interest: reaching it
    /// proves the probe, the fenced holder create and the plan render all ran in production order.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_address_establish_measures_is_the_address_its_plan_pinholes() {
        let work = stand_in_work_dir("proxy");
        let script = stand_in_docker(&work, "");

        let outcome = establish_with(
            &DockerCli::stand_in(&script),
            FenceBounds::production(),
            "mx-scratch",
            "holder:local",
            "sidecar:local",
            "host.docker.internal",
            "proxy-route",
            "seat",
            1000,
            1000,
            Some(crate::sandbox_net::PortRange::new(9000, 9002).expect("valid range")),
            false,
            vec!["10.0.0.53".to_owned()],
            2_000_000_000,
        )
        .await;

        let error = outcome.expect_err("the stand-in applier reports a short count");
        assert!(
            error.contains("containment is incomplete"),
            "the run must reach the applier's count cross-check, not fail earlier: {error}"
        );

        let plan = std::fs::read_to_string(work.join("stdin.txt"))
            .expect("production sent a plan to the applier");
        assert!(
            plan.contains("203.0.113.77"),
            "the plan must pinhole the address establish measured, not some other one:\n{plan}"
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    /// A CANCELLED `establish` does not leave the container it created behind.
    ///
    /// Cancellation mid-create is the production shape of the delayed-create race: the future is
    /// dropped while the blocking create is still running, and a cleanup that races it issues a
    /// remove for a container that does not exist yet. The container then arrives, unowned, pinning
    /// a namespace with nobody left to remove it.
    ///
    /// Two assertions, and both are needed: cleanup must OUTLAST the create (otherwise the removal
    /// it issued named nothing), and it must actually name the holder (otherwise it waited and then
    /// removed nothing).
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_establish_outlasts_its_create_and_removes_the_holder() {
        let work = stand_in_work_dir("cancel");
        let script = stand_in_docker(&work, "sleep 1");

        // Bound to the test, not to the call expression: the future below outlives the statement
        // that builds it, so the client it borrows has to as well.
        let client = DockerCli::stand_in(&script);
        let mut establishing = Box::pin(establish_with(
            &client,
            FenceBounds::production(),
            "mx-scratch",
            "holder:local",
            "sidecar:local",
            "host.docker.internal",
            "cancelled-establish",
            "seat",
            1000,
            1000,
            None,
            false,
            vec!["10.0.0.53".to_owned()],
            2_000_000_000,
        ));

        // Cancel on the CREATE ITSELF, not on a stopwatch. A fixed deadline raced the probe and
        // cancelled before the create had begun, which measures nothing: the marker is written by
        // the stand-in as the create starts, so the drop below always lands mid-create.
        let marker = work.join("creating");
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.exists() {
            tokio::select! {
                _ = establishing.as_mut() => panic!("establish cannot finish against this client"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            }
            assert!(std::time::Instant::now() < give_up, "the create never started");
        }

        let started = std::time::Instant::now();
        drop(establishing); // the cancellation under test; the holder's cleanup runs in here
        let elapsed = started.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(700),
            "cancellation returned in {elapsed:?}, while the create it had to outlast was still \
             running: the container arrives afterwards with nobody holding it"
        );

        let removed = std::fs::read_to_string(work.join("rm.log")).unwrap_or_default();
        assert!(
            removed.contains("cancelled-establish"),
            "a cancelled establish must remove the holder it created, got {removed:?}"
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    /// Cleanup whose wait for an in-flight create **times out** still removes only AFTER that create
    /// has settled.
    ///
    /// This is the delayed path, and it is the one the previous version got wrong. It waited 30s for
    /// a create the client itself allows 120s to run, and on expiry it removed anyway and printed
    /// that the container might be LEAKED. Every part of that is the bug: the removal names a
    /// container that does not exist yet, docker answers "No such container", cleanup treats that as
    /// success, and the create then lands with nobody holding it. The log line did not make it safe;
    /// it only made it documented.
    ///
    /// The bound is a parameter purely so this can be measured: `fast` expires here while the create
    /// is still running, which is exactly the production shape at a scale a test can wait out. The
    /// assertion is an ORDERING, not a duration — `rm` must appear after `create-end` in the client's
    /// own event log — because the property under test is "never removes ahead of a live create",
    /// and a stopwatch would pass for a version that simply slept longer before making the same
    /// mistake.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cleanup_that_outwaits_its_bound_removes_only_after_the_create_settles() {
        let work = stand_in_work_dir("late");
        let script = stand_in_docker(&work, "sleep 1");
        let client = DockerCli::stand_in(&script);
        // `fast` expires mid-create; `max` is generous enough that the owner waits the create out.
        let bounds = FenceBounds {
            fast: std::time::Duration::from_millis(50),
            max: std::time::Duration::from_secs(30),
            confirm: std::time::Duration::from_secs(10),
            retain: std::time::Duration::from_secs(30),
            reschedule: std::time::Duration::from_millis(10),
        };

        let mut establishing = Box::pin(establish_with(
            &client,
            bounds,
            "mx-scratch",
            "holder:local",
            "sidecar:local",
            "host.docker.internal",
            "late-cleanup",
            "seat",
            1000,
            1000,
            None,
            false,
            vec!["10.0.0.53".to_owned()],
            2_000_000_000,
        ));

        // Cancel on the create itself, so the drop below always lands while it is in flight.
        let marker = work.join("creating");
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.exists() {
            tokio::select! {
                _ = establishing.as_mut() => panic!("establish cannot finish against this client"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            }
            assert!(std::time::Instant::now() < give_up, "the create never started");
        }
        drop(establishing);

        // The owner runs past this scope, so the removal is awaited here rather than assumed.
        let events = work.join("events.log");
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let log = std::fs::read_to_string(&events).unwrap_or_default();
            if log.contains("rm ") {
                break;
            }
            assert!(
                std::time::Instant::now() < give_up,
                "cleanup never removed the holder at all; the event log was {log:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let log = std::fs::read_to_string(&events).expect("the stand-in recorded its calls");
        let lines: Vec<&str> = log.lines().collect();
        let create_end = lines
            .iter()
            .position(|line| line.trim() == "create-end")
            .expect("the create ran to completion");
        let removed = lines
            .iter()
            .position(|line| line.starts_with("rm ") && line.contains("late-cleanup"))
            .expect("cleanup removed the holder it created");
        assert!(
            removed > create_end,
            "cleanup removed the holder at step {removed} but the create only settled at step \
             {create_end}: the remove was issued ahead of a live create, which docker answers \
             \"No such container\" and cleanup then treats as done — the container lands afterwards \
             unowned. Event log:\n{log}"
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    /// The same delayed-create failure, reproduced on the **sidecar** path rather than the holder's.
    ///
    /// This is the half that registration alone does not cover, and the distinction the R3 verdict
    /// drew: pre-registering the name tells cleanup WHAT to remove and nothing about WHEN the
    /// container appears. A sidecar create still in flight when the holder drops gets removed by
    /// name, answered "No such container" because it does not exist yet, and marked done — then it
    /// lands, pinning the namespace the holder was tearing down.
    ///
    /// The future is genuinely CANCELLED here (dropped by `timeout`) while its blocking work runs
    /// on, which is the real shape of the bug: cancelling the future must not release the fence.
    /// Remove the `Some(holder.fence_creation())` argument in `run_sidecar_confirmed` and this
    /// fails, because `Drop` returns while the create is still running.
    #[cfg(feature = "acp")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sidecar_create_still_in_flight_fences_cleanup() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("mx-sc-fence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let slow = dir.join("slow");
        std::fs::write(&slow, "#!/bin/sh\nsleep 1\n").expect("write slow");
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        fn never_asked(_client: &DockerCli, _name: &str) -> Option<bool> {
            panic!("a cancelled create must not reach the absence check")
        }

        let holder = NetnsHolder::adopt("maxplayer-netns-sidecar-fence".into(), DockerCli::system());

        // Cancel the future ~100ms in, leaving roughly 900ms of blocking create still running.
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            run_sidecar_confirmed(
                &holder,
                "iface",
                vec![slow.to_string_lossy().into_owned(), "run".to_owned()],
                None,
                std::time::Duration::from_secs(10),
                never_asked,
            ),
        )
        .await;
        assert!(cancelled.is_err(), "the future must have been cancelled, not completed");
        assert_eq!(
            holder.sidecars.lock().expect("registry").len(),
            1,
            "a cancelled create must leave its name a cleanup target"
        );

        let started = std::time::Instant::now();
        drop(holder);
        let waited = started.elapsed();

        assert!(
            waited >= std::time::Duration::from_millis(400),
            "cleanup returned in {waited:?}, while the sidecar create it had to outlast was still \
             running: every remove it issued named a container that did not exist yet, and the one \
             that arrives afterwards pins the namespace with nobody holding it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The delayed-create timing failure, reproduced at the site that must fence it.
    ///
    /// The create runs on a blocking thread; cancelling the future above it does not stop that
    /// thread. Cleanup used to run immediately, ask docker to remove a container that did not exist
    /// YET, be told "No such container" — which is treated as success — and return satisfied, after
    /// which the create landed and left an untracked container.
    ///
    /// Reproduced here by holding a creation ticket that is released 700ms from now, as an in-flight
    /// create would be, and then dropping the holder. The assertion is not "the fence helper works":
    /// it is that **`Drop` had not finished before the create settled**. Remove the
    /// `wait_until_settled` call from `Drop` and this fails, because `Drop` returns while the flag
    /// is still false.
    ///
    /// `Drop` does issue real `docker rm` calls, which on this path answer "No such container"; they
    /// are not what makes this pass, and the elapsed-time assertion below is deliberately well under
    /// the settle delay so a slow removal cannot substitute for the wait.
    #[cfg(feature = "acp")]
    #[test]
    fn cleanup_does_not_remove_ahead_of_a_create_that_is_still_in_flight() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let holder = NetnsHolder::adopt("maxplayer-netns-fence-probe".into(), DockerCli::system());
        let settled = std::sync::Arc::new(AtomicBool::new(false));

        let ticket = holder.fence_creation();
        let flag = std::sync::Arc::clone(&settled);
        let creating = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(700));
            // The create finishes: the container now exists, and only now is removing it sound.
            flag.store(true, Ordering::SeqCst);
            drop(ticket);
        });

        let started = std::time::Instant::now();
        drop(holder);
        let waited = started.elapsed();

        assert!(
            settled.load(Ordering::SeqCst),
            "cleanup finished while a create was still in flight: every remove it issued was \
             aimed at a container that did not exist yet, and the one that arrived afterwards is \
             an orphan no one holds"
        );
        assert!(
            waited >= std::time::Duration::from_millis(500),
            "cleanup returned in {waited:?}, far sooner than the create it had to outlast — it \
             cannot have waited for the fence"
        );
        creating.join().expect("the create thread");
    }
}
