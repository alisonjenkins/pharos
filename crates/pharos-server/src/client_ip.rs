//! Which address a request really came from, for rate limiting.
//!
//! `X-Forwarded-For` is whatever the client wrote, so it is believed only when
//! the direct TCP peer is a configured trusted proxy (`[server].trusted_proxies`).
//! Behind an ingress every client otherwise shares the proxy's address and
//! one bad actor throttles everybody.

use std::net::{IpAddr, SocketAddr};

/// Configured trusted proxies: single addresses and CIDR ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies(Vec<(IpAddr, u8)>);

impl TrustedProxies {
    /// Parse `"10.0.0.0/8"`, `"fd00::/8"` or a bare `"192.168.1.5"` (a host
    /// route). The error names the offending entry so a typo fails boot loudly.
    pub fn parse(entries: &[String]) -> Result<Self, String> {
        entries
            .iter()
            .map(|raw| {
                parse_cidr(raw.trim()).ok_or_else(|| format!("invalid trusted proxy {raw:?}"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.iter().any(|&(net, prefix)| in_net(ip, net, prefix))
    }
}

fn max_prefix(ip: IpAddr) -> u8 {
    if ip.is_ipv4() {
        32
    } else {
        128
    }
}

fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = match s.split_once('/') {
        Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
        None => (s.parse::<IpAddr>().ok()?, None),
    };
    let addr = addr.to_canonical();
    let prefix = prefix.unwrap_or_else(|| max_prefix(addr));
    (prefix <= max_prefix(addr)).then_some((addr, prefix))
}

fn in_net(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    let (a, b, bits) = match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => (u128::from(u32::from(a)), u128::from(u32::from(b)), 32),
        (IpAddr::V6(a), IpAddr::V6(b)) => (u128::from(a), u128::from(b), 128),
        _ => return false,
    };
    let host_bits = u32::from(bits - prefix);
    // A /0 over IPv6 shifts by the full width, which `checked_shr` reports as
    // None on both sides: equal, so everything matches.
    a.checked_shr(host_bits) == b.checked_shr(host_bits)
}

fn parse_hop(s: &str) -> Option<IpAddr> {
    let s = s.trim();
    s.parse::<IpAddr>()
        .or_else(|_| s.parse::<SocketAddr>().map(|sa| sa.ip()))
        .ok()
        .map(|ip| ip.to_canonical())
}

/// The client address for a request from `peer` carrying the given
/// `X-Forwarded-For` value(s).
///
/// An untrusted peer is the client; its header is ignored. For a trusted peer
/// the chain is read from the right: each hop that is itself a trusted proxy
/// is skipped and the first one that is not is the client. Reading from the
/// left would let a client prepend a forged address that a proxy dutifully
/// appends to. A malformed hop, or a chain made only of trusted proxies,
/// falls back to the peer rather than guessing.
pub fn resolve_client_ip(peer: IpAddr, xff: Option<&str>, trusted: &TrustedProxies) -> IpAddr {
    let peer = peer.to_canonical();
    if !trusted.contains(peer) {
        return peer;
    }
    let Some(xff) = xff else {
        return peer;
    };
    for hop in xff.rsplit(',') {
        match parse_hop(hop) {
            None => return peer,
            Some(ip) if trusted.contains(ip) => continue,
            Some(ip) => return ip,
        }
    }
    peer
}

/// [`resolve_client_ip`] for an actix request. A request with no peer address
/// (only in-process test requests) is attributed to `0.0.0.0`.
pub fn client_ip(req: &actix_web::HttpRequest, trusted: &TrustedProxies) -> IpAddr {
    let peer = req
        .peer_addr()
        .map_or(IpAddr::from([0, 0, 0, 0]), |sa| sa.ip());
    let xff = req
        .headers()
        .get_all("x-forwarded-for")
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    resolve_client_ip(peer, (!xff.is_empty()).then_some(xff.as_str()), trusted)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn proxies(list: &[&str]) -> TrustedProxies {
        TrustedProxies::parse(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn trusted_peer_with_xff_resolves_to_the_client() {
        let t = proxies(&["10.0.0.0/8"]);
        assert_eq!(
            resolve_client_ip(ip("10.1.2.3"), Some("203.0.113.9"), &t),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn a_forged_leading_hop_is_ignored_because_the_chain_is_read_from_the_right() {
        let t = proxies(&["10.0.0.0/8"]);
        // The client sent "XFF: 1.1.1.1"; the proxy appended the real address.
        assert_eq!(
            resolve_client_ip(ip("10.1.2.3"), Some("1.1.1.1, 203.0.113.9"), &t),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn trusted_hops_inside_the_chain_are_skipped() {
        let t = proxies(&["10.0.0.0/8", "192.0.2.7"]);
        assert_eq!(
            resolve_client_ip(ip("10.0.0.1"), Some("203.0.113.9, 192.0.2.7, 10.0.0.5"), &t),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn untrusted_peer_with_xff_resolves_to_the_peer() {
        let t = proxies(&["10.0.0.0/8"]);
        assert_eq!(
            resolve_client_ip(ip("198.51.100.4"), Some("203.0.113.9"), &t),
            ip("198.51.100.4")
        );
        assert_eq!(
            resolve_client_ip(
                ip("198.51.100.4"),
                Some("203.0.113.9"),
                &TrustedProxies::default()
            ),
            ip("198.51.100.4"),
            "no proxies configured: header never believed"
        );
    }

    #[test]
    fn no_xff_resolves_to_the_peer() {
        let t = proxies(&["10.0.0.0/8"]);
        assert_eq!(resolve_client_ip(ip("10.1.2.3"), None, &t), ip("10.1.2.3"));
    }

    #[test]
    fn malformed_or_all_trusted_chains_resolve_to_the_peer() {
        let t = proxies(&["10.0.0.0/8"]);
        for bad in [
            "",
            "garbage",
            "203.0.113.9, not-an-ip",
            "10.0.0.2, 10.0.0.3",
        ] {
            assert_eq!(
                resolve_client_ip(ip("10.1.2.3"), Some(bad), &t),
                ip("10.1.2.3"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn hops_may_carry_ports_and_ipv6_and_mapped_addresses() {
        let t = proxies(&["10.0.0.0/8", "fd00::/8"]);
        assert_eq!(
            resolve_client_ip(ip("10.1.2.3"), Some("203.0.113.9:5555"), &t),
            ip("203.0.113.9")
        );
        assert_eq!(
            resolve_client_ip(ip("fd00::1"), Some("2001:db8::7"), &t),
            ip("2001:db8::7")
        );
        assert_eq!(
            resolve_client_ip(ip("::ffff:10.1.2.3"), Some("203.0.113.9"), &t),
            ip("203.0.113.9"),
            "an IPv4 peer seen on a dual-stack socket still matches its CIDR"
        );
    }

    #[test]
    fn cidr_membership_edges() {
        let t = proxies(&["10.0.0.0/8", "192.168.1.5", "0.0.0.0/0"]);
        assert!(t.contains(ip("10.255.255.255")));
        assert!(t.contains(ip("192.168.1.5")));
        assert!(t.contains(ip("8.8.8.8")), "/0 matches every IPv4 address");
        assert!(!t.contains(ip("2001:db8::1")), "families do not cross");
        let t = proxies(&["10.0.0.0/8", "192.168.1.5"]);
        assert!(!t.contains(ip("11.0.0.0")));
        assert!(!t.contains(ip("192.168.1.6")));
    }

    #[test]
    fn bad_entries_are_rejected_naming_the_entry() {
        for bad in ["nope", "10.0.0.0/33", "10.0.0.0/x", "fd00::/129", ""] {
            let err = TrustedProxies::parse(&[bad.to_string()]).unwrap_err();
            assert!(err.contains(&format!("{bad:?}")), "{err}");
        }
    }
}
