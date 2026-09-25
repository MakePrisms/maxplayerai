//! Conservative compatibility backend for kernels without cls_flower.
//!
//! Normal TCP/UDP/ICMP packets retain policy order and destinations. Before those
//! rules, reject header layouts that this fixed-offset backend cannot interpret:
//! IPv4 options/fragments, IPv6 extensions, and encapsulating/other protocols.
//! This is deliberately narrower than flower, never an unfiltered fallback.
use super::*;
use std::{collections::BTreeMap, net::IpAddr};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    offset: u16,
    value: u32,
    mask: u32,
}
#[derive(Debug, Clone)]
struct Rule {
    family: Family,
    keys: Vec<Key>,
    action: &'static str,
}
#[derive(Debug, Clone)]
pub struct U32Plan {
    dev: String,
    rules: Vec<Rule>,
}

// Minimal aligned power-of-two cover; unlike a single mask this cannot widen a range.
fn range_masks(mut lo: u32, hi: u32, bits: u32) -> Vec<(u32, u32)> {
    let all = (1u32 << bits) - 1;
    let mut out = Vec::new();
    while lo <= hi {
        let align = if lo == 0 {
            bits
        } else {
            lo.trailing_zeros().min(bits)
        };
        let fit = 31 - (hi - lo + 1).leading_zeros();
        let size = 1u32 << align.min(fit);
        out.push((lo, all & !(size - 1)));
        lo += size;
    }
    out
}
fn byte(offset: u16, value: u8, mask: u8) -> Key {
    let shift = 24 - (offset % 4) * 8;
    Key {
        offset: offset & !3,
        value: u32::from(value & mask) << shift,
        mask: u32::from(mask) << shift,
    }
}
fn normalized(keys: Vec<Key>) -> Result<Vec<Key>, String> {
    let mut words: BTreeMap<u16, (u32, u32)> = BTreeMap::new();
    for key in keys {
        let word = words.entry(key.offset).or_default();
        if (word.0 ^ key.value) & word.1 & key.mask != 0 {
            return Err("u32 contradictory match keys".into());
        }
        word.0 |= key.value & key.mask;
        word.1 |= key.mask;
    }
    Ok(words
        .into_iter()
        .map(|(offset, (value, mask))| Key {
            offset,
            value,
            mask,
        })
        .collect())
}
impl U32Plan {
    pub fn derive(plan: &IfacePlan) -> Result<Self, String> {
        let mut rules = Vec::new();
        // Any differing version/IHL bit is a refusal. ARP is untouched (different ethertype).
        for (family, expected, mask) in [(Family::V4, 0x45u8, 0xffu8), (Family::V6, 0x60, 0xf0)] {
            for bit in 0..8 {
                let bit = 1 << bit;
                if mask & bit != 0 {
                    rules.push(Rule {
                        family,
                        keys: vec![byte(0, expected ^ bit, bit)],
                        action: "drop",
                    });
                }
            }
            // Exclude protocols requiring dissection (including GRE and IP-in-IP), not just
            // the common IPv6 extension types. Future protocol numbers remain fail-closed.
            let (offset, allowed): (u16, &[u32]) = match family {
                Family::V4 => (9, &[1, 6, 17]),
                Family::V6 => (6, &[6, 17, 58]),
            };
            let mut start = 0;
            for stop in allowed.iter().copied().chain(std::iter::once(256)) {
                if start < stop {
                    for (value, mask) in range_masks(start, stop - 1, 8) {
                        rules.push(Rule {
                            family,
                            keys: vec![byte(offset, value as u8, mask as u8)],
                            action: "drop",
                        });
                    }
                }
                start = stop + 1;
            }
        }
        // Any fragment offset or MF bit; do not interpret fragment bytes as transport ports.
        for bit in 0..14 {
            let mask = 1u32 << bit;
            rules.push(Rule {
                family: Family::V4,
                keys: vec![Key {
                    offset: 4,
                    value: mask,
                    mask,
                }],
                action: "drop",
            });
        }
        for filter in &plan.filters {
            let mut keys = Vec::new();
            if let Some(dst) = &filter.dst {
                let (ip, prefix) = dst.split_once('/').unwrap_or((
                    dst,
                    if filter.family == Family::V4 {
                        "32"
                    } else {
                        "128"
                    },
                ));
                let ip: IpAddr = ip.parse().map_err(|_| "u32 invalid destination")?;
                let prefix: u32 = prefix.parse().map_err(|_| "u32 invalid prefix")?;
                let (bytes, base, max) = match (filter.family, ip) {
                    (Family::V4, IpAddr::V4(ip)) => (ip.octets().to_vec(), 16, 32),
                    (Family::V6, IpAddr::V6(ip)) => (ip.octets().to_vec(), 24, 128),
                    _ => return Err("u32 destination family mismatch".into()),
                };
                if prefix > max {
                    return Err("u32 prefix out of range".into());
                }
                for (i, word) in bytes.chunks_exact(4).enumerate() {
                    let n = prefix.saturating_sub(i as u32 * 32).min(32);
                    if n != 0 {
                        let mask = u32::MAX << (32 - n);
                        keys.push(Key {
                            offset: base + i as u16 * 4,
                            value: u32::from_be_bytes(word.try_into().unwrap()) & mask,
                            mask,
                        });
                    }
                }
            }
            if let Some(proto) = &filter.ip_proto {
                let proto = match proto.as_str() {
                    "tcp" => 6,
                    "udp" => 17,
                    "icmpv6" => 58,
                    _ => return Err(format!("u32 unsupported protocol rule: {proto}")),
                };
                keys.push(byte(
                    if filter.family == Family::V4 { 9 } else { 6 },
                    proto,
                    255,
                ));
            }
            if let Some(kind) = &filter.icmp_type {
                if filter.family != Family::V6 || filter.ip_proto.as_deref() != Some("icmpv6") {
                    return Err("u32 unsupported ICMP rule class".into());
                }
                keys.push(byte(
                    40,
                    kind.parse().map_err(|_| "u32 invalid ICMP type")?,
                    255,
                ));
            }
            if let Some(ttl) = &filter.ip_ttl {
                keys.push(byte(
                    if filter.family == Family::V4 { 8 } else { 7 },
                    ttl.parse().map_err(|_| "u32 invalid hop limit")?,
                    255,
                ));
            }
            let ports = if let Some(port) = &filter.dst_port {
                if !matches!(filter.ip_proto.as_deref(), Some("tcp" | "udp")) {
                    return Err("u32 unsupported destination-port rule class".into());
                }
                let (lo, hi) = port.split_once('-').unwrap_or((port, port));
                let lo: u16 = lo.parse().map_err(|_| "u32 invalid port")?;
                let hi: u16 = hi.parse().map_err(|_| "u32 invalid port")?;
                if lo > hi {
                    return Err("u32 reversed port range".into());
                }
                range_masks(u32::from(lo), u32::from(hi), 16)
            } else {
                vec![(0, 0)]
            };
            for (value, mask) in ports {
                let mut keys = keys.clone();
                if mask != 0 {
                    keys.push(Key {
                        offset: if filter.family == Family::V4 { 20 } else { 40 },
                        value,
                        mask,
                    });
                }
                if keys.is_empty() {
                    return Err("u32 refuses a wildcard policy rule".into());
                }
                if !matches!(filter.action, "pass" | "drop") {
                    return Err("u32 unsupported action".into());
                }
                rules.push(Rule {
                    family: filter.family,
                    keys: normalized(keys)?,
                    action: filter.action,
                });
            }
        }
        if rules.len() >= u16::MAX as usize {
            return Err("u32 plan exceeds priority space".into());
        }
        Ok(Self {
            dev: plan.dev.clone(),
            rules,
        })
    }

