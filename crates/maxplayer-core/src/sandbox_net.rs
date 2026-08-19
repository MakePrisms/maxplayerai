//! Network containment for a docker job (#797): deny the seller's LAN and the seller's own host
//! services, leave the public internet open.
//!
//! The container runs a stranger's code. `docs/SANDBOXING.md` §1 reasons about egress as
//! internet-shaped — docs pages, registries, obscure hosts — and that argument assumes the only thing
//! worth stealing is a durable credential. It is not. A job on a bridge network can also reach
//! whatever the seller runs locally: a Lightning node's REST port, a database, an admin UI bound to
//! `0.0.0.0`. That is a different and worse target, and nothing in the credential design touches it.
//!
//! ── The rules live in the job's OWN network namespace ────────────────────────────────────────────
//! Containment is installed *into the namespace the job's traffic cannot leave*, not into the host's
//! firewall. A short-lived sidecar container joins the namespace, appends these rules, and exits; the
//! job then joins the same namespace and starts with the rules already in force. Two consequences,
//! and both are the point:
//!
//!   * **Containment lifetime == job lifetime.** Nothing outlives the job and nothing has to be
//!     reapplied after a reboot, a docker restart that recreates the bridge, or a `nixos-rebuild`.
//!     The state "configured but not enforced" — the state a host-side ruleset can silently drift
//!     into while a job is running — is not representable.
//!   * **No root on the seller's box, ever.** `CAP_NET_ADMIN` is scoped to a throwaway namespace held
//!     by the sidecar. The job itself is launched `--user <uid>:<gid> --cap-drop ALL
//!     --security-opt no-new-privileges`, so it has an empty capability *bounding* set and cannot
//!     alter or even read these rules.
//!
//! ── One chain, because a namespace has no routed/host-terminating split ──────────────────────────
//! A host-side policy has to filter in two places, because container traffic divides by destination
//! before any filter chain sees it: traffic to the LAN or internet is ROUTED (`FORWARD`, which docker
//! hooks via `DOCKER-USER`), while traffic to an address the host itself owns is NOT routed and
//! terminates in `INPUT`. A deny written only into `DOCKER-USER` installs cleanly, reports success,
//! and blocks nothing on the host-services path.
//!
//! Inside the job's own namespace that split does not exist: **everything the job sends is locally
//! generated, so all of it traverses `OUTPUT`.** One chain covers both halves.
//!
//! ── Destination-scoped, never interface-scoped ───────────────────────────────────────────────────
//! Every rule matches on `-d <cidr>` and no rule names an interface. This is deliberate and it is
//! what makes the sidecar safe to run before the namespace has finished being plumbed: the rules live
//! in the namespace and apply *whenever* an interface appears. A host-side policy could not have this
//! property — its rules were `-i <bridge>` and therefore depended on the bridge already existing,
//! which is the same fragility that made a docker restart able to silently un-contain a seat.
//!
//! ── Order is load-bearing, and the pinhole is the reason ─────────────────────────────────────────
//! Credential containment (#647, PR #807) forwards `ANTHROPIC_BASE_URL` pointing at a per-job proxy
//! on the host, reached at the namespace's gateway address. **That gateway is itself inside a denied
//! range** — `172.x.0.1` falls in `172.16.0.0/12` on a Linux bridge, and Docker Desktop's
//! host-gateway `192.168.65.254` falls in `192.168.0.0/16`. So the breadth of the LAN deny is both
//! the feature and the hazard: the same rule that covers the seller's host services also covers the
//! one host service the job legitimately needs.
//!
//! The pinhole ACCEPT must therefore precede the range drops. Measured, appending it *after* them
//! leaves it inert — iptables takes the first match, the drop wins, and every job silently loses
//! access to its model while the ruleset looks correct.
//!
//! `169.254.169.254` — the cloud metadata endpoint — gets NO pinhole and is dropped by a rule of its
//! own, ahead of everything. It is already inside the denied `169.254.0.0/16`, so the standalone rule
//! adds no coverage today; it exists so that a future link-local exception cannot silently take the
//! metadata endpoint with it, and so the drop has its own log line.
//!
//! ── What is deliberately NOT denied ──────────────────────────────────────────────────────────────
//! Loopback. Docker's embedded DNS resolver lives at `127.0.0.11` inside the namespace, so denying
//! `127.0.0.0/8` would break name resolution for every job. Nothing else answers on the job's
//! loopback, so the range carries no reachable target worth denying.

use std::fmt;

/// The kernel chain every rule is appended to. Inside the job's namespace all of its traffic is
/// locally generated, so this is the only chain that can see it.
pub const OUTPUT_CHAIN: &str = "OUTPUT";

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
/// someone already self-hosting a Lightning node — has its tailnet on `100.64.0.0/10`. Without the
/// range it is reachable.
///
/// And it is not only an overlay concern. `crates/buzz/crates/buzz-core/src/network.rs` already
/// denies this range in this repo, for a second reason stated there: some providers serve INSTANCE
/// METADATA inside CGNAT space rather than at `169.254.169.254`. [`METADATA_ENDPOINT`] above is a
/// deliberate, ordered-first drop of one spelling of that endpoint; a provider using the CGNAT
/// spelling was reachable past it. Denying the range is what makes that drop provider-independent.
///
/// The remaining three carry no legitimate job traffic and are cheap to refuse: benchmarking
/// (`198.18.0.0/15`, RFC 2544), multicast (`224.0.0.0/4`) and reserved (`240.0.0.0/4`).
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

