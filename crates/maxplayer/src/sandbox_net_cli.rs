//! `maxplayer sandbox-net` — install, check and remove the host-side egress rules for docker jobs (#797).
//!
//! The daemon puts a job's container on a dedicated network ([`SandboxConfig::network`]). That is a
//! precondition, not the containment: joining a network denies nothing. The denial is host-side
//! firewall rules, and this is the surface that installs and — more importantly — CHECKS them.
//!
//! ── Why `verify` is the point of this command ────────────────────────────────────────────────────
//! A seat with `network` configured and no rules installed is indistinguishable, from inside a job,
//! from a seat that is fully contained. Nothing about the job's behaviour differs until the moment
//! something reaches the seller's LAN, and by then the reaching has happened. So the rules cannot be
//! "install once and assume" — a host reboot, a docker daemon restart that recreates the bridge, or
//! a `nixos-rebuild` on a declaratively-managed box all drop them silently.
//!
//! [`verify`] compares the LIVE ruleset against the rendered policy and exits non-zero on any
//! difference. It is meant to be run by the seller's boot doctor and by an operator's monitoring,
//! and its failure is loud precisely because the failure it detects is silent.
//!
//! ── What this command will not do ───────────────────────────────────────────────────────────────
//! It creates two chains of its own and one jump into each parent. It NEVER edits, reorders, flushes
//! or deletes a rule it did not create, and every rule it writes is scoped to the sandbox bridge
//! interface. A seller box runs real services — a Lightning node, a relay, a web server — and this
//! must be incapable of touching their traffic even when it is wrong.

use std::io::Write;
use std::process::Command;

use maxplayer_core::sandbox_net::{Chain, NetPolicy, PortRange};

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
/// A verify mismatch, or an apply/revert that did not complete. Distinct from a usage error so a
/// monitor can tell "you called it wrong" from "the box is not contained".
const NOT_CONTAINED: i32 = 2;

const USAGE: &str = "\
Usage:
  maxplayer sandbox-net plan     [--network <name>] [--bridge <iface>] [--gateway <addr>] [--ports <a-b>]
  maxplayer sandbox-net apply    [same flags]
  maxplayer sandbox-net verify   [same flags]
  maxplayer sandbox-net revert   [same flags]

Host-side egress containment for docker jobs (#797): deny the LAN and the seller's own host
services, keep the public internet open, and keep one pinhole to the #647 credential proxy.

  plan     print the rules and why each exists; changes nothing
  apply    install them (needs root)
  verify   compare the live ruleset against the policy; exit 2 if it does not match
  revert   remove exactly what apply added (needs root)

The network name defaults to `[sandbox] network` from the seller config, and the proxy port range to
`[sandbox] proxy_port_range`. The bridge interface and gateway are read from `docker network
inspect` unless given explicitly.
";

/// Parsed flags. Every one has a discovery path; the flags exist so an operator can run this against
/// a network the config does not name yet, and so tests need no docker daemon.
#[derive(Default)]
struct Args {
    network: Option<String>,
    bridge: Option<String>,
    gateway: Option<String>,
    ports: Option<String>,
    no_log: bool,
}

pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(action) = args.first().map(String::as_str) else {
        let _ = write!(err, "{USAGE}");
        return USAGE_ERROR;
    };
    if matches!(action, "--help" | "-h" | "help") {
        let _ = write!(out, "{USAGE}");
        return SUCCESS;
    }
    // The action is validated BEFORE any config or docker lookup. Resolving first would report a
    // missing-network error for `sandbox-net wat`, which sends an operator to fix a config that was
    // never the problem.
    if !matches!(action, "plan" | "apply" | "verify" | "revert") {
        let _ = writeln!(err, "unknown sandbox-net action: {action}");
        let _ = write!(err, "{USAGE}");
        return USAGE_ERROR;
    }
    let parsed = match parse_flags(&args[1..]) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            let _ = write!(err, "{USAGE}");
            return USAGE_ERROR;
        }
    };
    let (policy, facts) = match resolve_policy(&parsed) {
        Ok(resolved) => resolved,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            return USAGE_ERROR;
        }
    };
    match action {
        "plan" => plan(&policy, out),
        "apply" => apply(&policy, facts.as_ref(), out, err),
        "verify" => verify(&policy, facts.as_ref(), out, err),
        "revert" => revert(&policy, out, err),
        // Unreachable: the guard above accepts exactly these four.
        _ => USAGE_ERROR,
    }
}