    pub fn plan_stdin(&self) -> (String, usize) {
        let mut out = format!("tc qdisc add dev {} clsact\n", self.dev);
        for (i, rule) in self.rules.iter().enumerate() {
            out.push_str(&format!(
                "tc filter add dev {} egress pref {} protocol {} u32",
                self.dev,
                i + 1,
                tc_protocol(rule.family)
            ));
            for key in &rule.keys {
                out.push_str(&format!(
                    " match u32 0x{:08x} 0x{:08x} at {}",
                    key.value, key.mask, key.offset
                ));
            }
            out.push_str(&format!(" action {}\n", rule.action));
        }
        (out, self.rules.len() + 1)
    }

    /// Consume every header, root-table declaration, selector and action. No links, offsets,
    /// extra selectors, probabilistic actions or off-path chains may be hidden in a readback.
    pub fn verify_readback(&self, stdout: &str) -> Result<(), String> {
        let lines: Vec<_> = stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let mut at = 0;
        for (i, expected) in self.rules.iter().enumerate() {
            let header = format!(
                "filter protocol {} pref {} u32 chain 0",
                tc_protocol(expected.family),
                i + 1
            );
            if lines.get(at) != Some(&header.as_str()) {
                return Err(format!("u32 missing/out-of-order header at rule {}", i + 1));
            }
            at += 1;
            let table = lines
                .get(at)
                .and_then(|l| l.strip_prefix(&format!("{header} fh ")))
                .and_then(|l| l.strip_suffix(" ht divisor 1"))
                .and_then(|l| l.strip_suffix(':'))
                .ok_or("u32 missing root table")?;
            let table_id = u16::from_str_radix(table, 16).map_err(|_| "u32 invalid root table")?;
            if table_id == 0 || table_id > 0xfff {
                return Err("u32 invalid root table ID".into());
            }
            at += 1;
            let node = lines
                .get(at)
                .and_then(|l| l.strip_prefix(&format!("{header} fh {table}::")))
                .ok_or("u32 missing terminal node")?;
            let fields: Vec<_> = node.split_whitespace().collect();
            if fields.len() < 10 {
                return Err("u32 truncated node".into());
            }
            let node_id = u16::from_str_radix(fields[0], 16).map_err(|_| "u32 invalid node ID")?;
            let order = fields[2]
                .parse::<u16>()
                .map_err(|_| "u32 invalid node order")?;
            if node_id == 0
                || node_id > 0xfff
                || node_id != order
                || fields[1] != "order"
                || fields[3..10] != ["key", "ht", table, "bkt", "0", "terminal", "flowid"]
                || fields[10..]
                    .iter()
                    .any(|f| !matches!(*f, "not_in_hw" | "in_hw" | "skip_hw"))
            {
                return Err("u32 unexpected node semantics".into());
            }
            at += 1;
            let mut keys = Vec::new();
            let mut action = ReadbackFilter {
                protocol: tc_protocol(expected.family).into(),
                pref: (i + 1) as u16,
                chain: 0,
                handle: node_id.to_string(),
                keys: vec![],
                actions: vec![],
            };
            while at < lines.len() && !lines[at].starts_with("filter ") {
                let fields: Vec<_> = lines[at].split_whitespace().collect();
                match fields[0] {
                    "match" => {
                        if fields.len() != 4 || fields[2] != "at" {
                            return Err("u32 unknown match layout".into());
                        }
                        let (value, mask) = fields[1].split_once('/').ok_or("u32 invalid mask")?;
                        keys.push(Key {
                            value: u32::from_str_radix(value, 16)
                                .map_err(|_| "u32 invalid value")?,
                            mask: u32::from_str_radix(mask, 16).map_err(|_| "u32 invalid mask")?,
                            offset: fields[3]
                                .parse()
                                .map_err(|_| "u32 variable/invalid offset")?,
                        });
                    }
                    "action" => parse_action_line(&mut action, &fields, at + 1)?,
                    _ if is_action_detail(&fields) => {
                        parse_action_detail(&action, &fields, at + 1)?
                    }
                    _ if is_counter_line(&fields) => check_counter_line(&action, &fields, at + 1)?,
                    _ => return Err(format!("u32 unread token at line {}", at + 1)),
                }
                at += 1;
            }
            if keys != expected.keys || action.actions != [expected.action] {
                return Err(format!(
                    "u32 rule {} differs from the installed plan",
                    i + 1
                ));
            }
        }
        if at != lines.len() {
            return Err("u32 unexpected additional filters".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_net::PortRange;
    fn plan() -> U32Plan {
        U32Plan::derive(
            &IfacePlan::derive(
                "eth0",
                &NetPolicy {
                    gateway: "192.168.65.254".into(),
                    proxy_ports: Some(PortRange::new(49200, 49299).unwrap()),
                    log_connections: true,
                    dns_resolvers: vec![
                        "192.168.65.1".parse().unwrap(),
                        "fd00::53".parse().unwrap(),
                    ],
                },
            )
            .unwrap(),
        )
        .unwrap()
    }
    fn packet4(dst: [u8; 4], proto: u8, port: u16) -> Vec<u8> {
        let mut p = vec![0; 64];
        p[0] = 0x45;
        p[9] = proto;
        p[16..20].copy_from_slice(&dst);
        p[22..24].copy_from_slice(&port.to_be_bytes());
        p
    }
    fn packet6(dst: &str, proto: u8, port: u16) -> Vec<u8> {
        let mut p = vec![0; 80];
        p[0] = 0x60;
        p[6] = proto;
        p[7] = 255;
        let ip: std::net::Ipv6Addr = dst.parse().unwrap();
        p[24..40].copy_from_slice(&ip.octets());
        p[42..44].copy_from_slice(&port.to_be_bytes());
        p
    }
    fn decision(plan: &U32Plan, family: Family, p: &[u8]) -> &'static str {
        plan.rules
            .iter()
            .find(|r| {
                r.family == family
                    && r.keys.iter().all(|k| {
                        p.get(k.offset as usize..k.offset as usize + 4)
                            .is_some_and(|b| {
                                u32::from_be_bytes(b.try_into().unwrap()) & k.mask == k.value
                            })
                    })
            })
            .map_or("pass", |r| r.action)
    }
    #[test]
    fn all_proxy_ports_and_adjacent_denials() {
        let plan = plan();
        for port in 0..=u16::MAX {
            assert_eq!(
                decision(&plan, Family::V4, &packet4([192, 168, 65, 254], 6, port)),
                if (49200..=49299).contains(&port) {
                    "pass"
                } else {
                    "drop"
                },
                "port {port}"
            );
        }
    }
    #[test]
    fn dns_nd_public_and_private_packets() {
        let plan = plan();
        for proto in [6, 17] {
            assert_eq!(
                decision(&plan, Family::V4, &packet4([192, 168, 65, 1], proto, 53)),
                "pass"
            );
            assert_eq!(
                decision(&plan, Family::V4, &packet4([192, 168, 65, 1], proto, 54)),
                "drop"
            );
            assert_eq!(
                decision(&plan, Family::V6, &packet6("fd00::53", proto, 53)),
                "pass"
            );
            assert_eq!(
                decision(&plan, Family::V6, &packet6("fd00::53", proto, 54)),
                "drop"
            );
            assert_eq!(
                decision(&plan, Family::V4, &packet4([8, 8, 8, 8], proto, 443)),
                "pass"
            );
            assert_eq!(
                decision(&plan, Family::V6, &packet6("2606:4700::1111", proto, 443)),
                "pass"
            );
            assert_eq!(
                decision(&plan, Family::V4, &packet4([169, 254, 169, 254], proto, 53)),
                "drop"
            );
        }
        let mut ns = packet6("ff02::1:ff00:1", 58, 0);
        ns[40] = 135;
        assert_eq!(decision(&plan, Family::V6, &ns), "pass");
        ns[7] = 254;
        assert_eq!(decision(&plan, Family::V6, &ns), "drop");
        let mut na = packet6("fe80::1", 58, 0);
        na[40] = 136;
        assert_eq!(decision(&plan, Family::V6, &na), "pass");
        na[40] = 128;
        assert_eq!(decision(&plan, Family::V6, &na), "drop");
    }
    #[test]
    fn unparsed_packets_never_reach_exceptions_or_public_default() {
        let plan = plan();
        for proto in 0..=255 {
            let p = packet4([8, 8, 8, 8], proto, 443);
            assert_eq!(
                decision(&plan, Family::V4, &p),
                if [1, 6, 17].contains(&proto) {
                    "pass"
                } else {
                    "drop"
                }
            );
            let p = packet6("2606:4700::1111", proto, 443);
            assert_eq!(
                decision(&plan, Family::V6, &p),
                if [6, 17, 58].contains(&proto) {
                    "pass"
                } else {
                    "drop"
                }
            );
        }
        for first in 0..=255 {
            let mut p = packet4([8, 8, 8, 8], 6, 443);
            p[0] = first;
            assert_eq!(
                decision(&plan, Family::V4, &p),
                if first == 0x45 { "pass" } else { "drop" }
            );
        }
        for bit in 0..14 {
            let mut p = packet4([8, 8, 8, 8], 6, 443);
            p[6..8].copy_from_slice(&(1u16 << bit).to_be_bytes());
            assert_eq!(decision(&plan, Family::V4, &p), "drop");
        }
        let mut p = packet4([8, 8, 8, 8], 6, 443);
        p[6] = 0x40; // DF is normal TCP, not fragmentation.
        assert_eq!(decision(&plan, Family::V4, &p), "pass");
    }
    #[test]
    fn every_policy_prefix_boundary_matches_the_logical_plan() {
        let source = IfacePlan::derive(
            "eth0",
            &NetPolicy {
                gateway: "192.168.65.254".into(),
                proxy_ports: Some(PortRange::new(49200, 49299).unwrap()),
                log_connections: true,
                dns_resolvers: vec!["192.168.65.1".parse().unwrap(), "fd00::53".parse().unwrap()],
            },
        )
        .unwrap();
        let actual = U32Plan::derive(&source).unwrap();
        let prefix = |text: &str, family| {
            let bits = if family == Family::V4 { 32 } else { 128 };
            let (ip, n) = text
                .split_once('/')
                .map_or((text, bits), |(ip, n)| (ip, n.parse::<u32>().unwrap()));
            let ip: IpAddr = ip.parse().unwrap();
            let ip = match ip {
                IpAddr::V4(ip) => u128::from(u32::from(ip)),
                IpAddr::V6(ip) => u128::from(ip),
            };
            let hostmask = if bits - n == 128 {
                u128::MAX
            } else {
                (1u128 << (bits - n)) - 1
            };
            (ip & !hostmask, hostmask)
        };
        for seed in &source.filters {
            let Some(dst) = &seed.dst else { continue };
            let (base, mask) = prefix(dst, seed.family);
            for address in [
                base,
                base | mask,
                base.saturating_sub(1),
                (base | mask).saturating_add(1),
            ] {
                if seed.family == Family::V4 && address > u32::MAX as u128 {
                    continue;
                }
                for (proto, name) in [(6, "tcp"), (17, "udp"), (58, "icmpv6")] {
                    if proto == 58 && seed.family == Family::V4 {
                        continue;
                    }
                    for port in [0, 53, 54, 443, 49199, 49200, 49299, 49300, 65535] {
                        for kind in [128, 135, 136] {
                            let mut p = if seed.family == Family::V4 {
                                packet4((address as u32).to_be_bytes(), proto, port)
                            } else {
                                packet6(&std::net::Ipv6Addr::from(address).to_string(), proto, port)
                            };
                            if seed.family == Family::V6 && proto == 58 {
                                p[40] = kind;
                            }
                            let expected = source
                                .filters
                                .iter()
                                .find(|r| {
                                    r.family == seed.family
                                        && r.dst.as_ref().is_none_or(|d| {
                                            let (base, mask) = prefix(d, r.family);
                                            address & !mask == base
                                        })
                                        && r.ip_proto.as_deref().is_none_or(|v| v == name)
                                        && r.dst_port.as_ref().is_none_or(|s| {
                                            let (lo, hi) = s.split_once('-').unwrap_or((s, s));
                                            (lo.parse::<u16>().unwrap()
                                                ..=hi.parse::<u16>().unwrap())
                                                .contains(&port)
                                        })
                                        && r.icmp_type
                                            .as_ref()
                                            .is_none_or(|v| v.parse::<u8>().unwrap() == kind)
                                        && r.ip_ttl.as_ref().is_none_or(|v| v == "255")
                                })
                                .map_or("pass", |r| r.action);
                            assert_eq!(
                                decision(&actual, seed.family, &p),
                                expected,
                                "address={address:x} protocol={proto} port={port} type={kind}"
                            );
                        }
                    }
                }
            }
        }
    }

    fn readback(plan: &U32Plan) -> String {
        // iproute2 f_u32.c print_raw/u32_print_opt grammar, not a live capture.
        let mut s = String::new();
        for (i, r) in plan.rules.iter().enumerate() {
            let h = format!(
                "filter protocol {} pref {} u32 chain 0",
                tc_protocol(r.family),
                i + 1
            );
            s += &format!(
                "{h}\n{h} fh 800: ht divisor 1\n{h} fh 800::800 order 2048 key ht 800 bkt 0 terminal flowid not_in_hw\n"
            );
            for k in &r.keys {
                s += &format!("  match {:08x}/{:08x} at {}\n", k.value, k.mask, k.offset);
            }
            s += &format!(
                "action order 1: gact action {}\nrandom type none pass val 0\nindex 1 ref 1 bind 1\n",
                r.action
            );
        }
        s
    }
    #[test]
    fn readback_rejects_tampering_truncation_and_unknowns() {
        let plan = plan();
        let good = readback(&plan);
        plan.verify_readback(&good).unwrap();
        for bad in [
            good.replacen("chain 0", "chain 7", 1),
            good.replacen("at 0", "at nexthdr+0", 1),
            good.replacen("gact action drop", "gact action pass", 1),
            good.replacen("not_in_hw", "skip_sw", 1),
            good.replacen("ht divisor 1", "ht divisor 2", 1),
            good.replacen("bkt 0", "bkt 1", 1),
            good.replacen("random type none", "random type netrand", 1),
            good.replacen("index 1 ref 1 bind 1", "index 1 ref 1 bind 1 link 900:", 1),
            good.replacen(
                "action order 1:",
                "match 00000000/ffffffff at 12\naction order 1:",
                1,
            ),
            format!("{good}{good}"),
            good[..good.len() / 2].to_owned(),
        ] {
            assert!(plan.verify_readback(&bad).is_err(), "accepted bad readback");
        }
    }
    #[test]
    fn render_counts_and_range_masks_are_exact() {
        for (lo, hi) in [(0, 65535), (49200, 49299), (53, 53), (1, 65534)] {
            let masks = range_masks(lo, hi, 16);
            for port in 0..=65535 {
                assert_eq!(
                    masks.iter().filter(|(v, m)| port & m == *v).count(),
                    usize::from((lo..=hi).contains(&port))
                );
            }
        }
        let plan = plan();
        let (text, count) = plan.plan_stdin();
        assert_eq!(text.lines().count(), count);
        assert!(text.lines().skip(1).all(|l| l.contains(" u32 match u32 ")));
        assert_eq!(plan.rules.len() + 1, count);
    }
    #[test]
    fn fallback_only_for_missing_classifier_and_isolated_probes() {
        let missing = "exit 3: Error: TC classifier not found.".to_owned();
        assert_eq!(
            select_classifier(Ok(()), None).unwrap(),
            IfaceClassifier::Flower
        );
        assert_eq!(
            select_classifier(Err(missing.clone()), Some(Ok(()))).unwrap(),
            IfaceClassifier::U32
        );
        let error =
            select_classifier(Err(missing), Some(Err("u32 unavailable".into()))).unwrap_err();
        assert!(error.contains("tried flower and u32"));
        assert!(select_classifier(Err("Operation not permitted".into()), Some(Ok(()))).is_err());
        let argv = classifier_probe_argv("job", "image");
        assert!(argv.windows(2).any(|w| w == ["--network", "none"]));
        assert!(!argv.iter().any(|a| a.starts_with("container:")));
    }
}