/// The IPv6 equivalents of the list above.
///
/// A whole unfiltered address family is the cheapest bypass there is, and the one least likely to be
/// noticed: a v4-only policy reads as complete, every v4 test passes, and a job on a v6-enabled
/// network routes straight out. ULA `fc00::/7` is v6's RFC1918, `fe80::/10` its link-local, and
/// `ff00::/8` its multicast.
///
/// There is no v6 pinhole. The credential proxy is reached over v4 at the namespace gateway, so v6
/// carries no destination a job legitimately needs.
pub const DENIED_DESTINATIONS_V6: &[&str] = &["fc00::/7", "fe80::/10", "ff00::/8"];

/// Log lines are rate-limited so a job cannot fill the seller's disk by hammering a denied address.
const LOG_RATE: &str = "6/min";
const LOG_BURST: &str = "12";

/// The `--log-prefix` values, and the one hard constraint on them: **no whitespace**.
///
/// The install plan reaches the sidecar as one whitespace-delimited argv per line, and the applier
/// word-splits that line — the fields *are* the argv, which is what keeps the applier from needing a
/// quoting grammar or an `eval`. So an argument containing a space arrives as two arguments.
///
/// This is measured, not theoretical. A prefix of `"sbx-net conn: "` made iptables reject rule 1 of
/// 24 with ``Bad argument `conn:'``; the applier then exited 3 and the namespace was left with **no
/// rules at all**. Every rendering test still passed, because they assert the rendering and never
/// execute it. Hyphens keep each prefix a single field, and each is inside iptables' 29-character
/// limit. [`NetPolicy::rules`] is checked against this invariant by
/// `no_rendered_argument_contains_whitespace`.
const LOG_PREFIX_CONN: &str = "sbx-net-conn:";
const LOG_PREFIX_DNS: &str = "sbx-net-dns:";
const LOG_PREFIX_DENY_METADATA: &str = "sbx-net-deny-metadata:";
const LOG_PREFIX_DENY: &str = "sbx-net-deny:";

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

/// Which address family a rule belongs to, and therefore which binary installs it.
///
/// Carried as data rather than split into two rule lists so that ordering within a family is
/// expressed once, and so a readback can compare each family against exactly what was rendered for
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    /// The binary that installs and reads back this family's rules.
    pub fn binary(self) -> &'static str {
        match self {
            Self::V4 => "iptables",
            Self::V6 => "ip6tables",
        }
    }
}

/// One rendered rule: its address family, its `iptables` arguments, and why it exists.
///
/// `why` is carried as data rather than a comment so the policy can be printed with the reasoning
/// beside each rule. A bare argv list is not a reviewable artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub family: Family,
    pub args: Vec<String>,
    pub why: &'static str,
}

impl Rule {
    fn new(family: Family, args: Vec<&str>, why: &'static str) -> Self {
        Self {
            family,
            args: args.into_iter().map(String::from).collect(),
            why,
        }
    }

    /// The full argv that appends this rule, without the leading binary name.
    pub fn append_argv(&self) -> Vec<String> {
        let mut argv = vec!["-A".to_owned(), OUTPUT_CHAIN.to_owned()];
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// The `-j` target this rule jumps to, if it names one.
    pub fn target(&self) -> Option<&str> {
        arg_value(&self.args, "-j")
    }

    /// The `-d` destination this rule matches, if it names one.
    pub fn destination(&self) -> Option<&str> {
        arg_value(&self.args, "-d")
    }
}

/// The value following `flag` in an argv, if present.
fn arg_value<'a, S: AsRef<str>>(args: &'a [S], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg.as_ref() == flag)
        .and_then(|at| args.get(at + 1))
        .map(AsRef::as_ref)
}

/// An address as iptables prints it: a bare host address gains an explicit prefix length.
///
/// Measured, not assumed — `-d 172.17.0.1` reads back as `-d 172.17.0.1/32`. Comparing the two
/// directly would report a missing pinhole on a namespace whose pinhole is present and correct.
fn with_prefix_len(address: &str, family: Family) -> String {
    if address.contains('/') {
        return address.to_owned();
    }
    match family {
        Family::V4 => format!("{address}/32"),
        Family::V6 => format!("{address}/128"),
    }
}

/// One appended rule as a live namespace reports it, reduced to the fields that decide whether
/// containment holds.
///
/// iptables' cosmetic rewriting is discarded deliberately — see [`NetPolicy::verify_readback`] for
/// what it does to a rule between being given one and printing it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadbackRule {
    /// The `-d` value, exactly as printed (so already carrying its prefix length).
    pub destination: Option<String>,
    /// The `-j` target.
    pub target: Option<String>,
    /// The `--dport` value; a range for the pinhole.
    pub dport: Option<String>,
}