fn parse_flags(tokens: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut rest = tokens.iter();
    while let Some(token) = rest.next() {
        let mut take = |name: &str| {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match token.as_str() {
            "--network" => args.network = Some(take("--network")?),
            "--bridge" => args.bridge = Some(take("--bridge")?),
            "--gateway" => args.gateway = Some(take("--gateway")?),
            "--ports" => args.ports = Some(take("--ports")?),
            "--no-log" => args.no_log = true,
            other => return Err(format!("unknown sandbox-net option: {other}")),
        }
    }
    Ok(args)
}

/// Build the policy from flags, falling back to the seller config and then to `docker network
/// inspect`.
///
/// The proxy port range resolves to `None` when unset, and that is deliberate rather than an error:
/// a seat running no contained credential needs no pinhole, and rendering one it does not need would
/// open a host port for nothing. What it must never do is silently widen — an unset range yields no
/// pinhole at all, never a permissive one.
fn resolve_policy(args: &Args) -> Result<(NetPolicy, Option<NetworkFacts>), String> {
    // Same home resolution the doctor uses: `MAXPLAYER_HOME`, then `~/.maxplayer`. A seat with no
    // home yet is not an error here — the flags can supply everything, which is what lets an
    // operator plan the rules before the seat exists.
    let config = maxplayer_core::home::default_home_dir()
        .and_then(|root| maxplayer_core::home::bootstrap(&root))
        .ok()
        .and_then(|home| home.config.sandbox.clone());

    let network = args
        .network
        .clone()
        .or_else(|| config.as_ref().and_then(|sandbox| sandbox.network.clone()))
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            "no sandbox network: pass --network, or set `[sandbox] network` in the seller config"
                .to_owned()
        })?;

    let ports = args
        .ports
        .clone()
        .or_else(|| {
            config
                .as_ref()
                .and_then(|sandbox| sandbox.proxy_port_range.clone())
        })
        .map(|range| range.trim().to_owned())
        .filter(|range| !range.is_empty())
        .map(|range| PortRange::parse(&range).map_err(|error| format!("--ports: {error}")))
        .transpose()?;

    // Both flags supplied ⇒ docker is never asked, so there are no network facts to check. That is a
    // deliberate hole and `verify` NAMES it rather than passing quietly: a skipped check must not be
    // reported as a satisfied one.
    let (discovered_bridge, discovered_gateway, facts) = match (&args.bridge, &args.gateway) {
        (Some(_), Some(_)) => (None, None, None),
        _ => {
            let (bridge, gateway, facts) = inspect_network(&network)?;
            (bridge, gateway, Some(facts))
        }
    };
    let bridge = args
        .bridge
        .clone()
        .or(discovered_bridge)
        .ok_or_else(|| format!("could not determine the bridge interface for network {network}"))?;
    let gateway = args
        .gateway
        .clone()
        .or(discovered_gateway)
        .ok_or_else(|| format!("could not determine the gateway address for network {network}"))?;

    Ok((
        NetPolicy {
            bridge,
            gateway,
            proxy_ports: ports,
            log_connections: !args.no_log,
        },
        facts,
    ))
}

/// What `docker network inspect` says about the NETWORK, as distinct from the ruleset.
///
/// The iptables policy cannot express either of these. `enable_icc` decides whether two containers
/// on this bridge may talk to each other at all, and `EnableIPv6` decides whether there is a second
/// address family that these v4 chains do not touch. Both are set when the network is CREATED, so
/// they are properties to be checked, never installed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NetworkFacts {
    name: String,
    /// Whether containers on this network may reach each other.
    ///
    /// Docker's default is ON, and in that case the key is ABSENT from `Options` entirely — so an
    /// absent key reads as `true`. A missing setting must never read as the safe one.
    inter_container: bool,
    ipv6: bool,
}

