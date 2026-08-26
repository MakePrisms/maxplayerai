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
//! Name resolution is unaffected: a container joining the namespace still gets its own
//! `/etc/resolv.conf` pointing at docker's embedded resolver on `127.0.0.11`, which is why
//! `sandbox_net`'s "loopback is never denied" test is load-bearing rather than decorative.

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

/// A running holder container, and the guarantee that it goes away.
///
/// Constructed the instant the container exists, so that every `?` after that point tears it down on
/// the way out — the holder is a resource with a lifetime, not a step in a procedure.
#[derive(Debug)]
pub struct NetnsHolder {
    name: String,
}

impl NetnsHolder {
    /// Adopt an already-created container as the holder. Private on purpose: a `NetnsHolder` that
    /// does not correspond to a running container would promise a teardown it cannot perform.
    fn adopt(name: String) -> Self {
        Self { name }
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

impl Drop for NetnsHolder {
    /// Destroy the holder, **synchronously**.
    ///
    /// Deliberately a blocking `std::process::Command` and not a spawned task: a task spawned from
    /// `Drop` can be discarded when the runtime shuts down, and runtime shutdown is exactly the path a
    /// panicking or aborted job takes. A leaked holder is a container pinned to a namespace nothing
    /// will ever clean up, so the ~100 ms block is the cheaper end of that trade.
    ///
    /// Failure is logged, never propagated: `Drop` cannot return, and the reaper in
    /// [`reap_orphans`] is the backstop for the case where this did not work.
    fn drop(&mut self) {
        let outcome = std::process::Command::new("docker")
            .args(["rm", "--force", "--volumes", &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        match outcome {
            Ok(done) if done.status.success() => {}
            Ok(done) => eprintln!(
                "sandbox: could not remove netns holder {}: {}",
                self.name,
                String::from_utf8_lossy(&done.stderr).trim()
            ),
            Err(error) => {
                eprintln!("sandbox: could not run docker rm for netns holder {}: {error}", self.name)
            }
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
    let (listing, _) = run_docker(list_holders_argv(seat), None)
        .await
        .map_err(|error| format!("could not list containment holders — {error}"))?;
    let holders = parse_holder_listing(&listing);
    if holders.is_empty() {
        return Ok(Vec::new());
    }

    let (all, _) = run_docker(list_all_containers_argv(), None)
        .await
        .map_err(|error| format!("could not list containers — {error}"))?;
    let all: Vec<String> = all.lines().map(str::trim).filter(|id| !id.is_empty()).map(str::to_owned).collect();
    let (modes, _) = run_docker(network_modes_argv(&all), None)
        .await
        .map_err(|error| format!("could not read container network modes — {error}"))?;

    Ok(reapable_holders(&holders, seat, &modes))
}

/// Remove `seat`'s own holders that no job is attached to, and return what was removed.
///
/// `seat` is the caller's seller public key hex. It is the *only* thing that makes this safe to run on
/// a host shared with other seller daemons: see [`reapable_holders`] for why ownership has to be
/// carried and why attachment state cannot stand in for it.
///
/// Best-effort by design: a leaked holder is a resource leak, not an open door — it owns a namespace and
/// holds no policy, and the job that could have used it is already gone. So a failure here is reported
/// and never blocks a boot, whereas a failure to *establish* containment refuses the job outright. The
/// two are deliberately not symmetrical.
#[cfg(feature = "acp")]
pub async fn reap_orphans(seat: &str) -> Result<Vec<String>, String> {
    let mut reaped = Vec::new();
    for holder in reapable_holders_live(seat).await? {
        match run_docker(
            ["docker", "rm", "--force", "--volumes", holder.as_str()]
                .into_iter()
                .map(String::from)
                .collect(),
            None,
        )
        .await
        {
            Ok(_) => reaped.push(holder),
            // One stuck holder must not stop the others being cleaned up.
            Err(error) => {
                eprintln!("sandbox: could not reap orphaned netns holder {holder}: {error}")
            }
        }
    }
    Ok(reaped)
}

/// Run a `docker` argv to completion, optionally feeding `stdin`, and return `(stdout, stderr)`.
///
/// `std::process::Command` on a blocking pool thread, not `tokio::process`: this crate's tokio is
/// built without the `process` feature, and reaching for it would widen the dependency of every
/// default build to enable three calls that happen once per job.
#[cfg(feature = "acp")]
async fn run_docker(argv: Vec<String>, stdin: Option<String>) -> Result<(String, String), String> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let (program, args) = argv.split_first().expect("a docker argv is never empty");
        let mut child = Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run `{program}`: {error}"))?;
        if let Some(plan) = stdin {
            child
                .stdin
                .as_mut()
                .ok_or_else(|| "docker stdin was not piped".to_string())?
                .write_all(plan.as_bytes())
                .map_err(|error| format!("could not write the plan to the sidecar: {error}"))?;
            // Dropped so the sidecar's `read` loop sees EOF; without this it waits forever and the
            // job's launch hangs instead of failing.
            drop(child.stdin.take());
        }
        let done = child
            .wait_with_output()
            .map_err(|error| format!("could not wait for `{program}`: {error}"))?;
        let stdout = String::from_utf8_lossy(&done.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&done.stderr).trim().to_owned();
        match done.status.code() {
            Some(0) => Ok((stdout, stderr)),
            // The sidecar's codes are an interface; pass them through in the message so the caller's
            // error names WHICH refusal happened rather than "it failed".
            Some(code) => Err(format!("exit {code}: {}", if stderr.is_empty() { &stdout } else { &stderr })),
            None => Err("killed by a signal".to_string()),
        }
    })
    .await
    .map_err(|error| format!("docker task panicked: {error}"))?
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
) -> Result<Containment, String> {
    // Measured BEFORE the holder exists, so a probe failure needs no cleanup.
    let (probe_stdout, _) = run_docker(host_gateway_probe_argv(sidecar_image, proxy_alias), None)
        .await
        .map_err(|error| format!("could not resolve {proxy_alias} for the pinhole — {error}"))?;
    let proxy_host = parse_getent_ipv4(&probe_stdout).ok_or_else(|| {
        format!("resolving {proxy_alias} produced no IPv4 address (got {probe_stdout:?})")
    })?;

    let name = holder_name(job_id);
    run_docker(holder_argv(&name, network, holder_image, uid, gid, job_id, seat), None)
        .await
        .map_err(|error| format!("could not start the netns holder {name} — {error}"))?;
    // From here on the container exists, so every early return must tear it down. Adopting it into
    // the guard immediately is what makes that automatic rather than remembered.
    let holder = NetnsHolder::adopt(name);

    let policy = NetPolicy {
        gateway: proxy_host.clone(),
        proxy_ports,
        log_connections,
    };
    let (plan, expected) = plan_stdin(&policy);
    let (applied, _) = run_docker(sidecar_argv(&holder, sidecar_image), Some(plan))
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
        let (readback, _) = run_docker(readback_argv(holder.name(), sidecar_image, family), None)
            .await
            .map_err(|error| {
                format!("could not read {} rules back from the namespace — {error}", family.binary())
            })?;
        policy.verify_readback(family, &readback).map_err(|error| {
            format!("containment did not verify after installation — {error}")
        })?;
    }

    Ok(Containment { holder, proxy_host })
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
        let holder = NetnsHolder::adopt("maxplayer-netns-abc".into());
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
        let holder = NetnsHolder::adopt("h".into());
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
        let holder = NetnsHolder::adopt("h".into());
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
        };
        let (stdin, _) = plan_stdin(&policy);
        let accepts: Vec<&str> = stdin.lines().filter(|l| l.contains("ACCEPT")).collect();
        assert_eq!(accepts.len(), 1, "exactly one pinhole: {accepts:?}");
        assert!(accepts[0].contains(&measured), "the pinhole must name the measured host: {accepts:?}");
    }
}
