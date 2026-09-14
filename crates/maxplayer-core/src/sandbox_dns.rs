//! The resolver a contained job can actually reach, and the `/etc/resolv.conf` that names it.
//!
//! ## Why this module exists at all
//!
//! Docker's embedded DNS resolver answers at `127.0.0.11` inside a container on any *user-defined*
//! network. It is not a process in the container: it is a socket the daemon binds inside that
//! network namespace, reached through NAT rules installed in the same namespace.
//!
//! Under gVisor (`--runtime runsc`) the sandbox runs its own network stack and terminates loopback
//! inside the sentry, so a packet a job sends to `127.0.0.11:53` never reaches those rules or that
//! socket. Measured on Ubuntu 24.04 with runsc release-20260817.0, image
//! `maxplayer-sandbox:v0.5.8`, one named bridge, identical container flags, only `--runtime`
//! differing:
//!
//! ```text
//! runsc:  dns.lookup("relay.maxplayer.ai") -> EAI_AGAIN     raw udp to 127.0.0.11:53 -> no answer
//! runc:   dns.lookup("relay.maxplayer.ai") -> 34.225.223.145
//! ```
//!
//! `docker run --dns <addr>` does **not** move it: on a user-defined network the daemon still writes
//! `nameserver 127.0.0.11` into the container and merely forwards upstream from its own side. So the
//! only lever that reaches the job is the file itself — the job is handed a `/etc/resolv.conf`
//! naming real upstream resolvers, read-only, and [`crate::sandbox_net::NetPolicy`] opens port 53 to
//! exactly those addresses and nothing wider.
//!
//! ## What is deliberately refused
//!
//! A loopback resolver (`127.0.0.0/8`, `::1`) is refused rather than written. On a systemd host
//! `/etc/resolv.conf` names the local stub `127.0.0.53`, which is unreachable from inside the
//! sandbox for the very same reason `127.0.0.11` is — writing it would reproduce the bug with a
//! different address and a more confusing error.
//!
//! When no resolver can be established, this module returns an error. It never falls back to a
//! public resolver of its own choosing: that would be a silent host-side decision about where a
//! stranger's job sends its lookups, and an operator who intended a specific resolver would never
//! learn it was ignored.

use std::fmt;
use std::net::IpAddr;

/// Where a resolver list came from, carried so an operator reading a doctor line or a job failure
/// knows which knob moved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverSource {
    /// `[sandbox] dns_servers` named them explicitly.
    Configured,
    /// Discovered from the host's own resolver configuration.
    HostResolvConf,
    /// Discovered from `resolvectl status`, because the host's `/etc/resolv.conf` named only a local
    /// stub.
    HostResolvectl,
}

impl fmt::Display for ResolverSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Configured => "[sandbox] dns_servers",
            Self::HostResolvConf => "the host's /etc/resolv.conf",
            Self::HostResolvectl => "resolvectl status (the host file named only a local stub)",
        };
        f.write_str(text)
    }
}

/// A resolver set a contained job can use, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolvers {
    addresses: Vec<String>,
    source: ResolverSource,
}

impl Resolvers {
    /// The addresses, in the order they will be written and opened.
    pub fn addresses(&self) -> &[String] {
        &self.addresses
    }

    /// Where they came from.
    pub fn source(&self) -> ResolverSource {
        self.source
    }

    /// The `/etc/resolv.conf` body a job receives.
    ///
    /// `options ndots:0` is deliberate: without it a lookup of a dotted public name is first tried
    /// against every entry of a `search` list, and this file names no search domain at all, so the
    /// option states what the absent list already implies rather than leaving it to resolver
    /// defaults that differ between libc and musl images.
    pub fn render_resolv_conf(&self) -> String {
        // The header deliberately carries no resolver address of its own. Someone debugging a
        // broken job greps this file for the address it is using, and a commented-out address would
        // answer that question wrongly.
        let mut body = String::from(
            "# Written by maxplayer for a contained job. Docker's embedded resolver is unreachable\n\
             # from a gVisor sandbox, so this file names upstream resolvers directly and the job's\n\
             # egress policy opens port 53 to exactly these addresses.\n",
        );
        for address in &self.addresses {
            body.push_str("nameserver ");
            body.push_str(address);
            body.push('\n');
        }
        body.push_str("options ndots:0\n");
        body
    }
}