impl ReadbackRule {
    /// Parse the appended rules out of `iptables -S <chain>` output, in order.
    ///
    /// Only `-A` lines are rules. `-S` also prints the chain's default policy (`-P OUTPUT ACCEPT`),
    /// which is not a rule: counting it would inflate the total by one and make a namespace missing
    /// exactly one rule look complete.
    pub fn parse_all(stdout: &str) -> Vec<Self> {
        stdout
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("-A "))
            .map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                Self {
                    destination: arg_value(&fields, "-d").map(str::to_owned),
                    target: arg_value(&fields, "-j").map(str::to_owned),
                    dport: arg_value(&fields, "--dport").map(str::to_owned),
                }
            })
            .collect()
    }
}

/// The containment policy for one job's namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetPolicy {
    /// The namespace's gateway address — where the per-job credential proxy is reached. Inside a
    /// denied range by construction, which is why [`NetPolicy::proxy_ports`] exists.
    pub gateway: String,
    /// The proxy pinhole. `None` ⇒ no host reach at all, which is correct for a seat that runs no
    /// contained credential, and fatal for one that does.
    pub proxy_ports: Option<PortRange>,
    /// Connection and DNS logging (#797 requirement 3). Worth having with or without an allowlist:
    /// it is how anyone notices a job probing the LAN.
    pub log_connections: bool,
}

