//! Host-side network containment for a docker job (#797): deny the seller's LAN and the seller's own
//! host services, leave the public internet open.
//!
//! The container runs a stranger's code. `docs/SANDBOXING.md` §1 reasons about egress as
//! internet-shaped — docs pages, registries, obscure hosts — and that argument assumes the only thing
//! worth stealing is a durable credential. It is not. A job on a bridge network can also reach
//! whatever the seller runs locally: a Lightning node's REST port, a database, an admin UI bound to
//! `0.0.0.0`. That is a different and worse target, and nothing in the credential design touches it.
//!
//! ── Why two chains, and why one of them is the whole point ───────────────────────────────────────
//! Container traffic splits by destination BEFORE any filter chain sees it, and the two halves land
//! in different places:
//!
//!   * container → LAN host / internet — ROUTED. Traverses `FORWARD`, which docker hooks via
//!     `DOCKER-USER`.
//!   * container → the HOST ITSELF (the bridge gateway, and equally the host's own LAN address) —
//!     NOT routed. The packet is destined to a local address, so it terminates here and traverses
//!     `INPUT`. **`DOCKER-USER` lives in `FORWARD` and never sees it.**
//!
//! So a host-services deny written only into `DOCKER-USER` installs cleanly, reports success, and
//! blocks nothing on the path that matters most. Reaching the seller's own services is the more
//! interesting half of this ticket and it is precisely the half `DOCKER-USER` cannot filter. Both
//! chains are rendered here, from one policy, so neither can be forgotten independently.
//!
//! The `INPUT` leg also covers a case a gateway-address rule would miss: a job dialling the host's
//! *LAN* address (not the bridge address) is still talking to the host, still arrives on the sandbox
//! bridge, and is still `INPUT`. [`Chain::Input`]'s deny is written against the INTERFACE, not
//! against an address, so every host-local address is covered by construction.
//!
//! ── The pinhole, and the one address that never gets one ─────────────────────────────────────────
//! Credential containment (#647, PR #807) forwards `ANTHROPIC_BASE_URL=http://host.docker.internal:<port>`
//! and needs container→host reach to the per-job proxy. A blanket host deny kills the control the ADR
//! calls the v1 exfiltration boundary, so the deny carries exactly one exception: TCP, to the bridge
//! gateway, on the proxy port range ([`crate::home::SandboxConfig::proxy_port_range`]). Nothing else.
//!
//! `169.254.169.254` — the cloud metadata endpoint — gets NO pinhole and is dropped by a rule of its
//! own, ahead of everything. It is already inside the denied `169.254.0.0/16`, so the standalone rule
//! adds no coverage today; it exists so that a future link-local exception cannot silently take the
//! metadata endpoint with it, and so the drop has its own log line.
//!
//! ── Rules live in our own chains, and only ever match our own interface ──────────────────────────
//! Every rule is scoped `-i <sandbox bridge>`. A seller box runs real services — this policy must be
//! incapable of matching traffic that is not a sandbox job's, and interface scoping is what makes
//! that true rather than merely intended. [`rules`] is asserted against that invariant in tests.
//!
//! Both chains are OURS ([`FORWARD_CHAIN`], [`INPUT_CHAIN`]), reached by a jump inserted at the top
//! of `DOCKER-USER` and `INPUT`. Nothing edits, reorders or flushes a rule this crate did not create,
//! so install and revert are surgical on a box whose firewall belongs to someone else.

use std::fmt;

/// Our child chain for ROUTED sandbox traffic, jumped to from `DOCKER-USER`.
pub const FORWARD_CHAIN: &str = "MAXPLAYER-SBX-FWD";
/// Our child chain for sandbox traffic terminating on the HOST, jumped to from `INPUT`.
pub const INPUT_CHAIN: &str = "MAXPLAYER-SBX-IN";