/// The one address a resolver exception can never reach, whatever order the rules are in: the cloud
/// metadata endpoint's DROP is installed ahead of every ACCEPT on purpose, so a "resolver" here
/// would be a rule that renders, reads back, and silently answers nothing.
///
/// Kept as the bare address rather than borrowing [`crate::sandbox_net::METADATA_ENDPOINT`], which
/// carries a `/32` because it is an iptables argument.
pub const METADATA_RESOLVER: &str = "169.254.169.254";

/// An address no contained job can use as a resolver, whatever this module does with it.
///
/// Loopback for the reason the whole module exists: the sandbox terminates loopback in its own
/// network stack. The metadata endpoint because its DROP deliberately precedes every ACCEPT, so an
/// exception naming it is inert by design and not by accident.
fn unusable(parsed: &IpAddr) -> bool {
    parsed.is_loopback() || parsed.to_string() == METADATA_RESOLVER
}

/// Why no usable resolver could be established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolverError {
    /// An address in `[sandbox] dns_servers` is not an IP address.
    NotAnAddress(String),
    /// An address is a loopback address, which no sandbox can reach.
    Loopback(String),
    /// An address is the cloud metadata endpoint, whose unconditional DROP precedes every ACCEPT
    /// this policy can render.
    MetadataEndpoint(String),
    /// Nothing usable was configured and nothing usable was discovered.
    NoneFound(String),
}

impl fmt::Display for ResolverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnAddress(value) => write!(
                f,
                "[sandbox] dns_servers: {value:?} is not an IP address — a resolver must be \
                 addressed, not named, because resolving the resolver is the problem being fixed"
            ),
            Self::Loopback(value) => write!(
                f,
                "[sandbox] dns_servers: {value:?} is a loopback address, which is unreachable from \
                 inside the job's sandbox — that is exactly why docker's own 127.0.0.11 fails under \
                 gVisor; name the upstream resolver itself"
            ),
            Self::MetadataEndpoint(value) => write!(
                f,
                "[sandbox] dns_servers: {value:?} is the cloud metadata endpoint, which every job's \
                 egress policy drops BEFORE any exception it renders — a resolver there would be a \
                 rule that changes nothing while the job fails every lookup; name a real resolver"
            ),
            Self::NoneFound(detail) => write!(
                f,
                "no usable DNS resolver for contained jobs: {detail}. Set `[sandbox] dns_servers` to \
                 the resolver addresses this host's jobs should use — jobs are refused rather than \
                 pointed at a resolver nobody chose"
            ),
        }
    }
}

/// Validate operator-named resolvers. Empty input ⇒ `Ok(None)`, meaning "nothing configured", not
/// "nothing usable".
pub fn from_config(configured: &[String]) -> Result<Option<Resolvers>, ResolverError> {
    let named: Vec<&str> = configured
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect();
    if named.is_empty() {
        return Ok(None);
    }
    let mut addresses = Vec::new();
    for value in named {
        let parsed: IpAddr = value
            .parse()
            .map_err(|_| ResolverError::NotAnAddress(value.to_owned()))?;
        if parsed.is_loopback() {
            return Err(ResolverError::Loopback(value.to_owned()));
        }
        if unusable(&parsed) {
            return Err(ResolverError::MetadataEndpoint(value.to_owned()));
        }
        let canonical = parsed.to_string();
        if !addresses.contains(&canonical) {
            addresses.push(canonical);
        }
    }
    Ok(Some(Resolvers {
        addresses,
        source: ResolverSource::Configured,
    }))
}

/// The usable `nameserver` lines of a `resolv.conf` body: parsed, de-duplicated, and stripped of
/// loopback stubs.
///
/// Returned separately from the stub count so a caller can tell "this host names no resolver" from
/// "this host names only a stub" — the second is the systemd case that has an answer, and reporting
/// it as the first would send an operator to fix DNS that is working.
pub fn parse_resolv_conf(body: &str) -> (Vec<String>, usize) {
    let mut addresses = Vec::new();
    let mut stubs = 0usize;
    for line in body.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some(rest) = line.strip_prefix("nameserver") else {
            continue;
        };
        let Ok(parsed) = rest.trim().parse::<IpAddr>() else {
            continue;
        };
        if parsed.is_loopback() {
            stubs += 1;
            continue;
        }
        // Not counted as a stub: a stub has an upstream worth chasing through `resolvectl`, and this
        // address has nothing behind it. Dropping it here means a host whose file named only this
        // fails with "names no resolver at all", which is the truth.
        if unusable(&parsed) {
            continue;
        }
        let canonical = parsed.to_string();
        if !addresses.contains(&canonical) {
            addresses.push(canonical);
        }
    }
    (addresses, stubs)
}

