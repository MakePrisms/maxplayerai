//! Job-local egress filtering on the holder namespace's own veth.
//!
//! # The hole this closes
//!
//! [`crate::sandbox_net`] renders an iptables plan and [`crate::sandbox_netns`] installs it into the
//! job's network namespace. That plan contains a `runc` job completely. It does **not** contain a
//! `runsc` (gVisor) one: gVisor runs its own userspace netstack and hands finished packets to the
//! sandbox's network endpoint, so a payload's egress never traverses the host kernel's `OUTPUT`
//! chain. The rules are installed, the readback proves they are installed, and the denied
//! destinations stay reachable anyway — measured on this repo's own fixtures, both address families,
//! over TCP.
//!
//! The packets do cross one thing the host kernel owns: the **veth** the namespace is built on.
//! Every packet leaving that namespace, from any runtime, is transmitted on that interface. So the
//! filter goes there, as a `clsact` egress qdisc with `flower` classifiers, installed by the same
//! trusted sidecar that installs the iptables plan and before any payload exists.
//!
//! # Why this is not a replacement for the iptables plan
//!
//! Both are installed. The iptables plan keeps doing what it already did — in particular it is what
//! LOGs a job probing the LAN, which `tc` has no equivalent of — and the veth filter closes the
//! runtime-dependent gap underneath it. Removing either one would be a widening, and neither is
//! sufficient alone.
//!
//! # Where the policy comes from
//!
//! Nowhere here. Every prefix, every exception and their order are **derived from
//! [`NetPolicy::rules`]** — the same rendered policy the iptables plan is built from, unit-tested in
//! `sandbox_net`. This module translates that policy into `tc` argv; it does not hold a second copy
//! of it. A denied range added there appears here without anyone remembering to, and the parity
//! tests below fail if a translation is ever dropped.
//!
//! # What is deliberately different from the iptables rendering
//!
//! **The drops carry no protocol match.** The iptables drops do not either, but this is the property
//! the whole exercise turns on: the demonstrated leak was TCP, and a TCP-only filter would look
//! green against the very fixture that found the hole while leaving UDP and everything else open.
//! [`drops_are_protocol_independent`] is the test that keeps it that way.
//!
//! **Loopback is not filtered and does not need to be.** Docker's embedded resolver answers at
//! `127.0.0.11` inside the namespace, and loopback traffic is never transmitted on the veth, so an
//! egress filter on that interface cannot reach it. The `sandbox_net` invariant that loopback is
//! never denied survives here by construction rather than by a rule.

use crate::sandbox_net::{Family, NetPolicy};

/// The qdisc the filters attach to.
///
/// `clsact` rather than the older `ingress` qdisc because it is the one that offers an **egress**
/// hook, which is the direction a payload's traffic leaves in. It is also classless and holds no
/// queueing behaviour of its own: attaching it changes no scheduling, adds no shaping, and cannot
/// reorder or delay a packet it does not drop.
pub const EGRESS_QDISC: &str = "clsact";

/// The `tc` hook filters are attached to.
pub const EGRESS_HOOK: &str = "egress";

/// The first `tc` priority this module uses; each rendered filter takes the next one.
///
/// `tc` evaluates filters in ascending `pref` and takes the **first match**, which is the same
/// first-match semantics `iptables` gives an `OUTPUT` chain of terminating targets. So the order is
/// not re-derived here: the filters are emitted in exactly [`NetPolicy::rules`]' order and numbered
/// consecutively, and every ordering property that file establishes and tests — the metadata drop
/// ahead of everything that could pass it, the proxy pinhole ahead of the range drop that covers the
/// gateway — is carried across rather than reinvented.
///
/// Starting at 100 rather than 1 leaves room below for a future filter that must precede all of
/// these, and makes a hand-added filter visible as an out-of-band number in a readback.
pub const PREF_BASE: u16 = 100;

/// One rendered `tc` filter, kept as data so the plan can be printed, compared and tested rather
/// than only executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceFilter {
    pub family: Family,
    pub pref: u16,
    /// The destination prefix, exactly as the policy spells it.
    pub dst: String,
    /// `Some("tcp")` for an exception that names a protocol; **always `None` for a drop**.
    pub ip_proto: Option<String>,
    /// The destination port match in `tc` spelling (`49200-49299`), if the source rule had one.
    pub dst_port: Option<String>,
    /// `pass` or `drop`.
    pub action: &'static str,
    /// Why this filter exists, carried from the policy rule it was derived from.
    pub why: &'static str,
}

impl IfaceFilter {
    /// The `tc` argv that installs this filter on `dev`, without the leading binary name.
    pub fn add_argv(&self, dev: &str) -> Vec<String> {
        let mut argv: Vec<String> = ["filter", "add", "dev", dev, EGRESS_HOOK]
            .into_iter()
            .map(String::from)
            .collect();
        // `pref` before `protocol` before `flower`: the exact argument order the prototype gate
        // installed and read back on a live kernel. `tc` accepts other orders, but this is the one
        // with a measurement behind it.
        argv.push("pref".into());
        argv.push(self.pref.to_string());
        argv.push("protocol".into());
        argv.push(tc_protocol(self.family).into());
        argv.push("flower".into());
        if let Some(proto) = &self.ip_proto {
            argv.push("ip_proto".into());
            argv.push(proto.clone());
        }
        argv.push("dst_ip".into());
        argv.push(self.dst.clone());
        if let Some(port) = &self.dst_port {
            argv.push("dst_port".into());
            argv.push(port.clone());
        }
        argv.push("action".into());
        argv.push(self.action.to_owned());
        argv
    }
}

/// The complete interface plan for one job: the qdisc, then the filters in the order they must be
/// installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfacePlan {
    pub dev: String,
    pub filters: Vec<IfaceFilter>,
}

impl IfacePlan {
    /// Derive the plan for `dev` from `policy`.
    ///
    /// `Err` rather than a silent omission whenever a policy rule cannot be translated: an
    /// untranslatable deny is a hole in this layer, and the only safe response is to refuse to launch
    /// the payload. "Rendered fewer filters than the policy has drops" must never be representable.
    pub fn derive(dev: &str, policy: &NetPolicy) -> Result<Self, String> {
        let mut filters = Vec::new();
        let mut rendered = 0u16;

        for rule in policy.rules() {
            let target = rule.target();
            let action = match target {
                Some("ACCEPT") => "pass",
                Some("DROP") => "drop",
                // LOG rules have no `tc` equivalent and are not containment: they observe. They stay
                // in the iptables plan, which still runs. Skipping them here is not a widening —
                // there is nothing to widen, a LOG rule denies nothing.
                Some("LOG") => continue,
                other => {
                    return Err(format!(
                        "policy rule {:?} jumps to {other:?}, which this layer cannot translate — \
                         refusing to render a partial interface filter",
                        rule.args
                    ))
                }
            };

            let Some(dst) = rule.destination() else {
                // A destination-less ACCEPT is an egress hole; a destination-less DROP cannot be
                // expressed as a flower prefix. Either way the answer is to refuse, not to guess.
                return Err(format!(
                    "policy rule {:?} names no -d destination, so it has no flower equivalent",
                    rule.args
                ));
            };

            rendered += 1;
            let pref = PREF_BASE + rendered;

            filters.push(IfaceFilter {
                family: rule.family,
                pref,
                dst: dst.to_owned(),
                // Protocol is carried for an exception — a pinhole must stay as narrow as the
                // iptables one, and widening it here would be a widening of containment. It is
                // dropped for a deny, because the leak this closes is protocol-independent and a
                // TCP-only deny is the exact shape of the bug.
                ip_proto: match action {
                    "pass" => arg_after(&rule.args, "-p").map(str::to_owned),
                    _ => None,
                },
                dst_port: match action {
                    "pass" => arg_after(&rule.args, "--dport").map(to_tc_port_range),
                    _ => None,
                },
                action,
                why: rule.why,
            });
        }

        if filters.is_empty() {
            return Err(
                "the policy rendered no interface filters at all — refusing an unfiltered veth"
                    .to_owned(),
            );
        }
        // Checked on the render as well as on the readback. A shadowed pinhole is not a typo, it is
        // the measured failure `sandbox_net` documents — the ACCEPT appended below the range drop
        // that covers the gateway, leaving every job without its model while every shape test stays
        // green — and it must not be renderable, let alone installable.
        no_shadowed_exception(&filters)?;

        Ok(Self { dev: dev.to_owned(), filters })
    }

    /// Every `tc` argv this plan runs, in order: the qdisc first, then the filters.
    pub fn install_plan(&self) -> Vec<Vec<String>> {
        let mut plan = vec![vec![
            "qdisc".to_owned(),
            "add".to_owned(),
            "dev".to_owned(),
            self.dev.clone(),
            EGRESS_QDISC.to_owned(),
        ]];
        plan.extend(self.filters.iter().map(|filter| filter.add_argv(&self.dev)));
        plan
    }

    /// How many filters this plan installs for one address family.
    pub fn filter_count(&self, family: Family) -> usize {
        self.filters.iter().filter(|filter| filter.family == family).count()
    }
}