/// Docker's own hook point in `FORWARD`, guaranteed to exist wherever the daemon runs and documented
/// as the place operator rules belong.
pub const DOCKER_USER_CHAIN: &str = "DOCKER-USER";
/// The kernel's `INPUT` chain — the only chain that sees container→host traffic.
pub const HOST_INPUT_CHAIN: &str = "INPUT";

/// The cloud metadata endpoint. Reachable from a container on most cloud hosts, and it serves
/// instance credentials to anything that asks.
pub const METADATA_ENDPOINT: &str = "169.254.169.254/32";

/// Destinations a job may never reach: RFC1918 private space, link-local, CGNAT, and the ranges no
/// job has any business routing to.
///
/// Link-local (`169.254.0.0/16`) is in the list for the metadata endpoint, but denying the whole /16
/// is correct on its own terms — nothing a job legitimately fetches lives there.
///
/// CGNAT (`100.64.0.0/10`, RFC 6598) is the range this list most needed and least obviously covers.
/// "The LAN" is not only RFC1918: a seller running Tailscale or Headscale — a plausible setup for
/// someone already self-hosting a Lightning node — has its tailnet on `100.64.0.0/10`, reached over
/// `tailscale0` rather than the LAN interface. Traffic to it still enters on the sandbox bridge, so
/// the deny applies; without the range it routes out and matches nothing.
///
/// And it is not only an overlay concern. `crates/buzz/crates/buzz-core/src/network.rs` already
/// denies this range in this repo, for a second reason stated there: some providers serve INSTANCE
/// METADATA inside CGNAT space rather than at `169.254.169.254`. [`METADATA_ENDPOINT`] below is a
/// deliberate, ordered-first drop of one spelling of that endpoint; a provider using the CGNAT
/// spelling was reachable past it. Denying the range is what makes that drop provider-independent.
///
/// The remaining three carry no legitimate job traffic and are cheap to refuse: benchmarking
/// (`198.18.0.0/15`, RFC 2544), multicast (`224.0.0.0/4`) and reserved (`240.0.0.0/4`).
///
/// IPv6 has no equivalent list here because these rules are `iptables` (v4). The v6 LAN-equivalents
/// are ULA `fc00::/7` and link-local `fe80::/10` — see the `EnableIPv6` assertion in
/// `maxplayer sandbox-net verify`, which refuses to call a v6-enabled network contained rather than
/// leave a second address family silently unfiltered.
pub const DENIED_DESTINATIONS: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "100.64.0.0/10",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// Log lines are rate-limited so a job cannot fill the seller's disk by hammering a denied address.
const LOG_RATE: &str = "6/min";
const LOG_BURST: &str = "12";

/// A contiguous TCP port range the credential proxy binds inside, so a static firewall rule can name
/// the pinhole.
///
/// The proxy otherwise binds port 0 — a fresh random high port per job — which no static rule can
/// express. Configuring a range is what makes the pinhole writable; leaving it unset preserves the
/// random-port default (see [`crate::home::SandboxConfig::proxy_port_range`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    start: u16,
    end: u16,
}

impl PortRange {
    /// Refuses an inverted range and port 0. Port 0 is not a port here — it is the kernel's
    /// "choose one for me" sentinel, and accepting it in a range would produce a rule matching a
    /// port the proxy can never actually be bound to.
    pub fn new(start: u16, end: u16) -> Result<Self, PortRangeError> {
        if start == 0 || end == 0 {
            return Err(PortRangeError::ZeroPort);
        }
        if start > end {
            return Err(PortRangeError::Inverted { start, end });
        }
        Ok(Self { start, end })
    }

    /// Parse `"49200-49299"`, or `"49200"` for a single port.
    pub fn parse(text: &str) -> Result<Self, PortRangeError> {
        let text = text.trim();
        let (start, end) = match text.split_once('-') {
            Some((start, end)) => (start.trim(), end.trim()),
            None => (text, text),
        };
        let parse_one = |value: &str| {
            value
                .parse::<u16>()
                .map_err(|_| PortRangeError::Unparsable(text.to_owned()))
        };
        Self::new(parse_one(start)?, parse_one(end)?)
    }