/// The upstream resolvers in `resolvectl status` output — the "DNS Servers:" entries, which is where
/// a systemd host keeps the addresses its `127.0.0.53` stub forwards to.
pub fn parse_resolvectl(stdout: &str) -> Vec<String> {
    let mut addresses = Vec::new();
    let mut in_block = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("DNS Servers:") {
            in_block = true;
            push_addresses(rest, &mut addresses);
            continue;
        }
        if in_block {
            // Continuation lines are indented and carry nothing but addresses; anything with a
            // colon-terminated label starts a new field and ends the block.
            let is_continuation = line.starts_with(char::is_whitespace)
                && !trimmed.is_empty()
                && trimmed.split_whitespace().all(|token| token.parse::<IpAddr>().is_ok());
            if is_continuation {
                push_addresses(trimmed, &mut addresses);
                continue;
            }
            in_block = false;
        }
    }
    addresses
}

fn push_addresses(text: &str, into: &mut Vec<String>) {
    for token in text.split_whitespace() {
        let Ok(parsed) = token.parse::<IpAddr>() else {
            continue;
        };
        if unusable(&parsed) {
            continue;
        }
        let canonical = parsed.to_string();
        if !into.contains(&canonical) {
            into.push(canonical);
        }
    }
}

/// Resolve the resolver set for this seat: configuration first, then the host's own resolvers, then
/// an error. Never a guessed public resolver.
///
/// The two host readers are injected so every branch — including "the host names only a stub and
/// `resolvectl` is absent" — is testable on a machine that is none of those things.
pub fn resolve(
    configured: &[String],
    read_resolv_conf: impl FnOnce() -> Option<String>,
    read_resolvectl: impl FnOnce() -> Option<String>,
) -> Result<Resolvers, ResolverError> {
    if let Some(resolvers) = from_config(configured)? {
        return Ok(resolvers);
    }
    let host_body = read_resolv_conf();
    let (host_addresses, stubs) = host_body
        .as_deref()
        .map(parse_resolv_conf)
        .unwrap_or_else(|| (Vec::new(), 0));
    if !host_addresses.is_empty() {
        return Ok(Resolvers {
            addresses: host_addresses,
            source: ResolverSource::HostResolvConf,
        });
    }
    if stubs > 0 {
        if let Some(stdout) = read_resolvectl() {
            let upstreams = parse_resolvectl(&stdout);
            if !upstreams.is_empty() {
                return Ok(Resolvers {
                    addresses: upstreams,
                    source: ResolverSource::HostResolvectl,
                });
            }
        }
        return Err(ResolverError::NoneFound(
            "this host's /etc/resolv.conf names only a local stub (systemd-resolved), and \
             `resolvectl status` reported no upstream address"
                .to_owned(),
        ));
    }
    Err(ResolverError::NoneFound(
        "this host's /etc/resolv.conf names no resolver at all".to_owned(),
    ))
}

/// Read the host's `/etc/resolv.conf`, if it can be read.
pub fn host_resolv_conf() -> Option<String> {
    std::fs::read_to_string("/etc/resolv.conf").ok()
}

