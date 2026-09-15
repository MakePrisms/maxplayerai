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
#[derive(Debug, Default)]
struct CreationFence {
    in_flight: std::sync::Mutex<usize>,
    settled: std::sync::Condvar,
}

impl CreationFence {
    /// Take custody of one create that is about to be issued.
    fn begin(self: &std::sync::Arc<Self>) -> CreationTicket {
        {
            let mut in_flight =
                self.in_flight.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *in_flight += 1;
        }
        CreationTicket { fence: std::sync::Arc::clone(self) }
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

impl Drop for CreationTicket {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = self.fence.in_flight.lock() {
            *in_flight = in_flight.saturating_sub(1);
        }
        self.fence.settled.notify_all();
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
            cleanup.sweep();
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
    fn sweep(&self) {
        for joiner in &self.joiners {
            if let Err(error) = NetnsHolder::force_remove(&self.client, joiner) {
                eprintln!(
                    "sandbox: could not remove sidecar {joiner} joined to netns holder {}: {error} \
                     — the namespace may still be pinned by it",
                    self.name
                );
            }
        }
        if let Err(error) = NetnsHolder::force_remove(&self.client, &self.name) {
            eprintln!(
                "sandbox: could not remove netns holder {}: {error} — this holder is LEAKED, not \
                 destroyed; the boot reaper is the only remaining backstop",
                self.name
            );
        }
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
            // Custody ends here, but it ends as a KNOWN leak — never as a clean release, and never
            // on an absence answer. While the create is still running, "No such container" is
            // indistinguishable from "has not landed yet": the container can appear the instant
            // after the daemon answers. Treating that emptiness as proof is how the orphan this
            // whole fence exists to prevent gets manufactured by the cleanup path itself, so the
            // question is not asked and the honest verdict is recorded instead.
            eprintln!(
                "sandbox: a create against netns holder {} was STILL IN FLIGHT after {:?} and did \
                 not land within the further {:?} this owner retained it — its removal has been \
                 issued, but absence CANNOT be confirmed while the create is running, so this \
                 holder and its {} joiner(s) are reported LEAKED rather than clean; the boot reaper \
                 is the only remaining backstop",
                self.name,
                self.bounds.max,
                self.bounds.retain,
                self.joiners.len()
            );
            return;
        }
        if let Err(pending) = self.confirm_all_absent() {
            eprintln!(
                "sandbox: could not confirm {} absent within {:?} after the create settled — these \
                 are LEAKED, not destroyed; the boot reaper is the only remaining backstop",
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
pub fn holder_argv(
    name: &str,
    network: &str,
    image: &str,
    uid: u32,
    gid: u32,
    job_id: &str,
    seat: &str,
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
        run_bounded_blocking(&client, argv, stdin, DOCKER_DEADLINE, queued_at, &mut child_exited)
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
        let outcome =
            run_bounded_blocking(&client, argv, stdin, deadline, queued_at, &mut child_exited);
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
                std::thread::spawn(move || {
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
                Err(error) => return Err(format!("could not wait for `{program}`: {error}")),
            }
            if started.elapsed() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
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
        let (drained_tx, drained_rx) = std::sync::mpsc::channel::<(&'static str, Vec<u8>)>();
        let mut pending: Vec<&'static str> = Vec::new();
        if let Some(mut pipe) = child.stdout.take() {
            let tx = drained_tx.clone();
            pending.push("stdout");
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                let _ = tx.send(("stdout", buffer));
            });
        }
        if let Some(mut pipe) = child.stderr.take() {
            let tx = drained_tx.clone();
            pending.push("stderr");
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                let _ = tx.send(("stderr", buffer));
            });
        }
        drop(drained_tx);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while !pending.is_empty() {
            match drained_rx.recv_timeout(remaining()) {
                Ok((which, buffer)) => {
                    pending.retain(|name| *name != which);
                    if which == "stdout" {
                        stdout = buffer;
                    } else {
                        stderr = buffer;
                    }
                }
                Err(_) => break,
            }
        }
        let done = std::process::Output { status, stdout, stderr };
        let stdout = String::from_utf8_lossy(&done.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&done.stderr).trim().to_owned();
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
            // error names WHICH refusal happened rather than "it failed".
            Some(code) => Err(format!("exit {code}: {}", if stderr.is_empty() { &stdout } else { &stderr })),
            None => Err("killed by a signal".to_string()),
        }
    }
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
        holder_argv(&name, network, holder_image, uid, gid, job_id, seat),
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
        let argv = holder_argv("h", "net", "img", 1000, 1000, "abc", &seat_b());
        let tail = &argv[argv.len() - 4..];
        assert_eq!(tail, ["--entrypoint", "sleep", "img", "infinity"]);
        // A shell anywhere in the argv would mean the holder runs something that parses a string.
        assert!(!argv.iter().any(|a| a == "sh" || a == "bash" || a == "-c"), "{argv:?}");
    }

    #[test]
    fn the_holder_is_locked_down_and_labelled_for_reaping() {
        let argv = holder_argv("h", "net", "img", 1000, 1000, "abc", &seat_b());
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
        let argv = holder_argv("h", "net", "img", 1000, 1000, "abc", &seat_b());
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
        let holder_argv = holder_argv("h", "net", "img", 1000, 1000, "abc", &seat_b());
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
        // The client exits immediately; the backgrounded descendant inherits stdout and holds it
        // open, so stdout never reaches EOF.
        std::fs::write(&script, "#!/bin/sh\nsleep 30 &\nexit 0\n").expect("write stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let deadline = std::time::Duration::from_millis(400);
        let mut child_exited = false;
        let started = std::time::Instant::now();
        let outcome = run_bounded_blocking(
            &DockerCli::stand_in(&script),
            vec!["docker".to_owned(), "create".to_owned()],
            None,
            deadline,
            started,
            &mut child_exited,
        );
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the flow ran unbounded AFTER the child was reaped: a descendant held stdout open and \
             the drain waited {elapsed:?} against a {deadline:?} bound. A create whose tail is \
             unbounded is a create nobody is waiting on."
        );
        let error = outcome.expect_err("an output this process never finished reading is not a result it may report");
        assert!(
            error.contains("stdout") && error.contains("descendant"),
            "the call ended on its bound but did not name the unread stream or the reason, so a \
             caller cannot tell a complete output from a truncated one. Got:\n{error}"
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
