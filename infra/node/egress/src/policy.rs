//! What the proxy lets through: allowlisted names, public addresses only.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// "api.example.com", "*.example.org" (subdomains only).
pub fn parse_allow(list: &str) -> Vec<String> {
    list.split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect()
}

pub fn allowed(host: &str, allow: &[String]) -> bool {
    let h = host.to_ascii_lowercase();
    let h = h.strip_suffix('.').unwrap_or(&h);
    allow.iter().any(|a| match a.strip_prefix('*') {
        // "*.example.org": h ends with ".example.org" and has a label before it.
        Some(suffix) if suffix.starts_with('.') => h.len() > suffix.len() && h.ends_with(suffix),
        _ => h == a,
    })
}

/// A host name as the proxy accepts one: letters, digits, dots and hyphens,
/// never an IP address (an allowlist of names says nothing about raw IPs).
pub fn host_name(host: &str) -> bool {
    (1..=253).contains(&host.len())
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && host.parse::<IpAddr>().is_err()
}

const BLOCKED_V4: [(Ipv4Addr, u8); 15] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

const BLOCKED_V6: [(Ipv6Addr, u8); 11] = [
    // ::, ::1 and the deprecated IPv4-compatible ::a.b.c.d.
    (Ipv6Addr::UNSPECIFIED, 96),
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

fn in_net(addr: u128, net: u128, prefix: u8, bits: u32) -> bool {
    // No entry has prefix 0, which would shift by the full width.
    let shift = bits - u32::from(prefix);
    (addr >> shift) == (net >> shift)
}

/// Whether an address is on the public internet: everything else (the LAN,
/// the node, the control plane, documentation and special ranges) is refused.
pub fn is_public(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(a) => !BLOCKED_V4
            .iter()
            .any(|(n, p)| in_net(u128::from(u32::from(a)), u128::from(u32::from(*n)), *p, 32)),
        // IPv4-mapped (::ffff:10.0.0.1): judge the IPv4 address.
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => is_public(IpAddr::V4(v4)),
            None => !BLOCKED_V6
                .iter()
                .any(|(n, p)| in_net(u128::from(a), u128::from(*n), *p, 128)),
        },
    }
}

/// "http://host[:port]/path?query" of a plain-HTTP proxy request: the host,
/// the port and the origin-form target. No user info, http only.
pub fn absolute_http(uri: &str) -> Option<(String, u16, String)> {
    let rest = uri
        .get(..7)
        .filter(|s| s.eq_ignore_ascii_case("http://"))
        .map(|_| &uri[7..])?;
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    if authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (authority, 80),
    };
    if !host_name(host) {
        return None;
    }
    let path = match path {
        "" => "/".to_owned(),
        p if p.starts_with('?') => format!("/{p}"),
        p => p.to_owned(),
    };
    Some((host.to_ascii_lowercase(), port, path))
}

/// "host:port" of a CONNECT request.
pub fn connect_target(target: &str) -> Option<(String, u16)> {
    let (host, port) = target.rsplit_once(':')?;
    if port.is_empty()
        || port.len() > 5
        || !port.bytes().all(|b| b.is_ascii_digit())
        || !host_name(host)
    {
        return None;
    }
    Some((host.to_ascii_lowercase(), port.parse().ok()?))
}

/// A string as a JSON string literal, for the log.
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars().take(255) {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_exact_names_and_subdomain_wildcards() {
        let allow = parse_allow("api.example.com, *.example.org");
        assert!(allowed("api.example.com", &allow));
        assert!(allowed("API.example.com.", &allow));
        assert!(allowed("a.b.example.org", &allow));
        assert!(!allowed("example.org", &allow));
        assert!(!allowed("evilexample.org", &allow));
        assert!(!allowed("api.example.com.evil.net", &allow));
        assert!(!allowed("anything", &[]));
    }

    #[test]
    fn only_public_addresses() {
        for a in [
            "10.1.2.3",
            "192.168.1.1",
            "172.20.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "::ffff:a00:1",
            "0.0.0.0",
            "::",
            "::a00:1",
            "224.0.0.1",
            "64:ff9b::a00:1",
            "2002::1",
        ] {
            assert!(!is_public(a.parse().unwrap()), "{a}");
        }
        for a in [
            "1.1.1.1",
            "8.8.8.8",
            "2606:4700:4700::1111",
            "172.32.0.1",
            "100.128.0.1",
        ] {
            assert!(is_public(a.parse().unwrap()), "{a}");
        }
    }

    #[test]
    fn targets() {
        assert_eq!(
            connect_target("api.example.com:443"),
            Some(("api.example.com".into(), 443))
        );
        assert_eq!(connect_target("10.0.0.1:443"), None);
        assert_eq!(connect_target("[::1]:443"), None);
        assert_eq!(connect_target("a b:443"), None);
        assert_eq!(connect_target("host:99999"), None);
        assert_eq!(connect_target("host:"), None);
        assert_eq!(
            absolute_http("http://Example.com/a?b=1"),
            Some(("example.com".into(), 80, "/a?b=1".into()))
        );
        assert_eq!(
            absolute_http("http://example.com"),
            Some(("example.com".into(), 80, "/".into()))
        );
        assert_eq!(
            absolute_http("http://example.com?x"),
            Some(("example.com".into(), 80, "/?x".into()))
        );
        assert_eq!(
            absolute_http("http://example.com:8080/"),
            Some(("example.com".into(), 8080, "/".into()))
        );
        assert_eq!(absolute_http("http://user@example.com/"), None);
        assert_eq!(absolute_http("https://example.com/"), None);
        assert_eq!(absolute_http("http://127.0.0.1/"), None);
        assert_eq!(absolute_http("/"), None);
    }

    #[test]
    fn log_strings() {
        assert_eq!(json_str("a\"b\\\n"), r#""a\"b\\\u000a""#);
    }
}