    pub fn start(self) -> u16 {
        self.start
    }

    pub fn end(self) -> u16 {
        self.end
    }

    /// How many ports the range offers — the ceiling on concurrent contained jobs, since each job
    /// holds its own listener for its lifetime.
    pub fn capacity(self) -> u32 {
        u32::from(self.end - self.start) + 1
    }

    /// `iptables --dport` syntax: `start:end`.
    pub fn to_match(self) -> String {
        format!("{}:{}", self.start, self.end)
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortRangeError {
    ZeroPort,
    Inverted { start: u16, end: u16 },
    Unparsable(String),
}

impl fmt::Display for PortRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPort => write!(
                f,
                "port 0 is the kernel's \"pick one\" sentinel, not a port a rule can name"
            ),
            Self::Inverted { start, end } => {
                write!(f, "port range {start}-{end} ends before it starts")
            }
            Self::Unparsable(text) => {
                write!(f, "port range {text:?} is not `<port>` or `<start>-<end>`")
            }
        }
    }
}

impl std::error::Error for PortRangeError {}

/// Which chain a rule belongs in, and therefore which half of the split traffic it can filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    /// Routed traffic: container → LAN, container → internet.
    Forward,
    /// Host-terminating traffic: container → any address the host itself owns.
    Input,
}

impl Chain {
    /// The chain the rule is written into.
    pub fn own_chain(self) -> &'static str {
        match self {
            Self::Forward => FORWARD_CHAIN,
            Self::Input => INPUT_CHAIN,
        }
    }

    /// The chain our chain is jumped to FROM.
    pub fn parent_chain(self) -> &'static str {
        match self {
            Self::Forward => DOCKER_USER_CHAIN,
            Self::Input => HOST_INPUT_CHAIN,
        }
    }
}

/// One rendered rule: the chain it belongs to, its `iptables` arguments, and why it exists.
///
/// `why` is carried as data rather than a comment so `sandbox-net plan` can print the reasoning
/// beside each rule. An operator is being asked to install firewall rules on a live box; a bare
/// argv list is not a reviewable artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub chain: Chain,
    pub args: Vec<String>,
    pub why: &'static str,
}

impl Rule {
    fn new(chain: Chain, args: Vec<&str>, why: &'static str) -> Self {
        Self {
            chain,
            args: args.into_iter().map(String::from).collect(),
            why,
        }
    }

    /// The full `iptables` argv that appends this rule to its own chain.
    pub fn append_argv(&self) -> Vec<String> {
        let mut argv = vec!["-A".to_owned(), self.chain.own_chain().to_owned()];
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// How this rule reads back from `iptables -S <chain>`, for verification against a live box.
    pub fn as_spec_line(&self) -> String {
        format!("-A {} {}", self.chain.own_chain(), self.args.join(" "))
    }
}

/// The sandbox network as the host sees it, plus the knobs the policy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetPolicy {
    /// Host-side interface of the sandbox docker network (`br-<id>`, or a `com.docker.network.bridge.name`).
    /// Every rule is scoped to it.
    pub bridge: String,
    /// The bridge's host address — the address `host.docker.internal` resolves to inside a container.
    pub gateway: String,
    /// The proxy pinhole. `None` ⇒ no host reach at all, which is correct for a seat that runs no
    /// contained credential, and fatal for one that does.
    pub proxy_ports: Option<PortRange>,
    /// Connection and DNS logging (#797 requirement 3). Worth having with or without an allowlist:
    /// it is how anyone notices a job probing the LAN.
    pub log_connections: bool,
}