/// Run `resolvectl status` and return its stdout, if the command exists and succeeds.
pub fn host_resolvectl() -> Option<String> {
    let output = std::process::Command::new("resolvectl")
        .arg("status")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_resolver_wins_and_is_never_read_from_the_host() {
        let resolvers = resolve(
            &["9.9.9.9".to_owned()],
            || panic!("the host must not be consulted when an operator named a resolver"),
            || panic!("resolvectl must not run when an operator named a resolver"),
        )
        .expect("configured resolvers resolve");
        assert_eq!(resolvers.addresses(), ["9.9.9.9"]);
        assert_eq!(resolvers.source(), ResolverSource::Configured);
    }

    #[test]
    fn a_loopback_resolver_is_refused_rather_than_written() {
        // The whole bug is that a loopback resolver is unreachable from the sandbox. Accepting one
        // here would reproduce it with a different address.
        let error = from_config(&["127.0.0.53".to_owned()]).expect_err("loopback is refused");
        assert_eq!(error, ResolverError::Loopback("127.0.0.53".to_owned()));
        assert!(error.to_string().contains("unreachable from inside the job's sandbox"));
    }

    #[test]
    fn a_named_resolver_is_refused_because_resolving_it_is_the_problem() {
        let error =
            from_config(&["dns.example.com".to_owned()]).expect_err("a hostname is refused");
        assert!(matches!(error, ResolverError::NotAnAddress(_)));
    }

    #[test]
    fn the_hosts_real_resolvers_are_used_when_it_has_any() {
        let resolvers = resolve(
            &[],
            || Some("nameserver 10.0.0.2\nnameserver 10.0.0.3\n".to_owned()),
            || panic!("resolvectl must not run when the host file already names real resolvers"),
        )
        .expect("host resolvers resolve");
        assert_eq!(resolvers.addresses(), ["10.0.0.2", "10.0.0.3"]);
        assert_eq!(resolvers.source(), ResolverSource::HostResolvConf);
    }

    #[test]
    fn a_systemd_stub_falls_through_to_the_upstreams_resolvectl_reports() {
        // Exactly the shape of the host in the reported failure: /etc/resolv.conf names 127.0.0.53
        // and nothing else, so the answer lives in resolvectl.
        let resolvers = resolve(
            &[],
            || Some("nameserver 127.0.0.53\noptions edns0\n".to_owned()),
            || {
                Some(
                    "Global\n       Protocols: -LLMNR\n     DNS Servers: 1.1.1.1 1.0.0.1\n\
                     \n Link 2 (eth0)\n Current Scopes: DNS\n"
                        .to_owned(),
                )
            },
        )
        .expect("upstreams resolve");
        assert_eq!(resolvers.addresses(), ["1.1.1.1", "1.0.0.1"]);
        assert_eq!(resolvers.source(), ResolverSource::HostResolvectl);
    }

    #[test]
    fn a_stub_with_no_discoverable_upstream_fails_rather_than_guessing() {
        // The refusal this whole module exists to make: no public resolver is invented here.
        let error = resolve(
            &[],
            || Some("nameserver 127.0.0.53\n".to_owned()),
            || None,
        )
        .expect_err("no upstream is an error");
        let text = error.to_string();
        assert!(text.contains("local stub"), "{text}");
        assert!(text.contains("dns_servers"), "{text}");
        assert!(!text.contains("8.8.8.8"), "no resolver may be guessed: {text}");
    }

    #[test]
    fn a_host_with_no_resolver_at_all_fails_with_its_own_reason() {
        let error = resolve(&[], || Some(String::new()), || None).expect_err("nothing to use");
        assert!(error.to_string().contains("names no resolver at all"));
    }

    #[test]
    fn resolvectl_continuation_lines_are_read_and_labels_end_the_block() {
        let addresses = parse_resolvectl(
            "     DNS Servers: 1.1.1.1\n                  1.0.0.1\n      DNS Domain: lan\n",
        );
        assert_eq!(addresses, ["1.1.1.1", "1.0.0.1"]);
    }

    #[test]
    fn the_rendered_file_names_every_resolver_and_no_search_domain() {
        let resolvers = from_config(&["1.1.1.1".to_owned(), "9.9.9.9".to_owned()])
            .expect("valid")
            .expect("configured");
        let body = resolvers.render_resolv_conf();
        assert!(body.contains("nameserver 1.1.1.1\n"), "{body}");
        assert!(body.contains("nameserver 9.9.9.9\n"), "{body}");
        assert!(body.contains("options ndots:0"), "{body}");
        assert!(!body.contains("127.0.0.11"), "the unreachable resolver must not appear: {body}");
        assert!(!body.contains("search "), "a search domain would change lookup shape: {body}");
    }

    /// The file the job reads and the rules its packets meet must name the SAME addresses.
    ///
    /// This is the failure that has no symptom worth the name: a job pointed at a resolver its own
    /// egress policy drops reports `EAI_AGAIN`, exactly as if no resolver had been configured at
    /// all. `prepare_launch` resolves once and hands that one value to both sides; this test is what
    /// says a second discovery would not be tolerated.
    #[test]
    fn the_resolvers_written_into_the_job_are_exactly_the_ones_its_policy_opens_port_53_to() {
        use crate::sandbox_net::{Family, NetPolicy};

        let resolvers = resolve(
            &[],
            || Some("nameserver 10.0.0.2\nnameserver 2001:4860:4860::8888\n".to_owned()),
            || None,
        )
        .expect("the host names two resolvers");

        // What the job reads.
        let in_file: Vec<String> = resolvers
            .render_resolv_conf()
            .lines()
            .filter_map(|line| line.strip_prefix("nameserver "))
            .map(str::to_owned)
            .collect();
        assert_eq!(in_file, resolvers.addresses(), "the file is rendered from these addresses");

        // What its packets meet: every ACCEPT on port 53, across both chains, by destination.
        let policy = NetPolicy {
            gateway: "172.17.0.1".to_owned(),
            proxy_ports: None,
            log_connections: false,
            dns_resolvers: resolvers.addresses().to_vec(),
        };
        let mut opened: Vec<String> = policy
            .rules()
            .iter()
            .filter(|rule| rule.target() == Some("ACCEPT") && rule.args.contains(&"53".to_owned()))
            .filter_map(|rule| rule.destination().map(str::to_owned))
            .collect();
        opened.sort();
        opened.dedup();

        let mut expected: Vec<String> = in_file
            .iter()
            .map(|address| {
                if address.contains(':') {
                    format!("{address}/128")
                } else {
                    format!("{address}/32")
                }
            })
            .collect();
        expected.sort();
        assert_eq!(opened, expected, "a resolver in the file with no exception cannot answer");

        // And both families really are represented, so the equality above is not comparing two
        // single-family lists that happen to agree.
        assert_eq!(policy.dns_pinhole_count(Family::V4), 2);
        assert_eq!(policy.dns_pinhole_count(Family::V6), 2);
    }

    #[test]
    fn the_metadata_endpoint_is_refused_as_a_resolver_rather_than_rendered_inert() {
        // The metadata DROP is installed ahead of every ACCEPT deliberately, so an exception naming
        // that address reads back perfectly and resolves nothing. Refusing it at config time is the
        // difference between a named error and a seat whose jobs all fail EAI_AGAIN.
        let error =
            from_config(&[METADATA_RESOLVER.to_owned()]).expect_err("the metadata endpoint is refused");
        assert_eq!(error, ResolverError::MetadataEndpoint(METADATA_RESOLVER.to_owned()));
        assert!(error.to_string().contains("drops BEFORE any exception"), "{error}");
    }

    #[test]
    fn a_discovered_metadata_address_is_skipped_and_never_written_into_a_job() {
        // The host's own file is not a trusted resolver list: on a cloud box it can name the
        // metadata address, and passing that through would hand the job a resolver its own firewall
        // drops first.
        let (addresses, stubs) =
            parse_resolv_conf(&format!("nameserver {METADATA_RESOLVER}\nnameserver 10.0.0.2\n"));
        assert_eq!(addresses, ["10.0.0.2"]);
        assert_eq!(stubs, 0, "the metadata endpoint is not a stub with an upstream to chase");

        let error = resolve(&[], || Some(format!("nameserver {METADATA_RESOLVER}\n")), || None)
            .expect_err("a host naming only the metadata endpoint has no resolver");
        assert!(error.to_string().contains("names no resolver at all"), "{error}");

        assert_eq!(
            parse_resolvectl(&format!("     DNS Servers: {METADATA_RESOLVER} 1.1.1.1\n")),
            ["1.1.1.1"]
        );
    }

    #[test]
    fn duplicate_resolvers_collapse_so_the_policy_opens_one_pinhole_pair() {
        let resolvers = from_config(&["1.1.1.1".to_owned(), "1.1.1.1".to_owned()])
            .expect("valid")
            .expect("configured");
        assert_eq!(resolvers.addresses(), ["1.1.1.1"]);
    }
}
