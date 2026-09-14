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

/// Which family an address literal belongs to. A colon is the only thing that distinguishes them
/// here, and it is sufficient: these are addresses an operator configured or the host printed, not
/// hostnames — a resolver named rather than addressed is refused before it reaches a policy
/// ([`crate::sandbox_dns::from_config`]).
fn resolver_family(address: &str) -> Family {
    if address.contains(':') {
        Family::V6
    } else {
        Family::V4
    }
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

/// One predicate of a rule as a live namespace printed it: the flag, its values, and whether
/// iptables printed `!` in front of it.
///
/// **The inversion is the point.** `-d 10.0.0.2/32` and `! -d 10.0.0.2/32` are opposite rules that
/// differ by one token; a reader that keeps the address and drops the `!` reads the second as the
/// first and reports a namespace contained when it is wide open — UDP port 53 to every destination
/// except the resolver, ahead of the DROPs that would have stopped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Predicate {
    pub negated: bool,
    pub key: String,
    pub values: Vec<String>,
}

/// One appended rule as a live namespace reports it: its chain and every predicate it carries, in
/// printed order, with inversions retained.
///
/// Nothing is discarded at parse time. What a rule *means* depends on predicates a projection would
/// throw away — an inverted match, a source address, an inbound interface, a connection-state
/// match — so they are kept, and the judgement about which of them are permissible is made where
/// the rule's role is known ([`ReadbackRule::as_exception`]). iptables' cosmetic rewriting is
/// normalised there too, never dropped here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadbackRule {
    /// The chain the rule was appended to, from the `-A <chain>` that opens the line.
    pub chain: String,
    /// Every predicate, in printed order.
    pub predicates: Vec<Predicate>,
}

/// An exception rule reduced to the three things that decide what it lets through, and only after
/// its shape has been proved to be one this policy renders.
///
/// Used as a multiset key: the renderer can legally emit two identical ACCEPTs (a resolver that is
/// also the gateway, with a proxy port range of `53-53`), and role identity is carried alongside
/// rather than inferred from the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exception {
    pub destination: String,
    pub protocol: String,
    pub dport: String,
}

impl fmt::Display for Exception {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "-d {} -p {} --dport {} -j ACCEPT",
            self.destination, self.protocol, self.dport
        )
    }
}