impl NetPolicy {
    /// The rules, in the order they must be installed. Order is load-bearing: the pinhole accept
    /// precedes the host deny, and the metadata drop precedes everything that could pass it.
    pub fn rules(&self) -> Vec<Rule> {
        let mut rules = Vec::new();
        let bridge = self.bridge.as_str();

        // ── Routed traffic: deny the LAN, leave the internet alone ──────────────────────────────
        //
        // Logging comes first so a denied attempt is logged before it is dropped. A LOG rule after
        // the drops would only ever see traffic that was allowed, which is the opposite of the
        // question "is a job probing the LAN?".
        if self.log_connections {
            rules.push(Rule::new(
                Chain::Forward,
                vec![
                    "-i", bridge, "-p", "tcp", "--syn", "-m", "limit", "--limit", LOG_RATE,
                    "--limit-burst", LOG_BURST, "-j", "LOG", "--log-prefix", "sbx-net conn: ",
                ],
                "log every new outbound TCP connection a job opens",
            ));
            rules.push(Rule::new(
                Chain::Forward,
                vec![
                    "-i", bridge, "-p", "udp", "--dport", "53", "-m", "limit", "--limit", LOG_RATE,
                    "--limit-burst", LOG_BURST, "-j", "LOG", "--log-prefix", "sbx-net dns: ",
                ],
                "log DNS queries, including the ones the denies below then drop",
            ));
            rules.push(Rule::new(
                Chain::Forward,
                vec![
                    "-i", bridge, "-d", METADATA_ENDPOINT, "-m", "limit", "--limit", LOG_RATE,
                    "--limit-burst", LOG_BURST, "-j", "LOG", "--log-prefix",
                    "sbx-net deny metadata: ",
                ],
                "a job reaching for instance credentials is worth its own log line",
            ));
        }
        rules.push(Rule::new(
            Chain::Forward,
            vec!["-i", bridge, "-d", METADATA_ENDPOINT, "-j", "DROP"],
            "cloud metadata serves instance credentials to anything that asks — never a pinhole",
        ));
        for denied in DENIED_DESTINATIONS {
            if self.log_connections {
                rules.push(Rule::new(
                    Chain::Forward,
                    vec![
                        "-i", bridge, "-d", denied, "-m", "limit", "--limit", LOG_RATE,
                        "--limit-burst", LOG_BURST, "-j", "LOG", "--log-prefix", "sbx-net deny: ",
                    ],
                    "log the LAN probe before dropping it",
                ));
            }
            rules.push(Rule::new(
                Chain::Forward,
                vec!["-i", bridge, "-d", denied, "-j", "DROP"],
                "the seller's LAN is not the job's to reach",
            ));
        }

        // ── Host-terminating traffic: deny the host, except the proxy pinhole ───────────────────
        //
        // This is the leg `DOCKER-USER` cannot filter. Note what is NOT here: no address match on
        // the deny. The rule denies by INTERFACE, so it covers the bridge gateway, the host's LAN
        // address, and any other address the host happens to own.
        rules.push(Rule::new(
            Chain::Input,
            vec![
                "-i", bridge, "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT",
            ],
            "replies to host-initiated flows — without this the deny below breaks the return leg",
        ));
        if let Some(ports) = self.proxy_ports {
            rules.push(Rule::new(
                Chain::Input,
                vec![
                    "-i",
                    bridge,
                    "-p",
                    "tcp",
                    "-d",
                    self.gateway.as_str(),
                    "--dport",
                    &ports.to_match(),
                    "-j",
                    "ACCEPT",
                ],
                "the #647 credential proxy — the single host service a job may reach",
            ));
        }
        if self.log_connections {
            rules.push(Rule::new(
                Chain::Input,
                vec![
                    "-i", bridge, "-m", "limit", "--limit", LOG_RATE, "--limit-burst", LOG_BURST,
                    "-j", "LOG", "--log-prefix", "sbx-net deny host: ",
                ],
                "a job reaching for a host service is the signal this ticket exists to surface",
            ));
        }
        rules.push(Rule::new(
            Chain::Input,
            vec!["-i", bridge, "-j", "DROP"],
            "every host service except the pinhole above",
        ));

        rules
    }