/// The plan as the sidecar reads it: one `tc <args…>` line per step, plus the count, so the caller
/// can cross-check the sidecar's echoed total against what was rendered.
///
/// The same cross-check `sandbox_netns::plan_stdin` exists for, for the same reason: a truncated
/// stdin applies perfectly and exits 0, and only the count reveals it.
pub fn plan_stdin(plan: &IfacePlan) -> (String, usize) {
    let steps = plan.install_plan();
    let mut out = String::new();
    for step in &steps {
        out.push_str("tc");
        for arg in step {
            out.push(' ');
            out.push_str(arg);
        }
        out.push('\n');
    }
    (out, steps.len())
}

// ---------------------------------------------------------------------------------------------
// Which interface, and the proof it is the right one
// ---------------------------------------------------------------------------------------------

/// One link as `ip -details -oneline link show` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub index: u32,
    pub name: String,
    /// The peer's ifindex from a `name@ifN` suffix. A veth has one; nothing else does.
    pub peer_index: Option<u32>,
    /// The link kind `-details` prints (`veth`, `bridge`, …); `None` for a plain device.
    pub kind: Option<String>,
    pub loopback: bool,
    pub up: bool,
}

/// `docker run` argv that enumerates the links inside the holder's namespace.
///
/// **Unprivileged.** Listing links needs no capability, and this container must not be able to
/// change one: the whole question it answers is "what is here", and a reader that could also move an
/// interface would be a worse answer to it. `--entrypoint ip` replaces the applier, so the image's
/// privileged entrypoint is not reachable from here even if the arguments were wrong.
pub fn link_probe_argv(holder_name: &str, image: &str) -> Vec<String> {
    [
        "docker",
        "run",
        "--rm",
        "--network",
        &crate::sandbox_netns::NetnsHolder::network_mode_for(holder_name),
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--entrypoint",
        "ip",
        image,
        "-details",
        "-oneline",
        "link",
        "show",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Parse `ip -details -oneline link show` output.
///
/// **A nonempty line that does not parse is a refusal, not a skip.** Silently dropping unparsable
/// records let a namespace holding `lo`, a veth and one malformed third record present itself as the
/// two-link shape [`select_egress_link`] accepts — the discarded record is exactly the link that
/// would have forced a refusal.
pub fn parse_links(stdout: &str) -> Result<Vec<Link>, String> {
    let mut links = Vec::new();
    for (number, line) in stdout.lines().enumerate() {
        let at = number + 1;
        if line.trim().is_empty() {
            continue;
        }
        // `<index>: <name>[@peer]: <FLAGS> mtu … \    link/<type> … [kind] …`
        let mut head = line.splitn(3, ": ");
        let index: u32 = head
            .next()
            .and_then(|field| field.trim().parse().ok())
            .ok_or(format!("line {at}: link record names no ifindex: {line:?}"))?;
        let name_field = head
            .next()
            .ok_or(format!("line {at}: link record names no interface: {line:?}"))?
            .trim();
        if name_field.is_empty() {
            return Err(format!("line {at}: link record has an empty interface name: {line:?}"));
        }
        let rest = head.next().unwrap_or_default();
        let (name, peer_index) = match name_field.split_once('@') {
            Some((name, peer)) => (
                name.to_owned(),
                peer.strip_prefix("if").and_then(|digits| digits.parse().ok()),
            ),
            None => (name_field.to_owned(), None),
        };
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if !tokens.iter().any(|token| token.starts_with("link/")) {
            return Err(format!(
                "line {at}: link record for {name:?} names no link type: {line:?} — an unreadable \
                 record is refused, because the link it hides is the one that would force a refusal"
            ));
        }
        links.push(Link {
            index,
            name,
            peer_index,
            kind: LINK_KINDS
                .iter()
                .find(|kind| tokens.contains(kind))
                .map(|kind| (*kind).to_owned()),
            loopback: tokens.iter().any(|token| *token == "link/loopback"),
            up: rest
                .split_once('>')
                .map(|(flags, _)| flags.contains(",UP") || flags.contains("<UP"))
                .unwrap_or(false),
        });
    }
    Ok(links)
}

/// The link kinds `-details` may print that this module needs to recognise. A kind it does not know
/// parses as `None`, which fails the veth check below rather than passing it.
const LINK_KINDS: &[&str] = &["veth", "bridge", "bond", "tun", "macvlan", "ipvlan", "vlan", "dummy"];

/// Choose the interface to filter, and refuse unless it is provably the job's own veth.
///
/// # What each refusal is for
///
/// This runs with `--network container:<holder>`, so it is in the holder's namespace by
/// construction. These checks are what catches the case where that construction did **not** hold —
/// a mis-resolved holder name, a runtime that ignored the network mode, a future caller that passes
/// the wrong container. The failure being designed against is filtering the **host's** interface,
/// which would be a host-global mutation this design forbids outright, so every ambiguity refuses.
pub fn select_egress_link(links: &[Link]) -> Result<Link, String> {
    if links.is_empty() {
        return Err("the namespace reported no links at all — the probe did not see a namespace"
            .to_owned());
    }
    // A namespace holding one job has exactly two links: loopback and one veth. The host's has many,
    // and `docker0` or any bridge among them is the loudest possible "this is not a job namespace".
    if let Some(bridge) = links.iter().find(|link| link.kind.as_deref() == Some("bridge")) {
        return Err(format!(
            "the namespace contains a bridge ({}) — this is a host or shared namespace, not a job's, \
             and nothing here may filter it",
            bridge.name
        ));
    }
    if !links.iter().any(|link| link.loopback) {
        return Err("the namespace has no loopback link, so it is not a namespace this build made"
            .to_owned());
    }

    let candidates: Vec<&Link> = links.iter().filter(|link| !link.loopback).collect();
    let [candidate] = candidates.as_slice() else {
        return Err(format!(
            "expected exactly one non-loopback link in the job's namespace, found {}: {:?} — \
             filtering one of several would leave the others open",
            candidates.len(),
            candidates.iter().map(|link| &link.name).collect::<Vec<_>>()
        ));
    };
    if candidate.kind.as_deref() != Some("veth") {
        return Err(format!(
            "{} is a {:?}, not a veth — a physical or host-owned interface is never filtered by a job",
            candidate.name, candidate.kind
        ));
    }
    match candidate.peer_index {
        None => {
            return Err(format!(
                "{} names no peer index, so it is not one end of a veth pair this namespace owns",
                candidate.name
            ))
        }
        Some(peer) if peer == candidate.index => {
            return Err(format!(
                "{} claims itself as its own veth peer (ifindex {peer})",
                candidate.name
            ))
        }
        Some(_) => {}
    }
    if !candidate.up {
        return Err(format!(
            "{} is down; filtering a down interface proves nothing about the one the job will use",
            candidate.name
        ));
    }
    Ok((*candidate).clone())
}

/// The `tc` spelling of an address family's ethertype selector.
///
/// A free function rather than a method on [`Family`] so this whole layer adds nothing to
/// `sandbox_net`: that file is being rewritten by two other lanes at the same time, and a filter
/// that lives entirely in its own module is a filter that cannot lose a three-way merge.
pub fn tc_protocol(family: Family) -> &'static str {
    match family {
        Family::V4 => "ip",
        Family::V6 => "ipv6",
    }
}

/// What `flower` prints back for the ethertype it matched. `tc` takes `protocol ip` on the command
/// line and lists `eth_type ipv4`, so readback compares this spelling rather than [`tc_protocol`].
pub fn tc_eth_type(family: Family) -> &'static str {
    match family {
        Family::V4 => "ipv4",
        Family::V6 => "ipv6",
    }
}