/// A single-port range as iptables may print it: `53:53` and `53` are the same rule.
///
/// Normalising the printed form is safe; normalising a *wider* range would not be, so only the
/// start==end case collapses.
fn normalize_dport(printed: &str) -> String {
    match printed.split_once(':') {
        Some((start, end)) if start == end => start.to_owned(),
        _ => printed.to_owned(),
    }
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
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                fields.next()?; // the `-A`
                let chain = fields.next()?.to_owned();
                let mut predicates: Vec<Predicate> = Vec::new();
                let mut negated = false;
                for field in fields {
                    if field == "!" {
                        // iptables prints the inversion as its own token, before the flag.
                        negated = true;
                        continue;
                    }
                    if field.starts_with('-') {
                        predicates.push(Predicate {
                            negated,
                            key: field.to_owned(),
                            values: Vec::new(),
                        });
                        negated = false;
                    } else if let Some(current) = predicates.last_mut() {
                        // A flag can take more than one value: `--tcp-flags FIN,SYN,RST,ACK SYN`.
                        current.values.push(field.to_owned());
                    }
                }
                Some(Self { chain, predicates })
            })
            .collect()
    }

    /// The value of `key`, when the rule carries that flag exactly once, un-inverted, with exactly
    /// one value.
    ///
    /// `None` for a repeated, inverted or multi-valued flag rather than the first value found: each
    /// of those is a different rule from the one this policy renders, and answering with the value
    /// anyway is how an inverted match passes for a positive one.
    pub fn value(&self, key: &str) -> Option<&str> {
        let mut matching = self.predicates.iter().filter(|predicate| predicate.key == key);
        let first = matching.next()?;
        if matching.next().is_some() || first.negated || first.values.len() != 1 {
            return None;
        }
        Some(first.values[0].as_str())
    }

    /// The `-j` target, under the rules of [`ReadbackRule::value`].
    pub fn target(&self) -> Option<&str> {
        self.value("-j")
    }

    /// The `-d` destination, under the rules of [`ReadbackRule::value`].
    pub fn destination(&self) -> Option<&str> {
        self.value("-d")
    }

    /// This rule as one of the exceptions the policy renders, or why it is not one.
    ///
    /// **Whitelist, not projection.** The renderer emits exceptions in exactly one shape —
    /// `-p <transport> -d <host> --dport <ports> -j ACCEPT` in the OUTPUT chain — so anything else
    /// in an ACCEPT is a rule this policy never asked for, and the honest answer is to refuse it
    /// rather than to read the parts that look familiar. Each rejected shape below is a rule that a
    /// four-field projection would have accepted while it let through traffic nobody authorised:
    ///
    /// * an inverted predicate (`! -d`, `! --dport`) — the complement of the rule we rendered;
    /// * an extra predicate (`-s`, `-i`, `-o`, a state match) — either a narrowing that makes the
    ///   exception inert, so the job cannot resolve, or a widening nobody reviewed;
    /// * a repeated predicate — iptables prints one `-d` per rule, so two means this is not the
    ///   output of the plan we sent;
    /// * another chain — a rule in the wrong chain does not filter this job's egress at all.
    ///
    /// Only understood cosmetics are normalised: the redundant `-m udp`/`-m tcp` match module
    /// iptables adds for its own `--dport`, a single-port range printed bare, and a bare host
    /// address printed without its prefix length.
    pub fn as_exception(&self, family: Family) -> Result<Exception, String> {
        if self.chain != OUTPUT_CHAIN {
            return Err(format!(
                "an ACCEPT in chain {} rather than {OUTPUT_CHAIN} — it does not filter this job's \
                 egress",
                self.chain
            ));
        }
        if let Some(negated) = self.predicates.iter().find(|predicate| predicate.negated) {
            return Err(format!(
                "an ACCEPT whose {} match is INVERTED — it permits the complement of the rule this \
                 policy renders",
                negated.key
            ));
        }

        let protocol = self
            .value("-p")
            .ok_or_else(|| "an ACCEPT that names no single transport".to_owned())?
            .to_owned();
        if protocol != "udp" && protocol != "tcp" {
            return Err(format!("an ACCEPT for transport {protocol:?}, which this policy never opens"));
        }
        let destination = self
            .value("-d")
            .ok_or_else(|| {
                "an ACCEPT that names no single destination — unaddressed, it permits every host"
                    .to_owned()
            })?
            .to_owned();
        let dport = self
            .value("--dport")
            .ok_or_else(|| "an ACCEPT that names no single destination port".to_owned())?
            .to_owned();

        for predicate in &self.predicates {
            let permitted = match predicate.key.as_str() {
                "-p" | "-d" | "--dport" | "-j" => true,
                // The match module iptables inserts for its own port match, and nothing else: `-m
                // conntrack`, `-m state`, `-m owner` all change what the rule matches.
                "-m" => predicate.values == [protocol.clone()],
                _ => false,
            };
            if !permitted {
                return Err(format!(
                    "an ACCEPT carrying `{} {}`, a predicate this policy never renders on an \
                     exception — it either narrows the exception until the job cannot use it or \
                     widens it beyond what was reviewed",
                    predicate.key,
                    predicate.values.join(" ")
                ));
            }
            let allowed_repeats = usize::from(predicate.key == "-m");
            let seen = self
                .predicates
                .iter()
                .filter(|other| other.key == predicate.key)
                .count();
            if seen > 1 + allowed_repeats {
                return Err(format!(
                    "an ACCEPT carrying {seen} `{}` predicates — iptables prints one per rule, so \
                     this is not the plan this policy sent",
                    predicate.key
                ));
            }
        }
        if self.target() != Some("ACCEPT") {
            return Err("a rule read as an exception does not jump to ACCEPT".to_owned());
        }

        Ok(Exception {
            destination: with_prefix_len(&destination, family),
            protocol,
            dport: normalize_dport(&dport),
        })
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
    /// The upstream resolvers the job's `/etc/resolv.conf` names, each opened on port 53 and nothing
    /// else. Empty ⇒ no DNS pinhole at all, which is correct only for a seat whose jobs need no name
    /// resolution.
    ///
    /// **Why this field exists at all.** Docker's embedded resolver at `127.0.0.11` is a daemon-side
    /// socket reached through NAT rules inside the container's namespace. Under gVisor the sandbox
    /// terminates loopback in its own network stack, so those packets never arrive and every lookup
    /// fails `EAI_AGAIN` — measured, with a runc control that succeeds on the identical image and
    /// network, and with a bare UDP datagram to `127.0.0.11:53` timing out. `docker run --dns` does
    /// not help: on a user-defined network the daemon writes `nameserver 127.0.0.11` regardless. So a
    /// gVisor job is handed real upstream resolvers, and those resolvers need to be reachable through
    /// a policy that otherwise denies the private ranges wholesale.
    ///
    /// Each address is opened as a single host (`/32`, or `/128` for v6) on port 53 only. Never a
    /// subnet: an operator whose resolver is a LAN address gets that one address, not their LAN.
    pub dns_resolvers: Vec<String>,
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

        // The DNS pinholes, also BEFORE the range drops and for the same reason: a resolver on a
        // private address is inside a denied range, and a job that cannot resolve cannot deliver.
        // One rule per transport, because a truncated UDP answer is retried over TCP and a seat that
        // opened only UDP fails on exactly the large answers (DNSSEC, long CNAME chains) that are
        // hardest to attribute later.
        //
        // Each resolver is pinned to a single host address and to port 53. Never a subnet and never
        // a wider port range: an operator whose resolver sits on their LAN gets that one address
        // opened for lookups, not the LAN the rest of this policy exists to deny.
        for resolver in &self.dns_resolvers {
            let family = resolver_family(resolver);
            let destination = with_prefix_len(resolver, family);
            for protocol in ["udp", "tcp"] {
                rules.push(Rule::new(
                    family,
                    vec![
                        "-p",
                        protocol,
                        "-d",
                        destination.as_str(),
                        "--dport",
                        "53",
                        "-j",
                        "ACCEPT",
                    ],
                    "the sandbox's own resolver — docker's embedded one is unreachable under gVisor",
                ));
            }
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
    /// * **the metadata DROP precedes every ACCEPT** — order is load-bearing, and an ACCEPT above
    ///   the metadata drop reopens the one destination the policy exists to close;
    /// * **each resolver has exactly one udp and one tcp ACCEPT, on port 53, at its own host
    ///   address, above the first range DROP** — a count cannot tell a udp+tcp pair from two udp
    ///   rules, cannot tell a `/32` from a `/24` that covers it, and cannot tell a live exception
    ///   from one appended below the drop that shadows it. All three pass a count and all three are
    ///   wrong, one of them silently open.
    ///
    /// Both families are judged the same way here. v6 carries no proxy pinhole, so its only
    /// permitted ACCEPTs are resolver pairs, and an extra one is a hole in the family least likely
    /// to be looked at.
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
                rule.chain == OUTPUT_CHAIN
                    && rule.target() == Some("DROP")
                    && rule.destination() == Some(*destination)
            });
            if !dropped {
                return Err(format!(
                    "{destination} has no DROP in the live namespace — that range is reachable from \
                     the job"
                ));
            }
        }

        // The first range DROP of this family. Every exception this policy renders — the proxy
        // pinhole and each resolver pair — is rendered above it, and iptables takes the first match,
        // so an exception read back BELOW it is inert: present, countable, and doing nothing.
        // Compared against this index rather than an absolute position, because how many log rules
        // sit ahead of it is a policy choice and not a safety property.
        let first_range_drop = found.iter().position(|rule| {
            rule.target() == Some("DROP")
                && rule.destination().is_some_and(|found| denied.contains(&found))
        });

        // The metadata DROP is v4-only because the endpoint is a v4 address, and every ACCEPT in
        // that family must sit below it.
        let metadata_dropped_at = if family == Family::V4 {
            let at = found.iter().position(|rule| {
                rule.chain == OUTPUT_CHAIN
                    && rule.target() == Some("DROP")
                    && rule.destination() == Some(METADATA_ENDPOINT)
            });
            let Some(at) = at else {
                return Err(format!(
                    "{METADATA_ENDPOINT} has no DROP in the live namespace — instance credentials are \
                     reachable from the job"
                ));
            };
            Some(at)
        } else {
            None
        };

        // Every ACCEPT the namespace carries, against every exception this policy rendered, matched
        // as a MULTISET and consumed one for one.
        //
        // A multiset rather than a search by address, because the renderer can legally emit two
        // byte-identical ACCEPTs: a seat whose resolver IS its gateway, configured with a proxy port
        // range of `53-53`, renders the DNS TCP exception and the proxy pinhole as the same rule.
        // Searching for "the TCP port-53 rule for that address" finds two and refuses a namespace
        // that is exactly right. Roles are carried alongside each expectation instead of inferred
        // from the rule, so both are still judged on their own terms — including their positions.
        let mut expected: Vec<(String, Exception, bool)> = Vec::new();
        for resolver in self.dns_resolvers.iter().filter(|r| resolver_family(r) == family) {
            let destination = with_prefix_len(resolver, family);
            for protocol in ["udp", "tcp"] {
                expected.push((
                    format!("the {protocol} port-53 exception for the resolver {destination}"),
                    Exception {
                        destination: destination.clone(),
                        protocol: protocol.to_owned(),
                        dport: "53".to_owned(),
                    },
                    false,
                ));
            }
        }
        if let (Family::V4, Some(ports)) = (family, self.proxy_ports) {
            expected.push((
                "the credential proxy pinhole".to_owned(),
                Exception {
                    destination: with_prefix_len(&self.gateway, Family::V4),
                    protocol: "tcp".to_owned(),
                    dport: normalize_dport(&ports.to_match()),
                },
                false,
            ));
        }

        let mut unrendered: Vec<String> = Vec::new();
        let mut matched: Vec<(String, usize)> = Vec::new();
        for (at, rule) in found.iter().enumerate() {
            // Judged by the jump, so a rule whose `-j` is repeated or inverted still arrives here
            // rather than being skipped as "not an ACCEPT".
            let jumps_to_accept = rule
                .predicates
                .iter()
                .any(|predicate| predicate.key == "-j" && predicate.values == ["ACCEPT"]);
            if !jumps_to_accept {
                continue;
            }
            match rule.as_exception(family) {
                Ok(exception) => {
                    match expected
                        .iter_mut()
                        .find(|(_, wanted, taken)| !*taken && *wanted == exception)
                    {
                        Some((role, _, taken)) => {
                            *taken = true;
                            matched.push((role.clone(), at));
                        }
                        None => unrendered.push(format!("`{exception}` at index {at}")),
                    }
                }
                Err(why) => unrendered.push(format!("{why} (at index {at})")),
            }
        }

        let missing: Vec<String> = expected
            .iter()
            .filter(|(_, _, taken)| !*taken)
            .map(|(role, exception, _)| format!("{role} (`{exception}`)"))
            .collect();
        if !unrendered.is_empty() || !missing.is_empty() {
            // Both halves in one error deliberately: "an ACCEPT nobody rendered" and "an exception
            // that is gone" are usually the same edit seen from two sides, and reporting only one
            // of them sends the reader looking for the wrong fault.
            let mut detail = String::new();
            if !unrendered.is_empty() {
                detail.push_str(&format!(
                    "it carries {} this policy never rendered: {}",
                    if unrendered.len() == 1 { "an ACCEPT" } else { "ACCEPTs" },
                    unrendered.join("; ")
                ));
            }
            if !unrendered.is_empty() && !missing.is_empty() {
                detail.push_str(", and ");
            }
            if !missing.is_empty() {
                detail.push_str(&format!("it is missing {}", missing.join("; ")));
            }
            return Err(format!(
                "the exceptions in the live {} OUTPUT chain are not the ones this policy rendered: \
                 {detail} — an ACCEPT nobody rendered is an egress hole, and a missing one is reach \
                 the job was promised and does not have",
                family.binary()
            ));
        }

        // Position, per matched exception. Every exception is rendered above the range DROPs and
        // below the metadata DROP, and iptables takes the first match: below the DROP that covers
        // its address an exception is inert, and above the metadata DROP it reopens the one endpoint
        // this policy exists to close.
        for (role, at) in matched {
            if let Some(first_range_drop) = first_range_drop {
                if at > first_range_drop {
                    return Err(format!(
                        "{role} is at index {at}, below the first range DROP at {first_range_drop} — \
                         iptables takes the first match, so that rule is inert and the reach it \
                         grants does not exist"
                    ));
                }
            }
            if let Some(metadata_dropped_at) = metadata_dropped_at {
                if at < metadata_dropped_at {
                    return Err(format!(
                        "{role} is at index {at}, above the metadata DROP at {metadata_dropped_at} — \
                         an ACCEPT above that drop reopens {METADATA_ENDPOINT}"
                    ));
                }
            }
        }

        Ok(())
    }

    /// How many ACCEPT rules this policy's resolvers install for one family: two per resolver, one
    /// per transport.
    ///
    /// The arithmetic the readback's ACCEPT total is built on, so it is public and directly tested:
    /// a wrong count here would either refuse a correctly contained namespace or make room in the
    /// total for an ACCEPT nobody rendered.
    pub fn dns_pinhole_count(&self, family: Family) -> usize {
        self.dns_resolvers.iter().filter(|resolver| resolver_family(resolver) == family).count() * 2
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
            dns_resolvers: Vec::new(),
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
            dns_resolvers: Vec::new(),
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
            dns_resolvers: Vec::new(),
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
            dns_resolvers: Vec::new(),
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
                    // Both families, because a v6 resolver renders a different destination and the
                    // whitespace invariant covers every rendered argument or it covers nothing.
                    dns_resolvers: vec!["10.0.0.2".to_owned(), "2001:4860:4860::8888".to_owned()],
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

    // ---------------------------------------------------------------------------------------------
    // The resolver exceptions.
    //
    // These assert the *content* of the rendered rules — protocol, host address, prefix length,
    // port, target, and position — not how many of them there are. A count check passes on a policy
    // that opened two UDP rules and no TCP one, or opened port 53 to the wrong address; both are
    // failures a job would report as "DNS still broken" or as a hole nobody asked for.
    // ---------------------------------------------------------------------------------------------

    /// The exact argv for one resolver, in order, with nothing inferred.
    fn expected_pinhole(protocol: &str, destination: &str) -> Vec<String> {
        ["-p", protocol, "-d", destination, "--dport", "53", "-j", "ACCEPT"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn policy_with_resolvers(resolvers: &[&str]) -> NetPolicy {
        NetPolicy {
            gateway: "172.17.0.1".to_owned(),
            proxy_ports: Some(PortRange::new(49200, 49299).unwrap()),
            log_connections: true,
            dns_resolvers: resolvers.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    /// A v4 resolver opens exactly one UDP and one TCP rule, to that one address, on port 53 only.
    #[test]
    fn a_v4_resolver_renders_one_udp_and_one_tcp_rule_pinned_to_that_address_and_port() {
        let policy = policy_with_resolvers(&["10.0.0.2"]);
        let rendered: Vec<&Rule> = policy
            .rules()
            .iter()
            .filter(|rule| rule.args.contains(&"53".to_owned()) && rule.target() == Some("ACCEPT"))
            .cloned()
            .collect::<Vec<_>>()
            .leak()
            .iter()
            .collect();

        assert_eq!(rendered.len(), 2, "one transport pair, no more and no fewer");
        assert_eq!(rendered[0].args, expected_pinhole("udp", "10.0.0.2/32"));
        assert_eq!(rendered[1].args, expected_pinhole("tcp", "10.0.0.2/32"));
        assert!(rendered.iter().all(|rule| rule.family == Family::V4));
        assert!(
            rendered.iter().all(|rule| rule.family.binary() == "iptables"),
            "a v4 exception installed by ip6tables is not installed at all"
        );
    }

    /// A v6 resolver is a different address family: `/128`, and the other binary. Rendering it as a
    /// v4 rule would make `iptables` refuse the line and leave the namespace short one rule — the
    /// exact shape that once left a namespace with no rules at all.
    #[test]
    fn a_v6_resolver_renders_128_rules_on_the_v6_binary() {
        let policy = policy_with_resolvers(&["2001:4860:4860::8888"]);
        let rendered: Vec<Rule> = policy
            .rules()
            .into_iter()
            .filter(|rule| rule.args.contains(&"53".to_owned()) && rule.target() == Some("ACCEPT"))
            .collect();

        assert_eq!(rendered.len(), 2);
        assert_eq!(rendered[0].args, expected_pinhole("udp", "2001:4860:4860::8888/128"));
        assert_eq!(rendered[1].args, expected_pinhole("tcp", "2001:4860:4860::8888/128"));
        assert!(rendered.iter().all(|rule| rule.family == Family::V6), "v6 address, v6 family");
        assert!(rendered.iter().all(|rule| rule.family.binary() == "ip6tables"));

        // And it adds nothing to the v4 chain: the v4 rule count is what it was without a resolver.
        assert_eq!(
            policy.rule_count(Family::V4),
            measured_policy().rule_count(Family::V4),
            "a v6 resolver changed the v4 chain"
        );
    }

    /// Position is the whole property: below the range DROPs the rule is dead (iptables takes the
    /// first match), above the metadata DROP it would reopen the metadata endpoint.
    #[test]
    fn the_resolver_rules_sit_below_the_metadata_drop_and_above_the_range_drops() {
        let policy = policy_with_resolvers(&["10.0.0.2"]);
        let v4: Vec<Rule> =
            policy.rules().into_iter().filter(|rule| rule.family == Family::V4).collect();
        let at = |predicate: &dyn Fn(&Rule) -> bool| {
            v4.iter().position(|rule| predicate(rule)).expect("the rule exists")
        };

        let metadata_drop = at(&|rule| {
            rule.destination() == Some(METADATA_ENDPOINT) && rule.target() == Some("DROP")
        });
        // Filtered to ACCEPT deliberately: the port-53 LOG rule sits at the top of the chain by
        // design, and matching it here would make this test pass on a policy whose exceptions were
        // never rendered at all.
        let first_dns = at(&|rule| {
            rule.args.contains(&"53".to_owned()) && rule.target() == Some("ACCEPT")
        });
        let last_dns = v4
            .iter()
            .rposition(|rule| rule.args.contains(&"53".to_owned()) && rule.target() == Some("ACCEPT"))
            .expect("a resolver rule");
        let first_range_drop = at(&|rule| {
            rule.target() == Some("DROP")
                && rule.destination().is_some_and(|d| DENIED_DESTINATIONS.contains(&d))
        });

        assert!(
            metadata_drop < first_dns,
            "a resolver exception above the metadata DROP reopens 169.254.169.254"
        );
        assert!(
            last_dns < first_range_drop,
            "a resolver exception below the range DROPs never matches: the job cannot resolve"
        );
    }

    /// The pinhole count is per family, because the two chains are installed by different binaries
    /// and verified separately. A total would make a v6-only seat look like it owed v4 rules.
    #[test]
    fn the_pinhole_count_is_per_family() {
        let policy = policy_with_resolvers(&["10.0.0.2", "1.1.1.1", "2001:4860:4860::8888"]);
        assert_eq!(policy.dns_pinhole_count(Family::V4), 4, "two v4 resolvers, two transports each");
        assert_eq!(policy.dns_pinhole_count(Family::V6), 2);
        assert_eq!(measured_policy().dns_pinhole_count(Family::V4), 0, "no resolver, no exception");
        assert_eq!(measured_policy().dns_pinhole_count(Family::V6), 0);
    }

    /// The v4 readback for a policy that carries one resolver.
    ///
    /// NOT a live capture, unlike [`MEASURED_V4`]: it is that fixture with the resolver rules
    /// inserted where the renderer puts them, spelled the way iptables rewrote every other rule in
    /// the measured run — `-d` hoisted ahead of `-p`, the `-m udp`/`-m tcp` match module made
    /// explicit, the bare address given its `/32`. Written this way so the verifier is exercised
    /// against iptables' spelling rather than against our own.
    fn readback_with_resolver() -> String {
        MEASURED_V4.replace(
            "-A OUTPUT -d 10.0.0.0/8 -m limit",
            "-A OUTPUT -d 10.0.0.2/32 -p udp -m udp --dport 53 -j ACCEPT\n\
             -A OUTPUT -d 10.0.0.2/32 -p tcp -m tcp --dport 53 -j ACCEPT\n\
             -A OUTPUT -d 10.0.0.0/8 -m limit",
        )
    }

    /// The positive control: a namespace carrying exactly the resolver rules this policy asked for
    /// verifies, and the same namespace verified against a policy with no resolver does not — so the
    /// check cannot be passing by ignoring them.
    #[test]
    fn a_namespace_with_the_configured_resolver_rules_verifies() {
        let policy = policy_with_resolvers(&["10.0.0.2"]);
        assert_eq!(policy.verify_readback(Family::V4, &readback_with_resolver()), Ok(()));

        // The same two rules, in a namespace whose policy configured no resolver — with the count
        // held at 21 by displacing two LOG rules, so the ACCEPT accounting is the only check that
        // can catch them. Two port-53 holes nobody asked for must be refused, not tolerated because
        // they happen to look like DNS.
        let smuggled = MEASURED_V4
            .replace(
                "-A OUTPUT -d 224.0.0.0/4 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"",
                "-A OUTPUT -d 10.0.0.2/32 -p udp -m udp --dport 53 -j ACCEPT",
            )
            .replace(
                "-A OUTPUT -d 240.0.0.0/4 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix \"sbx-net-deny:\"",
                "-A OUTPUT -d 10.0.0.2/32 -p tcp -m tcp --dport 53 -j ACCEPT",
            );
        assert_eq!(
            ReadbackRule::parse_all(&smuggled).len(),
            measured_policy().rule_count(Family::V4),
            "the smuggled pair must keep the count right, or the count check fires instead"
        );
        let unexpected = measured_policy()
            .verify_readback(Family::V4, &smuggled)
            .expect_err("two ACCEPTs nobody configured");
        assert!(unexpected.contains("ACCEPT"), "{unexpected}");
    }

    /// Every way a resolver exception can be wrong while the rule *count* is right. Each case keeps
    /// the count intact deliberately: a count check would pass on all four, and each is a job that
    /// silently cannot resolve or a hole that was never authorised.
    #[test]
    fn a_resolver_exception_that_is_present_but_wrong_is_refused() {
        let policy = policy_with_resolvers(&["10.0.0.2"]);
        let good = readback_with_resolver();
        assert_eq!(
            ReadbackRule::parse_all(&good).len(),
            policy.rule_count(Family::V4),
            "the fixture must have the right count, or every case below fires the count check"
        );

        // TCP missing for the configured resolver, its slot filled by a TCP exception to a
        // different address — so the rule count, the ACCEPT count and the UDP rule are all intact
        // and only the per-resolver transport proof can catch it. This is the shape where a job
        // resolves until an answer is truncated, then fails on exactly the large answers (DNSSEC,
        // long CNAME chains) that are hardest to attribute later.
        let udp_only = good.replace(
            "-A OUTPUT -d 10.0.0.2/32 -p tcp -m tcp --dport 53 -j ACCEPT",
            "-A OUTPUT -d 10.0.0.9/32 -p tcp -m tcp --dport 53 -j ACCEPT",
        );
        let no_tcp = policy.verify_readback(Family::V4, &udp_only).expect_err("tcp missing");
        assert!(no_tcp.contains("tcp"), "{no_tcp}");

        // Port 53 opened to a different address — the job cannot resolve, and something else gained
        // a hole.
        let wrong_address = good.replace("-d 10.0.0.2/32 -p", "-d 10.0.0.9/32 -p");
        let moved = policy.verify_readback(Family::V4, &wrong_address).expect_err("wrong address");
        assert!(moved.contains("10.0.0.2/32"), "{moved}");

        // The exception widened past port 53 to the resolver's whole host: an operator's LAN
        // resolver box becomes reachable on every port, from inside a sandbox.
        let widened = good.replace("--dport 53 -j ACCEPT", "--dport 1:65535 -j ACCEPT");
        let wide = policy.verify_readback(Family::V4, &widened).expect_err("widened");
        assert!(wide.contains("53"), "{wide}");

        // Both rules present and correct, but below the range DROP that covers 10.0.0.0/8 — iptables
        // takes the first match, so this namespace looks configured and resolves nothing.
        let sunk = MEASURED_V4.replace(
            "-A OUTPUT -d 172.16.0.0/12 -m limit",
            "-A OUTPUT -d 10.0.0.2/32 -p udp -m udp --dport 53 -j ACCEPT\n\
             -A OUTPUT -d 10.0.0.2/32 -p tcp -m tcp --dport 53 -j ACCEPT\n\
             -A OUTPUT -d 172.16.0.0/12 -m limit",
        );
        let below = policy.verify_readback(Family::V4, &sunk).expect_err("below the range DROP");
        assert!(below.contains("below the first range DROP"), "{below}");
    }

    /// This policy's own rules, spelled the way `iptables -S` printed every rule in the measured
    /// capture: `-d` hoisted ahead of `-p`, the redundant `-m <transport>` match module made
    /// explicit, a bare host address given its prefix length, and a single-port range collapsed to a
    /// bare port.
    ///
    /// Synthetic, and labelled so: it is the *normalisation* contract under test, derived from what
    /// [`MEASURED_V4`] showed the kernel does. A namespace whose readback needs any other rewriting
    /// accepted is not covered by this helper.
    fn normalized_readback(policy: &NetPolicy, family: Family) -> String {
        policy
            .rules()
            .iter()
            .filter(|rule| rule.family == family)
            .map(|rule| {
                let sent: Vec<&str> = rule.args.iter().map(String::as_str).collect();
                let exception = rule.target() == Some("ACCEPT");
                match (exception, arg_value(&sent, "-p"), arg_value(&sent, "--dport")) {
                    (true, Some(protocol), Some(dport)) => format!(
                        "-A {OUTPUT_CHAIN} -d {} -p {protocol} -m {protocol} --dport {} -j ACCEPT",
                        with_prefix_len(rule.destination().unwrap_or_default(), family),
                        normalize_dport(dport),
                    ),
                    _ => format!("-A {OUTPUT_CHAIN} {}", rule.args.join(" ")),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The positive control for every negative case below: the policy's own rules, after the
    /// kernel's cosmetic rewriting, verify for both families.
    ///
    /// Without this the negatives prove nothing — a verifier that refuses everything would pass all
    /// of them.
    #[test]
    fn the_normalized_spelling_of_this_policys_own_rules_verifies_in_both_families() {
        let policy = policy_with_resolvers(&["10.0.0.2", "2001:4860:4860::8888"]);
        assert_eq!(
            policy.verify_readback(Family::V4, &normalized_readback(&policy, Family::V4)),
            Ok(())
        );
        assert_eq!(
            policy.verify_readback(Family::V6, &normalized_readback(&policy, Family::V6)),
            Ok(())
        );
    }

    /// **An exception is judged by its whole shape, not by the fields a reader finds familiar.**
    ///
    /// Every case here keeps the rule count, the ACCEPT count and every position unchanged, and
    /// substitutes one semantically different rule for a rendered exception. A reader that projects
    /// a rule down to destination/target/port/protocol accepts all of them: the inverted matches
    /// permit the complement of what was rendered (UDP53 to every host EXCEPT the resolver — which
    /// includes the private ranges whose DROPs sit below — or every port except 53 on the resolver
    /// host), and the extra predicates either make the exception inert or widen it past review.
    ///
    /// Both families, because the v6 chain carries no proxy pinhole and is the one less likely to be
    /// looked at.
    #[test]
    fn a_semantically_different_exception_with_the_same_projection_is_refused() {
        let policy = policy_with_resolvers(&["10.0.0.2", "2001:4860:4860::8888"]);
        let cases: [(&str, &str); 8] = [
            // The destination inverted: port 53 open to everything that is NOT the resolver.
            ("inverted destination", "-A OUTPUT ! -d ADDR -p udp -m udp --dport 53 -j ACCEPT"),
            // The port inverted: every port on the resolver host EXCEPT the one it answers on.
            ("inverted port", "-A OUTPUT -d ADDR -p udp -m udp ! --dport 53 -j ACCEPT"),
            // A source match nobody rendered: the exception matches only packets from an address
            // the job does not have, so it is inert and the job cannot resolve.
            ("extra source", "-A OUTPUT -d ADDR -s SRC -p udp -m udp --dport 53 -j ACCEPT"),
            // A connection-state match: same projection, different rule.
            (
                "extra state match",
                "-A OUTPUT -d ADDR -p udp -m udp -m conntrack --ctstate NEW,ESTABLISHED --dport 53 -j ACCEPT",
            ),
            // An outbound interface nobody rendered: the exception only applies to packets leaving
            // by that device, so on any other path the job's DNS is dropped — and if the device is
            // one the policy never reasoned about, the reach it grants was never reviewed.
            ("extra out interface", "-A OUTPUT -d ADDR -o eth0 -p udp -m udp --dport 53 -j ACCEPT"),
            // Two destinations in one rule: not output this plan could have produced.
            ("duplicate destination", "-A OUTPUT -d ADDR -d 8.8.8.8/32 -p udp -m udp --dport 53 -j ACCEPT"),
            // Two ports in one rule. Kept separate from the duplicate destination because the
            // field a reader trusts most is the one it is easiest to read twice and report once.
            ("duplicate port", "-A OUTPUT -d ADDR -p udp -m udp --dport 53 --dport 5353 -j ACCEPT"),
            // The right rule in the wrong chain: it does not filter this job's egress at all.
            ("wrong chain", "-A FORWARD -d ADDR -p udp -m udp --dport 53 -j ACCEPT"),
        ];

        for (family, address, source) in [
            (Family::V4, "10.0.0.2/32", "10.9.9.9/32"),
            (Family::V6, "2001:4860:4860::8888/128", "2001:db8::9/128"),
        ] {
            let good = normalized_readback(&policy, family);
            let rendered = format!("-A {OUTPUT_CHAIN} -d {address} -p udp -m udp --dport 53 -j ACCEPT");
            assert!(good.contains(&rendered), "the fixture must contain the rule being replaced");

            for (name, substitute) in cases {
                let substitute = substitute.replace("ADDR", address).replace("SRC", source);
                let broken = good.replace(&rendered, &substitute);
                assert_eq!(
                    ReadbackRule::parse_all(&broken).len(),
                    policy.rule_count(family),
                    "{name}: the count must stay right, or the count check is what catches it"
                );
                let refused = match policy.verify_readback(family, &broken) {
                    Ok(()) => panic!(
                        "{name} ({}) was ACCEPTED by the readback verifier: {substitute}",
                        family.binary()
                    ),
                    Err(refused) => refused,
                };
                assert!(
                    refused.contains("udp port-53 exception") || refused.contains("never rendered"),
                    "{name}: refused for the wrong reason: {refused}"
                );
            }
        }
    }

    /// The transport, the prefix length and the port are each load-bearing on their own, and each is
    /// wrong in a way a count cannot see.
    ///
    /// Both families: the v6 chain is read back by a different binary and carries no proxy pinhole,
    /// so a v4-only proof of this leaves the half nobody looks at unproven. The widened prefix is
    /// per-family on purpose — a `/24` and a `/64` are the same mistake at very different scale.
    #[test]
    fn a_wrong_transport_prefix_or_port_in_an_exception_is_refused() {
        let policy = policy_with_resolvers(&["10.0.0.2", "2001:4860:4860::8888"]);

        for (family, host, widened) in [
            // A `/24` that covers the resolver: the job resolves, and it also reaches 254 other
            // hosts on that LAN before the range DROP below would have stopped it.
            (Family::V4, "10.0.0.2/32", "10.0.0.0/24"),
            // The v6 equivalent, and a whole /64 of them.
            (Family::V6, "2001:4860:4860::8888/128", "2001:4860:4860::/64"),
        ] {
            let good = normalized_readback(&policy, family);
            let rendered = format!("-A OUTPUT -d {host} -p udp -m udp --dport 53 -j ACCEPT");
            assert!(
                good.contains(&rendered),
                "{}: the fixture must contain the rule being replaced",
                family.binary()
            );

            let widened_prefix = good.replace(
                &rendered,
                &format!("-A OUTPUT -d {widened} -p udp -m udp --dport 53 -j ACCEPT"),
            );
            let prefix = policy
                .verify_readback(family, &widened_prefix)
                .expect_err("a range is not a host");
            assert!(prefix.contains(host), "{}: {prefix}", family.binary());

            // sctp on port 53: the projection sees a port-53 ACCEPT to the right host.
            let wrong_transport = good.replace(
                &rendered,
                &format!("-A OUTPUT -d {host} -p sctp --dport 53 -j ACCEPT"),
            );
            let transport = policy
                .verify_readback(family, &wrong_transport)
                .expect_err("sctp is not a transport this policy opens");
            assert!(
                transport.contains("never rendered"),
                "{}: {transport}",
                family.binary()
            );

            // Port 5353 — mDNS, not DNS, and not what was rendered.
            let wrong_port = good.replace(
                &rendered,
                &format!("-A OUTPUT -d {host} -p udp -m udp --dport 5353 -j ACCEPT"),
            );
            let port = policy
                .verify_readback(family, &wrong_port)
                .expect_err("5353 is not 53");
            assert!(port.contains("5353"), "{}: {port}", family.binary());
        }
    }

    /// An exception duplicated: the count is wrong, and if something else is missing to make room
    /// for it, the multiset match is what catches it.
    #[test]
    fn an_extra_copy_of_a_rendered_exception_is_refused() {
        let policy = policy_with_resolvers(&["10.0.0.2"]);
        let good = normalized_readback(&policy, Family::V4);
        let rendered = "-A OUTPUT -d 10.0.0.2/32 -p udp -m udp --dport 53 -j ACCEPT";

        // Duplicated in place of the TCP exception, so the total is untouched: one transport gains a
        // rule it does not need and the other loses the one it does.
        let doubled = good.replace(
            "-A OUTPUT -d 10.0.0.2/32 -p tcp -m tcp --dport 53 -j ACCEPT",
            rendered,
        );
        assert_eq!(ReadbackRule::parse_all(&doubled).len(), policy.rule_count(Family::V4));
        let refused = policy.verify_readback(Family::V4, &doubled).expect_err("udp twice");
        assert!(refused.contains("tcp port-53 exception"), "{refused}");
        assert!(refused.contains("never rendered"), "{refused}");
    }

    /// The v6 half of the two count-preserving shapes that a v4-only proof leaves open: an extra
    /// copy of a rendered exception, and a correct exception REORDERED below the DROP that covers
    /// its address.
    ///
    /// Separate from the v4 tests rather than folded into them because the v6 chain is read back by
    /// its own binary, carries no proxy pinhole and no metadata DROP, and is the one an edit is
    /// likelier to break unnoticed.
    #[test]
    fn an_extra_copy_or_a_sunk_exception_is_refused_in_the_v6_chain() {
        let policy = policy_with_resolvers(&["2001:4860:4860::8888"]);
        let good = normalized_readback(&policy, Family::V6);
        let udp = "-A OUTPUT -d 2001:4860:4860::8888/128 -p udp -m udp --dport 53 -j ACCEPT";
        let tcp = "-A OUTPUT -d 2001:4860:4860::8888/128 -p tcp -m tcp --dport 53 -j ACCEPT";
        assert!(good.contains(udp) && good.contains(tcp), "the fixture must hold both: {good}");

        // UDP twice in place of TCP: same total, same ACCEPT total, one transport gains a rule it
        // does not need while the other loses the one it does.
        let doubled = good.replace(tcp, udp);
        assert_eq!(ReadbackRule::parse_all(&doubled).len(), policy.rule_count(Family::V6));
        let refused = policy
            .verify_readback(Family::V6, &doubled)
            .expect_err("udp twice in the v6 chain");
        assert!(refused.contains("tcp port-53 exception"), "{refused}");
        assert!(refused.contains("never rendered"), "{refused}");

        // Both exceptions present and correct, the UDP one moved below the first DROP. iptables
        // takes the first match, so it is present, countable, and doing nothing.
        let mut lines: Vec<&str> = good.lines().collect();
        let at = lines.iter().position(|line| *line == udp).expect("the udp exception");
        lines.remove(at);
        let first_drop = lines
            .iter()
            .position(|line| line.contains("-j DROP"))
            .expect("the v6 chain must carry a DROP, or there is nothing to sink below");
        lines.insert(first_drop + 1, udp);
        let sunk = lines.join("\n");
        assert_eq!(
            ReadbackRule::parse_all(&sunk).len(),
            policy.rule_count(Family::V6),
            "reordering must not change the count, or the count check is what catches it"
        );
        let below = policy
            .verify_readback(Family::V6, &sunk)
            .expect_err("an exception below the range DROP is inert");
        assert!(below.contains("below the first range DROP"), "{below}");
    }

    /// **A resolver that is also the gateway, with a proxy range of `53-53`, renders two identical
    /// ACCEPTs — and that namespace is correct.**
    ///
    /// `PortRange::new(53, 53)` is legal, so this is a policy an operator can configure. The two
    /// rules are byte-identical after the kernel collapses `53:53` to `53`, so a verifier that looks
    /// up "the tcp port-53 rule for this address" finds two and refuses its own valid readback. Roles
    /// are matched as a multiset and consumed once each, which is what makes this pass while an
    /// unexpected third copy still fails.
    #[test]
    fn a_resolver_that_is_also_the_proxy_on_port_53_verifies_and_still_refuses_an_extra_copy() {
        let policy = NetPolicy {
            gateway: "172.17.0.1".to_owned(),
            proxy_ports: Some(PortRange::new(53, 53).unwrap()),
            log_connections: true,
            dns_resolvers: vec!["172.17.0.1".to_owned()],
        };
        // Three exceptions rendered: udp53 and tcp53 for the resolver, tcp53:53 for the proxy — the
        // last two identical once normalized.
        assert_eq!(policy.dns_pinhole_count(Family::V4), 2);
        let readback = normalized_readback(&policy, Family::V4);
        assert_eq!(
            readback.matches("-d 172.17.0.1/32 -p tcp -m tcp --dport 53 -j ACCEPT").count(),
            2,
            "the renderer really does emit the identical pair this test exists for"
        );
        assert_eq!(policy.verify_readback(Family::V4, &readback), Ok(()));

        // A third copy, with a LOG rule displaced so the count still matches: an unexpected
        // duplicate is still an ACCEPT nobody rendered.
        let extra = readback.replace(
            "-d 224.0.0.0/4 -m limit --limit 6/min --limit-burst 12 -j LOG --log-prefix sbx-net-deny:",
            "-d 172.17.0.1/32 -p tcp -m tcp --dport 53 -j ACCEPT",
        );
        assert_eq!(ReadbackRule::parse_all(&extra).len(), policy.rule_count(Family::V4));
        let refused = policy.verify_readback(Family::V4, &extra).expect_err("a third copy");
        assert!(refused.contains("never rendered"), "{refused}");
    }

    /// The same coexistence with distinct ports: the resolver is the gateway, the proxy is on its own
    /// range, and the three exceptions are told apart by shape.
    #[test]
    fn a_resolver_that_is_also_the_proxy_on_other_ports_verifies() {
        let policy = NetPolicy {
            gateway: "172.17.0.1".to_owned(),
            proxy_ports: Some(PortRange::new(49200, 49299).unwrap()),
            log_connections: true,
            dns_resolvers: vec!["172.17.0.1".to_owned()],
        };
        let readback = normalized_readback(&policy, Family::V4);
        assert_eq!(policy.verify_readback(Family::V4, &readback), Ok(()));

        // And the proxy's own range is still not a DNS exception: moving the DNS rules onto the
        // proxy range leaves the same three ACCEPTs to the same host, and must still be refused.
        let confused = readback.replace(
            "-d 172.17.0.1/32 -p udp -m udp --dport 53 -j ACCEPT",
            "-d 172.17.0.1/32 -p udp -m udp --dport 49200:49299 -j ACCEPT",
        );
        let refused = policy.verify_readback(Family::V4, &confused).expect_err("not a resolver rule");
        assert!(refused.contains("udp port-53 exception"), "{refused}");
    }

    /// A v6 resolver is verified against the v6 chain and is invisible to the v4 one — the two are
    /// read back from different binaries, and crossing them would report a correct namespace broken.
    #[test]
    fn a_v6_resolver_is_verified_against_the_v6_chain() {
        let policy = policy_with_resolvers(&["2001:4860:4860::8888"]);
        let v6 = format!(
            "-A OUTPUT -d 2001:4860:4860::8888/128 -p udp -m udp --dport 53 -j ACCEPT\n\
             -A OUTPUT -d 2001:4860:4860::8888/128 -p tcp -m tcp --dport 53 -j ACCEPT\n{MEASURED_V6}"
        );
        assert_eq!(policy.verify_readback(Family::V6, &v6), Ok(()));
        assert_eq!(policy.verify_readback(Family::V4, MEASURED_V4), Ok(()), "v4 chain unchanged");

        // The exceptions opened for a *different* v6 resolver: same rule count, same ACCEPT count,
        // same port — only the address is wrong. The refusal has to name the resolver this policy
        // configured, or it is proving presence rather than identity.
        let elsewhere = v6.replace("-d 2001:4860:4860::8888/128", "-d 2606:4700:4700::1111/128");
        assert_eq!(ReadbackRule::parse_all(&elsewhere).len(), policy.rule_count(Family::V6));
        let missing = policy.verify_readback(Family::V6, &elsewhere).expect_err("wrong v6 resolver");
        assert!(missing.contains("2001:4860:4860::8888/128"), "{missing}");
    }
}