/// Ask docker for the network's host-side interface, gateway, and the two properties the ruleset
/// cannot cover.
///
/// Docker names a user-defined bridge `br-<first 12 chars of the network id>` unless the operator
/// pinned `com.docker.network.bridge.name`, so the pinned name is preferred and the derived one is
/// the fallback. Getting this wrong is not a silent failure — a rule scoped to a non-existent
/// interface matches nothing, and `verify` reports the mismatch.
fn inspect_network(
    network: &str,
) -> Result<(Option<String>, Option<String>, NetworkFacts), String> {
    let output = Command::new("docker")
        .args([
            "network",
            "inspect",
            network,
            "--format",
            "{{.Id}}\t{{index .Options \"com.docker.network.bridge.name\"}}\t{{range .IPAM.Config}}{{.Gateway}}{{end}}\t{{.EnableIPv6}}\t{{index .Options \"com.docker.network.bridge.enable_icc\"}}",
        ])
        .output()
        .map_err(|error| format!("could not run `docker network inspect`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker network inspect {network} failed: {}\n\
             create it first, e.g. `docker network create {network}`",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let mut fields = line.split('\t');
    let id = fields.next().unwrap_or_default().to_owned();
    let pinned = fields.next().unwrap_or_default().trim().to_owned();
    let gateway = fields.next().unwrap_or_default().trim().to_owned();
    let ipv6 = fields.next().unwrap_or_default().trim().to_owned();
    let icc = fields.next().unwrap_or_default().trim().to_owned();

    let bridge = if !pinned.is_empty() && pinned != "<no value>" {
        Some(pinned)
    } else if id.len() >= 12 {
        Some(format!("br-{}", &id[..12]))
    } else {
        None
    };
    Ok((
        bridge,
        (!gateway.is_empty()).then_some(gateway),
        NetworkFacts {
            name: network.to_owned(),
            inter_container: parse_icc(&icc),
            ipv6: parse_ipv6(&ipv6),
        },
    ))
}

/// Read docker's `enable_icc` option. ABSENT or unparseable ⇒ `true`, because docker's default is to
/// allow inter-container traffic and the key only appears in `Options` when someone set it. Reading
/// an absent key as `false` would report every default network as contained.
fn parse_icc(raw: &str) -> bool {
    !matches!(raw.trim().to_ascii_lowercase().as_str(), "false" | "0")
}

/// Read docker's `EnableIPv6`. Unlike `enable_icc` this is a real field and is `false` by default, so
/// an EMPTY value means the template returned nothing — which is a broken read, not a safe network.
/// Only an explicit `false` counts as off.
fn parse_ipv6(raw: &str) -> bool {
    !matches!(raw.trim().to_ascii_lowercase().as_str(), "false" | "0")
}

/// Print the rules and the reason for each, then the exact commands `apply` would run.
///
/// The reasons are printed because an operator is being asked to install firewall rules on a box
/// running real services. A bare argv list is not something anyone can review.
fn plan(policy: &NetPolicy, out: &mut dyn Write) -> i32 {
    let _ = writeln!(
        out,
        "sandbox egress policy — bridge {}, gateway {}\n",
        policy.bridge, policy.gateway
    );
    for (chain, label) in [
        (Chain::Forward, "ROUTED (container → LAN / internet), via DOCKER-USER"),
        (Chain::Input, "HOST-TERMINATING (container → this host), via INPUT"),
    ] {
        let _ = writeln!(out, "{label}");
        for rule in policy.rules().iter().filter(|rule| rule.chain == chain) {
            let _ = writeln!(out, "  {}\n      why: {}", rule.args.join(" "), rule.why);
        }
        let _ = writeln!(out);
    }
    if policy.proxy_ports.is_none() {
        let _ = writeln!(
            out,
            "NOTE: no proxy port range configured, so NO host pinhole is opened. A seat running \
             contained credentials (#647) needs `[sandbox] proxy_port_range` set, or every job \
             will fail to reach its model.\n"
        );
    }
    let _ = writeln!(out, "commands `apply` would run:");
    for step in policy.install_plan() {
        let _ = writeln!(out, "  iptables {}", shell_join(&step));
    }
    SUCCESS
}

/// Render an argv as a copy-pasteable shell command.
///
/// `apply` hands these to `iptables` as an argv vector, where `--log-prefix sbx-net conn: ` is ONE
/// argument. Printed bare, a shell splits it into three and installs a different rule — so an
/// operator following `plan` would get a ruleset `verify` then rejects, with nothing naming the
/// quoting as the cause. The printed form and the executed form have to mean the same thing.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=,+".contains(c))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Install the rules, but only when they are not already installed.
///
/// `apply` used to append unconditionally. `-A` does not care that a rule is already there, so a
/// second `apply` doubled every rule, and `verify`'s exact-sequence comparison then reported NOT
/// CONTAINED on a box that was genuinely contained. Fail-loud rather than dangerous, but it made the
/// obvious operator reflex — run apply again — produce a scary result.
///
/// So: compare first. Already in force ⇒ do nothing. Our chains present but DIFFERENT ⇒ refuse and
/// send the operator through `revert`, because appending onto a mismatched chain is what produces the
/// duplicates. Refusing is deliberate over reverting automatically: this command edits the firewall
/// of a box running real services, and it should not remove rules the operator did not ask it to.
/// An upgrade — this release adds ranges to the deny list — is therefore `revert` then `apply`.
fn apply(
    policy: &NetPolicy,
    facts: Option<&NetworkFacts>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let (mut quiet_out, mut quiet_err) = (Vec::new(), Vec::new());
    if verify(policy, facts, &mut quiet_out, &mut quiet_err) == SUCCESS {
        let _ = writeln!(
            out,
            "already in force: both chains match the policy, nothing to install"
        );
        return SUCCESS;
    }
    // A chain of ours that exists but does not match is the duplicate-producing case.
    if [Chain::Forward, Chain::Input]
        .iter()
        .any(|chain| iptables_spec(chain.own_chain()).is_ok())
    {
        let _ = writeln!(
            err,
            "REFUSING to apply: our chains already exist and do not match the policy."
        );
        let _ = writeln!(
            err,
            "Appending onto them would duplicate every rule. Run `maxplayer sandbox-net revert` \
             first, then apply."
        );
        return NOT_CONTAINED;
    }
    for step in policy.install_plan() {
        // `-N` on an existing chain is an error, and it is the expected one on re-apply. Treat only
        // that as benign; everything else stops the run rather than pressing on with a half-built
        // chain that would be neither the old posture nor the new one.
        if let Err(message) = run_iptables(&step) {
            if step.first().map(String::as_str) == Some("-N") {
                continue;
            }
            let _ = writeln!(err, "apply failed at `iptables {}`: {message}", step.join(" "));
            let _ = writeln!(
                err,
                "the ruleset is now PARTIAL. Run `maxplayer sandbox-net revert` and re-apply."
            );
            return NOT_CONTAINED;
        }
    }
    let _ = writeln!(out, "installed. verifying:");
    verify(policy, facts, out, err)
}

/// Compare the live chains against the rendered policy.
///
/// Exact-sequence comparison, not a subset check: a rule present but in the wrong ORDER is a real
/// defect here (a deny above the pinhole silently breaks every contained job; a pinhole above the
/// metadata drop would be worse), and a subset check cannot see order at all.
fn verify(
    policy: &NetPolicy,
    facts: Option<&NetworkFacts>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let mut mismatched = false;
    for chain in [Chain::Forward, Chain::Input] {
        let own = chain.own_chain();
        let live = match iptables_spec(own) {
            Ok(live) => live,
            Err(message) => {
                let _ = writeln!(err, "NOT CONTAINED: chain {own} is not installed ({message})");
                mismatched = true;
                continue;
            }
        };
        let expected: Vec<String> = policy
            .expected_spec_lines()
            .into_iter()
            .filter(|line| line.starts_with(&format!("-A {own} ")))
            .collect();
        // `iptables -S <chain>` leads with the chain's own `-N` declaration; the rules follow.
        let live_rules: Vec<String> = live
            .lines()
            .filter(|line| line.starts_with("-A "))
            .map(|line| line.trim().to_owned())
            .collect();
        if live_rules != expected {
            mismatched = true;
            let _ = writeln!(err, "NOT CONTAINED: chain {own} does not match the policy");
            for line in expected.iter().filter(|line| !live_rules.contains(line)) {
                let _ = writeln!(err, "  missing: {line}");
            }
            for line in live_rules.iter().filter(|line| !expected.contains(line)) {
                let _ = writeln!(err, "  unexpected: {line}");
            }
            if live_rules.len() == expected.len()
                && live_rules.iter().all(|line| expected.contains(line))
            {
                let _ = writeln!(err, "  (same rules, wrong ORDER — see the sequence above)");
            }
        }
        // The rules can be perfect and still filter nothing if the parent never jumps to them.
        match iptables_spec(chain.parent_chain()) {
            Ok(parent) if parent.contains(&format!("-j {own}")) => {}
            Ok(_) => {
                mismatched = true;
                let _ = writeln!(
                    err,
                    "NOT CONTAINED: {} does not jump to {own}, so its rules are never reached",
                    chain.parent_chain()
                );
            }
            Err(message) => {
                mismatched = true;
                let _ = writeln!(err, "could not read {}: {message}", chain.parent_chain());
            }
        }
    }
    if check_network_facts(facts, out, err) {
        mismatched = true;
    }
    if mismatched {
        return NOT_CONTAINED;
    }
    let _ = match facts {
        Some(_) => writeln!(
            out,
            "contained: both chains match the policy, both parents jump to them, \
             inter-container is off and IPv6 is off"
        ),
        None => writeln!(
            out,
            "rules match: both chains match the policy and both parents jump to them \
             (network properties unchecked, see the note above)"
        ),
    };
    SUCCESS
}

/// The two properties no iptables rule can express, checked against `docker network inspect`.
///
/// Both are decided when the network is CREATED, so a failure here is NOT fixed by re-applying the
/// rules — it needs the network rebuilt, and the messages say so rather than leaving an operator to
/// guess. Kept separate from [`verify`] so it is testable without root, iptables or a docker daemon.
///
/// Returns whether it found a containment problem.
fn check_network_facts(
    facts: Option<&NetworkFacts>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    let mut mismatched = false;
    match facts {
        Some(facts) => {
            if facts.inter_container {
                mismatched = true;
                let _ = writeln!(
                    err,
                    "NOT CONTAINED: network {} allows inter-container traffic, so one job can reach \
                     another on the same bridge",
                    facts.name
                );
                let _ = writeln!(
                    err,
                    "  today a job→job packet is denied only INCIDENTALLY, by the RFC1918 rule plus \
                     net.bridge.bridge-nf-call-iptables=1. Pick a non-RFC1918 subnet or lose that \
                     sysctl and it is silently gone."
                );
                let _ = writeln!(
                    err,
                    "  fix (recreate — icc cannot be changed on an existing network, so stop jobs \
                     first):\n    docker network rm {0}\n    docker network create \
                     --opt com.docker.network.bridge.enable_icc=false {0}",
                    facts.name
                );
            }
            if facts.ipv6 {
                mismatched = true;
                let _ = writeln!(
                    err,
                    "NOT CONTAINED: network {} has IPv6 enabled, and these rules are iptables (v4) \
                     only",
                    facts.name
                );
                let _ = writeln!(
                    err,
                    "  a v6-enabled network gives the job a second address family that NONE of these \
                     chains touch — v6 has its own INPUT and DOCKER-USER, and the LAN-equivalents \
                     there are ULA fc00::/7 and link-local fe80::/10."
                );
                let _ = writeln!(
                    err,
                    "  fix: recreate the network without --ipv6, or render an ip6tables mirror of \
                     this policy (not implemented)."
                );
            }
        }
        // Absence of a check is reported, never counted as a pass.
        None => {
            let _ = writeln!(
                out,
                "note: inter-container and IPv6 were NOT checked — --bridge and --gateway were both \
                 supplied, so docker was never asked about the network."
            );
        }
    }
    mismatched
}

fn revert(policy: &NetPolicy, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut failed = false;
    for step in policy.revert_plan() {
        // Revert is idempotent by intent: a jump already gone or a chain already deleted is the
        // desired end state, not a failure. Report them, do not stop — stopping would leave the
        // remaining chains installed.
        if let Err(message) = run_iptables(&step) {
            let _ = writeln!(err, "note: `iptables {}` — {message}", step.join(" "));
            failed = true;
        }
    }
    if failed {
        let _ = writeln!(
            out,
            "revert finished with notes above (usually 'already absent', which is the intended end \
             state). Confirm with `iptables -S`."
        );
    } else {
        let _ = writeln!(out, "removed both chains and their jumps");
    }
    SUCCESS
}

fn run_iptables(step: &[String]) -> Result<(), String> {
    let output = Command::new("iptables")
        .args(step)
        .output()
        .map_err(|error| format!("could not run iptables: {error} (run as root?)"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

fn iptables_spec(chain: &str) -> Result<String, String> {
    let output = Command::new("iptables")
        .args(["-S", chain])
        .output()
        .map_err(|error| format!("could not run iptables: {error} (run as root?)"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> NetPolicy {
        NetPolicy {
            bridge: "br-sbx0".to_owned(),
            gateway: "172.31.0.1".to_owned(),
            proxy_ports: Some(PortRange::new(49200, 49299).unwrap()),
            log_connections: true,
        }
    }

    #[test]
    fn plan_prints_both_chains_and_a_reason_for_every_rule() {
        let mut out = Vec::new();
        assert_eq!(plan(&policy(), &mut out), SUCCESS);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("via DOCKER-USER"), "{printed}");
        assert!(
            printed.contains("via INPUT"),
            "the host-terminating chain must be in the plan — it is the half DOCKER-USER cannot \
             filter: {printed}"
        );
        // Every rule line is followed by its reason, so the plan is reviewable rather than an argv dump.
        let rules = printed.matches("      why: ").count();
        assert_eq!(
            rules,
            policy().rules().len(),
            "every rule must print its reason: {printed}"
        );
    }

    /// A seat with no configured range gets a loud note, not a silent absence. Without the pinhole
    /// a contained job cannot reach its model, and the operator needs to know that before applying.
    #[test]
    fn plan_warns_when_no_pinhole_will_be_opened() {
        let mut unconfigured = policy();
        unconfigured.proxy_ports = None;
        let mut out = Vec::new();
        assert_eq!(plan(&unconfigured, &mut out), SUCCESS);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("NO host pinhole"), "{printed}");

        // Control: the configured case must NOT print the warning, or it is unconditional text
        // rather than a check.
        let mut out = Vec::new();
        assert_eq!(plan(&policy(), &mut out), SUCCESS);
        assert!(!String::from_utf8(out).unwrap().contains("NO host pinhole"));
    }

    /// The printed plan and the executed plan must mean the same thing. `--log-prefix` values carry
    /// a trailing space, which is one argv element to `apply` and three shell words when pasted.
    #[test]
    fn printed_commands_quote_arguments_that_a_shell_would_split() {
        let printed = shell_join(&[
            "-j".to_owned(),
            "LOG".to_owned(),
            "--log-prefix".to_owned(),
            "sbx-net conn: ".to_owned(),
        ]);
        assert_eq!(printed, "-j LOG --log-prefix 'sbx-net conn: '");
        // Ordinary arguments stay bare, or every rule becomes unreadable.
        assert_eq!(
            shell_join(&["-i".to_owned(), "br-sbx0".to_owned(), "-d".to_owned(), "10.0.0.0/8".to_owned()]),
            "-i br-sbx0 -d 10.0.0.0/8"
        );
        assert_eq!(shell_join(&["49200:49299".to_owned()]), "49200:49299");
        // An embedded quote cannot break out of the quoting.
        assert_eq!(shell_join(&["a'b".to_owned()]), r"'a'\''b'");

        // And the property that matters, asserted over the REAL plan: every printed line that
        // contains a space inside an argument must carry a quote.
        let plan_lines: Vec<String> = policy()
            .install_plan()
            .iter()
            .map(|step| shell_join(step))
            .collect();
        let log_lines: Vec<&String> = plan_lines
            .iter()
            .filter(|line| line.contains("--log-prefix"))
            .collect();
        assert!(!log_lines.is_empty(), "the policy must render LOG rules to test");
        for line in log_lines {
            assert!(
                line.contains("'sbx-net"),
                "a log prefix must be quoted or a paste installs a different rule: {line}"
            );
        }
    }

    #[test]
    fn unknown_action_and_missing_action_are_usage_errors() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&[], &mut out, &mut err), USAGE_ERROR);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run(&["wat".to_owned()], &mut out, &mut err),
            USAGE_ERROR
        );
        assert!(String::from_utf8(err).unwrap().contains("unknown sandbox-net action"));
    }

    #[test]
    fn help_is_success_and_names_every_action() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&["--help".to_owned()], &mut out, &mut err), SUCCESS);
        let printed = String::from_utf8(out).unwrap();
        for action in ["plan", "apply", "verify", "revert"] {
            assert!(printed.contains(action), "usage must name {action}: {printed}");
        }
    }

    #[test]
    fn flags_parse_and_an_unknown_flag_is_refused() {
        let parsed = parse_flags(&[
            "--network".to_owned(),
            "sbx".to_owned(),
            "--bridge".to_owned(),
            "br-x".to_owned(),
            "--gateway".to_owned(),
            "10.1.2.3".to_owned(),
            "--ports".to_owned(),
            "49200-49299".to_owned(),
            "--no-log".to_owned(),
        ])
        .expect("valid flags");
        assert_eq!(parsed.network.as_deref(), Some("sbx"));
        assert_eq!(parsed.bridge.as_deref(), Some("br-x"));
        assert_eq!(parsed.gateway.as_deref(), Some("10.1.2.3"));
        assert_eq!(parsed.ports.as_deref(), Some("49200-49299"));
        assert!(parsed.no_log);

        assert!(parse_flags(&["--nope".to_owned()]).is_err());
        // A flag with no value is refused rather than silently treated as absent.
        assert!(parse_flags(&["--network".to_owned()]).is_err());
    }

    /// `--no-log` drops the LOG rules and nothing else. Observability is requirement 3 of the
    /// ticket, so turning it off must not quietly turn off a deny with it.
    #[test]
    fn disabling_logging_removes_only_log_rules() {
        let mut quiet = policy();
        quiet.log_connections = false;
        let drops = |policy: &NetPolicy| {
            policy
                .rules()
                .iter()
                .filter(|rule| rule.args.last().map(String::as_str) == Some("DROP"))
                .count()
        };
        assert_eq!(
            drops(&quiet),
            drops(&policy()),
            "logging must not change which destinations are denied"
        );
        assert!(
            !quiet.rules().iter().any(|rule| rule.args.contains(&"LOG".to_owned())),
            "--no-log must remove the LOG rules"
        );
    }

    /// The whole point of these two readers is which way they fail. `enable_icc` is ABSENT from
    /// `Options` on a default network, so an absent value must read as ENABLED — read the other way
    /// round, every default docker network would report itself contained.
    #[test]
    fn a_missing_docker_option_reads_as_the_unsafe_value() {
        // Absent (`{{index}}` on a missing key), empty, and unparseable all mean "not turned off".
        for raw in ["", "<no value>", "true", "1", "yes", "garbage"] {
            assert!(
                parse_icc(raw),
                "icc {raw:?} must read as ENABLED: docker's default is on and the key is absent"
            );
        }
        for raw in ["false", "FALSE", " false ", "0"] {
            assert!(!parse_icc(raw), "icc {raw:?} is an explicit off");
        }
        // EnableIPv6 is a real field, so an empty read is a BROKEN read, not a safe network.
        for raw in ["", "<no value>", "true", "1"] {
            assert!(parse_ipv6(raw), "ipv6 {raw:?} must not read as off");
        }
        for raw in ["false", "FALSE", "0"] {
            assert!(!parse_ipv6(raw), "ipv6 {raw:?} is an explicit off");
        }
    }

    fn facts(inter_container: bool, ipv6: bool) -> NetworkFacts {
        NetworkFacts {
            name: "mp-sandbox".to_owned(),
            inter_container,
            ipv6,
        }
    }

    #[test]
    fn inter_container_and_ipv6_each_fail_containment_and_name_their_own_fix() {
        for (given, needle) in [
            (facts(true, false), "inter-container"),
            (facts(false, true), "IPv6"),
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert!(
                check_network_facts(Some(&given), &mut out, &mut err),
                "{needle} must fail containment"
            );
            let err = String::from_utf8(err).unwrap();
            assert!(err.contains("NOT CONTAINED"), "{err}");
            assert!(err.contains(needle), "the message must name the property: {err}");
            // Re-applying rules cannot fix either one, so the message has to say what does.
            assert!(
                err.contains("docker network") || err.contains("recreate"),
                "the fix must name recreating the network, not re-applying rules: {err}"
            );
        }
    }

    /// A skipped check must not read as a satisfied one. With both flags supplied docker is never
    /// asked, and `verify` has to say so out loud.
    #[test]
    fn unchecked_network_properties_are_reported_rather_than_passed_quietly() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(!check_network_facts(None, &mut out, &mut err));
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("NOT checked"),
            "an unevaluated check must be named: {out}"
        );
        assert!(String::from_utf8(err).unwrap().is_empty());
    }

    /// The positive control for the two tests above: a network with both properties off must pass
    /// and print nothing on stderr. Without this, a `check_network_facts` that always returned true
    /// would satisfy every other assertion here.
    #[test]
    fn a_contained_network_passes_with_no_complaint() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(!check_network_facts(Some(&facts(false, false)), &mut out, &mut err));
        assert!(String::from_utf8(err).unwrap().is_empty());
        assert!(String::from_utf8(out).unwrap().is_empty());
    }
}
