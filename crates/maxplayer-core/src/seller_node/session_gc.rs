//! Session GC and parked-session measurement (docs/specs/session-jobs.md §5.2–§5.5).
//!
//! Everything docker-facing goes through the synchronous [`DockerCli`] seam the job-container
//! cleanup already uses, so the sequence (remove container, then holder, then files, then the
//! record) is checked by recording calls rather than by a live daemon. Inert while
//! `[sessions] enabled = false`: the store then holds no session rows, and the caller does not
//! even construct a [`SessionGc`].
use std::path::{Path, PathBuf};

use crate::seller_exec::{DockerCli, force_remove_argv};
use crate::session::{self, HolderNetwork, HolderObservation, SessionState};

use super::store::{SellerStore, SessionRecord, StoreError};

/// `docker container inspect --size` → `SizeRw`: the container's writable layer in bytes.
pub fn size_rw_argv(container: &str) -> Vec<String> {
    [
        "docker",
        "container",
        "inspect",
        "--size",
        "--format",
        "{{.SizeRw}}",
        container,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker inspect` of the netns holder: running flag and `State.StartedAt` (§5.5).
pub fn holder_state_argv(holder: &str) -> Vec<String> {
    [
        "docker",
        "inspect",
        "--format",
        "{{.State.Running}} {{.State.StartedAt}}",
        holder,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker stop` for a job container still running after the orchestrator's exit grace (§5.2).
pub fn stop_argv(container: &str) -> Vec<String> {
    ["docker", "stop", "--time", "10", container]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Read [`holder_state_argv`]'s answer. A daemon error that names a missing container is
/// `Missing`; any other error is treated as missing too — the job container is never started
/// without proof its firewall is the one it was created into (§5.5).
pub fn parse_holder_state(answer: Result<String, String>) -> HolderObservation {
    let Ok(out) = answer else {
        return HolderObservation::Missing;
    };
    let mut parts = out.trim().splitn(2, ' ');
    match (parts.next(), parts.next()) {
        (Some("true"), Some(started)) if !started.trim().is_empty() => HolderObservation::Running {
            started_at: started.trim().to_owned(),
        },
        (Some("false"), _) => HolderObservation::NotRunning,
        _ => HolderObservation::Missing,
    }
}

pub fn observe_holder<D: DockerCli>(cli: &mut D, holder: &str) -> HolderObservation {
    parse_holder_state(cli.run(&holder_state_argv(holder)))
}

/// `docker inspect` of the holder's docker-recorded IPv4 address(es), one per joined network.
pub fn holder_ip_argv(holder: &str) -> Vec<String> {
    [
        "docker",
        "inspect",
        "--format",
        "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
        holder,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker run` argv that runs `ip -4 -oneline <what>` INSIDE the holder's namespace.
///
/// The same shape as [`crate::sandbox_iface::link_probe_argv`]: the sidecar image (it carries
/// iproute2), `--network container:<holder>`, no capability, `--entrypoint ip` so the image's
/// privileged applier is unreachable. `labels` are this job's helper labels
/// ([`crate::sandbox_netns::helper_label_args`]), so a probe that outlives its client is judged by
/// the same sweep as every other helper.
pub fn holder_ip_probe_argv(
    holder: &str,
    image: &str,
    labels: &[String],
    what: &[&str],
) -> Vec<String> {
    let mut argv: Vec<String> = [
        "docker",
        "run",
        "--rm",
        "--network",
        &crate::sandbox_netns::NetnsHolder::network_mode_for(holder),
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    argv.extend(labels.iter().cloned());
    argv.extend(["--entrypoint", "ip", image, "-4", "-oneline"].map(String::from));
    argv.extend(what.iter().map(|s| (*s).to_owned()));
    argv
}

/// The non-loopback IPv4 addresses in `ip -4 -oneline addr show` output.
pub fn parse_ipv4_addrs(stdout: &str) -> Vec<std::net::Ipv4Addr> {
    stdout
        .lines()
        .flat_map(|line| {
            let mut words = line.split_whitespace();
            let mut found = Vec::new();
            while let Some(word) = words.next() {
                if word == "inet"
                    && let Some(cidr) = words.next()
                    && let Ok(ip) = cidr.split('/').next().unwrap_or("").parse()
                {
                    found.push(ip);
                }
            }
            found
        })
        .filter(|ip: &std::net::Ipv4Addr| !ip.is_loopback())
        .collect()
}

/// Judge the holder namespace's network from the three answers [`observe_holder_network`] reads.
/// Every error is `ProbeFailed`: the turn is never started on a namespace nobody could inspect.
pub fn judge_holder_network(
    recorded_ip: Result<String, String>,
    addrs: Result<String, String>,
    routes: Result<String, String>,
) -> HolderNetwork {
    let recorded_ip = match recorded_ip {
        Ok(out) => out
            .split_whitespace()
            .filter_map(|w| w.parse::<std::net::Ipv4Addr>().ok())
            .collect::<Vec<_>>(),
        Err(error) => return HolderNetwork::ProbeFailed(format!("holder inspect: {error}")),
    };
    let present = match addrs {
        Ok(out) => parse_ipv4_addrs(&out),
        Err(error) => return HolderNetwork::ProbeFailed(format!("addr probe: {error}")),
    };
    // The address docker recorded for the holder must still be in the namespace; with none recorded,
    // any non-loopback address will do.
    let has_address = if recorded_ip.is_empty() {
        !present.is_empty()
    } else {
        recorded_ip.iter().any(|ip| present.contains(ip))
    };
    if !has_address {
        return HolderNetwork::NoAddress;
    }
    match routes {
        Ok(out) if out.lines().any(|l| l.trim_start().starts_with("default")) => HolderNetwork::Up,
        Ok(_) => HolderNetwork::NoDefaultRoute,
        Err(error) => HolderNetwork::ProbeFailed(format!("route probe: {error}")),
    }
}

/// Per-call bound for the docker calls in the resume-time holder-network check (probe, runsc
/// restore, re-verify). Each is a `docker inspect` or a `docker run --rm` of the already-pulled
/// netfilter image, which finishes in about a second; thirty is far outside that and still ends a
/// resume whose daemon has stopped answering.
pub const HOLDER_NETWORK_CALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Total budget for every docker call in one resume's holder-network check. A full runsc restore is
/// about twenty calls, so a per-call bound alone would allow ten minutes against a hung daemon.
pub const HOLDER_NETWORK_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);

/// A [`DockerCli`] whose every call is bounded, for the resume-time holder-network check.
///
/// The same rules as the sidecar runner in [`crate::sandbox_netns`] (which is `acp`-only and async,
/// so it cannot be called from here directly): ONE clock, started when this value is built (before
/// the work is queued for a blocking thread), that every call spends. A call whose budget is
/// already spent is refused before it is spawned. A call that outlives its bound is killed AND
/// reaped. Either way the caller gets an `Err` naming the timeout. The check treats that like any
/// other docker error, which fails closed (`holder_network_lost`).
///
/// Output is shaped exactly like [`crate::seller_exec::RealDockerCli`]: trimmed stdout on exit 0,
/// `exit <code>: <stderr, else stdout>` otherwise. A call that finishes in time therefore reads the
/// same as it did before.
pub struct BoundedDockerCli {
    started: std::time::Instant,
    per_call: std::time::Duration,
    budget: std::time::Duration,
    /// Test-only: what a leading `docker` is replaced with at the spawn site. Empty in production.
    stand_in: Vec<String>,
}

impl BoundedDockerCli {
    pub fn new(per_call: std::time::Duration, budget: std::time::Duration) -> Self {
        Self {
            started: std::time::Instant::now(),
            per_call,
            budget,
            stand_in: Vec::new(),
        }
    }

    /// Test-only: run `program` (with its own leading args) wherever an argv says `docker`.
    #[cfg(test)]
    fn stand_in(
        program: &[&str],
        per_call: std::time::Duration,
        budget: std::time::Duration,
    ) -> Self {
        Self {
            stand_in: program.iter().map(|a| (*a).to_owned()).collect(),
            ..Self::new(per_call, budget)
        }
    }

    /// The production bounds: [`HOLDER_NETWORK_CALL_DEADLINE`] per call within
    /// [`HOLDER_NETWORK_BUDGET`].
    pub fn for_holder_network() -> Self {
        Self::new(HOLDER_NETWORK_CALL_DEADLINE, HOLDER_NETWORK_BUDGET)
    }
}

impl DockerCli for BoundedDockerCli {
    fn run(&mut self, argv: &[String]) -> Result<String, String> {
        use std::io::Read as _;
        use std::sync::mpsc::RecvTimeoutError;

        let substituted: Vec<String>;
        let argv =
            if argv.first().map(String::as_str) == Some("docker") && !self.stand_in.is_empty() {
                substituted = self.stand_in.iter().chain(&argv[1..]).cloned().collect();
                &substituted[..]
            } else {
                argv
            };
        let (program, args) = argv.split_first().ok_or("an empty docker argv")?;
        let left = self.budget.saturating_sub(self.started.elapsed());
        if left.is_zero() {
            return Err(format!(
                "timed out: `{program}` was not started, the {}s budget for this check is spent",
                self.budget.as_secs()
            ));
        }
        let bound = self.per_call.min(left);
        let expires = std::time::Instant::now() + bound;
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run `{program}`: {error}"))?;
        // Drained on their own threads so a chatty child cannot fill a pipe and stall its own exit,
        // and received with what is left of the bound, so a pipe held open cannot outlast it.
        let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            if let Some(mut pipe) = pipe {
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let _ = pipe.read_to_end(&mut bytes);
                    let _ = tx.send(bytes);
                });
            }
            rx
        };
        let stdout_rx = drain(
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        );
        let stderr_rx = drain(
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        );
        let timed_out = |what: &str| {
            format!(
                "timed out: `{program}` {what} within {}ms",
                bound.as_millis()
            )
        };
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() >= expires => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(timed_out("did not finish and was killed"));
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("could not wait for `{program}`: {error}"));
                }
            }
        };
        let read = |rx: std::sync::mpsc::Receiver<Vec<u8>>| match rx
            .recv_timeout(expires.saturating_duration_since(std::time::Instant::now()))
        {
            Ok(bytes) => Ok(bytes),
            Err(RecvTimeoutError::Disconnected) => Ok(Vec::new()),
            Err(RecvTimeoutError::Timeout) => {
                Err(timed_out("exited but its output was not drained"))
            }
        };
        let stdout = String::from_utf8_lossy(&read(stdout_rx)?).trim().to_owned();
        let stderr = String::from_utf8_lossy(&read(stderr_rx)?).trim().to_owned();
        match status.code() {
            Some(0) => Ok(stdout),
            Some(code) => Err(format!(
                "exit {code}: {}",
                if stderr.is_empty() { &stdout } else { &stderr }
            )),
            None => Err("killed by a signal".to_string()),
        }
    }
}

/// §5.5 holder network check: does the parked holder's namespace still have its address and a
/// default route? Runs before any job container starts; see [`HolderNetwork`].
pub fn observe_holder_network<D: DockerCli>(
    cli: &mut D,
    holder: &str,
    image: &str,
    labels: &[String],
) -> HolderNetwork {
    let recorded = cli.run(&holder_ip_argv(holder));
    if let Err(error) = recorded {
        return HolderNetwork::ProbeFailed(format!("holder inspect: {error}"));
    }
    let addrs = cli.run(&holder_ip_probe_argv(
        holder,
        image,
        labels,
        &["addr", "show"],
    ));
    if let Err(error) = addrs {
        return HolderNetwork::ProbeFailed(format!("addr probe: {error}"));
    }
    let routes = cli.run(&holder_ip_probe_argv(
        holder,
        image,
        labels,
        &["route", "show", "default"],
    ));
    judge_holder_network(recorded, addrs, routes)
}

/// `docker inspect` of what docker recorded for the holder's network: `<ip>/<prefix> <gateway>`, one
/// line per joined network. The restore puts back exactly these values and nothing it computed.
pub fn holder_recorded_network_argv(holder: &str) -> Vec<String> {
    [
        "docker",
        "inspect",
        "--format",
        "{{range .NetworkSettings.Networks}}{{.IPAddress}}/{{.IPPrefixLen}} {{.Gateway}}\n{{end}}",
        holder,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Docker's record of the holder's one network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedNetwork {
    pub address: std::net::Ipv4Addr,
    pub prefix_len: u8,
    pub gateway: std::net::Ipv4Addr,
}

/// Parse [`holder_recorded_network_argv`] output. Anything but exactly one network with a usable
/// address, prefix and an on-link gateway is "the recorded address is unavailable": no restore.
pub fn parse_recorded_network(stdout: &str) -> Result<RecordedNetwork, String> {
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let [line] = lines.as_slice() else {
        return Err(format!(
            "expected exactly one recorded network on the holder, found {}: {stdout:?}",
            lines.len()
        ));
    };
    let unavailable = || format!("the holder's recorded network is unusable: {line:?}");
    let (cidr, gateway) = line.split_once(' ').ok_or_else(unavailable)?;
    let (address, prefix_len) = cidr.split_once('/').ok_or_else(unavailable)?;
    let address: std::net::Ipv4Addr = address.parse().map_err(|_| unavailable())?;
    let prefix_len: u8 = prefix_len.parse().map_err(|_| unavailable())?;
    let gateway: std::net::Ipv4Addr = gateway.trim().parse().map_err(|_| unavailable())?;
    let on_link = crate::sandbox_iface::prefix_contains(
        &format!("{address}/{prefix_len}"),
        &gateway.to_string(),
    ) == Some(true);
    if address.is_unspecified()
        || address.is_loopback()
        || gateway.is_unspecified()
        || !(1..=30).contains(&prefix_len)
        || gateway == address
        || !on_link
    {
        return Err(unavailable());
    }
    Ok(RecordedNetwork {
        address,
        prefix_len,
        gateway,
    })
}

/// The containment policy turn 1 installed into this session's holder, rebuilt from what the seat
/// recorded: the measured proxy address (`proxy_host`), this seat's `proxy_port_range`, and the
/// resolvers in the session's own `resolv-<job>.conf` (the file `establish` was handed the same
/// addresses as). `log_connections` is `true`, as every contained launch installs it. A missing piece
/// is an error: a policy that cannot be rebuilt cannot be verified, and nothing is restored.
pub fn session_containment_policy(
    proxy_host: Option<&str>,
    proxy_ports: Option<crate::sandbox_net::PortRange>,
    resolv_conf: Option<&str>,
) -> Result<crate::sandbox_net::NetPolicy, String> {
    let gateway = proxy_host
        .filter(|h| !h.trim().is_empty())
        .ok_or("the session recorded no proxy address")?;
    let resolv_conf = resolv_conf.ok_or("the session's resolver file is missing")?;
    Ok(crate::sandbox_net::NetPolicy {
        gateway: gateway.trim().to_owned(),
        proxy_ports,
        log_connections: true,
        dns_resolvers: crate::sandbox_dns::parse_resolv_conf(resolv_conf).0,
    })
}

/// The namespace's IPv4 addresses and default gateway, read by the unprivileged probes.
struct NetState {
    addrs: Vec<std::net::Ipv4Addr>,
    default_via: Option<String>,
}

fn read_net_state<D: DockerCli>(
    cli: &mut D,
    holder: &str,
    image: &str,
    labels: &[String],
) -> Result<NetState, String> {
    let addrs = cli
        .run(&holder_ip_probe_argv(
            holder,
            image,
            labels,
            &["addr", "show"],
        ))
        .map_err(|e| format!("addr probe: {e}"))?;
    let routes = cli
        .run(&holder_ip_probe_argv(
            holder,
            image,
            labels,
            &["route", "show", "default"],
        ))
        .map_err(|e| format!("route probe: {e}"))?;
    let default_via = routes.lines().map(str::split_whitespace).find_map(|mut w| {
        (w.next() == Some("default")).then(|| {
            let words: Vec<&str> = w.collect();
            words
                .windows(2)
                .find(|p| p[0] == "via")
                .map_or_else(String::new, |p| p[1].to_owned())
        })
    });
    Ok(NetState {
        addrs: parse_ipv4_addrs(&addrs),
        default_via,
    })
}

/// The namespace's own veth, present and UP ([`crate::sandbox_iface::select_egress_link`], the
/// same identity check setup applies before filtering it).
fn holder_egress_dev<D: DockerCli>(
    cli: &mut D,
    holder: &str,
    image: &str,
    labels: &[String],
) -> Result<String, String> {
    let argv = crate::sandbox_netns::with_helper_labels(
        crate::sandbox_iface::link_probe_argv(holder, image),
        labels,
    )?;
    let links = cli.run(&argv).map_err(|e| format!("link probe: {e}"))?;
    let parsed = crate::sandbox_iface::parse_links(&links)?;
    Ok(crate::sandbox_iface::select_egress_link(&parsed)?.name)
}

/// The containment checks `sandbox_netns::establish` applies when the holder is set up, applied to
/// the parked namespace: both iptables families read back and judged against `policy`, then the
/// veth's `tc` egress filters read back and judged against the plan derived for `dev` (flower, or
/// the u32 compatibility plan setup falls back to where flower is missing).
pub fn verify_holder_containment<D: DockerCli>(
    cli: &mut D,
    holder: &str,
    image: &str,
    labels: &[String],
    policy: &crate::sandbox_net::NetPolicy,
    dev: &str,
) -> Result<(), String> {
    use crate::sandbox_net::Family;
    for family in [Family::V4, Family::V6] {
        let argv = crate::sandbox_netns::with_helper_labels(
            crate::sandbox_netns::readback_argv(holder, image, family),
            labels,
        )?;
        let readback = cli
            .run(&argv)
            .map_err(|e| format!("{} readback: {e}", family.binary()))?;
        policy.verify_readback(family, &readback)?;
    }
    let iface = crate::sandbox_iface::IfacePlan::derive(dev, policy)?;
    let argv = crate::sandbox_netns::with_helper_labels(
        crate::sandbox_iface::filter_readback_argv(holder, image, dev),
        labels,
    )?;
    let readback = cli
        .run(&argv)
        .map_err(|e| format!("tc readback on {dev}: {e}"))?;
    match iface.verify_readback(&readback) {
        Ok(()) => Ok(()),
        Err(flower) => crate::sandbox_iface::U32Plan::derive(&iface)?
            .verify_readback(&readback)
            .map_err(|u32| {
                format!("egress filters on {dev} match neither the flower plan ({flower}) nor the u32 plan ({u32})")
            }),
    }
}

/// runsc only: put back the address and/or default route a gVisor sandbox took from the parked
/// holder's namespace, then re-verify the whole containment. `Ok(())` only when every check passes.
///
/// Order is the point:
/// 1. the namespace's own veth must be present and UP — otherwise nothing is written;
/// 2. docker's record of the holder must be usable — otherwise nothing is written;
/// 3. the firewall and the veth filters must already be exactly as setup installed them — so a
///    route is never added to a namespace that lacks its filters, even for an instant;
/// 4. only the missing piece is written, from docker's record, by the setup sidecar's shape;
/// 5. the same veth, the same filters, and the address-and-route probe are checked again.
fn restore_and_verify<D: DockerCli>(
    cli: &mut D,
    holder: &str,
    image: &str,
    labels: &[String],
    policy: &crate::sandbox_net::NetPolicy,
) -> Result<(), String> {
    let dev = holder_egress_dev(cli, holder, image, labels)
        .map_err(|e| format!("no restore, the holder's interface is not usable: {e}"))?;
    let recorded = cli
        .run(&holder_recorded_network_argv(holder))
        .map_err(|e| format!("no restore, holder inspect failed: {e}"))
        .and_then(|out| parse_recorded_network(&out).map_err(|e| format!("no restore, {e}")))?;
    verify_holder_containment(cli, holder, image, labels, policy, &dev)
        .map_err(|e| format!("no restore, containment is not as installed: {e}"))?;
    let state =
        read_net_state(cli, holder, image, labels).map_err(|e| format!("no restore, {e}"))?;
    let gateway = recorded.gateway.to_string();
    if state.addrs.iter().any(|a| *a != recorded.address) {
        return Err(format!(
            "no restore, the namespace holds an address docker did not record: {:?}",
            state.addrs
        ));
    }
    if let Some(via) = &state.default_via
        && *via != gateway
    {
        return Err(format!(
            "no restore, the namespace's default route is via {via:?}, docker recorded {gateway}"
        ));
    }
    if state.addrs.is_empty() {
        cli.run(&crate::sandbox_netns::restore_address_argv(
            holder,
            image,
            labels,
            &dev,
            recorded.address,
            recorded.prefix_len,
        ))
        .map_err(|e| format!("address restore failed: {e}"))?;
    }
    if state.default_via.is_none() {
        cli.run(&crate::sandbox_netns::restore_default_route_argv(
            holder,
            image,
            labels,
            &dev,
            recorded.gateway,
        ))
        .map_err(|e| format!("route restore failed: {e}"))?;
    }
    // Re-verify everything, from fresh reads.
    let dev_after = holder_egress_dev(cli, holder, image, labels)
        .map_err(|e| format!("re-verify: interface: {e}"))?;
    if dev_after != dev {
        return Err(format!(
            "re-verify: the egress interface changed from {dev} to {dev_after}"
        ));
    }
    verify_holder_containment(cli, holder, image, labels, policy, &dev)
        .map_err(|e| format!("re-verify: containment: {e}"))?;
    match observe_holder_network(cli, holder, image, labels) {
        HolderNetwork::Up => {}
        other => return Err(format!("re-verify: holder network is {other:?}")),
    }
    let after =
        read_net_state(cli, holder, image, labels).map_err(|e| format!("re-verify: {e}"))?;
    if after.addrs != [recorded.address] || after.default_via.as_deref() != Some(gateway.as_str()) {
        return Err(format!(
            "re-verify: namespace holds {:?} via {:?}, docker recorded {}/{} via {gateway}",
            after.addrs, after.default_via, recorded.address, recorded.prefix_len
        ));
    }
    Ok(())
}

/// The §5.5 holder network check at resume, with the runsc restore.
///
/// `runsc_policy` is `None` on a runc (or default-runtime) seat: the probe alone decides, exactly as
/// before. On a runsc seat it carries the containment policy turn 1 installed (or why it could not be
/// rebuilt); a namespace that lost its address or default route is then restored and re-verified
/// ([`restore_and_verify`]). Every other probe answer — `Up`, or a probe that could not answer — is
/// returned untouched. Returns the verdict and whether a restore was written and re-verified.
pub fn resume_holder_network<D: DockerCli>(
    cli: &mut D,
    holder: &str,
    image: &str,
    labels: &[String],
    runsc_policy: Option<Result<&crate::sandbox_net::NetPolicy, String>>,
) -> (HolderNetwork, bool) {
    let observed = observe_holder_network(cli, holder, image, labels);
    let Some(policy) = runsc_policy else {
        return (observed, false);
    };
    if !matches!(
        observed,
        HolderNetwork::NoAddress | HolderNetwork::NoDefaultRoute
    ) {
        return (observed, false);
    }
    let policy = match policy {
        Ok(policy) => policy,
        Err(error) => {
            return (
                HolderNetwork::RestoreFailed(format!(
                    "no restore, containment policy unavailable: {error}"
                )),
                false,
            );
        }
    };
    match restore_and_verify(cli, holder, image, labels, policy) {
        Ok(()) => (HolderNetwork::Up, true),
        Err(error) => (HolderNetwork::RestoreFailed(error), false),
    }
}

/// Bytes under `dir`, never following a symlink (a job could plant one to `/`). Unreadable
/// entries count zero; this is a measurement for the budget, not an audit.
pub fn du_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if meta.is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// The parked size (§5.5): `SizeRw` of the stopped container plus `du` of the host workdir. A
/// container size the daemon will not give is counted as zero and the workdir still counts.
pub fn measure_session<D: DockerCli>(cli: &mut D, container: &str, workdir: &Path) -> u64 {
    let size_rw = cli
        .run(&size_rw_argv(container))
        .ok()
        .and_then(|out| out.trim().parse::<u64>().ok())
        .unwrap_or(0);
    size_rw.saturating_add(du_bytes(workdir))
}

/// Paths a session owns on the host, removed with it (§5.3, Appendix A1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    pub workdir: PathBuf,
    pub resolv_conf: PathBuf,
    pub exchange_dir: PathBuf,
}

impl SessionPaths {
    pub fn for_job(home_root: &Path, job_id: &str) -> Self {
        let jobs = home_root.join("seller-jobs");
        Self {
            workdir: jobs.join(job_id),
            resolv_conf: jobs.join(format!("resolv-{job_id}.conf")),
            exchange_dir: home_root.join("seller-delivery").join(job_id),
        }
    }
}

/// What one GC pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub expired_lifetime: Vec<String>,
    pub evicted_disk: Vec<String>,
    /// Removals the daemon refused; the session is still marked expired (its container can never
    /// be started again: no record, no resume) and the next expiry sweep removes the holder.
    pub removal_errors: Vec<(String, String)>,
}

/// End a session: remove the job container, then the holder (a joiner pins the namespace, so
/// the container goes first), then the files, then mark the record `state`. A running session is
/// refused here as well as by the selectors, so a GC race can never remove a live turn.
pub fn end_session<D: DockerCli>(
    cli: &mut D,
    store: &SellerStore,
    record: &SessionRecord,
    paths: &SessionPaths,
    state: SessionState,
    now_unix: u64,
) -> Result<Vec<String>, StoreError> {
    if record.state == SessionState::Running && state == SessionState::Expired {
        return Ok(vec!["refused: session is running".into()]);
    }
    let mut errors = Vec::new();
    if let Err(error) = cli.run(&force_remove_argv(&record.container)) {
        errors.push(format!("container {}: {error}", record.container));
    }
    if let Some(holder) = &record.holder
        && let Err(error) = cli.run(&force_remove_argv(holder))
    {
        errors.push(format!("holder {holder}: {error}"));
    }
    let _ = std::fs::remove_dir_all(&paths.workdir);
    let _ = std::fs::remove_file(&paths.resolv_conf);
    let _ = std::fs::remove_dir_all(&paths.exchange_dir);
    store.set_session_state(&record.job_id, state, now_unix)?;
    Ok(errors)
}

/// Configuration a GC pass needs; built by the run loop only when `[sessions] enabled`.
#[derive(Clone)]
pub struct SessionGc {
    pub store: SellerStore,
    pub home_root: PathBuf,
    pub lifetime_secs: u64,
    pub disk_budget_bytes: u64,
}

impl SessionGc {
    /// One pass (§5.5): expire parked sessions past their lifetime, then evict the oldest parked
    /// sessions while the measured total exceeds the disk budget. Never touches a running one.
    pub fn pass<D: DockerCli>(&self, cli: &mut D, now_unix: u64) -> Result<GcReport, StoreError> {
        let mut report = GcReport::default();
        let sessions = self.store.sessions()?;
        let sizes: Vec<_> = sessions
            .iter()
            .filter(|s| s.state.is_live())
            .map(SessionRecord::size)
            .collect();
        report.expired_lifetime =
            session::expired_by_lifetime(&sizes, now_unix, self.lifetime_secs);
        let remaining: Vec<_> = sizes
            .into_iter()
            .filter(|s| !report.expired_lifetime.contains(&s.job_id))
            .collect();
        report.evicted_disk = session::evict_over_budget(&remaining, self.disk_budget_bytes);
        for job in report
            .expired_lifetime
            .iter()
            .chain(report.evicted_disk.iter())
        {
            let Some(record) = sessions.iter().find(|s| &s.job_id == job) else {
                continue;
            };
            let paths = SessionPaths::for_job(&self.home_root, job);
            for error in end_session(
                cli,
                &self.store,
                record,
                &paths,
                SessionState::Expired,
                now_unix,
            )? {
                report.removal_errors.push((job.clone(), error));
            }
        }
        Ok(report)
    }

    /// The keep-set for the expiry sweep, read AFTER the pass so a just-expired session's holder
    /// is swept like any other.
    pub fn keep(&self) -> std::collections::BTreeSet<String> {
        self.store.live_session_jobs().unwrap_or_default()
    }

    /// Re-measure one parked session and record it (§5.5, after each park). Returns the bytes.
    pub fn measure_and_record<D: DockerCli>(
        &self,
        cli: &mut D,
        job_id: &str,
        container: &str,
    ) -> Result<u64, StoreError> {
        let paths = SessionPaths::for_job(&self.home_root, job_id);
        let bytes = measure_session(cli, container, &paths.workdir);
        self.store.set_session_disk_bytes(job_id, bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every docker call; answers `SizeRw` with a fixed size and `rm` with success.
    struct Recording {
        calls: Vec<Vec<String>>,
        size_rw: String,
        holder: Result<String, String>,
    }
    impl Recording {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                size_rw: "1000".into(),
                holder: Ok("true 2026-10-21T10:00:00Z".into()),
            }
        }
        fn removed(&self) -> Vec<String> {
            self.calls
                .iter()
                .filter(|c| c.get(1).map(String::as_str) == Some("rm"))
                .filter_map(|c| c.last().cloned())
                .collect()
        }
    }
    impl DockerCli for Recording {
        fn run(&mut self, argv: &[String]) -> Result<String, String> {
            self.calls.push(argv.to_vec());
            match argv.get(1).map(String::as_str) {
                Some("container") => Ok(self.size_rw.clone()),
                Some("inspect") => self.holder.clone(),
                _ => Ok(String::new()),
            }
        }
    }

    fn record(job: &str, at: u64, bytes: u64, state: SessionState) -> SessionRecord {
        SessionRecord {
            job_id: job.into(),
            buyer_pubkey: "b".repeat(64),
            container: format!("maxplayer-job-{job}"),
            holder: Some(format!("maxplayer-netns-{job}")),
            holder_started_at: Some("2026-10-21T10:00:00Z".into()),
            image_id: "sha256:img".into(),
            orchestrator_version: session::SESSION_PROTOCOL.to_string(),
            acp_session_id: Some("acp".into()),
            harness: "claude".into(),
            model: None,
            turn: 1,
            last_turn_at: at,
            disk_bytes: bytes,
            state,
            proxy_host: None,
            base_oid: None,
        }
    }

    fn gc(tag: &str) -> (SessionGc, PathBuf) {
        let root = std::env::temp_dir().join(format!("mx-session-gc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let store = SellerStore::open(root.join("seller.sqlite")).expect("store");
        (
            SessionGc {
                store,
                home_root: root.clone(),
                lifetime_secs: 100,
                disk_budget_bytes: 2_500,
            },
            root,
        )
    }

    /// §5.5 lifetime + failure test 12 (disk): past its lifetime a parked session is expired
    /// (container, then holder, then files, then the record); over budget the OLDEST parked
    /// session is evicted; a running one never is, whatever its age or size.
    #[test]
    fn gc_expires_by_lifetime_and_evicts_oldest_parked_never_running() {
        let (gc, root) = gc("pass");
        for r in [
            record("old", 0, 500, SessionState::Parked),
            record("mid", 905, 1_500, SessionState::Parked),
            record("new", 950, 1_500, SessionState::Parked),
            record("run", 0, 9_000, SessionState::Running),
        ] {
            gc.store.put_session(&r).expect("put");
            let paths = SessionPaths::for_job(&root, &r.job_id);
            std::fs::create_dir_all(&paths.workdir).expect("workdir");
            std::fs::write(&paths.resolv_conf, "nameserver 1.1.1.1\n").expect("resolv");
        }
        let mut cli = Recording::new();
        let report = gc.pass(&mut cli, 1_000).expect("pass");
        assert_eq!(report.expired_lifetime, vec!["old".to_owned()]);
        // mid + new = 3000 > 2500: the older of the two goes; the running 9000 is not counted.
        assert_eq!(report.evicted_disk, vec!["mid".to_owned()]);
        assert_eq!(
            cli.removed(),
            vec![
                "maxplayer-job-old".to_owned(),
                "maxplayer-netns-old".to_owned(),
                "maxplayer-job-mid".to_owned(),
                "maxplayer-netns-mid".to_owned(),
            ],
            "the container goes before the holder it pins"
        );
        for gone in ["old", "mid"] {
            let paths = SessionPaths::for_job(&root, gone);
            assert!(!paths.workdir.exists() && !paths.resolv_conf.exists());
            assert_eq!(
                gc.store.session(gone).unwrap().unwrap().state,
                SessionState::Expired
            );
        }
        assert!(SessionPaths::for_job(&root, "run").workdir.exists());
        assert_eq!(
            gc.keep(),
            ["new".to_owned(), "run".to_owned()].into_iter().collect(),
            "the sweep keeps exactly the live sessions after the pass"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `end_session` refuses to expire a running session even when called directly.
    #[test]
    fn end_session_never_expires_a_running_turn() {
        let (gc, root) = gc("running");
        let r = record("live", 0, 0, SessionState::Running);
        gc.store.put_session(&r).expect("put");
        let mut cli = Recording::new();
        let paths = SessionPaths::for_job(&root, "live");
        let errors =
            end_session(&mut cli, &gc.store, &r, &paths, SessionState::Expired, 10).expect("end");
        assert_eq!(errors.len(), 1);
        assert!(cli.calls.is_empty(), "no docker call at all");
        assert_eq!(
            gc.store.session("live").unwrap().unwrap().state,
            SessionState::Running
        );
        // A buyer close of a parked session ends it as `closed`.
        let parked = record("p", 0, 0, SessionState::Parked);
        gc.store.put_session(&parked).expect("put");
        end_session(
            &mut cli,
            &gc.store,
            &parked,
            &SessionPaths::for_job(&root, "p"),
            SessionState::Closed,
            10,
        )
        .expect("close");
        assert_eq!(
            gc.store.session("p").unwrap().unwrap().state,
            SessionState::Closed
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The parked-size measurement: `SizeRw` plus the workdir, symlinks not followed.
    #[test]
    fn measurement_is_size_rw_plus_workdir_and_ignores_symlinks() {
        let (gc, root) = gc("measure");
        gc.store
            .put_session(&record("m", 0, 0, SessionState::Parked))
            .expect("put");
        let paths = SessionPaths::for_job(&root, "m");
        std::fs::create_dir_all(paths.workdir.join("sub")).expect("mkdir");
        std::fs::write(paths.workdir.join("a"), vec![0u8; 300]).expect("a");
        std::fs::write(paths.workdir.join("sub/b"), vec![0u8; 200]).expect("b");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", paths.workdir.join("root-link")).expect("link");
        let mut cli = Recording::new();
        let bytes = gc
            .measure_and_record(&mut cli, "m", "maxplayer-job-m")
            .expect("measure");
        assert_eq!(bytes, 1_000 + 500);
        assert_eq!(gc.store.session("m").unwrap().unwrap().disk_bytes, 1_500);
        assert_eq!(cli.calls[0], size_rw_argv("maxplayer-job-m"));
        // The daemon will not answer: the workdir still counts.
        cli.size_rw = "<no value>".into();
        assert_eq!(
            gc.measure_and_record(&mut cli, "m", "c").expect("measure"),
            500
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Failure test 6 (parse half): what `docker inspect` says about the holder.
    #[test]
    fn holder_state_parses_running_stopped_and_missing() {
        assert_eq!(
            parse_holder_state(Ok("true 2026-10-21T10:00:00.1Z\n".into())),
            HolderObservation::Running {
                started_at: "2026-10-21T10:00:00.1Z".into()
            }
        );
        assert_eq!(
            parse_holder_state(Ok("false 0001-01-01T00:00:00Z".into())),
            HolderObservation::NotRunning
        );
        assert_eq!(
            parse_holder_state(Err("Error: No such object: maxplayer-netns-x".into())),
            HolderObservation::Missing
        );
        assert_eq!(
            parse_holder_state(Ok(String::new())),
            HolderObservation::Missing
        );
        let mut cli = Recording::new();
        cli.holder = Ok("true 2026-10-22T00:00:00Z".into());
        // Restarted holder => the resume gate expires.
        assert_eq!(
            session::resume_gate(
                Some("2026-10-21T10:00:00Z"),
                &observe_holder(&mut cli, "h"),
                session::SESSION_PROTOCOL,
                &HolderNetwork::Up
            ),
            session::ResumeGate::Expire("holder_restarted")
        );
    }

    /// What `ip -4 -oneline addr show` printed in a healthy holder, and what the runsc sweep saw.
    const ADDRS_UP: &str = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n\
         2: eth0    inet 172.18.0.5/16 brd 172.18.255.255 scope global eth0\\       valid_lft forever preferred_lft forever\n";
    const ADDRS_LO_ONLY: &str =
        "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n";

    #[test]
    fn holder_network_is_judged_from_recorded_ip_addresses_and_default_route() {
        let ok = |s: &str| Ok::<String, String>(s.to_owned());
        assert_eq!(
            judge_holder_network(
                ok("172.18.0.5 "),
                ok(ADDRS_UP),
                ok("default via 172.18.0.1 dev eth0")
            ),
            HolderNetwork::Up
        );
        // The runsc failure: only `lo` left, no routes.
        assert_eq!(
            judge_holder_network(ok("172.18.0.5 "), ok(ADDRS_LO_ONLY), ok("")),
            HolderNetwork::NoAddress
        );
        // An address, but not the one docker recorded for the holder.
        assert_eq!(
            judge_holder_network(
                ok("172.18.0.9 "),
                ok(ADDRS_UP),
                ok("default via 172.18.0.1 dev eth0")
            ),
            HolderNetwork::NoAddress
        );
        // Nothing recorded: any non-loopback address will do.
        assert_eq!(
            judge_holder_network(ok(" "), ok(ADDRS_UP), ok("default via 172.18.0.1 dev eth0")),
            HolderNetwork::Up
        );
        assert_eq!(
            judge_holder_network(ok("172.18.0.5"), ok(ADDRS_UP), ok("")),
            HolderNetwork::NoDefaultRoute
        );
        // Every probe error fails closed.
        for (a, b, c) in [
            (Err("x".to_owned()), ok(ADDRS_UP), ok("default via x")),
            (ok("172.18.0.5"), Err("x".to_owned()), ok("default via x")),
            (ok("172.18.0.5"), ok(ADDRS_UP), Err("x".to_owned())),
        ] {
            assert!(matches!(
                judge_holder_network(a, b, c),
                HolderNetwork::ProbeFailed(_)
            ));
        }
    }

    /// Answers the holder-network probe as a fake daemon would, recording every call.
    struct NetProbe {
        calls: Vec<Vec<String>>,
        ip: Result<String, String>,
        addrs: Result<String, String>,
        routes: Result<String, String>,
    }
    impl DockerCli for NetProbe {
        fn run(&mut self, argv: &[String]) -> Result<String, String> {
            self.calls.push(argv.to_vec());
            match (
                argv.get(1).map(String::as_str),
                argv.iter().rev().nth(1).map(String::as_str),
            ) {
                (Some("inspect"), _) => self.ip.clone(),
                (Some("run"), Some("addr")) => self.addrs.clone(),
                (Some("run"), _) => self.routes.clone(),
                _ => Err("unexpected".into()),
            }
        }
    }

    #[test]
    fn holder_network_probe_joins_the_holder_unprivileged_and_fails_closed() {
        let labels = crate::sandbox_netns::helper_label_args("j", "seat", 99);
        let mut cli = NetProbe {
            calls: Vec::new(),
            ip: Ok("172.18.0.5 ".into()),
            addrs: Ok(ADDRS_UP.into()),
            routes: Ok("default via 172.18.0.1 dev eth0".into()),
        };
        assert_eq!(
            observe_holder_network(&mut cli, "maxplayer-netns-j", "img", &labels),
            HolderNetwork::Up
        );
        assert_eq!(cli.calls.len(), 3);
        for probe in &cli.calls[1..] {
            assert!(
                probe.contains(&"container:maxplayer-netns-j".to_owned()),
                "{probe:?}"
            );
            assert!(probe.contains(&"--rm".to_owned()));
            assert!(
                !probe.iter().any(|a| a == "NET_ADMIN"),
                "a reader needs no capability"
            );
            assert!(!probe.iter().any(|a| a == "nsenter"));
            assert!(probe.windows(2).any(|w| w == ["--entrypoint", "ip"]));
            for label in &labels {
                assert!(probe.contains(label), "helper labels: {probe:?}");
            }
        }
        // The runsc failure, measured: lo only.
        cli.addrs = Ok(ADDRS_LO_ONLY.into());
        cli.routes = Ok(String::new());
        assert_eq!(
            observe_holder_network(&mut cli, "maxplayer-netns-j", "img", &labels),
            HolderNetwork::NoAddress
        );
        // A daemon that cannot run the probe: fail closed, and stop at the first error.
        cli.calls.clear();
        cli.addrs = Err("exit 125: no such image".into());
        assert!(matches!(
            observe_holder_network(&mut cli, "maxplayer-netns-j", "img", &labels),
            HolderNetwork::ProbeFailed(_)
        ));
        assert_eq!(cli.calls.len(), 2);
        // Through the gate: that expires the session.
        assert_eq!(
            session::resume_gate(
                Some("t"),
                &HolderObservation::Running {
                    started_at: "t".into()
                },
                session::SESSION_PROTOCOL,
                &HolderNetwork::ProbeFailed("x".into())
            ),
            session::ResumeGate::Expire("holder_network_lost")
        );
    }

    // ── runsc restore at resume ─────────────────────────────────────────────────────────────

    const LINKS_UP: &str = "1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN mode DEFAULT group default qlen 1000\\    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00 promiscuity 0 minmtu 0 maxmtu 0 numtxqueues 1 numrxqueues 1 gso_max_size 65536 gso_max_segs 65535 \n\
19: eth0@if20: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP mode DEFAULT group default \\    link/ether 02:42:ac:13:00:05 brd ff:ff:ff:ff:ff:ff link-netnsid 0 promiscuity 0 minmtu 68 maxmtu 65535 veth numtxqueues 4 numrxqueues 4 gso_max_size 65536 gso_max_segs 65535 \n";
    const LINKS_LO_ONLY: &str = "1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN mode DEFAULT group default qlen 1000\\    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00 promiscuity 0 minmtu 0 maxmtu 0 numtxqueues 1 numrxqueues 1 gso_max_size 65536 gso_max_segs 65535 \n";
    const HOLDER: &str = "maxplayer-netns-j";

    fn runsc_policy() -> crate::sandbox_net::NetPolicy {
        crate::sandbox_net::NetPolicy {
            gateway: "172.17.0.1".into(),
            proxy_ports: Some(crate::sandbox_net::PortRange::new(9320, 9325).unwrap()),
            log_connections: true,
            dns_resolvers: vec!["1.1.1.1".into()],
        }
    }

    /// A fake daemon holding one parked runsc holder's namespace: answers the probes from its state,
    /// applies `ip … add` writes to it, and records every call.
    struct Ns {
        calls: Vec<Vec<String>>,
        links: String,
        recorded: Result<String, String>,
        addr: bool,
        route: bool,
        v4: String,
        v6: String,
        tc: String,
        /// `Some(e)`: every `ip … add` write fails with `e` and changes nothing.
        write_error: Option<String>,
        /// Writes report success but do not take (re-verify must catch it).
        write_noop: bool,
        /// A write wipes the veth filters (re-verify must catch it).
        write_wipes_tc: bool,
    }
    impl Ns {
        fn parked(addr: bool, route: bool) -> Self {
            let policy = runsc_policy();
            let plan = crate::sandbox_iface::IfacePlan::derive("eth0", &policy).unwrap();
            Self {
                calls: Vec::new(),
                links: LINKS_UP.into(),
                recorded: Ok("172.19.0.5/16 172.19.0.1\n".into()),
                addr,
                route,
                v4: crate::sandbox_net::readback_fixture(&policy, crate::sandbox_net::Family::V4),
                v6: crate::sandbox_net::readback_fixture(&policy, crate::sandbox_net::Family::V6),
                tc: crate::sandbox_iface::readback_fixture(&plan),
                write_error: None,
                write_noop: false,
                write_wipes_tc: false,
            }
        }
        fn writes(&self) -> Vec<Vec<String>> {
            self.calls
                .iter()
                .filter(|c| {
                    c.windows(2)
                        .any(|w| w[1] == "add" && (w[0] == "addr" || w[0] == "route"))
                })
                .cloned()
                .collect()
        }
        fn first_index(&self, pred: impl Fn(&[String]) -> bool) -> Option<usize> {
            self.calls.iter().position(|c| pred(c))
        }
    }
    impl DockerCli for Ns {
        fn run(&mut self, argv: &[String]) -> Result<String, String> {
            self.calls.push(argv.to_vec());
            let has = |w: &[&str]| argv.windows(w.len()).any(|x| x == w);
            if argv.get(1).map(String::as_str) == Some("inspect") {
                return if argv.iter().any(|a| a.contains("IPPrefixLen")) {
                    self.recorded.clone()
                } else {
                    Ok("172.19.0.5 ".into())
                };
            }
            let entry = argv
                .windows(2)
                .find(|w| w[0] == "--entrypoint")
                .map(|w| w[1].clone())
                .unwrap_or_default();
            match entry.as_str() {
                "iptables" => Ok(self.v4.clone()),
                "ip6tables" => Ok(self.v6.clone()),
                "tc" => Ok(self.tc.clone()),
                "ip" if has(&["link", "show"]) => Ok(self.links.clone()),
                "ip" if has(&["addr", "show"]) => Ok(if self.addr {
                    "1: lo    inet 127.0.0.1/8 scope host lo\n19: eth0    inet 172.19.0.5/16 brd 172.19.255.255 scope global eth0\n".into()
                } else {
                    "1: lo    inet 127.0.0.1/8 scope host lo\n".into()
                }),
                "ip" if has(&["route", "show", "default"]) => Ok(if self.route {
                    "default via 172.19.0.1 dev eth0 \n".into()
                } else {
                    String::new()
                }),
                "ip" if has(&["addr", "add"]) || has(&["route", "add"]) => {
                    if let Some(error) = &self.write_error {
                        return Err(error.clone());
                    }
                    if self.write_wipes_tc {
                        self.tc.clear();
                    }
                    if !self.write_noop {
                        if has(&["addr", "add"]) {
                            self.addr = true;
                        } else {
                            self.route = true;
                        }
                    }
                    Ok(String::new())
                }
                _ => Err(format!("unexpected argv {argv:?}")),
            }
        }
    }

    fn labels() -> Vec<String> {
        crate::sandbox_netns::helper_label_args("j", "seat", 99)
    }

    fn resume(
        ns: &mut Ns,
        policy: Option<Result<&crate::sandbox_net::NetPolicy, String>>,
    ) -> (HolderNetwork, bool) {
        resume_holder_network(ns, HOLDER, "img", &labels(), policy)
    }

    /// The measured runsc state (address AND route gone): both are put back from docker's record
    /// with the setup sidecar's shape, only after the filters were verified, and re-verified after.
    #[test]
    fn runsc_restore_puts_back_docker_recorded_address_and_route_then_reverifies() {
        let policy = runsc_policy();
        let mut ns = Ns::parked(false, false);
        assert_eq!(
            resume(&mut ns, Some(Ok(&policy))),
            (HolderNetwork::Up, true)
        );
        let writes = ns.writes();
        assert_eq!(writes.len(), 2, "{writes:?}");
        assert!(
            writes[0].ends_with(
                &[
                    "--entrypoint",
                    "ip",
                    "img",
                    "-4",
                    "addr",
                    "add",
                    "172.19.0.5/16",
                    "brd",
                    "+",
                    "dev",
                    "eth0"
                ]
                .map(String::from)
            ),
            "{:?}",
            writes[0]
        );
        assert!(
            writes[1].ends_with(
                &[
                    "--entrypoint",
                    "ip",
                    "img",
                    "-4",
                    "route",
                    "add",
                    "default",
                    "via",
                    "172.19.0.1",
                    "dev",
                    "eth0"
                ]
                .map(String::from)
            ),
            "{:?}",
            writes[1]
        );
        for w in &writes {
            assert!(
                w.windows(2)
                    .any(|p| p == ["--network", "container:maxplayer-netns-j"])
            );
            let caps: Vec<_> = w
                .windows(2)
                .filter(|p| p[0] == "--cap-add")
                .map(|p| p[1].clone())
                .collect();
            assert_eq!(caps, ["NET_ADMIN"]);
            for l in &labels() {
                assert!(w.contains(l));
            }
        }
        // Filters verified (both iptables families and tc) BEFORE the first write…
        let first_write = ns
            .first_index(|c| c.windows(2).any(|w| w == ["addr", "add"]))
            .unwrap();
        for entry in ["iptables", "ip6tables", "tc"] {
            let read = ns
                .first_index(|c| c.windows(2).any(|w| w == ["--entrypoint", entry]))
                .unwrap();
            assert!(
                read < first_write,
                "{entry} read at {read}, first write at {first_write}"
            );
        }
        // …and again after the last one.
        let last_write = ns
            .calls
            .iter()
            .rposition(|c| c.windows(2).any(|w| w == ["route", "add"]))
            .unwrap();
        for entry in ["iptables", "ip6tables", "tc"] {
            assert!(
                ns.calls[last_write..]
                    .iter()
                    .any(|c| c.windows(2).any(|w| w == ["--entrypoint", entry])),
                "{entry} not re-verified after the restore"
            );
        }
        // Every read helper also carries the job's helper labels.
        for c in ns
            .calls
            .iter()
            .filter(|c| c.get(1).map(String::as_str) == Some("run"))
        {
            for l in &labels() {
                assert!(c.contains(l), "{c:?}");
            }
        }
    }

    #[test]
    fn runsc_restore_writes_only_the_missing_address() {
        let policy = runsc_policy();
        let mut ns = Ns::parked(false, true);
        assert_eq!(
            resume(&mut ns, Some(Ok(&policy))),
            (HolderNetwork::Up, true)
        );
        let writes = ns.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert!(writes[0].windows(2).any(|w| w == ["addr", "add"]));
    }

    #[test]
    fn runsc_restore_writes_only_the_missing_route() {
        let policy = runsc_policy();
        let mut ns = Ns::parked(true, false);
        assert_eq!(
            resume(&mut ns, Some(Ok(&policy))),
            (HolderNetwork::Up, true)
        );
        let writes = ns.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert!(writes[0].windows(2).any(|w| w == ["route", "add"]));
    }

    /// No veth (or one that is down): expire with no restore attempt.
    #[test]
    fn runsc_missing_interface_expires_without_a_restore() {
        let policy = runsc_policy();
        for links in [
            LINKS_LO_ONLY.to_owned(),
            LINKS_UP.replace("<BROADCAST,MULTICAST,UP,LOWER_UP>", "<BROADCAST,MULTICAST>"),
        ] {
            let mut ns = Ns::parked(false, false);
            ns.links = links;
            let (network, restored) = resume(&mut ns, Some(Ok(&policy)));
            assert!(
                matches!(&network, HolderNetwork::RestoreFailed(e) if e.contains("interface")),
                "{network:?}"
            );
            assert!(!restored);
            assert!(ns.writes().is_empty());
            assert_eq!(
                session::resume_gate(
                    Some("t"),
                    &HolderObservation::Running {
                        started_at: "t".into()
                    },
                    session::SESSION_PROTOCOL,
                    &network
                ),
                session::ResumeGate::Expire("holder_network_lost")
            );
        }
    }

    /// Docker's record unusable (none, two networks, garbage, inspect error): no restore.
    #[test]
    fn runsc_unavailable_recorded_address_expires_without_a_restore() {
        let policy = runsc_policy();
        for recorded in [
            Ok("/0 \n".to_owned()),
            Ok(String::new()),
            Ok("172.19.0.5/16 172.19.0.1\n10.0.0.5/24 10.0.0.1\n".to_owned()),
            Ok("172.19.0.5/16 10.9.9.9\n".to_owned()),
            Err("exit 1: No such object".to_owned()),
        ] {
            let mut ns = Ns::parked(false, false);
            ns.recorded = recorded.clone();
            let (network, restored) = resume(&mut ns, Some(Ok(&policy)));
            assert!(
                matches!(network, HolderNetwork::RestoreFailed(_)),
                "{recorded:?}: {network:?}"
            );
            assert!(!restored);
            assert!(ns.writes().is_empty(), "{recorded:?}");
        }
    }

    /// Filters not as installed before the restore: nothing is written, so a route is never added to
    /// a namespace without its filters.
    #[test]
    fn runsc_namespace_without_its_filters_is_never_given_a_route() {
        let policy = runsc_policy();
        let cases: [fn(&mut Ns); 4] = [
            |ns| ns.tc.clear(),
            |ns| ns.v4 = ns.v4.replace("-j DROP", "-j ACCEPT"),
            |ns| ns.v6.clear(),
            |ns| ns.v4 = ns.v4.replace("9320:9325", "1:65535"),
        ];
        for (i, break_it) in cases.into_iter().enumerate() {
            let mut ns = Ns::parked(false, false);
            break_it(&mut ns);
            let (network, restored) = resume(&mut ns, Some(Ok(&policy)));
            assert!(
                matches!(&network, HolderNetwork::RestoreFailed(e) if e.contains("containment")),
                "case {i}: {network:?}"
            );
            assert!(!restored);
            assert!(ns.writes().is_empty(), "case {i}");
        }
        // A policy that could not be rebuilt (e.g. no resolver file) also writes nothing.
        let mut ns = Ns::parked(false, false);
        let (network, _) = resume(
            &mut ns,
            Some(Err("the session's resolver file is missing".into())),
        );
        assert!(matches!(network, HolderNetwork::RestoreFailed(_)));
        assert!(ns.writes().is_empty());
    }

    /// A restore that did not take, or that cost the veth its filters, fails re-verification.
    #[test]
    fn runsc_reverify_mismatch_expires() {
        let policy = runsc_policy();
        let mut ns = Ns::parked(false, false);
        ns.write_wipes_tc = true;
        let (network, restored) = resume(&mut ns, Some(Ok(&policy)));
        assert!(
            matches!(&network, HolderNetwork::RestoreFailed(e) if e.starts_with("re-verify")),
            "{network:?}"
        );
        assert!(!restored);
        let mut ns = Ns::parked(false, true);
        ns.write_noop = true;
        let (network, _) = resume(&mut ns, Some(Ok(&policy)));
        assert!(
            matches!(&network, HolderNetwork::RestoreFailed(e) if e.starts_with("re-verify")),
            "{network:?}"
        );
        assert_eq!(
            session::resume_gate(
                Some("t"),
                &HolderObservation::Running {
                    started_at: "t".into()
                },
                session::SESSION_PROTOCOL,
                &network
            ),
            session::ResumeGate::Expire("holder_network_lost")
        );
    }

    #[test]
    fn runsc_helper_error_expires() {
        let policy = runsc_policy();
        let mut ns = Ns::parked(false, false);
        ns.write_error = Some("exit 2: RTNETLINK answers: Operation not permitted".into());
        let (network, restored) = resume(&mut ns, Some(Ok(&policy)));
        assert!(
            matches!(&network, HolderNetwork::RestoreFailed(e) if e.contains("address restore failed")),
            "{network:?}"
        );
        assert!(!restored);
        // Stops at the failed write: nothing after it.
        assert_eq!(ns.writes().len(), 1);
    }

    /// A foreign address or a default route to somewhere docker did not record is not "missing": no
    /// restore over it.
    #[test]
    fn runsc_unexpected_namespace_state_is_not_overwritten() {
        let policy = runsc_policy();
        let mut ns = Ns::parked(false, false);
        ns.route = true;
        ns.recorded = Ok("172.19.0.5/16 172.19.0.254\n".into());
        let (network, _) = resume(&mut ns, Some(Ok(&policy)));
        assert!(
            matches!(&network, HolderNetwork::RestoreFailed(e) if e.contains("default route")),
            "{network:?}"
        );
        assert!(ns.writes().is_empty());
    }

    /// runc (no runsc policy): the probe alone decides; no link probe, readback or write is issued.
    #[test]
    fn runc_resume_is_probe_only_and_untouched() {
        for (addr, route, want) in [
            (true, true, HolderNetwork::Up),
            (false, false, HolderNetwork::NoAddress),
            (true, false, HolderNetwork::NoDefaultRoute),
        ] {
            let mut ns = Ns::parked(addr, route);
            assert_eq!(resume(&mut ns, None), (want, false));
            assert_eq!(ns.calls.len(), 3, "{:?}", ns.calls);
            assert!(ns.writes().is_empty());
            assert!(!ns.calls.iter().any(|c| c.iter().any(|a| a == "NET_ADMIN")));
        }
        // A healthy runsc holder is not touched either: probe only.
        let policy = runsc_policy();
        let mut ns = Ns::parked(true, true);
        assert_eq!(
            resume(&mut ns, Some(Ok(&policy))),
            (HolderNetwork::Up, false)
        );
        assert_eq!(ns.calls.len(), 3);
        // Nor is one whose probe could not answer.
        let mut failing = NetProbe {
            calls: Vec::new(),
            ip: Err("daemon down".into()),
            addrs: Ok(String::new()),
            routes: Ok(String::new()),
        };
        let (network, restored) =
            resume_holder_network(&mut failing, HOLDER, "img", &labels(), Some(Ok(&policy)));
        assert!(matches!(network, HolderNetwork::ProbeFailed(_)));
        assert!(!restored);
        assert_eq!(failing.calls.len(), 1);
    }

    #[test]
    fn recorded_network_parses_only_one_usable_on_link_record() {
        assert_eq!(
            parse_recorded_network("172.19.0.5/16 172.19.0.1\n"),
            Ok(RecordedNetwork {
                address: "172.19.0.5".parse().unwrap(),
                prefix_len: 16,
                gateway: "172.19.0.1".parse().unwrap(),
            })
        );
        for bad in [
            "",
            "/0 ",
            "172.19.0.5/16 ",
            "172.19.0.5/0 172.19.0.1",
            "172.19.0.5/32 172.19.0.1",
            "172.19.0.5/16 10.0.0.1",
            "172.19.0.5/16 172.19.0.5",
            "fe80::1/64 fe80::2",
        ] {
            assert!(parse_recorded_network(bad).is_err(), "{bad:?}");
        }
        assert!(
            holder_recorded_network_argv("h")
                .iter()
                .any(|a| a.contains("IPPrefixLen") && a.contains("Gateway"))
        );
    }

    #[test]
    fn the_containment_policy_is_rebuilt_from_the_session_record_and_resolver_file() {
        let ports = Some(crate::sandbox_net::PortRange::new(9320, 9325).unwrap());
        let resolv = "# Written by maxplayer\nnameserver 1.1.1.1\noptions ndots:0\n";
        assert_eq!(
            session_containment_policy(Some("172.17.0.1"), ports, Some(resolv)),
            Ok(runsc_policy())
        );
        assert!(session_containment_policy(None, ports, Some(resolv)).is_err());
        assert!(session_containment_policy(Some("172.17.0.1"), ports, None).is_err());
    }

    // ── bounded docker calls at resume ──────────────────────────────────────────────────────

    fn ms(n: u64) -> std::time::Duration {
        std::time::Duration::from_millis(n)
    }

    /// Finished in time: the same output shape `RealDockerCli` gives, so nothing downstream changes.
    #[test]
    fn bounded_docker_call_in_time_reads_like_the_unbounded_one() {
        let mut ok = BoundedDockerCli::stand_in(
            &["sh", "-c", "echo ' 172.19.0.5 '; echo noise >&2"],
            ms(5_000),
            ms(10_000),
        );
        assert_eq!(
            ok.run(&["docker".into(), "inspect".into()]),
            Ok("172.19.0.5".into())
        );
        let mut refused = BoundedDockerCli::stand_in(
            &["sh", "-c", "echo out; echo 'No such object' >&2; exit 3"],
            ms(5_000),
            ms(10_000),
        );
        assert_eq!(
            refused.run(&["docker".into()]),
            Err("exit 3: No such object".into())
        );
    }

    /// A docker call that never returns is killed at its bound and reported as a timeout, and a call
    /// issued after the check's total budget is spent is refused before it starts.
    #[test]
    fn bounded_docker_call_times_out_and_the_spent_budget_refuses_the_next() {
        let mut hung = BoundedDockerCli::stand_in(&["sh", "-c", "sleep 30"], ms(200), ms(400));
        let started = std::time::Instant::now();
        let first = hung.run(&["docker".into(), "inspect".into(), "h".into()]);
        assert!(
            matches!(&first, Err(e) if e.starts_with("timed out") && e.contains("killed")),
            "{first:?}"
        );
        assert!(
            started.elapsed() < ms(5_000),
            "the bound was not enforced: {:?}",
            started.elapsed()
        );
        // The second call gets only what is left of the 400ms budget, the third gets nothing.
        let second = hung.run(&["docker".into()]);
        assert!(
            matches!(&second, Err(e) if e.starts_with("timed out")),
            "{second:?}"
        );
        let third = hung.run(&["docker".into()]);
        assert!(
            matches!(&third, Err(e) if e.contains("was not started")),
            "{third:?}"
        );
        assert!(started.elapsed() < ms(5_000));
    }

    /// A probe that times out expires the session with `holder_network_lost`, on runc and runsc
    /// alike, and nothing is written.
    #[test]
    fn a_timed_out_holder_network_probe_expires_the_session() {
        let policy = runsc_policy();
        for runsc in [false, true] {
            let mut hung =
                BoundedDockerCli::stand_in(&["sh", "-c", "sleep 30"], ms(150), ms(10_000));
            let (network, restored) = resume_holder_network(
                &mut hung,
                HOLDER,
                "img",
                &labels(),
                runsc.then_some(Ok(&policy)),
            );
            assert!(
                matches!(&network, HolderNetwork::ProbeFailed(e) if e.contains("timed out")),
                "{network:?}"
            );
            assert!(!restored);
            assert_eq!(
                session::resume_gate(
                    Some("t"),
                    &HolderObservation::Running {
                        started_at: "t".into()
                    },
                    session::SESSION_PROTOCOL,
                    &network
                ),
                session::ResumeGate::Expire("holder_network_lost")
            );
        }
    }

    /// A runsc restore whose write hangs is killed at its bound and the session expires with
    /// `holder_network_lost`; nothing after the timed-out write runs.
    #[test]
    fn a_timed_out_runsc_restore_write_expires_the_session() {
        struct HangOnWrite {
            ns: Ns,
            hung: BoundedDockerCli,
        }
        impl DockerCli for HangOnWrite {
            fn run(&mut self, argv: &[String]) -> Result<String, String> {
                if argv
                    .windows(2)
                    .any(|w| w[1] == "add" && (w[0] == "addr" || w[0] == "route"))
                {
                    self.ns.calls.push(argv.to_vec());
                    return self.hung.run(argv);
                }
                self.ns.run(argv)
            }
        }
        let policy = runsc_policy();
        let mut cli = HangOnWrite {
            ns: Ns::parked(false, false),
            hung: BoundedDockerCli::stand_in(&["sh", "-c", "sleep 30"], ms(150), ms(10_000)),
        };
        let (network, restored) =
            resume_holder_network(&mut cli, HOLDER, "img", &labels(), Some(Ok(&policy)));
        assert!(
            matches!(&network, HolderNetwork::RestoreFailed(e) if e.contains("address restore failed") && e.contains("timed out")),
            "{network:?}"
        );
        assert!(!restored);
        assert_eq!(cli.ns.writes().len(), 1, "stopped at the timed-out write");
        assert_eq!(
            session::resume_gate(
                Some("t"),
                &HolderObservation::Running {
                    started_at: "t".into()
                },
                session::SESSION_PROTOCOL,
                &network
            ),
            session::ResumeGate::Expire("holder_network_lost")
        );
    }
}
