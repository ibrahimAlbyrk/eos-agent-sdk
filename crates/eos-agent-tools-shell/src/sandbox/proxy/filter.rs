//! Which hosts a sandboxed command may reach: Claude Code's host patterns
//! (`A7e`, `Qwr`), its filter order (`mI`) and the address guard applied
//! after the proxy resolves a name (sandbox-runtime's `lookupFor`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// What the static lists say about a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    Deny(&'static str),
    /// Nothing decides it: ask, if anyone can be asked.
    Unlisted,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HostPolicy {
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
    pub strict: bool,
    /// Dial public addresses only: no loopback, no private network, even
    /// for a listed address (a confined host command).
    pub public_only: bool,
}

impl HostPolicy {
    /// Malformed → deny; on the deny list → deny; on the allow list → allow;
    /// strict allowlist → deny; else unlisted.
    pub(crate) fn judge(&self, host: &str, port: u16) -> Verdict {
        if !well_formed(host) {
            return Verdict::Deny("malformed host");
        }
        let host = canonical(host);
        if self.denied.iter().any(|p| matches(&host, port, p)) {
            return Verdict::Deny("host is on the deny list");
        }
        if self.allowed.iter().any(|p| matches(&host, port, p)) {
            return Verdict::Allow;
        }
        if self.strict {
            return Verdict::Deny("host is not on the allow list");
        }
        Verdict::Unlisted
    }

    /// The IP literals (and CIDR ranges) of a list, for the address guard.
    fn ranges(list: &[String]) -> Vec<(Range, Option<u16>)> {
        list.iter()
            .filter_map(|p| {
                let (host, port) = split_port(p);
                Some((Range::parse(&host)?, port))
            })
            .collect()
    }

    /// `None` when the proxy may dial `addr` for `host`, else why not.
    pub(crate) fn guard(&self, host: &str, addr: IpAddr, port: u16, local: &[IpAddr]) -> Option<&'static str> {
        if self.public_only {
            let addr = unmap(addr);
            return special(addr)
                .or_else(|| private(addr))
                .or_else(|| local.contains(&addr).then_some("one of this host's addresses"));
        }
        if canonical(host).parse::<IpAddr>().is_ok() {
            return None; // An address asked for by name was judged as such.
        }
        let hit = |list: &[String]| {
            Self::ranges(list).iter().any(|(r, p)| p.is_none_or(|p| p == port) && r.contains(unmap(addr)))
        };
        if hit(&self.denied) {
            return Some("a deny-listed address");
        }
        if hit(&self.allowed) {
            return None;
        }
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host == "localhost" || host.ends_with(".localhost") {
            return (!unmap(addr).is_loopback()).then_some("a non-loopback address");
        }
        let addr = unmap(addr);
        if let Some(why) = special(addr) {
            return Some(why);
        }
        local.contains(&addr).then_some("one of this host's addresses")
    }
}

/// `*` matches every host; `*.x.com` its strict subdomains (never the apex,
/// never an address); anything else the host exactly, case-insensitively.
/// A pattern may end in `:port`.
pub(crate) fn matches(host: &str, port: u16, pattern: &str) -> bool {
    let (pattern, want) = split_port(pattern);
    if want.is_some_and(|w| w != port) {
        return false;
    }
    let host = host.to_ascii_lowercase();
    if pattern == "*" {
        return true;
    }
    if let Some(domain) = pattern.strip_prefix("*.") {
        if host.parse::<IpAddr>().is_ok() {
            return false;
        }
        return host.ends_with(&format!(".{}", domain.to_ascii_lowercase()));
    }
    host == pattern.to_ascii_lowercase()
}

/// `host:port`, `[v6]:port`, `[v6]`, a bare v6 address or a bare host.
fn split_port(pattern: &str) -> (String, Option<u16>) {
    if let Some(rest) = pattern.strip_prefix('[') {
        let Some(end) = rest.find(']') else { return (pattern.to_owned(), None) };
        let host = canonical(&rest[..end]);
        let tail = &rest[end + 1..];
        return match tail.strip_prefix(':').and_then(|p| p.parse().ok()) {
            Some(port) => (host, Some(port)),
            None if tail.is_empty() => (host, None),
            None => (pattern.to_owned(), None),
        };
    }
    match pattern.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (host.to_owned(), Some(port)),
            Err(_) => (pattern.to_owned(), None),
        },
        Some(_) => (canonical(pattern), None),
        None => (pattern.to_owned(), None),
    }
}