    /// The full install plan as `iptables` argv lists: create both chains, fill them, then jump to
    /// them from their parents.
    ///
    /// The jumps go in LAST and are INSERTED AT THE TOP of their parent chains. Last, because a jump
    /// installed before the rules exist would route live traffic through an empty chain. At the top,
    /// because `DOCKER-USER` on a docker install may already end in `RETURN`, and a rule appended
    /// after a `RETURN` is never reached — it installs cleanly and filters nothing.
    pub fn install_plan(&self) -> Vec<Vec<String>> {
        let mut plan: Vec<Vec<String>> = Vec::new();
        for chain in [Chain::Forward, Chain::Input] {
            plan.push(argv(["-N", chain.own_chain()]));
        }
        for rule in self.rules() {
            plan.push(rule.append_argv());
        }
        for chain in [Chain::Forward, Chain::Input] {
            plan.push(argv([
                "-I",
                chain.parent_chain(),
                "1",
                "-j",
                chain.own_chain(),
            ]));
        }
        plan
    }

    /// The revert plan: drop the jumps first, then empty and remove our chains.
    ///
    /// Jump-first is the same ordering argument inverted — a chain cannot be deleted while something
    /// still jumps to it, and removing the jump first means traffic stops traversing our rules
    /// before those rules start disappearing.
    pub fn revert_plan(&self) -> Vec<Vec<String>> {
        let mut plan: Vec<Vec<String>> = Vec::new();
        for chain in [Chain::Forward, Chain::Input] {
            plan.push(argv([
                "-D",
                chain.parent_chain(),
                "-j",
                chain.own_chain(),
            ]));
        }
        for chain in [Chain::Forward, Chain::Input] {
            plan.push(argv(["-F", chain.own_chain()]));
            plan.push(argv(["-X", chain.own_chain()]));
        }
        plan
    }

    /// What a live box must read back for the policy to be in force, as `iptables -S` lines.
    ///
    /// This is the artifact a verify reads against. A policy that is configured but not installed is
    /// the failure this exists to catch: the config says contained, the box says otherwise, and
    /// nothing in a job's behaviour distinguishes the two.
    pub fn expected_spec_lines(&self) -> Vec<String> {
        self.rules().iter().map(Rule::as_spec_line).collect()
    }
}