/// iptables spells a port range `49200:49299`; `tc` flower spells it `49200-49299`.
fn to_tc_port_range(dport: &str) -> String {
    dport.replace(':', "-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_net::PortRange;

    const DEV: &str = "eth0";

    fn policy() -> NetPolicy {
        NetPolicy {
            gateway: "172.17.0.1".to_owned(),
            proxy_ports: Some(PortRange::new(49200, 49299).expect("valid range")),
            log_connections: true,
        }
    }

    fn plan() -> IfacePlan {
        IfacePlan::derive(DEV, &policy()).expect("the shipped policy must render")
    }

    /// Render a plan the way `tc filter show` prints it.
    ///
    /// **This is a shape, not a measurement, and it cannot prove the parser reads real `tc` output**
    /// — for the positive case it is circular by construction. It exists so the *mutations* below
    /// have something to mutate. The anchors against reality are
    /// [`the_parser_reads_the_shape_tc_actually_prints`], which parses a literal capture, and the
    /// live gate in `tests/sandbox_iface_live.rs`, which asks a real kernel.
    fn as_tc_output(plan: &IfacePlan) -> String {
        let mut out = String::new();
        for filter in &plan.filters {
            let protocol = tc_protocol(filter.family);
            out.push_str(&format!(
                "filter protocol {protocol} pref {} flower chain 0 \n",
                filter.pref
            ));
            out.push_str(&format!(
                "filter protocol {protocol} pref {} flower chain 0 handle 0x1 \n",
                filter.pref
            ));
            out.push_str(&format!("  eth_type {}\n", tc_eth_type(filter.family)));
            if let Some(proto) = &filter.ip_proto {
                out.push_str(&format!("  ip_proto {proto}\n"));
            }
            out.push_str(&format!("  dst_ip {}\n", filter.dst));
            if let Some(port) = &filter.dst_port {
                out.push_str(&format!("  dst_port {port}\n"));
            }
            out.push_str("  skip_hw\n");
            out.push_str("  not_in_hw\n");
            out.push_str(&format!("\taction order 1: gact action {}\n", filter.action));
            out.push_str("\t random type none pass val 0\n");
            out.push_str("\t index 1 ref 1 bind 1\n");
        }
        out
    }

    /// THE point of this module. The leak it closes was found over TCP, so a TCP-only translation
    /// would pass every fixture that found it and contain nothing else.
    #[test]
    fn drops_are_protocol_independent() {
        let plan = plan();
        let offenders: Vec<&IfaceFilter> = plan
            .filters
            .iter()
            .filter(|filter| filter.action == "drop" && filter.ip_proto.is_some())
            .collect();
        assert!(
            offenders.is_empty(),
            "a drop filter names a protocol, so every other protocol reaches the denied prefix: \
             {offenders:?}"
        );
    }

    /// Parity, in the direction that matters: nothing the iptables policy denies may be missing here.
    /// A translation that quietly rendered seven of eight denied ranges would otherwise be invisible.
    #[test]
    fn every_policy_deny_is_translated_for_its_own_family() {
        let policy = policy();
        let plan = plan();
        for rule in policy.rules().iter().filter(|rule| rule.target() == Some("DROP")) {
            let destination = rule.destination().expect("a deny names a destination");
            assert!(
                plan.filters.iter().any(|filter| {
                    filter.family == rule.family
                        && filter.dst == destination
                        && filter.action == "drop"
                }),
                "the policy denies {destination} on {:?} and the interface plan does not: {:#?}",
                rule.family,
                plan.filters
            );
        }
    }

    /// And parity in the other direction, which is where a widening would hide: every exception here
    /// must be one the policy already makes, as narrow as the policy makes it.
    #[test]
    fn every_exception_is_the_policys_own_and_no_wider() {
        let policy = policy();
        let plan = plan();
        let accepts: Vec<_> =
            policy.rules().into_iter().filter(|rule| rule.target() == Some("ACCEPT")).collect();
        let passes: Vec<_> = plan.filters.iter().filter(|f| f.action == "pass").collect();
        assert_eq!(accepts.len(), passes.len(), "{passes:#?}");
        for (rule, filter) in accepts.iter().zip(passes.iter()) {
            assert_eq!(filter.dst, rule.destination().expect("an accept names a destination"));
            assert_eq!(
                filter.ip_proto.as_deref(),
                arg_after(&rule.args, "-p"),
                "the pinhole must stay bound to the protocol the policy bound it to"
            );
            assert_eq!(
                filter.dst_port.as_deref(),
                Some("49200-49299"),
                "a pinhole that lost its port range is a pinhole onto every port"
            );
        }
    }

    /// A policy with no pinhole is a valid policy; it must not silently gain one, and must not render
    /// an empty plan either.
    #[test]
    fn a_policy_without_a_pinhole_renders_only_denies() {
        let mut unconfigured = policy();
        unconfigured.proxy_ports = None;
        let plan = IfacePlan::derive(DEV, &unconfigured).expect("renders");
        assert!(
            plan.filters.iter().all(|filter| filter.action == "drop"),
            "{:#?}",
            plan.filters
        );
        assert!(plan.filter_count(Family::V4) > 0 && plan.filter_count(Family::V6) > 0);
    }

    #[test]
    fn the_plan_installs_the_clsact_qdisc_before_any_filter() {
        let plan = plan();
        let steps = plan.install_plan();
        assert_eq!(steps[0], vec!["qdisc", "add", "dev", DEV, EGRESS_QDISC]);
        assert!(steps[1..].iter().all(|step| step[0] == "filter"), "{steps:#?}");

        let (stdin, count) = plan_stdin(&plan);
        assert_eq!(count, steps.len());
        assert_eq!(stdin.lines().count(), count, "one line per step, or the count lies");
        assert!(stdin.lines().all(|line| line.starts_with("tc ")), "{stdin}");
        assert!(
            !stdin.lines().any(|line| line.contains('\t')),
            "a tab in a plan line would split into an argument the applier never rendered"
        );
    }

    /// Both families, on the interface as well as in the chain. An unfiltered family is the cheapest
    /// bypass there is.
    #[test]
    fn both_address_families_are_filtered() {
        let plan = plan();
        assert!(plan.filter_count(Family::V4) > 0);
        assert!(plan.filter_count(Family::V6) > 0);
    }

    #[test]
    fn prefix_containment_knows_what_covers_the_gateway() {
        assert_eq!(prefix_contains("172.16.0.0/12", "172.17.0.1"), Some(true));
        assert_eq!(prefix_contains("10.0.0.0/8", "172.17.0.1"), Some(false));
        assert_eq!(prefix_contains("169.254.0.0/16", "169.254.169.254/32"), Some(true));
        assert_eq!(prefix_contains("fc00::/7", "fd00::1"), Some(true));
        assert_eq!(prefix_contains("fe80::/10", "fd00::1"), Some(false));
        assert_eq!(prefix_contains("10.0.0.0/8", "fd00::1"), Some(false));
        assert_eq!(prefix_contains("not-a-prefix", "10.0.0.1"), None);
    }

    /// The measured iptables failure, reproduced in `tc` terms: the pinhole below the range drop that
    /// covers the gateway. Present, correct, and never reached.
    #[test]
    fn a_shadowed_pinhole_is_refused_rather_than_installed() {
        let shadowed = vec![
            IfaceFilter {
                family: Family::V4,
                pref: 101,
                dst: "172.16.0.0/12".into(),
                ip_proto: None,
                dst_port: None,
                action: "drop",
                why: "range deny",
            },
            IfaceFilter {
                family: Family::V4,
                pref: 102,
                dst: "172.17.0.1".into(),
                ip_proto: Some("tcp".into()),
                dst_port: Some("49200-49299".into()),
                action: "pass",
                why: "the proxy pinhole",
            },
        ];
        let refused = no_shadowed_exception(&shadowed).expect_err("must refuse");
        assert!(refused.contains("inert"), "{refused}");
        // The same two, the right way round, must be accepted — otherwise this test passes for a
        // checker that refuses everything.
        let mut ordered = shadowed;
        ordered.swap(0, 1);
        ordered[0].pref = 101;
        ordered[1].pref = 102;
        no_shadowed_exception(&ordered).expect("the pinhole above its covering deny is correct");
    }

    /// The parser **and the verifier**, against a literal `tc filter show dev eth0 egress` block
    /// rather than against something this file generated.
    ///
    /// The verifier leg is the one that matters: every other `verify_readback` test feeds it
    /// [`as_tc_output`], which this file writes, so a parser and a renderer that agreed on a shape
    /// the kernel never prints would pass all of them together. Here the expectation is hand-written
    /// from the two filters the capture describes, and the input is the kernel's own text —
    /// statistics lines, `installed`/`used` suffixes, trailing spaces and all.
    #[test]
    fn the_parser_and_the_verifier_both_read_the_shape_tc_actually_prints() {
        const CAPTURE: &str = "\
filter protocol ip pref 102 flower chain 0 
filter protocol ip pref 102 flower chain 0 handle 0x1 
  eth_type ipv4
  ip_proto tcp
  dst_ip 172.17.0.1
  dst_port 49200-49299
  skip_hw
	not_in_hw
	action order 1: gact action pass
	 random type none pass val 0
	 index 1 ref 1 bind 1 installed 2 sec used 2 sec
	Action statistics:
	Sent 0 bytes 0 pkt (dropped 0, overlimits 0 requeues 0) 
filter protocol ipv6 pref 111 flower chain 0 
filter protocol ipv6 pref 111 flower chain 0 handle 0x1 
  eth_type ipv6
  dst_ip fc00::/7
  skip_hw
	not_in_hw
	action order 1: gact action drop
	 random type none pass val 0
	 index 2 ref 1 bind 1 installed 2 sec used 0 sec
	Action statistics:
	Sent 168 bytes 4 pkt (dropped 4, overlimits 0 requeues 0) 
";
        let parsed = parse_filters(CAPTURE).expect("real tc output must parse");
        assert_eq!(parsed.len(), 2, "the handle-less header lines are not filters: {parsed:#?}");
        assert_eq!(parsed[0].protocol, "ip");
        assert_eq!(parsed[0].pref, 102);
        assert_eq!(parsed[0].chain, ACTIVE_CHAIN);
        assert_eq!(parsed[0].handle, "0x1");
        assert_eq!(parsed[0].key("dst_ip"), Some("172.17.0.1"));
        assert_eq!(parsed[0].key("ip_proto"), Some("tcp"));
        assert_eq!(parsed[0].key("dst_port"), Some("49200-49299"));
        assert_eq!(parsed[0].key("eth_type"), Some("ipv4"));
        assert_eq!(parsed[0].actions, vec!["pass".to_owned()]);
        assert_eq!(parsed[1].protocol, "ipv6");
        assert_eq!(parsed[1].key("dst_ip"), Some("fc00::/7"));
        assert_eq!(parsed[1].key("ip_proto"), None);
        assert_eq!(parsed[1].actions, vec!["drop".to_owned()]);
        // Complete accounting: every key the capture prints is retained, none invented.
        assert_eq!(
            parsed[1].keys.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            vec!["eth_type", "dst_ip"]
        );

        // The non-circular positive control. This plan is written out by hand from what the capture
        // above *means* — a proxy pinhole and a unique-local deny — and never rendered by this file.
        //
        // The capture is one filter short of a verifiable plan: the box it came from had no IPv4
        // drop, and `verify_readback` refuses a plan with an unfiltered family. So the third block
        // is derived from the capture's own IPv6 drop block — every byte of layout, indentation,
        // statistics and trailing whitespace is the kernel's, and only the family tokens and the
        // prefix are changed. That keeps this leg a test of real `tc` text rather than of
        // [`as_tc_output`].
        let v6_drop_block: String = CAPTURE
            .lines()
            .skip_while(|line| !line.starts_with("filter protocol ipv6"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert!(v6_drop_block.contains("gact action drop"), "{v6_drop_block}");
        let v4_drop_block = v6_drop_block
            .replace("protocol ipv6", "protocol ip")
            .replace("pref 111", "pref 101")
            .replace("eth_type ipv6", "eth_type ipv4")
            .replace("dst_ip fc00::/7", "dst_ip 10.0.0.0/8");
        let captured_text = format!("{CAPTURE}{v4_drop_block}");
        let captured = IfacePlan {
            dev: DEV.to_owned(),
            filters: vec![
                IfaceFilter {
                    family: Family::V4,
                    pref: 102,
                    dst: "172.17.0.1".to_owned(),
                    ip_proto: Some("tcp".to_owned()),
                    dst_port: Some("49200-49299".to_owned()),
                    action: "pass",
                    why: "the proxy pinhole, as the kernel printed it",
                },
                IfaceFilter {
                    family: Family::V6,
                    pref: 111,
                    dst: "fc00::/7".to_owned(),
                    ip_proto: None,
                    dst_port: None,
                    action: "drop",
                    why: "the unique-local deny, as the kernel printed it",
                },
                IfaceFilter {
                    family: Family::V4,
                    pref: 101,
                    dst: "10.0.0.0/8".to_owned(),
                    ip_proto: None,
                    dst_port: None,
                    action: "drop",
                    why: "the private-range deny, in the kernel's own layout",
                },
            ],
        };
        captured
            .verify_readback(&captured_text)
            .expect("the verifier must accept real tc output for the plan it describes");

        // ...and it is not accepting it blindly: one word of that same capture changed, and the
        // same verifier refuses. Without this leg the acceptance above could come from a verifier
        // that accepts anything shaped like tc output.
        let parked =
            captured_text.replace("ipv6 pref 111 flower chain 0", "ipv6 pref 111 flower chain 7");
        let refused = captured
            .verify_readback(&parked)
            .expect_err("a real capture with one filter parked in chain 7 must be refused");
        assert!(refused.contains("chain"), "{refused}");

        // F1, strict token accounting: the lines this parser *skips* are the ones worth hiding in.
        // Statistics lines and action bookkeeping carry nothing that is compared, so each is read to
        // the end and refused on anything unrecognised rather than skipped wholesale.
        let hidden: &[(&str, &str, &str)] = &[
            (
                "a match key smuggled onto a statistics line",
                "Sent 168 bytes 4 pkt (dropped 4, overlimits 0 requeues 0) ",
                "Sent 168 bytes 4 pkt (dropped 4, overlimits 0 requeues 0) dst_ip 0.0.0.0/0",
            ),
            (
                "an unread token on the action bookkeeping line",
                " index 2 ref 1 bind 1 installed 2 sec used 0 sec",
                " index 2 ref 1 bind 1 installed 2 sec used 0 sec goto chain 9",
            ),
            (
                "a probability restored under a `random type none` prefix",
                " random type none pass val 0\n\t index 2",
                " random type none pass val 7\n\t index 2",
            ),
            (
                "a non-numeric hardware count, whose value this parser steps over",
                "  skip_hw\n\tnot_in_hw\n\taction order 1: gact action drop",
                "  skip_hw in_hw_count dst_ip\n\tnot_in_hw\n\taction order 1: gact action drop",
            ),
        ];
        for (what, from, to) in hidden {
            let mutated = captured_text.replace(from, to);
            assert_ne!(&mutated, &captured_text, "{what}: the mutation did not apply");
            let refused = captured
                .verify_readback(&mutated)
                .expect_err(&format!("{what} must not verify"));
            assert!(!refused.is_empty(), "{what}: {refused}");
        }
    }

    /// Rewrite only the lines that belong to a filter of `protocol`, leaving the other family's
    /// lines exactly as they were.
    ///
    /// Every mutation below has to be applied per family and refused in both: a check that fires for
    /// IPv4 and not for IPv6 is not a check, it is an IPv6-shaped hole with a passing test over it.
    /// `edit` returns `Some(replacement)` for a line it rewrites (the replacement may be several
    /// lines) and `None` to leave it alone.
    fn in_family(text: &str, protocol: &str, edit: impl Fn(&str) -> Option<String>) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut current = String::new();
        for line in text.lines() {
            if let Some(rest) = line.trim_start().strip_prefix("filter protocol ") {
                current = rest.split_whitespace().next().unwrap_or_default().to_owned();
            }
            match edit(line).filter(|_| current == protocol) {
                Some(replacement) => out.push(replacement),
                None => out.push(line.to_owned()),
            }
        }
        let mut text = out.join("\n");
        text.push('\n');
        text
    }

    /// F1: the substitutions that survive a *lossy* readback untouched. Every one of these is a
    /// valid rule `tc` would accept and list, agreeing on protocol, pref, destination, ip_proto,
    /// port and terminal action — every field the old projection compared — while being a
    /// different filter. Both families, because a check applied to one is a bypass in the other.
    #[test]
    fn a_rule_that_differs_only_in_what_the_old_parser_discarded_is_refused() {
        let plan = plan();
        let faithful = as_tc_output(&plan);

        // 1. A NARROWER DENY: the same destination drop, restricted to one source. Traffic from
        //    every other source misses it, and nothing the old parser read has changed.
        for (family, src) in
            [("v4", "  src_ip 192.0.2.123\n"), ("v6", "  src_ip 2001:db8::123\n")]
        {
            let anchor = if family == "v4" { "  dst_ip 10.0.0.0/8\n" } else { "  dst_ip fc00::/7\n" };
            assert!(faithful.contains(anchor), "fixture anchor {anchor:?} missing");
            let narrowed = faithful.replace(anchor, &format!("{anchor}{src}"));
            let refused = plan
                .verify_readback(&narrowed)
                .expect_err("a source-restricted deny must not verify as the full deny");
            assert!(refused.contains("src_ip"), "{family}: {refused}");
        }

        // 2. AN INACTIVE CHAIN: installed, listed, never consulted on the egress path.
        for family in ["ip", "ipv6"] {
            let parked = faithful.replace(
                &format!("filter protocol {family} pref"),
                &format!("filter protocol {family} PREF_MARKER"),
            );
            let parked = parked.replace("PREF_MARKER", "pref");
            let parked = parked
                .lines()
                .map(|line| {
                    if line.trim_start().starts_with(&format!("filter protocol {family} ")) {
                        line.replace("chain 0", "chain 7")
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let refused = plan
                .verify_readback(&parked)
                .expect_err("a filter in an unreferenced chain must not verify");
            assert!(refused.contains("chain"), "{family}: {refused}");
        }

        // 3. A SECOND ACTION after the terminal one — the old parser kept only the last verb it saw.
        for family in ["ip", "ipv6"] {
            let two_actions = in_family(&faithful, family, |line| {
                line.trim_start()
                    .starts_with("action order 1: gact action")
                    .then(|| format!("{line}\n\taction order 2: gact action pass"))
            });
            let refused = plan
                .verify_readback(&two_actions)
                .expect_err("two actions on one filter must not verify");
            assert!(refused.contains("action"), "{family}: {refused}");
        }

        // 4. A DUPLICATE key, where the second silently overwrote the first.
        for family in ["ip", "ipv6"] {
            let duplicated = in_family(&faithful, family, |line| {
                line.trim_start().starts_with("dst_ip ").then(|| format!("{line}\n{line}"))
            });
            let refused = plan
                .verify_readback(&duplicated)
                .expect_err("a duplicated match key must not verify");
            assert!(refused.contains("twice"), "{family}: {refused}");
        }

        // 5. A DIFFERENT CLASSIFIER matching by different rules under the same header fields.
        for family in ["ip", "ipv6"] {
            let u32_classifier =
                in_family(&faithful, family, |line| Some(line.replace("flower", "u32")));
            let refused = plan
                .verify_readback(&u32_classifier)
                .expect_err("a non-flower classifier must not verify");
            assert!(refused.contains("classifier"), "{family}: {refused}");
        }

        // 6. TRUNCATION: the listing stops after a header, before the rule it describes.
        for family in ["ip", "ipv6"] {
            let cut = format!(
                "{}\nfilter protocol {family} pref 140 flower chain 0\n",
                faithful.trim_end()
            );
            let refused =
                plan.verify_readback(&cut).expect_err("a truncated listing must not verify");
            assert!(
                refused.contains("truncated") || refused.contains("filters"),
                "{family}: {refused}"
            );
        }

        // 7. An unknown predicate that is not a known bypass — refused because it is unread, not
        //    because this module happens to know what it does.
        for family in ["ip", "ipv6"] {
            let unknown = in_family(&faithful, family, |line| {
                line.trim_start()
                    .starts_with("dst_ip ")
                    .then(|| format!("{line}\n  tcp_flags 0x2"))
            });
            let refused =
                plan.verify_readback(&unknown).expect_err("an unknown predicate must not verify");
            assert!(refused.contains("unknown match key"), "{family}: {refused}");
        }

        // A verifier that refused everything would pass all seven. The real shape still verifies.
        plan.verify_readback(&faithful).expect("the faithful readback must still verify");
    }

    /// `skip_sw` means the software path never evaluates the rule: listed, and inert.
    #[test]
    fn a_filter_the_software_path_never_evaluates_is_refused() {
        let plan = plan();
        let inert = as_tc_output(&plan).replace("  skip_hw\n", "  skip_sw\n");
        let refused = plan.verify_readback(&inert).expect_err("skip_sw must not verify");
        assert!(refused.contains("skip_sw"), "{refused}");
    }

    #[test]
    fn a_faithful_readback_verifies() {
        let plan = plan();
        plan.verify_readback(&as_tc_output(&plan)).expect("the plan must verify against itself");
    }

    /// Each of these is a specific way containment can be broken while everything else stays intact,
    /// and each must be named by the refusal. A verifier that returns `Ok` for all of them passes
    /// [`a_faithful_readback_verifies`] just as well.
    #[test]
    fn a_broken_readback_is_refused_and_says_what_broke() {
        let plan = plan();
        let faithful = as_tc_output(&plan);

        let empty = plan.verify_readback("").expect_err("an unfiltered veth must not verify");
        assert!(empty.contains("expected"), "{empty}");

        // One filter never landed — a partial application, which exits 0 for every rule that did.
        let truncated: String = faithful
            .lines()
            .take_while(|line| !line.contains("pref 111"))
            .collect::<Vec<_>>()
            .join("\n");
        let short = plan.verify_readback(&truncated).expect_err("a short list must not verify");
        assert!(short.contains("egress filters"), "{short}");

        // The pinhole widened to every port, still exactly one pass filter.
        let widened = faithful.replace("dst_port 49200-49299", "dst_port 1-65535");
        let widened = plan.verify_readback(&widened).expect_err("a widened pinhole must not verify");
        assert!(widened.contains("dst_port"), "{widened}");

        // A deny that became TCP-only: the exact bug this module exists to close, installed under
        // the exact rule name that is supposed to close it.
        let tcp_only = faithful.replace("  dst_ip 10.0.0.0/8", "  ip_proto tcp\n  dst_ip 10.0.0.0/8");
        let tcp_only =
            plan.verify_readback(&tcp_only).expect_err("a TCP-only deny must not verify");
        assert!(tcp_only.contains("ip_proto"), "{tcp_only}");

        // A deny quietly turned into a pass.
        let flipped = faithful.replacen("gact action drop", "gact action pass", 1);
        let flipped = plan.verify_readback(&flipped).expect_err("a flipped action must not verify");
        assert!(flipped.contains("action"), "{flipped}");

        // An extra filter nobody rendered.
        let injected = format!(
            "{faithful}filter protocol ip pref 99 flower chain 0 handle 0x9 \n  dst_ip 0.0.0.0/0\n\
             \taction order 1: gact action pass\n"
        );
        let injected = plan.verify_readback(&injected).expect_err("an extra filter must not verify");
        assert!(injected.contains("egress filters"), "{injected}");
    }

    /// A v4-only namespace reads as complete and routes straight out over v6.
    #[test]
    fn a_readback_missing_a_whole_family_is_refused() {
        let policy = policy();
        let plan = plan();
        let v4_only: Vec<IfaceFilter> =
            plan.filters.iter().filter(|f| f.family == Family::V4).cloned().collect();
        let v4_plan = IfacePlan { dev: DEV.to_owned(), filters: v4_only };
        let refused = plan
            .verify_readback(&as_tc_output(&v4_plan))
            .expect_err("a v4-only namespace must not verify against a two-family plan");
        assert!(refused.contains("egress filters"), "{refused}");

        // …and even a plan that only ever asked for v4 must be refused, because the policy denies v6
        // and this layer is not allowed to be narrower than the policy.
        let v4_refused = v4_plan
            .verify_readback(&as_tc_output(&v4_plan))
            .expect_err("a plan with no v6 filters must not verify");
        assert!(v4_refused.contains("ipv6"), "{v4_refused}");
        assert!(policy.rule_count(Family::V6) > 0);
    }

    // -- interface identity -------------------------------------------------------------------

    const HOLDER_LINKS: &str = "\
1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN mode DEFAULT group default qlen 1000\\    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00 promiscuity 0 minmtu 0 maxmtu 0 numtxqueues 1 numrxqueues 1 gso_max_size 65536 gso_max_segs 65535 
107: eth0@if108: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP mode DEFAULT group default \\    link/ether 02:42:ac:11:00:02 brd ff:ff:ff:ff:ff:ff link-netnsid 0 promiscuity 0 minmtu 68 maxmtu 65535 veth numtxqueues 4 numrxqueues 4 gso_max_size 65536 gso_max_segs 65535 
";

    /// The host's own namespace, which this must never filter: a physical interface and a bridge.
    const HOST_LINKS: &str = "\
1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN mode DEFAULT group default qlen 1000\\    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00 
2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc mq state UP mode DEFAULT group default qlen 1000\\    link/ether 5a:94:ef:12:00:01 brd ff:ff:ff:ff:ff:ff 
3: docker0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP mode DEFAULT group default \\    link/ether 02:42:1b:aa:bb:cc brd ff:ff:ff:ff:ff:ff promiscuity 0 bridge forward_delay 1500 
108: veth9a1b@if107: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue master docker0 state UP mode DEFAULT group default \\    link/ether 9a:1b:2c:3d:4e:5f brd ff:ff:ff:ff:ff:ff link-netnsid 1 promiscuity 1 veth 
";

    /// Fixture links that are expected to be readable. A fixture that stopped parsing would
    /// otherwise turn every identity test below into a vacuous refusal.
    fn links_of(text: &str) -> Vec<Link> {
        parse_links(text).expect("fixture link records must parse")
    }

    #[test]
    fn the_job_veth_is_selected_inside_the_holders_namespace() {
        let links = links_of(HOLDER_LINKS);
        assert_eq!(links.len(), 2, "{links:#?}");
        let chosen = select_egress_link(&links).expect("the holder's veth must be selectable");
        assert_eq!(chosen.name, "eth0");
        assert_eq!(chosen.index, 107);
        assert_eq!(chosen.peer_index, Some(108));
        assert_eq!(chosen.kind.as_deref(), Some("veth"));
    }

    /// The refusal that matters most: a host namespace is never filtered, and the error says so
    /// rather than picking whichever interface sorted first.
    #[test]
    fn the_hosts_own_namespace_is_refused() {
        let refused = select_egress_link(&links_of(HOST_LINKS)).expect_err("must refuse");
        assert!(refused.contains("bridge") || refused.contains("host"), "{refused}");
    }

    /// F1: a link record that does not parse is refused, not skipped. Skipping it let a namespace
    /// holding `lo`, a veth and one unreadable third link present the exact two-link shape
    /// [`select_egress_link`] accepts — and the discarded record is the one that would have forced
    /// the refusal.
    #[test]
    fn an_unreadable_link_record_is_refused_rather_than_skipped() {
        let with_garbage = format!("{HOLDER_LINKS}109: eth1@if110: <BROADCAST,UP> mtu 1500 \n");
        let refused = parse_links(&with_garbage)
            .expect_err("a record with no link type must not be silently dropped");
        assert!(refused.contains("link type"), "{refused}");

        assert!(parse_links("not a link record at all\n").is_err());
        assert!(parse_links("7: : <UP> \\    link/ether 02:42 veth \n").is_err(), "empty name");
        // The honest capture still parses, so this is not a parser that refuses everything.
        assert_eq!(links_of(HOLDER_LINKS).len(), 2);
    }

    #[test]
    fn every_ambiguous_or_wrong_shaped_namespace_is_refused() {
        assert!(select_egress_link(&[]).is_err(), "no links at all");
        assert!(
            select_egress_link(&links_of(
                "107: eth0@if108: <BROADCAST,UP> mtu 1500 \\    link/ether 02:42 veth \n"
            ))
            .is_err(),
            "a namespace with no loopback is not one this build made"
        );

        let two_veths = format!(
            "{HOLDER_LINKS}109: eth1@if110: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 \\    \
             link/ether 02:42:ac:11:00:03 veth \n"
        );
        let ambiguous = select_egress_link(&links_of(&two_veths)).expect_err("must refuse");
        assert!(ambiguous.contains("exactly one"), "{ambiguous}");

        let not_a_veth = HOLDER_LINKS.replace(" veth numtxqueues", " numtxqueues");
        let refused = select_egress_link(&links_of(&not_a_veth)).expect_err("must refuse");
        assert!(refused.contains("veth"), "{refused}");

        let down = HOLDER_LINKS.replace("<BROADCAST,MULTICAST,UP,LOWER_UP>", "<BROADCAST,MULTICAST>");
        let refused = select_egress_link(&links_of(&down)).expect_err("must refuse");
        assert!(refused.contains("down"), "{refused}");
    }

    #[test]
    fn the_probe_and_the_applier_are_different_containers_with_different_powers() {
        let probe = link_probe_argv("maxplayer-netns-j1", "img");
        assert!(probe.contains(&"--cap-drop".to_owned()));
        assert!(!probe.contains(&"NET_ADMIN".to_owned()), "a reader needs no capability: {probe:?}");
        assert!(probe.contains(&"container:maxplayer-netns-j1".to_owned()), "{probe:?}");

        let applier = iface_sidecar_argv("maxplayer-netns-j1", "img");
        assert!(applier.contains(&"NET_ADMIN".to_owned()));
        assert!(applier.contains(&"--interactive".to_owned()), "the plan arrives on stdin");
        assert!(applier.contains(&"/usr/local/bin/apply-iface".to_owned()), "{applier:?}");

        // The readback is a third container running a different verb, so it cannot install anything.
        let readback = filter_readback_argv("maxplayer-netns-j1", "img", DEV);
        assert!(readback.windows(2).any(|pair| pair == ["--entrypoint", "tc"]), "{readback:?}");
        assert!(readback.contains(&"show".to_owned()) && !readback.contains(&"add".to_owned()));
    }
}

/// Does `prefix` (CIDR, or a bare address meaning a host route) contain `address`?
///
/// `None` when either side does not parse or the families differ — an unanswerable question, which
/// every caller here treats as "cannot prove it is safe" rather than "safe".
pub fn prefix_contains(prefix: &str, address: &str) -> Option<bool> {
    use std::net::IpAddr;

    let (network, bits) = match prefix.split_once('/') {
        Some((network, len)) => (network.parse::<IpAddr>().ok()?, len.parse::<u32>().ok()?),
        None => {
            let network = prefix.parse::<IpAddr>().ok()?;
            let bits = if network.is_ipv4() { 32 } else { 128 };
            (network, bits)
        }
    };
    let address = address.split('/').next()?.parse::<IpAddr>().ok()?;

    match (network, address) {
        (IpAddr::V4(network), IpAddr::V4(address)) => {
            if bits > 32 {
                return None;
            }
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            Some(u32::from(network) & mask == u32::from(address) & mask)
        }
        (IpAddr::V6(network), IpAddr::V6(address)) => {
            if bits > 128 {
                return None;
            }
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            Some(u128::from(network) & mask == u128::from(address) & mask)
        }
        // Different families never contain one another, and `tc` keeps them on separate ethertypes
        // anyway.
        _ => Some(false),
    }
}

/// Refuse a filter list in which a `pass` is shadowed by an earlier `drop` covering its destination.
///
/// The failure it names is inertness, not incorrectness: the exception is present, spelled right,
/// and never reached. `sandbox_net` measured this exact shape on the iptables side — the pinhole
/// appended after the range drops "leaves it inert" — and a first-match classifier reproduces it
/// faithfully unless someone checks.
fn no_shadowed_exception(filters: &[IfaceFilter]) -> Result<(), String> {
    for pass in filters.iter().filter(|filter| filter.action == "pass") {
        for drop in filters
            .iter()
            .filter(|filter| filter.action == "drop" && filter.family == pass.family)
            .filter(|filter| filter.pref < pass.pref)
        {
            if prefix_contains(&drop.dst, &pass.dst) != Some(false) {
                return Err(format!(
                    "the exception for {} at pref {} sits below the drop for {} at pref {}, which \
                     covers it — tc takes the first match, so that exception is inert ({})",
                    pass.dst, pass.pref, drop.dst, drop.pref, pass.why
                ));
            }
        }
    }
    Ok(())
}

/// The value following `flag` in an argv, if present.
fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|at| args.get(at + 1))
        .map(String::as_str)
}

// ---------------------------------------------------------------------------------------------
// Asking the kernel what it actually holds
// ---------------------------------------------------------------------------------------------

/// `docker run` argv for the sidecar that applies an interface plan.
///
/// The same shape as `sandbox_netns::sidecar_argv` and for the same reasons: `NET_ADMIN` and nothing
/// else, in a container that joins the namespace, applies a plan it did not choose, and exits before
/// the payload exists. `--entrypoint` names the interface applier rather than the iptables one, so a
/// plan of one kind cannot be fed to the applier for the other.
pub fn iface_sidecar_argv(holder_name: &str, image: &str) -> Vec<String> {
    [
        "docker",
        "run",
        "--rm",
        "--interactive",
        "--network",
        &crate::sandbox_netns::NetnsHolder::network_mode_for(holder_name),
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_ADMIN",
        "--security-opt",
        "no-new-privileges",
        "--entrypoint",
        "/usr/local/bin/apply-iface",
        image,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker run` argv that reads the installed filters back out of the namespace.
///
/// A **separate container** running a **different verb** (`filter show`, not `filter add`), for the
/// reason `sandbox_netns::readback_argv` states: the question is what the kernel holds, not whether
/// the installer believes it succeeded. Both families come back in one read, because the order
/// between them is part of what is verified and two reads could not see it.
pub fn filter_readback_argv(holder_name: &str, image: &str, dev: &str) -> Vec<String> {
    [
        "docker",
        "run",
        "--rm",
        "--network",
        &crate::sandbox_netns::NetnsHolder::network_mode_for(holder_name),
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_ADMIN",
        "--security-opt",
        "no-new-privileges",
        "--entrypoint",
        "tc",
        image,
        "filter",
        "show",
        "dev",
        dev,
        EGRESS_HOOK,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// The only chain reached from the `clsact` egress hook by default. A filter parked in any other
/// chain is installed, listed, and never consulted — it looks exactly like containment and is none.
pub const ACTIVE_CHAIN: u32 = 0;

/// The classifier this module installs, and the only one readback will bless.
pub const CLASSIFIER: &str = "flower";

/// The match keys a rendered filter may carry. **This list is the security boundary**: any other
/// predicate — `src_ip` above all — narrows what the rule matches while leaving every field this
/// module compares untouched, so an unknown key is refused rather than ignored.
const KNOWN_KEYS: &[&str] = &["eth_type", "dst_ip", "ip_proto", "dst_port"];

/// Valueless tokens `tc` prints that say nothing about what the rule matches. `skip_sw` is
/// deliberately absent: it means the software path never evaluates the rule, so a filter carrying it
/// is listed and inert exactly like one in an unreferenced chain.
const KNOWN_FLAGS: &[&str] = &["not_in_hw", "in_hw", "skip_hw"];

/// The `gact` verbs this module renders, and the only ones a readback may name. `tc` reprints the
/// verb inside the `random type none <verb> val 0` detail line, so the same list gates both places.
const GACT_VERBS: &[&str] = &["pass", "drop"];

/// One filter as `tc filter show` prints it, with **every token accounted for**.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadbackFilter {
    /// `ip` or `ipv6`.
    pub protocol: String,
    pub pref: u16,
    /// The chain the filter sits in. Only [`ACTIVE_CHAIN`] is on the egress path.
    pub chain: u32,
    pub handle: String,
    /// Match keys in printed order, every one of them — not a chosen projection.
    pub keys: Vec<(String, String)>,
    /// The `gact` verbs, in printed order. More than one is a refusal, not a last-one-wins.
    pub actions: Vec<String>,
}

impl ReadbackFilter {
    /// The value of one match key, if the filter carries it.
    pub fn key(&self, name: &str) -> Option<&str> {
        self.keys.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    fn describe(&self) -> String {
        let keys: Vec<String> =
            self.keys.iter().map(|(key, value)| format!("{key} {value}")).collect();
        format!(
            "protocol {} pref {} chain {} handle {} [{}] actions {:?}",
            self.protocol,
            self.pref,
            self.chain,
            self.handle,
            keys.join(", "),
            self.actions
        )
    }
}

/// Parse `tc filter show dev <dev> egress` output strictly, in kernel order.
///
/// **Every token is consumed or the parse fails.** The previous version of this function recorded a
/// chosen projection — protocol, pref, destination, ip_proto, port, last action — and silently
/// dropped the rest, which let a rule that was narrower (`src_ip` added), parked off the egress path
/// (`chain 7`), or carrying a second action compare equal to the rule that was meant to be there.
/// A comparison cannot recover information the parser discarded, so nothing is discarded.
///
/// `tc` prints a bare `filter protocol … pref … flower chain 0` header line per priority **and** a
/// second line carrying `handle`, followed by the match keys. Only the handle-bearing block is a
/// filter; counting the header too would double every total and make a namespace holding half the
/// plan look complete. The header must be followed by its handle line: a header alone is truncation.
pub fn parse_filters(stdout: &str) -> Result<Vec<ReadbackFilter>, String> {
    let mut filters: Vec<ReadbackFilter> = Vec::new();
    let mut pending_header: Option<(String, u16, u32)> = None;

    for (number, line) in stdout.lines().enumerate() {
        let at = number + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let fields: Vec<&str> = trimmed.split_whitespace().collect();

        if fields[0] == "filter" {
            let header = parse_filter_header(&fields, at)?;
            match header.handle.clone() {
                None => {
                    if pending_header.is_some() {
                        return Err(format!(
                            "line {at}: a second bare filter header arrived before the first one's \
                             handle line — the listing is truncated or interleaved"
                        ));
                    }
                    pending_header = Some((header.protocol, header.pref, header.chain));
                }
                Some(handle) => {
                    if let Some((protocol, pref, chain)) = pending_header.take() {
                        if protocol != header.protocol
                            || pref != header.pref
                            || chain != header.chain
                        {
                            return Err(format!(
                                "line {at}: the handle line (protocol {} pref {} chain {}) does not \
                                 match the header it follows (protocol {protocol} pref {pref} chain \
                                 {chain})",
                                header.protocol, header.pref, header.chain
                            ));
                        }
                    }
                    filters.push(ReadbackFilter {
                        protocol: header.protocol,
                        pref: header.pref,
                        chain: header.chain,
                        handle,
                        keys: Vec::new(),
                        actions: Vec::new(),
                    });
                }
            }
            continue;
        }

        let Some(current) = filters.last_mut() else {
            return Err(format!(
                "line {at}: {trimmed:?} appears before any filter header — this is not the output \
                 of `tc filter show` on an egress hook"
            ));
        };

        if fields[0] == "action" {
            parse_action_line(current, &fields, at)?;
            continue;
        }
        if is_counter_line(&fields) {
            check_counter_line(current, &fields, at)?;
            continue;
        }
        if is_action_detail(&fields) {
            parse_action_detail(current, &fields, at)?;
            continue;
        }
        parse_key_line(current, &fields, at)?;
    }

    if let Some((protocol, pref, _)) = pending_header {
        return Err(format!(
            "the listing ends on a bare header (protocol {protocol} pref {pref}) with no handle line \
             — truncated output is not a verified namespace"
        ));
    }
    for filter in &filters {
        if filter.actions.is_empty() {
            return Err(format!(
                "filter {} carries no action — a classifier that matches and does nothing is not \
                 containment, and a listing that ends mid-filter is truncation",
                filter.describe()
            ));
        }
    }
    Ok(filters)
}

struct FilterHeader {
    protocol: String,
    pref: u16,
    chain: u32,
    handle: Option<String>,
}

/// `filter protocol ip pref 100 flower chain 0 [handle 0x1]`, and nothing else.
fn parse_filter_header(fields: &[&str], at: usize) -> Result<FilterHeader, String> {
    let want = |index: usize, keyword: &str| -> Result<(), String> {
        match fields.get(index) {
            Some(field) if *field == keyword => Ok(()),
            other => Err(format!(
                "line {at}: expected {keyword:?} at position {index} of a filter header, found \
                 {other:?}"
            )),
        }
    };
    want(1, "protocol")?;
    want(3, "pref")?;
    want(6, "chain")?;

    let protocol = (*fields.get(2).ok_or(format!("line {at}: filter header names no protocol"))?)
        .to_owned();
    let pref: u16 = fields
        .get(4)
        .ok_or(format!("line {at}: filter header names no pref"))?
        .parse()
        .map_err(|_| format!("line {at}: {:?} is not a pref", fields[4]))?;
    let kind = *fields.get(5).ok_or(format!("line {at}: filter header names no classifier"))?;
    if kind != CLASSIFIER {
        return Err(format!(
            "line {at}: classifier is {kind:?}, expected {CLASSIFIER:?} — a different classifier \
             matches by different rules, whatever the listing looks like"
        ));
    }
    let chain: u32 = fields
        .get(7)
        .ok_or(format!("line {at}: filter header names no chain"))?
        .parse()
        .map_err(|_| format!("line {at}: {:?} is not a chain number", fields[7]))?;

    let handle = match fields.len() {
        8 => None,
        10 if fields[8] == "handle" => Some(fields[9].to_owned()),
        _ => {
            return Err(format!(
                "line {at}: unexpected trailing tokens in a filter header: {:?}",
                &fields[8.min(fields.len())..]
            ))
        }
    };
    Ok(FilterHeader { protocol, pref, chain, handle })
}

/// `action order 1: gact action drop`
fn parse_action_line(
    filter: &mut ReadbackFilter,
    fields: &[&str],
    at: usize,
) -> Result<(), String> {
    if fields.get(1) != Some(&"order") {
        return Err(format!("line {at}: malformed action line {fields:?}"));
    }
    let order: usize = fields
        .get(2)
        .and_then(|field| field.trim_end_matches(':').parse().ok())
        .ok_or(format!("line {at}: action line names no order"))?;
    if order != filter.actions.len() + 1 {
        return Err(format!(
            "line {at}: action order {order} arrived after {} action(s) — the listing is out of \
             order or a line is missing",
            filter.actions.len()
        ));
    }
    let kind = *fields.get(3).ok_or(format!("line {at}: action line names no kind"))?;
    if kind != "gact" {
        return Err(format!(
            "line {at}: action kind is {kind:?}, expected \"gact\" — this module renders nothing \
             else, and an unrecognised action can do anything at all"
        ));
    }
    if fields.get(4) != Some(&"action") {
        return Err(format!("line {at}: malformed gact action line {fields:?}"));
    }
    let verb = *fields.get(5).ok_or(format!("line {at}: gact names no verb"))?;
    if fields.len() > 6 {
        return Err(format!("line {at}: unexpected trailing tokens after the verb: {:?}", &fields[6..]));
    }
    filter.actions.push(verb.to_owned());
    Ok(())
}

fn is_action_detail(fields: &[&str]) -> bool {
    matches!(fields[0], "random" | "index")
}

/// The counter block `tc` prints under an action in the captured real output. Packet and byte
/// counts carry no match semantics, so they are consumed rather than refused — but only these
/// spellings, so an unrecognised line is still a refusal.
fn is_counter_line(fields: &[&str]) -> bool {
    matches!(fields[0], "Sent" | "backlog")
        || (fields[0] == "Action" && fields.get(1) == Some(&"statistics:"))
}

/// Tokens that may only ever appear where this parser reads semantics: on a header, an action line
/// or a match-key line. Finding one inside a statistics line means the line is not the statistics
/// line it looks like.
const SEMANTIC_TOKENS: &[&str] =
    &["filter", "flower", "action", "gact", "chain", "handle", "pref", "protocol"];

/// A statistics line carries counts, and counts carry nothing this module compares. It is consumed
/// rather than parsed field by field — the numbers differ on every run — but it is first checked to
/// contain no token that could carry match or action semantics, so "consumed" cannot become a place
/// to hide a predicate.
fn check_counter_line(
    filter: &ReadbackFilter,
    fields: &[&str],
    at: usize,
) -> Result<(), String> {
    if let Some(token) =
        fields.iter().find(|token| SEMANTIC_TOKENS.contains(token) || KNOWN_KEYS.contains(token))
    {
        return Err(format!(
            "line {at}: filter {} has {token:?} inside what is otherwise a statistics line \
             ({fields:?}) — statistics are skipped, so a predicate hidden in one would be skipped \
             with them",
            filter.describe()
        ));
    }
    Ok(())
}

/// The lines `tc` prints under an action, token by token.
///
/// `random type none` is the only randomness accepted: any other spelling is a rule that drops a
/// *fraction* of what it claims to drop. The `index` line is bookkeeping — install order, reference
/// counts, age — but it is still read to the end, because "the rest of this line is bookkeeping" is
/// exactly the assumption an added token would hide behind.
fn parse_action_detail(
    filter: &ReadbackFilter,
    fields: &[&str],
    at: usize,
) -> Result<(), String> {
    let unexpected = |what: &str| {
        Err(format!(
            "line {at}: filter {} carries an action detail this parser does not read ({what} in \
             {fields:?}) — an unread token is an unchecked token",
            filter.describe()
        ))
    };

    if fields[0] == "random" {
        if fields.get(1..3) != Some(&["type", "none"][..]) {
            return Err(format!(
                "line {at}: filter {} carries a randomised action ({fields:?}) — a probabilistic \
                 drop passes traffic it claims to stop",
                filter.describe()
            ));
        }
        // `random type none pass val 0` is what iproute2 prints for a non-random gact; nothing
        // else is accepted after `none`.
        return match &fields[3..] {
            [] => Ok(()),
            [verb, "val", value] if GACT_VERBS.contains(verb) && *value == "0" => Ok(()),
            rest => unexpected(&format!("{rest:?}")),
        };
    }

    // `index 1 ref 1 bind 1 installed 2 sec used 2 sec [firstused 2 sec]`
    let mut index = 1;
    if !fields.get(index).is_some_and(|value| value.parse::<u64>().is_ok()) {
        return unexpected("a non-numeric action index");
    }
    index += 1;
    while index < fields.len() {
        let token = fields[index];
        let value = fields.get(index + 1);
        let numeric = value.is_some_and(|value| value.parse::<u64>().is_ok());
        match token {
            "ref" | "bind" if numeric => index += 2,
            "installed" | "used" | "firstused" | "expires" if numeric => {
                index += 2;
                if fields.get(index) == Some(&"sec") {
                    index += 1;
                }
            }
            _ => return unexpected(&format!("{token:?}")),
        }
    }
    Ok(())
}

/// Match keys and hardware flags. Unknown predicates and duplicates are refusals.
fn parse_key_line(
    filter: &mut ReadbackFilter,
    fields: &[&str],
    at: usize,
) -> Result<(), String> {
    let mut index = 0;
    while index < fields.len() {
        let token = fields[index];
        if KNOWN_FLAGS.contains(&token) {
            index += 1;
            continue;
        }
        if token == "in_hw_count" {
            let count = fields.get(index + 1).ok_or(format!(
                "line {at}: in_hw_count has no value — truncated output is not a verified namespace"
            ))?;
            if count.parse::<u64>().is_err() {
                return Err(format!(
                    "line {at}: in_hw_count is {count:?}, not a count — this parser skips the \
                     value, so anything may be hiding in it"
                ));
            }
            index += 2;
            continue;
        }
        if !KNOWN_KEYS.contains(&token) {
            return Err(format!(
                "line {at}: unknown match key or token {token:?} on filter {} — an unrecognised \
                 predicate narrows what the rule matches while every compared field stays equal, \
                 so it is refused rather than ignored",
                filter.describe()
            ));
        }
        let value = *fields.get(index + 1).ok_or(format!(
            "line {at}: match key {token:?} has no value — truncated output is not a verified \
             namespace"
        ))?;
        if filter.keys.iter().any(|(key, _)| key == token) {
            return Err(format!(
                "line {at}: match key {token:?} appears twice on one filter — a duplicate silently \
                 overwrote the first value in the old parser"
            ));
        }
        filter.keys.push((token.to_owned(), value.to_owned()));
        index += 2;
    }
    Ok(())
}

/// `tc` prints a single address without its prefix length; the policy spells one with it.
fn normalise_prefix(address: &str) -> &str {
    address
        .strip_suffix("/32")
        .or_else(|| address.strip_suffix("/128"))
        .unwrap_or(address)
}

impl IfacePlan {
    /// Verify a live namespace against this plan, from that namespace's own `tc filter show` output.
    ///
    /// `Ok(())` means the filters are in force; an `Err` names what is wrong and is a reason to
    /// refuse the job. What is checked, and why each one is here rather than assumed:
    ///
    /// * **every rendered filter is present, in order, with its own match keys** — a filter list
    ///   that merely has the right length can be the right length and the wrong policy.
    /// * **no extra filters** — an unrendered `pass` at a low priority is an egress hole, and it is
    ///   the one edit that leaves every other property intact.
    /// * **every exception is at a lower `pref` than every drop** — checked against the kernel's
    ///   answer, not against the render, because the render is what is under suspicion.
    /// * **no drop carries an `ip_proto` match** — the leak this closes is protocol-independent, and
    ///   a TCP-only drop passes the very fixture that found it.
    /// * **both families are filtered** — an unfiltered address family is the cheapest bypass there
    ///   is.
    /// * **the classifier, the chain and the exact set of match keys** — checked by the parser and
    ///   again here. A rule that is `flower` on the active chain with exactly the rendered keys is
    ///   the rule that was asked for; a rule that merely agrees on the fields an older parser chose
    ///   to read can be narrower (`src_ip`), parked off the egress path (`chain 7`), or carry a
    ///   second action, and every one of those reads as a pass.
    pub fn verify_readback(&self, stdout: &str) -> Result<(), String> {
        let live = parse_filters(stdout)?;
        if live.len() != self.filters.len() {
            return Err(format!(
                "the namespace holds {} egress filters, expected {} — {:?}",
                live.len(),
                self.filters.len(),
                live
            ));
        }

        for (at, (want, got)) in self.filters.iter().zip(live.iter()).enumerate() {
            let protocol = tc_protocol(want.family);
            if got.protocol != protocol || got.pref != want.pref {
                return Err(format!(
                    "filter {at} is protocol {} pref {}, expected {protocol} pref {} ({})",
                    got.protocol, got.pref, want.pref, want.why
                ));
            }
            if got.chain != ACTIVE_CHAIN {
                return Err(format!(
                    "filter {at} (pref {}, {}) sits in chain {}, not the active chain \
                     {ACTIVE_CHAIN} — a filter in an unreferenced chain is listed, is never \
                     consulted on egress, and looks exactly like containment",
                    want.pref, want.dst, got.chain
                ));
            }
            if got.actions.len() != 1 {
                return Err(format!(
                    "filter {at} (pref {}, {}) carries {} actions {:?}, expected exactly one — a \
                     second action runs after the first and can undo it",
                    want.pref,
                    want.dst,
                    got.actions.len(),
                    got.actions
                ));
            }
            if got.actions[0] != want.action {
                return Err(format!(
                    "filter {at} (pref {}, {}) has action {:?}, expected {}",
                    want.pref, want.dst, got.actions[0], want.action
                ));
            }

            // The whole key set, compared as a set. Anything the render did not ask for is a
            // different rule, whatever the fields an older parser happened to read.
            let mut expected: Vec<(&str, String)> =
                vec![("eth_type", tc_eth_type(want.family).to_owned())];
            expected.push(("dst_ip", normalise_prefix(&want.dst).to_owned()));
            if let Some(proto) = want.ip_proto.as_deref() {
                expected.push(("ip_proto", proto.to_owned()));
            }
            if let Some(port) = want.dst_port.as_deref() {
                expected.push(("dst_port", port.to_owned()));
            }
            let mut seen: Vec<(&str, String)> = got
                .keys
                .iter()
                .map(|(key, value)| {
                    let value = if key == "dst_ip" {
                        normalise_prefix(value).to_owned()
                    } else {
                        value.clone()
                    };
                    (key.as_str(), value)
                })
                .collect();
            expected.sort();
            seen.sort();
            if seen != expected {
                return Err(format!(
                    "filter {at} (pref {}, {}) matches on {:?}, expected exactly {:?} — {}",
                    want.pref, want.dst, seen, expected, want.why
                ));
            }
        }

        // Order, read off the kernel's own list rather than off the render above — the render is
        // what is under suspicion, so re-deriving the answer from it would prove nothing.
        let live_filters: Vec<IfaceFilter> = live
            .iter()
            .map(|filter| IfaceFilter {
                family: if filter.protocol == tc_protocol(Family::V6) {
                    Family::V6
                } else {
                    Family::V4
                },
                pref: filter.pref,
                dst: filter.key("dst_ip").unwrap_or_default().to_owned(),
                ip_proto: filter.key("ip_proto").map(str::to_owned),
                dst_port: filter.key("dst_port").map(str::to_owned),
                action: match filter.actions.first().map(String::as_str) {
                    Some("pass") => "pass",
                    _ => "drop",
                },
                why: "read back from the live namespace",
            })
            .collect();
        no_shadowed_exception(&live_filters)?;
        if live.iter().any(|filter| {
            filter.actions.first().map(String::as_str) == Some("drop")
                && filter.key("ip_proto").is_some()
        }) {
            return Err(
                "a live drop filter carries an ip_proto match — the containment this closes is \
                 protocol-independent and a TCP-only drop is the bug, not the fix"
                    .to_owned(),
            );
        }
        for family in [Family::V4, Family::V6] {
            let protocol = tc_protocol(family);
            if !live.iter().any(|filter| {
                filter.protocol == protocol
                    && filter.actions.first().map(String::as_str) == Some("drop")
            }) {
                return Err(format!(
                    "the namespace holds no {protocol} drop filter — an unfiltered address family is \
                     the cheapest bypass there is"
                ));
            }
        }
        Ok(())
    }
}
