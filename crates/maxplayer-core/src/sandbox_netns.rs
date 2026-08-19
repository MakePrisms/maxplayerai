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

use crate::sandbox_net::NetPolicy;

/// The containment sidecar image, pinned to this build's version exactly as
/// [`crate::seller_exec::DEFAULT_SANDBOX_IMAGE`] is. Both images are published by the same workflow
/// job on the same tag: a version that shipped one but not the other cannot start a contained job at
/// all, so they are deliberately impossible to skew.
pub const DEFAULT_NETFILTER_IMAGE: &str =
    concat!("ghcr.io/makeprisms/maxplayer-netfilter:v", env!("CARGO_PKG_VERSION"));

/// The docker label every holder carries, so an orphan left by a crashed daemon can be found and
/// reaped by something that never saw the job that created it.
pub const HOLDER_LABEL: &str = "ai.maxplayer.netns-holder";

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
    pub fn network_mode(&self) -> String {
        format!("container:{}", self.name)
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
    /// [`reap_orphans_argv`] is the backstop for the case where this did not work.
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
pub fn holder_argv(
    name: &str,
    network: &str,
    image: &str,
    uid: u32,
    gid: u32,
    job_id: &str,
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

/// `docker` argv listing holder containers, for orphan reaping at seller start-up.
pub fn reap_orphans_argv() -> Vec<String> {
    [
        "docker",
        "ps",
        "--all",
        "--quiet",
        "--filter",
        &format!("label={HOLDER_LABEL}"),
    ]
    .into_iter()
    .map(String::from)
    .collect()
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
    run_docker(holder_argv(&name, network, holder_image, uid, gid, job_id), None)
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

    #[test]
    fn the_job_joins_the_holders_namespace_and_never_names_a_network() {
        let holder = NetnsHolder::adopt("maxplayer-netns-abc".into());
        assert_eq!(holder.network_mode(), "container:maxplayer-netns-abc");
    }

    #[test]
    fn the_holder_runs_sleep_in_exec_form_with_no_shell() {
        let argv = holder_argv("h", "net", "img", 1000, 1000, "abc");
        let tail = &argv[argv.len() - 4..];
        assert_eq!(tail, ["--entrypoint", "sleep", "img", "infinity"]);
        // A shell anywhere in the argv would mean the holder runs something that parses a string.
        assert!(!argv.iter().any(|a| a == "sh" || a == "bash" || a == "-c"), "{argv:?}");
    }

    #[test]
    fn the_holder_is_locked_down_and_labelled_for_reaping() {
        let argv = holder_argv("h", "net", "img", 1000, 1000, "abc");
        for expected in ["--read-only", "--cap-drop", "ALL", "no-new-privileges"] {
            assert!(argv.iter().any(|a| a == expected), "missing {expected} in {argv:?}");
        }
        assert!(argv.iter().any(|a| a == "ai.maxplayer.netns-holder=abc"), "{argv:?}");
        // The reaper must be able to find what the holder was labelled with.
        let filter = reap_orphans_argv();
        assert!(filter.iter().any(|a| a == "label=ai.maxplayer.netns-holder"), "{filter:?}");
    }

    #[test]
    fn only_the_sidecar_is_granted_net_admin() {
        let holder = NetnsHolder::adopt("h".into());
        let sidecar = sidecar_argv(&holder, "netfilter");
        assert!(sidecar.windows(2).any(|w| w == ["--cap-add", "NET_ADMIN"]), "{sidecar:?}");
        // …and it still drops everything else first, so the grant is exactly one capability.
        assert!(sidecar.windows(2).any(|w| w == ["--cap-drop", "ALL"]), "{sidecar:?}");
        // The holder must never carry it: it shares its namespace with the job.
        let holder_argv = holder_argv("h", "net", "img", 1000, 1000, "abc");
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