impl NetPolicy {
    /// The rules, in the order they must be installed. Order is load-bearing: the metadata drop
    /// precedes everything that could pass it, and the pinhole accept precedes the range drops that
    /// would otherwise shadow it.
    pub fn rules(&self) -> Vec<Rule> {
        let mut rules = Vec::new();

        // Logging first, so a denied attempt is logged before it is dropped. A LOG rule placed after
        // the drops would only ever see traffic that was allowed, which is the opposite of the
        // question "is a job probing the LAN?".
        if self.log_connections {
            rules.push(Rule::new(
                Family::V4,
                vec![
                    "-p", "tcp", "--syn", "-m", "limit", "--limit", LOG_RATE, "--limit-burst",
                    LOG_BURST, "-j", "LOG", "--log-prefix", LOG_PREFIX_CONN,
                ],
                "log every new outbound TCP connection a job opens",
            ));
            rules.push(Rule::new(
                Family::V4,
                vec![
                    "-p", "udp", "--dport", "53", "-m", "limit", "--limit", LOG_RATE,
                    "--limit-burst", LOG_BURST, "-j", "LOG", "--log-prefix", LOG_PREFIX_DNS,
                ],
                "log DNS queries, including the ones the denies below then drop",
            ));
            rules.push(Rule::new(
                Family::V4,
                vec![
                    "-d", METADATA_ENDPOINT, "-m", "limit", "--limit", LOG_RATE, "--limit-burst",
                    LOG_BURST, "-j", "LOG", "--log-prefix", LOG_PREFIX_DENY_METADATA,
                ],
                "a job reaching for instance credentials is worth its own log line",
            ));
        }

        // The metadata drop goes ahead of everything, including the pinhole, so that no present or
        // future ACCEPT can be written above it by accident.
        rules.push(Rule::new(
            Family::V4,
            vec!["-d", METADATA_ENDPOINT, "-j", "DROP"],
            "cloud metadata serves instance credentials to anything that asks — never a pinhole",
        ));

        // The pinhole, BEFORE the range drops. The gateway is inside a denied range, so this
        // ordering is the difference between a working seat and one whose jobs cannot reach a model.
        if let Some(ports) = self.proxy_ports {
            rules.push(Rule::new(
                Family::V4,
                vec![
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

        for denied in DENIED_DESTINATIONS {
            if self.log_connections {
                rules.push(Rule::new(
                    Family::V4,
                    vec![
                        "-d", denied, "-m", "limit", "--limit", LOG_RATE, "--limit-burst",
                        LOG_BURST, "-j", "LOG", "--log-prefix", LOG_PREFIX_DENY,
                    ],
                    "log the LAN probe before dropping it",
                ));
            }
            rules.push(Rule::new(
                Family::V4,
                vec!["-d", denied, "-j", "DROP"],
                "the seller's LAN, and the seller's own host services, are not the job's to reach",
            ));
        }

        // IPv6. No pinhole and no logging split — the proxy is v4, and a job has no legitimate v6
        // destination inside these ranges.
        for denied in DENIED_DESTINATIONS_V6 {
            rules.push(Rule::new(
                Family::V6,
                vec!["-d", denied, "-j", "DROP"],
                "the v6 LAN-equivalents — an unfiltered address family is the cheapest bypass",
            ));
        }

        rules
    }

    /// The install plan, as `(binary, argv)` pairs to run inside the job's namespace, in order.
    ///
    /// There are no chains to create and no jumps to insert: `OUTPUT` already exists in every
    /// namespace, and the namespace contains nothing but this job, so appending is sufficient and
    /// there is no foreign ruleset to interleave with.
    pub fn install_plan(&self) -> Vec<(&'static str, Vec<String>)> {
        self.rules()
            .iter()
            .map(|rule| (rule.family.binary(), rule.append_argv()))
            .collect()
    }

    /// How many rules this policy installs for one address family.
    pub fn rule_count(&self, family: Family) -> usize {
        self.rules().iter().filter(|rule| rule.family == family).count()
    }

    /// Verify a live namespace against this policy, from that namespace's own `iptables -S OUTPUT`
    /// output. `Ok(())` means containment is in force; an `Err` names what is missing and is a reason
    /// to refuse the job.
    ///
    /// **This is deliberately not a string comparison, and that is a measured decision.** iptables
    /// rewrites a rule between being given it and printing it back. Measured in a live namespace, on
    /// 21 v4 rules, it: makes an implicit match module explicit (`-p tcp` ⇒ `-p tcp -m tcp`), expands
    /// `--syn` to `--tcp-flags FIN,SYN,RST,ACK SYN`, gives a bare address its prefix length
    /// (`172.17.0.1` ⇒ `172.17.0.1/32`), **reorders arguments** (`-d` moves ahead of `-p`), and quotes
    /// `--log-prefix`. **12 of 21 lines came back textually different while the policy was perfectly
    /// in force.** Comparing strings would refuse every single launch. Reproducing iptables' printer
    /// in Rust to compare canonical forms would be a second, unverified implementation of it, and any
    /// drift between the two versions fails jobs closed for no reason.
    ///
    /// So this checks the properties that make containment *true*, each one chosen because a specific
    /// failure would otherwise pass:
    ///
    /// * **the rule count** — a partial application leaves earlier rules behind and exits non-zero,
    ///   but a runtime that silently dropped `--cap-add` could report success having installed
    ///   nothing;
    /// * **every denied destination carries a DROP** — one missing range is one open route to the
    ///   seller's LAN, and it is invisible in a count that happens to match;
    /// * **exactly the expected number of ACCEPTs** — an extra ACCEPT is an egress hole, and it is the
    ///   shape an injected rule would take;
    /// * **the pinhole names the measured gateway and only the proxy's ports** — a widened pinhole
    ///   still looks like one rule;
    /// * **the metadata DROP precedes the pinhole** — order is load-bearing, and an ACCEPT above the
    ///   metadata drop reopens the one destination the policy exists to close.
    pub fn verify_readback(&self, family: Family, stdout: &str) -> Result<(), String> {
        let found = ReadbackRule::parse_all(stdout);
        let expected = self.rule_count(family);
        if found.len() != expected {
            return Err(format!(
                "{} reports {} rules in {OUTPUT_CHAIN}, expected {expected} — the namespace is not \
                 the one this policy was installed into, or the install was partial",
                family.binary(),
                found.len()
            ));
        }

        let denied: &[&str] = match family {
            Family::V4 => DENIED_DESTINATIONS,
            Family::V6 => DENIED_DESTINATIONS_V6,
        };
        for destination in denied {
            let dropped = found.iter().any(|rule| {
                rule.target.as_deref() == Some("DROP")
                    && rule.destination.as_deref() == Some(*destination)
            });
            if !dropped {
                return Err(format!(
                    "{destination} has no DROP in the live namespace — that range is reachable from \
                     the job"
                ));
            }
        }

        if family == Family::V4 {
            let metadata_dropped_at = found.iter().position(|rule| {
                rule.target.as_deref() == Some("DROP")
                    && rule.destination.as_deref() == Some(METADATA_ENDPOINT)
            });
            let Some(metadata_dropped_at) = metadata_dropped_at else {
                return Err(format!(
                    "{METADATA_ENDPOINT} has no DROP in the live namespace — instance credentials are \
                     reachable from the job"
                ));
            };

            let accepts: Vec<(usize, &ReadbackRule)> = found
                .iter()
                .enumerate()
                .filter(|(_, rule)| rule.target.as_deref() == Some("ACCEPT"))
                .collect();
            let wanted = usize::from(self.proxy_ports.is_some());
            if accepts.len() != wanted {
                return Err(format!(
                    "the live namespace has {} ACCEPT rules, expected {wanted} — an unexpected ACCEPT \
                     is an egress hole",
                    accepts.len()
                ));
            }

            if let Some(ports) = self.proxy_ports {
                let (accept_at, pinhole) = accepts[0];
                let gateway = with_prefix_len(&self.gateway, Family::V4);
                if pinhole.destination.as_deref() != Some(gateway.as_str()) {
                    return Err(format!(
                        "the pinhole points at {:?}, not the measured proxy address {gateway} — the \
                         job cannot reach its model, or something else can",
                        pinhole.destination
                    ));
                }
                // iptables collapses a single-port range to a bare port, so both spellings of the
                // same range must be accepted; anything wider is a hole. Derived from `to_match`
                // rather than from `Display`, which spells a range `start-end` — a form iptables
                // never prints, so matching against it would prove nothing.
                let range = ports.to_match();
                let mut acceptable = vec![range.clone()];
                if let Some((start, end)) = range.split_once(':') {
                    if start == end {
                        acceptable.push(start.to_owned());
                    }
                }
                let printed = pinhole.dport.as_deref().unwrap_or("");
                if !acceptable.iter().any(|form| form == printed) {
                    return Err(format!(
                        "the pinhole opens ports {printed:?}, not the proxy's {} — a wider pinhole is \
                         still one rule",
                        ports.to_match()
                    ));
                }
                if accept_at < metadata_dropped_at {
                    return Err(format!(
                        "the pinhole ACCEPT is at index {accept_at}, above the metadata DROP at \
                         {metadata_dropped_at} — an ACCEPT above that drop reopens {METADATA_ENDPOINT}"
                    ));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> NetPolicy {
        NetPolicy {
            gateway: "172.31.0.1".to_owned(),
            proxy_ports: Some(PortRange::new(49200, 49299).unwrap()),
            log_connections: true,
        }
    }

    fn v4(rules: &[Rule]) -> Vec<Rule> {
        rules.iter().filter(|r| r.family == Family::V4).cloned().collect()
    }

    /// The invariant that replaces host-side interface scoping. Inside the job's namespace an
    /// interface match is unnecessary, and worse: it would make the rules depend on an interface
    /// existing when the sidecar runs, which is exactly the fragility that let a host-side ruleset
    /// go quietly inert when docker recreated the bridge.
    #[test]
    fn no_rule_names_an_interface() {
        for rule in policy().rules() {
            for flag in ["-i", "-o"] {
                assert!(
                    !rule.args.iter().any(|arg| arg == flag),
                    "rule is interface-scoped, which the namespace makes both unnecessary and \
                     order-dependent on plumbing: {:?}",
                    rule.args
                );
            }
        }
    }

    /// The host-services half. On a host-side policy this needed a second chain, because
    /// container→host is never forwarded and `DOCKER-USER` cannot see it. In the namespace the same
    /// coverage falls out of the range denies — the gateway, and every other address the host owns,
    /// is inside one of them.
    #[test]
    fn the_host_gateway_is_covered_by_a_range_deny() {
        let covered = DENIED_DESTINATIONS
            .iter()
            .any(|cidr| *cidr == "172.16.0.0/12");
        assert!(
            covered,
            "a linux bridge gateway is 172.x.0.1; without 172.16.0.0/12 the seller's own host \
             services are reachable"
        );
        assert!(
            DENIED_DESTINATIONS.contains(&"192.168.0.0/16"),
            "Docker Desktop's host-gateway is 192.168.65.254 — a mac seat needs this range for the \
             same reason"
        );
    }

    /// Ordering is the difference between a working pinhole and a job that cannot reach its model.
    /// Measured: appending the ACCEPT after the drops leaves it inert.
    #[test]
    fn the_pinhole_accept_precedes_the_range_denies() {
        let rules = v4(&policy().rules());
        let accept = rules
            .iter()
            .position(|r| r.args.contains(&"49200:49299".to_owned()))
            .expect("the pinhole must be rendered when a port range is configured");
        let first_range_drop = rules
            .iter()
            .position(|r| {
                r.args.contains(&"172.16.0.0/12".to_owned())
                    && r.args.last().map(String::as_str) == Some("DROP")
            })
            .expect("the range deny that covers the gateway must exist");
        assert!(
            accept < first_range_drop,
            "the pinhole is shadowed by the deny that covers the gateway, so every job loses its \
             model while the ruleset looks correct: {rules:#?}"
        );
    }

    /// A seat with no configured range gets no pinhole at all — never a wider one.
    #[test]
    fn no_configured_range_opens_no_pinhole() {
        let configured = policy();
        let mut unconfigured = policy();
        unconfigured.proxy_ports = None;
        for rule in unconfigured.rules() {
            assert!(
                !rule.args.contains(&"172.31.0.1".to_owned()),
                "no configured range means the gateway is never singled out for access: {:?}",
                rule.args
            );
            assert_ne!(
                rule.args.last().map(String::as_str),
                Some("ACCEPT"),
                "an unconfigured range must close the namespace, not accept anything: {:?}",
                rule.args
            );
        }
        // Positive control: the same assertions MUST fail on a configured policy, or they are
        // asserting nothing and would pass against a renderer that never emits a pinhole at all.
        assert!(
            configured
                .rules()
                .iter()
                .any(|rule| rule.args.last().map(String::as_str) == Some("ACCEPT")),
            "the configured case must open the pinhole this test proves the unconfigured case does \
             not"
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
    fn every_denied_destination_is_dropped() {
        let rules = policy().rules();
        for denied in DENIED_DESTINATIONS {
            let dropped = rules.iter().any(|rule| {
                rule.family == Family::V4
                    && rule.args.contains(&(*denied).to_owned())
                    && rule.args.last().map(String::as_str) == Some("DROP")
            });
            assert!(dropped, "{denied} is not denied");
        }
        for denied in DENIED_DESTINATIONS_V6 {
            let dropped = rules.iter().any(|rule| {
                rule.family == Family::V6
                    && rule.args.contains(&(*denied).to_owned())
                    && rule.args.last().map(String::as_str) == Some("DROP")
            });
            assert!(dropped, "{denied} is not denied on the v6 path");
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
        for required in ["fc00::/7", "fe80::/10", "ff00::/8"] {
            assert!(
                DENIED_DESTINATIONS_V6.contains(&required),
                "{required} must stay in the v6 deny list: an unfiltered address family reads as a \
                 complete policy and every v4 test still passes"
            );
        }
    }

    /// Loopback must NOT be denied. Docker's embedded DNS answers at `127.0.0.11` inside the
    /// namespace, so a `127.0.0.0/8` drop would break name resolution for every job — a failure
    /// that looks like "the internet is broken" rather than like a firewall rule.
    #[test]
    fn loopback_is_never_denied() {
        for cidr in DENIED_DESTINATIONS {
            assert!(
                !cidr.starts_with("127."),
                "{cidr} denies loopback, which is where docker's embedded DNS lives"
            );
        }
        for rule in policy().rules() {
            assert!(
                !rule.args.iter().any(|arg| arg.starts_with("127.")),
                "no rule may name a loopback address: {:?}",
                rule.args
            );
        }
        // Positive control: the list is non-empty and does deny something, so the assertions above
        // are running against real content rather than an empty iteration.
        assert!(!DENIED_DESTINATIONS.is_empty());
    }

    /// v6 rules must be installed by `ip6tables`, v4 by `iptables`. Rendering both into one plan and
    /// running them through a single binary would silently drop a whole family: `iptables` rejects a
    /// v6 address rather than filtering it.
    #[test]
    fn each_family_is_installed_by_its_own_binary() {
        let plan = policy().install_plan();
        assert!(
            plan.iter().any(|(bin, _)| *bin == "ip6tables"),
            "no v6 rules in the plan — the family would be left unfiltered"
        );
        for (binary, argv) in &plan {
            let v6_arg = argv.iter().any(|arg| arg.contains("::"));
            if v6_arg {
                assert_eq!(*binary, "ip6tables", "v6 rule handed to iptables: {argv:?}");
            } else {
                assert_eq!(*binary, "iptables", "v4 rule handed to ip6tables: {argv:?}");
            }
        }
    }

    #[test]
    fn port_range_parses_and_refuses_the_sentinel_and_the_inverted() {
        assert_eq!(
            PortRange::parse("49200-49299").unwrap(),
            PortRange::new(49200, 49299).unwrap()
        );
        assert_eq!(PortRange::parse("49200").unwrap().capacity(), 1);
        assert_eq!(
            PortRange::parse(" 49200 - 49299 ").unwrap().to_match(),
            "49200:49299"
        );
        assert_eq!(PortRange::parse("0-10"), Err(PortRangeError::ZeroPort));
        assert_eq!(
            PortRange::parse("500-100"),
            Err(PortRangeError::Inverted {
                start: 500,
                end: 100
            })
        );
        assert!(matches!(
            PortRange::parse("not-a-range"),
            Err(PortRangeError::Unparsable(_))
        ));
        assert_eq!(PortRange::new(49200, 49299).unwrap().capacity(), 100);
    }

    /// `iptables -S OUTPUT` as a live namespace actually printed it, captured from a real container
    /// after this exact policy was applied through the real sidecar.
    ///
    /// Kept verbatim, because its whole value is being un-idealised: every difference from what we
    /// sent — `-m tcp`, the expanded `--tcp-flags`, the `/32`, the reordered `-d`, the quoted
    /// prefixes — is a way a string comparison would have failed a correctly contained job.
    const MEASURED_V4: &str = "\
-A OUTPUT -p tcp -m tcp --tcp-flags FIN,SYN,RST,ACK SYN -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-conn:\"
-A OUTPUT -p udp -m udp --dport 53 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-dns:\"
-A OUTPUT -d 169.254.169.254/32 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny-metadata:\"
-A OUTPUT -d 169.254.169.254/32 -j DROP
-A OUTPUT -d 172.17.0.1/32 -p tcp -m tcp --dport 49200:49299 -j ACCEPT
-A OUTPUT -d 10.0.0.0/8 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"
-A OUTPUT -d 10.0.0.0/8 -j DROP
-A OUTPUT -d 172.16.0.0/12 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"
-A OUTPUT -d 172.16.0.0/12 -j DROP
-A OUTPUT -d 192.168.0.0/16 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"
-A OUTPUT -d 192.168.0.0/16 -j DROP
-A OUTPUT -d 169.254.0.0/16 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"
-A OUTPUT -d 169.254.0.0/16 -j DROP
-A OUTPUT -d 100.64.0.0/10 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"
-A OUTPUT -d 100.64.0.0/10 -j DROP
-A OUTPUT -d 198.18.0.0/15 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"
-A OUTPUT -d 198.18.0.0/15 -j DROP
-A OUTPUT -d 224.0.0.0/4 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"
-A OUTPUT -d 224.0.0.0/4 -j DROP
-A OUTPUT -d 240.0.0.0/4 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"
-A OUTPUT -d 240.0.0.0/4 -j DROP";

    /// The v6 readback, measured in the same run. Textually identical to what was sent — these rules
    /// carry no match module and no bare address, so there is nothing for iptables to rewrite.
    const MEASURED_V6: &str = "\
-A OUTPUT -d fc00::/7 -j DROP
-A OUTPUT -d fe80::/10 -j DROP
-A OUTPUT -d ff00::/8 -j DROP";

    /// The policy that produced [`MEASURED_V4`], so the fixture and the expectation cannot drift.
    fn measured_policy() -> NetPolicy {
        NetPolicy {
            gateway: "172.17.0.1".to_owned(),
            proxy_ports: Some(PortRange::new(49200, 49299).unwrap()),
            log_connections: true,
        }
    }

    /// The positive control for the whole readback: real iptables output, for a namespace that really
    /// was contained, must verify.
    #[test]
    fn the_measured_readback_of_a_contained_namespace_verifies() {
        let policy = measured_policy();
        assert_eq!(policy.verify_readback(Family::V4, MEASURED_V4), Ok(()));
        assert_eq!(policy.verify_readback(Family::V6, MEASURED_V6), Ok(()));
    }

    /// Byte-comparing the same output against what we sent fails on 12 of 21 lines. This is the test
    /// that documents *why* `verify_readback` is not a string comparison — if a future change makes
    /// iptables echo argv verbatim, this test fails and the simpler design becomes available.
    #[test]
    fn an_exact_comparison_would_have_refused_this_contained_namespace() {
        let sent: Vec<String> = measured_policy()
            .rules()
            .iter()
            .filter(|rule| rule.family == Family::V4)
            .map(|rule| format!("-A {} {}", OUTPUT_CHAIN, rule.args.join(" ")))
            .collect();
        let read: Vec<&str> = MEASURED_V4.lines().collect();
        assert_eq!(sent.len(), read.len(), "same rule count, different spelling");
        let differing = sent.iter().zip(&read).filter(|(a, b)| a.as_str() != **b).count();
        assert_eq!(
            differing, 12,
            "iptables' rewriting is what makes an exact comparison unusable; if this number moved, \
             re-measure before trusting either design"
        );
    }

    /// `-S` prints the chain's default policy too, and it is not a rule.
    #[test]
    fn the_chain_policy_line_is_not_counted_as_a_rule() {
        let with_policy_line = format!("-P OUTPUT ACCEPT\n{MEASURED_V4}");
        assert_eq!(
            ReadbackRule::parse_all(&with_policy_line).len(),
            ReadbackRule::parse_all(MEASURED_V4).len(),
            "counting `-P` as a rule would make a namespace missing one rule look complete"
        );
        assert_eq!(measured_policy().verify_readback(Family::V4, &with_policy_line), Ok(()));
    }

    /// Every refusal branch, each derived from the measured fixture by breaking exactly one thing —
    /// so a branch that cannot fire is visible as a test that cannot fail.
    #[test]
    fn each_way_containment_can_be_absent_is_refused() {
        let policy = measured_policy();

        // A namespace with nothing in it at all — the shape a silently ignored `--cap-add` leaves.
        let empty = policy.verify_readback(Family::V4, "-P OUTPUT ACCEPT\n").expect_err("empty");
        assert!(empty.contains("reports 0 rules"), "{empty}");

        // One denied range's DROP removed, count made up by a duplicate so the count check cannot be
        // the thing that catches it.
        let without_lan: Vec<&str> = MEASURED_V4
            .lines()
            .filter(|line| *line != "-A OUTPUT -d 10.0.0.0/8 -j DROP")
            .collect();
        let padded = format!("{}\n{}", without_lan.join("\n"), "-A OUTPUT -d 224.0.0.0/4 -j DROP");
        let missing_drop = policy.verify_readback(Family::V4, &padded).expect_err("missing DROP");
        assert!(missing_drop.contains("10.0.0.0/8"), "{missing_drop}");

        // The metadata DROP removed, likewise padded so only the metadata check can catch it.
        let without_metadata: Vec<&str> = MEASURED_V4
            .lines()
            .filter(|line| *line != "-A OUTPUT -d 169.254.169.254/32 -j DROP")
            .collect();
        let padded = format!("{}\n{}", without_metadata.join("\n"), "-A OUTPUT -d 224.0.0.0/4 -j DROP");
        let no_metadata = policy.verify_readback(Family::V4, &padded).expect_err("metadata");
        assert!(no_metadata.contains(METADATA_ENDPOINT), "{no_metadata}");

        // An injected second ACCEPT — an egress hole that keeps every other property intact. It
        // displaces a LOG rule, not a DROP: swapping out a DROP would also break a denied range, and
        // the earlier check would then be the one that fired, leaving this branch never exercised.
        let injected = MEASURED_V4.replace(
            "-A OUTPUT -d 240.0.0.0/4 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"",
            "-A OUTPUT -j ACCEPT",
        );
        assert_eq!(
            ReadbackRule::parse_all(&injected).len(),
            ReadbackRule::parse_all(MEASURED_V4).len(),
            "the injection must not change the rule count, or the count check fires instead"
        );
        let extra_accept = policy.verify_readback(Family::V4, &injected).expect_err("2 ACCEPTs");
        assert!(extra_accept.contains("ACCEPT"), "{extra_accept}");

        // The pinhole moved to another address: the job loses its model, or something else gains one.
        let moved = MEASURED_V4.replace("-d 172.17.0.1/32 -p tcp", "-d 172.17.0.9/32 -p tcp");
        let wrong_host = policy.verify_readback(Family::V4, &moved).expect_err("wrong host");
        assert!(wrong_host.contains("172.17.0.1/32"), "{wrong_host}");

        // The pinhole widened to every port while still being exactly one ACCEPT rule.
        let widened = MEASURED_V4.replace("--dport 49200:49299", "--dport 1:65535");
        let wide = policy.verify_readback(Family::V4, &widened).expect_err("widened");
        assert!(wide.contains("1:65535"), "{wide}");

        // The ACCEPT hoisted above the metadata DROP, which reopens the metadata endpoint.
        let hoisted = format!(
            "-A OUTPUT -d 172.17.0.1/32 -p tcp -m tcp --dport 49200:49299 -j ACCEPT\n{}",
            MEASURED_V4.replace(
                "-A OUTPUT -d 172.17.0.1/32 -p tcp -m tcp --dport 49200:49299 -j ACCEPT\n",
                "",
            )
        );
        let above = policy.verify_readback(Family::V4, &hoisted).expect_err("hoisted");
        assert!(above.contains("above the metadata DROP"), "{above}");

        // A v6 range missing.
        let v6_short = "-A OUTPUT -d fc00::/7 -j DROP\n-A OUTPUT -d fe80::/10 -j DROP\n-A OUTPUT -d fc00::/7 -j DROP";
        let v6_missing = policy.verify_readback(Family::V6, v6_short).expect_err("v6");
        assert!(v6_missing.contains("ff00::/8"), "{v6_missing}");
    }

    /// A seat with no contained credential renders no pinhole, so any ACCEPT in its namespace is one
    /// nobody asked for.
    #[test]
    fn a_policy_with_no_pinhole_refuses_any_accept() {
        let policy = NetPolicy {
            gateway: "172.17.0.1".to_owned(),
            proxy_ports: None,
            log_connections: true,
        };
        // Its own readback is the measured one minus the pinhole.
        let without_pinhole = MEASURED_V4.replace(
            "-A OUTPUT -d 172.17.0.1/32 -p tcp -m tcp --dport 49200:49299 -j ACCEPT\n",
            "",
        );
        assert_eq!(policy.verify_readback(Family::V4, &without_pinhole), Ok(()));

        // Now one ACCEPT appears where a LOG rule was, so the count still matches and every denied
        // range still drops — the ACCEPT is the only thing wrong, which is what makes this a test of
        // the ACCEPT check rather than of the count check.
        let smuggled = without_pinhole.replace(
            "-A OUTPUT -d 240.0.0.0/4 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"",
            "-A OUTPUT -d 172.17.0.1/32 -p tcp -m tcp --dport 49200:49299 -j ACCEPT",
        );
        assert_eq!(
            ReadbackRule::parse_all(&smuggled).len(),
            policy.rule_count(Family::V4),
            "the smuggled ACCEPT must keep the count correct, or the count check fires instead"
        );
        let refused = policy.verify_readback(Family::V4, &smuggled).expect_err("accept");
        assert!(refused.contains("ACCEPT"), "{refused}");
    }

    /// iptables collapses a single-port range to a bare port, so the verifier must accept both.
    #[test]
    fn a_single_port_pinhole_is_accepted_in_either_spelling() {
        let policy = NetPolicy {
            gateway: "172.17.0.1".to_owned(),
            proxy_ports: Some(PortRange::new(49200, 49200).unwrap()),
            log_connections: false,
        };
        let bare = policy
            .rules()
            .iter()
            .filter(|rule| rule.family == Family::V4)
            .map(|rule| {
                let line = format!("-A {} {}", OUTPUT_CHAIN, rule.args.join(" "));
                line.replace("-d 172.17.0.1 ", "-d 172.17.0.1/32 ")
                    .replace("--dport 49200:49200", "--dport 49200")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(policy.verify_readback(Family::V4, &bare), Ok(()));
    }

    /// **No rendered argument may contain whitespace**, because the transport cannot carry one.
    ///
    /// The plan crosses into the sidecar as one whitespace-delimited argv per line and the applier
    /// word-splits it, so a single space inside an argument silently becomes an argument boundary.
    ///
    /// This is a regression test for a measured, total failure: `--log-prefix "sbx-net conn: "`
    /// reached iptables as `--log-prefix sbx-net` plus a stray `conn:`, which refused rule 1 of 24
    /// and left the namespace with **no rules at all** — while every other test in this file passed,
    /// because they assert what is *rendered* and never execute it.
    ///
    /// Every variant is enumerated deliberately. The bug existed only in the logging variant, so a
    /// test that checked one policy shape would have reproduced exactly the blind spot that shipped
    /// it.
    #[test]
    fn no_rendered_argument_contains_whitespace() {
        for log_connections in [true, false] {
            for proxy_ports in [Some(PortRange::new(49200, 49299).unwrap()), None] {
                let policy = NetPolicy {
                    gateway: "172.17.0.1".to_owned(),
                    proxy_ports,
                    log_connections,
                };
                for rule in policy.rules() {
                    for arg in &rule.args {
                        assert!(
                            !arg.chars().any(char::is_whitespace),
                            "argument {arg:?} contains whitespace, so the sidecar will split it into \
                             two arguments and iptables will refuse the rule — leaving the namespace \
                             uncontained (log_connections={log_connections})"
                        );
                    }
                }
            }
        }
    }

    /// A positive control for the test above: it must actually be able to see whitespace. A guard
    /// that inspects the wrong field passes on every input, including a broken one.
    #[test]
    fn the_whitespace_guard_can_detect_a_bad_prefix() {
        let bad = Rule::new(
            Family::V4,
            vec!["-j", "LOG", "--log-prefix", "sbx-net conn: "],
            "the exact rule that failed in a live namespace",
        );
        assert!(
            bad.args.iter().any(|arg| arg.chars().any(char::is_whitespace)),
            "the predicate used by no_rendered_argument_contains_whitespace cannot see the very \
             argument that broke containment"
        );
    }
}