/// A host pattern without its port (`EDn`): `api.x.com:443` → `api.x.com`.
pub(crate) fn host_of(pattern: &str) -> String {
    split_port(pattern).0
}

/// An address in its usual spelling (`[::1]` → `::1`); names unchanged.
pub(crate) fn canonical(host: &str) -> String {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    bare.parse::<IpAddr>().map_or_else(|_| host.to_owned(), |ip| ip.to_string())
}

/// A host name or address the proxy will dial: letters, digits, `-`, `_`,
/// `.`, or an IP literal; at most 253 chars.
pub(crate) fn well_formed(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if canonical(host).parse::<IpAddr>().is_ok() {
        return true;
    }
    host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && !host.starts_with('.')
        && !host.contains("..")
}

fn unmap(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        v4 => v4,
    }
}

/// Addresses never dialed for a name: loopback, unspecified, link-local,
/// multicast, broadcast and cloud metadata endpoints.
fn special(addr: IpAddr) -> Option<&'static str> {
    const METADATA: &[&str] = &[
        "100.100.100.200",
        "168.63.129.16",
        "192.0.0.192",
        "fd20:ce::254",
        "fd00:c1::a9fe:a9fe",
        "fd00:42::42",
        "fd00:a9fe:a9fe::1",
        "fd00:100::100:200",
    ];
    match addr {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                Some("a loopback address")
            } else if v4.octets()[0] == 0 {
                Some("an unspecified address")
            } else if v4.is_link_local() {
                Some("a link-local address")
            } else if v4.is_multicast() {
                Some("a multicast address")
            } else if v4.is_broadcast() {
                Some("the broadcast address")
            } else if METADATA.iter().any(|m| m.parse::<Ipv4Addr>().is_ok_and(|m| m == v4)) {
                Some("a cloud metadata address")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            if v6.is_loopback() {
                Some("a loopback address")
            } else if v6.is_unspecified() {
                Some("an unspecified address")
            } else if seg[0] & 0xffc0 == 0xfe80 {
                Some("a link-local address")
            } else if v6.is_multicast() {
                Some("a multicast address")
            } else if (seg[0] == 0xfd00 && seg[1] == 0x0ec2)
                || METADATA.iter().any(|m| m.parse::<Ipv6Addr>().is_ok_and(|m| m == v6))
            {
                Some("a cloud metadata address")
            } else {
                None
            }
        }
    }
}

/// Addresses of private networks: the LAN, carrier-grade NAT, unique local IPv6.
fn private(addr: IpAddr) -> Option<&'static str> {
    let private = match addr {
        IpAddr::V4(v4) => v4.is_private() || (v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64),
        IpAddr::V6(v6) => v6.is_unique_local(),
    };
    private.then_some("a private network address")
}

/// One address or a CIDR range.
#[derive(Debug, Clone, Copy)]
struct Range {
    base: IpAddr,
    bits: u8,
}

impl Range {
    fn parse(text: &str) -> Option<Self> {
        let (addr, bits) = match text.split_once('/') {
            Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
            None => (text, None),
        };
        let base: IpAddr = canonical(addr).parse().ok()?;
        let max = if base.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        (bits <= max).then_some(Self { base: unmap(base), bits })
    }

    fn contains(&self, addr: IpAddr) -> bool {
        match (self.base, addr) {
            (IpAddr::V4(b), IpAddr::V4(a)) => prefix_eq(&b.octets(), &a.octets(), self.bits),
            (IpAddr::V6(b), IpAddr::V6(a)) => prefix_eq(&b.octets(), &a.octets(), self.bits),
            _ => false,
        }
    }
}

fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
    let bits = usize::from(bits);
    let (full, rest) = (bits / 8, bits % 8);
    if a[..full] != b[..full] {
        return false;
    }
    rest == 0 || (a[full] ^ b[full]) >> (8 - rest) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_patterns_follow_claude_code() {
        assert!(matches("GitHub.com", 443, "github.com"));
        assert!(!matches("api.github.com", 443, "github.com"));
        assert!(matches("api.github.com", 443, "*.github.com"));
        assert!(!matches("github.com", 443, "*.github.com"), "not the apex");
        assert!(!matches("1.2.3.4", 443, "*.3.4"), "never an address");
        assert!(matches("anything", 1, "*"));
        assert!(matches("127.0.0.1", 8080, "127.0.0.1:8080"));
        assert!(!matches("127.0.0.1", 8081, "127.0.0.1:8080"));
        assert!(matches("::1", 80, "[::1]:80"));
        assert!(matches("::1", 80, "[0:0::1]"));
    }

    #[test]
    fn filter_order() {
        let policy = HostPolicy {
            allowed: vec!["*.example.com".into(), "bad.example.com".into()],
            denied: vec!["bad.example.com".into()],
            ..HostPolicy::default()
        };
        assert_eq!(policy.judge("a.example.com", 443), Verdict::Allow);
        assert_eq!(policy.judge("bad.example.com", 443), Verdict::Deny("host is on the deny list"));
        assert_eq!(policy.judge("other.org", 443), Verdict::Unlisted);
        assert_eq!(policy.judge("bad host", 443), Verdict::Deny("malformed host"));
        let strict = HostPolicy { strict: true, ..policy };
        assert_eq!(strict.judge("other.org", 443), Verdict::Deny("host is not on the allow list"));
    }

    #[test]
    fn the_guard_stops_names_that_resolve_to_local_services() {
        let policy = HostPolicy { allowed: vec!["10.0.0.0/8".into()], ..HostPolicy::default() };
        let ip = |s: &str| s.parse::<IpAddr>().expect("ip");
        let local = [ip("192.168.1.20")];
        assert_eq!(policy.guard("evil.com", ip("127.0.0.1"), 80, &local), Some("a loopback address"));
        assert_eq!(policy.guard("evil.com", ip("169.254.169.254"), 80, &local), Some("a link-local address"));
        assert_eq!(policy.guard("evil.com", ip("::ffff:127.0.0.1"), 80, &local), Some("a loopback address"));
        assert_eq!(policy.guard("evil.com", ip("192.168.1.20"), 80, &local), Some("one of this host's addresses"));
        assert_eq!(policy.guard("lan.com", ip("192.168.1.30"), 80, &local), None, "the LAN is not refused");
        assert_eq!(policy.guard("intra.com", ip("10.1.2.3"), 80, &local), None);
        assert_eq!(policy.guard("localhost", ip("127.0.0.1"), 80, &local), None);
        assert_eq!(policy.guard("localhost", ip("8.8.8.8"), 80, &local), Some("a non-loopback address"));
        assert_eq!(policy.guard("127.0.0.1", ip("127.0.0.1"), 80, &local), None, "an address, judged as such");
        assert_eq!(policy.guard("good.com", ip("93.184.216.34"), 443, &local), None);
    }

    #[test]
    fn a_public_only_guard_stops_loopback_and_private_networks_even_when_listed() {
        let policy = HostPolicy {
            allowed: vec!["localhost".into(), "127.0.0.1".into(), "10.0.0.0/8".into()],
            public_only: true,
            ..HostPolicy::default()
        };
        let ip = |s: &str| s.parse::<IpAddr>().expect("ip");
        let local = [ip("203.0.113.9")];
        assert_eq!(policy.guard("localhost", ip("127.0.0.1"), 80, &local), Some("a loopback address"));
        assert_eq!(policy.guard("127.0.0.1", ip("127.0.0.1"), 80, &local), Some("a loopback address"));
        assert_eq!(policy.guard("intra.com", ip("10.1.2.3"), 80, &local), Some("a private network address"));
        for lan in ["192.168.1.30", "172.20.0.1", "100.64.0.1", "fd12::1", "::ffff:192.168.0.1"] {
            assert_eq!(policy.guard("lan.com", ip(lan), 80, &local), Some("a private network address"), "{lan}");
        }
        assert_eq!(policy.guard("me.com", ip("203.0.113.9"), 80, &local), Some("one of this host's addresses"));
        assert_eq!(policy.guard("good.com", ip("93.184.216.34"), 443, &local), None);
        assert_eq!(policy.guard("good.com", ip("100.128.0.1"), 443, &local), None, "outside carrier-grade NAT");
    }
}