fn argv<'a, const N: usize>(parts: [&'a str; N]) -> Vec<String> {
    parts.into_iter().map(String::from).collect()
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

    /// The invariant that makes this safe to install on a box running real services: a rule that
    /// does not name our interface could match traffic that is not a job's.
    #[test]
    fn every_rule_is_scoped_to_the_sandbox_bridge() {
        for rule in policy().rules() {
            let scoped = rule
                .args
                .windows(2)
                .any(|pair| pair[0] == "-i" && pair[1] == "br-sbx0");
            assert!(scoped, "rule escapes the sandbox bridge: {:?}", rule.args);
        }
    }

    /// The finding this module exists for. A deny that covers only `DOCKER-USER` installs cleanly
    /// and leaves host services wide open, because container→host is never forwarded.
    #[test]
    fn host_denial_is_written_into_input_not_only_docker_user() {
        let rules = policy().rules();
        let input_drop = rules.iter().find(|rule| {
            rule.chain == Chain::Input && rule.args.last().map(String::as_str) == Some("DROP")
        });
        assert!(
            input_drop.is_some(),
            "no INPUT deny: DOCKER-USER cannot filter container→host, so this policy blocks nothing \
             on the host-services path"
        );
        assert_eq!(Chain::Input.parent_chain(), HOST_INPUT_CHAIN);
        assert_eq!(Chain::Forward.parent_chain(), DOCKER_USER_CHAIN);
    }

    /// Ordering is the difference between a working pinhole and a job that cannot reach its model.
    #[test]
    fn the_pinhole_accept_precedes_the_host_deny() {
        let rules = policy().rules();
        let input: Vec<&Rule> = rules.iter().filter(|r| r.chain == Chain::Input).collect();
        let accept = input
            .iter()
            .position(|r| r.args.contains(&"49200:49299".to_owned()))
            .expect("the pinhole must be rendered when a port range is configured");
        let deny = input
            .iter()
            .position(|r| r.args.last().map(String::as_str) == Some("DROP"))
            .expect("the host deny must exist");
        assert!(
            accept < deny,
            "the pinhole is shadowed by the deny above it: {input:#?}"
        );
    }

    /// A seat with no configured range gets no pinhole at all — never a wider one.
    ///
    /// Scoped to [`Chain::Input`] deliberately. An earlier form of this test asserted that no rule
    /// ANYWHERE named a `--dport`, which is a different property: it matched the DNS log rule on the
    /// routed chain and failed against correct output. A port on the host-terminating chain is the
    /// pinhole; a port on the routed chain is not.
    #[test]
    fn no_configured_range_opens_no_pinhole() {
        let configured = policy();
        let mut unconfigured = policy();
        unconfigured.proxy_ports = None;
        for rule in unconfigured.rules().iter().filter(|r| r.chain == Chain::Input) {
            assert!(
                !rule.args.iter().any(|arg| arg == "--dport"),
                "an unconfigured range must close the host, not open a port on it: {:?}",
                rule.args
            );
            assert!(
                !rule.args.contains(&"172.31.0.1".to_owned()),
                "no configured range means the gateway is never singled out for access: {:?}",
                rule.args
            );
        }
        // Positive control: the same assertions MUST fail on a configured policy, or they are
        // asserting nothing and would pass against a renderer that never emits a pinhole at all.
        assert!(
            configured
                .rules()
                .iter()
                .filter(|r| r.chain == Chain::Input)
                .any(|rule| rule.args.iter().any(|arg| arg == "--dport")),
            "the configured case must open the port this test proves the unconfigured case does not"
        );
    }

    /// The metadata endpoint is dropped, and no rule anywhere accepts it.
    #[test]
    fn metadata_is_dropped_and_never_accepted() {
        let rules = policy().rules();
        let dropped = rules.iter().any(|rule| {
            rule.args.contains(&METADATA_ENDPOINT.to_owned())
                && rule.args.last().map(String::as_str) == Some("DROP")
        });
        assert!(dropped, "the metadata endpoint must have its own drop");
        for rule in &rules {
            if rule.args.contains(&METADATA_ENDPOINT.to_owned()) {
                assert_ne!(
                    rule.args.last().map(String::as_str),
                    Some("ACCEPT"),
                    "metadata gets no pinhole, ever: {:?}",
                    rule.args
                );
            }
        }
    }

    #[test]
    fn every_denied_destination_is_dropped_on_the_routed_path() {
        let rules = policy().rules();
        for denied in DENIED_DESTINATIONS {
            let dropped = rules.iter().any(|rule| {
                rule.chain == Chain::Forward
                    && rule.args.contains(&(*denied).to_owned())
                    && rule.args.last().map(String::as_str) == Some("DROP")
            });
            assert!(dropped, "{denied} is not denied on the routed path");
        }
    }

    /// The loop above cannot catch a MISSING range: it iterates the same constant it checks, so
    /// deleting an entry deletes the assertion with it and the suite stays green. This names the
    /// ranges independently — the duplication IS the instrument, and it is the only thing here that
    /// can go red when a range is removed.
    ///
    /// Red-proved by deletion, not by inspection: dropping `100.64.0.0/10` from
    /// [`DENIED_DESTINATIONS`] fails this test and leaves the loop above passing.
    #[test]
    fn the_deny_list_names_every_lan_shaped_range_independently() {
        for required in [
            // RFC1918.
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            // Link-local, which carries the metadata endpoint.
            "169.254.0.0/16",
            // CGNAT (RFC 6598): Tailscale/Headscale tailnets, and instance metadata on providers
            // that do not serve it at 169.254.169.254. Denied in this repo already, at
            // crates/buzz/crates/buzz-core/src/network.rs, for both of those reasons.
            "100.64.0.0/10",
            // No legitimate job traffic: benchmarking (RFC 2544), multicast, reserved.
            "198.18.0.0/15",
            "224.0.0.0/4",
            "240.0.0.0/4",
        ] {
            assert!(
                DENIED_DESTINATIONS.contains(&required),
                "{required} must stay in the deny list: removing one silently re-opens a \
                 LAN-shaped range, and the rendering test cannot see the absence"
            );
        }
    }

    /// The jump must be inserted at the top of its parent, not appended: `DOCKER-USER` can already
    /// end in `RETURN`, and a rule after a `RETURN` is unreachable.
    #[test]
    fn jumps_are_inserted_at_the_top_and_installed_after_the_rules() {
        let plan = policy().install_plan();
        let jump = plan
            .iter()
            .position(|step| step[0] == "-I" && step[1] == DOCKER_USER_CHAIN)
            .expect("a jump from DOCKER-USER must exist");
        assert_eq!(plan[jump][2], "1", "the jump must go to the top of the chain");
        let last_rule = plan
            .iter()
            .rposition(|step| step[0] == "-A")
            .expect("rules must exist");
        assert!(
            last_rule < jump,
            "the jump was installed before the chain was filled — live traffic would traverse an \
             empty chain"
        );
    }

    /// Revert removes exactly what install added, and removes the jump before the chain it targets.
    #[test]
    fn revert_drops_the_jump_before_deleting_the_chain() {
        let plan = policy().revert_plan();
        let jump = plan
            .iter()
            .position(|step| step[0] == "-D" && step[1] == HOST_INPUT_CHAIN)
            .expect("the INPUT jump must be removed");
        let delete = plan
            .iter()
            .position(|step| step[0] == "-X" && step[1] == INPUT_CHAIN)
            .expect("our chain must be deleted");
        assert!(jump < delete, "a chain cannot be deleted while jumped to");
        assert!(
            plan.iter().all(|step| step[0] != "-F" || step[1] == FORWARD_CHAIN || step[1] == INPUT_CHAIN),
            "revert must only ever flush chains this crate created"
        );
    }

    #[test]
    fn port_range_parses_and_refuses_the_sentinel_and_the_inverted() {
        assert_eq!(
            PortRange::parse("49200-49299").unwrap(),
            PortRange::new(49200, 49299).unwrap()
        );
        assert_eq!(PortRange::parse("49200").unwrap().capacity(), 1);
        assert_eq!(PortRange::parse(" 49200 - 49299 ").unwrap().to_match(), "49200:49299");
        assert_eq!(PortRange::parse("0-10"), Err(PortRangeError::ZeroPort));
        assert_eq!(
            PortRange::parse("500-100"),
            Err(PortRangeError::Inverted { start: 500, end: 100 })
        );
        assert!(matches!(
            PortRange::parse("not-a-range"),
            Err(PortRangeError::Unparsable(_))
        ));
        assert_eq!(PortRange::new(49200, 49299).unwrap().capacity(), 100);
    }

    /// The spec lines are what a verify compares against a live box, so they must read back in the
    /// shape `iptables -S` prints.
    #[test]
    fn spec_lines_read_back_in_iptables_save_shape() {
        let lines = policy().expected_spec_lines();
        assert!(lines.iter().all(|line| line.starts_with("-A MAXPLAYER-SBX-")));
        assert!(lines
            .iter()
            .any(|line| line == "-A MAXPLAYER-SBX-FWD -i br-sbx0 -d 10.0.0.0/8 -j DROP"));
        assert!(lines
            .iter()
            .any(|line| line == "-A MAXPLAYER-SBX-IN -i br-sbx0 -j DROP"));
    }
}
