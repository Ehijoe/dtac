//! Which address a request really came from when DTAC runs behind reverse proxies.
//!
//! `X-Forwarded-For` is only believed when the TCP peer is a configured trusted proxy;
//! otherwise anyone could pick their own client IP and dodge the per-IP heuristics.
//! Behind trusted proxies, the client is the right-most address in the header that is
//! not itself a trusted proxy: every hop appends the address it received from, so the
//! entries to the left of that one were written by the client and can't be trusted.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// An IP network such as `10.0.0.0/8` or `::1/128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Whether `ip` is inside this network. IPv4-mapped IPv6 addresses count as IPv4.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                mask_v4(u32::from(ip), self.prefix) == u32::from(net)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                mask_v6(u128::from(ip), self.prefix) == u128::from(net)
            }
            _ => false,
        }
    }
}

const fn mask_v4(bits: u32, prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        bits & (u32::MAX << (32 - prefix))
    }
}

const fn mask_v6(bits: u128, prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        bits & (u128::MAX << (128 - prefix))
    }
}

/// Why a `--trusted-proxy` value didn't parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CidrError(String);

impl fmt::Display for CidrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CidrError {}

impl FromStr for Cidr {
    type Err = CidrError;

    /// `addr/prefix`, or a bare address for a single host. Host bits are cleared.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = s.split_once('/').map_or((s, None), |(a, p)| (a, Some(p)));
        let network: IpAddr = addr
            .parse()
            .map_err(|_| CidrError(format!("{s:?}: not an IP address or CIDR")))?;
        let network = network.to_canonical();
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|&p| p <= max)
                .ok_or_else(|| CidrError(format!("{s:?}: prefix must be 0..={max}")))?,
        };
        let network = match network {
            IpAddr::V4(v4) => IpAddr::V4(mask_v4(u32::from(v4), prefix).into()),
            IpAddr::V6(v6) => IpAddr::V6(mask_v6(u128::from(v6), prefix).into()),
        };
        Ok(Self { network, prefix })
    }
}

/// One `X-Forwarded-For` entry: an address, optionally with a port (`[v6]:port` for
/// IPv6). `None` for anything else, including `unknown` and obfuscated identifiers.
fn parse_hop(hop: &str) -> Option<IpAddr> {
    let hop = hop.trim();
    if let Ok(ip) = hop.parse::<IpAddr>() {
        return Some(ip.to_canonical());
    }
    if let Some(rest) = hop.strip_prefix('[') {
        let (addr, _port) = rest.split_once(']')?;
        return addr.parse::<IpAddr>().ok().map(|ip| ip.to_canonical());
    }
    let (addr, port) = hop.rsplit_once(':')?;
    port.parse::<u16>().ok()?;
    addr.parse::<std::net::Ipv4Addr>().ok().map(IpAddr::V4)
}

/// Whether `peer` is one of the `trusted` proxies, whose forwarded headers count.
pub fn is_trusted(peer: IpAddr, trusted: &[Cidr]) -> bool {
    trusted.iter().any(|cidr| cidr.contains(peer))
}

/// The client address for a request from TCP peer `peer` with the given
/// `X-Forwarded-For` header values (several headers are read as one list).
///
/// - Untrusted peer: the peer, whatever the header says.
/// - Trusted peer: the right-most hop that isn't a trusted proxy, or the left-most hop
///   if they all are.
/// - Missing or malformed header (any entry that isn't an address): the peer.
pub fn client_ip<'a>(
    peer: IpAddr,
    forwarded_for: impl IntoIterator<Item = &'a str>,
    trusted: &[Cidr],
) -> IpAddr {
    let peer = peer.to_canonical();
    if !is_trusted(peer, trusted) {
        return peer;
    }
    let hops: Option<Vec<IpAddr>> = forwarded_for
        .into_iter()
        .flat_map(|header| header.split(','))
        .map(parse_hop)
        .collect();
    let Some(hops) = hops.filter(|hops| !hops.is_empty()) else {
        return peer;
    };
    hops.iter()
        .rev()
        .copied()
        .find(|&ip| !is_trusted(ip, trusted))
        .unwrap_or(hops[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn cidrs(list: &[&str]) -> Vec<Cidr> {
        list.iter().map(|c| c.parse().unwrap()).collect()
    }

    #[test]
    fn cidr_parses_networks_and_hosts() {
        let net: Cidr = "10.1.2.3/8".parse().unwrap();
        assert!(net.contains(ip("10.200.0.1")));
        assert!(!net.contains(ip("11.0.0.1")));
        let host: Cidr = "127.0.0.1".parse().unwrap();
        assert!(host.contains(ip("127.0.0.1")));
        assert!(!host.contains(ip("127.0.0.2")));
        let v6: Cidr = "2001:db8::/32".parse().unwrap();
        assert!(v6.contains(ip("2001:db8:1::5")));
        assert!(!v6.contains(ip("10.0.0.1")));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(ip("203.0.113.9")));
        // IPv4-mapped IPv6 peers (dual-stack sockets) match IPv4 networks.
        assert!(host.contains(ip("::ffff:127.0.0.1")));
    }

    #[test]
    fn cidr_rejects_garbage() {
        for bad in [
            "",
            "10.0.0.0/33",
            "::/129",
            "10.0.0/8",
            "localhost",
            "10.0.0.0/x",
        ] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn untrusted_peer_is_the_client_whatever_the_header_says() {
        let trusted = cidrs(&["127.0.0.1/32"]);
        assert_eq!(
            client_ip(ip("198.51.100.7"), ["203.0.113.5"], &trusted),
            ip("198.51.100.7")
        );
    }

    #[test]
    fn trusted_peer_gives_the_right_most_untrusted_hop() {
        let trusted = cidrs(&["127.0.0.1/32", "10.0.0.0/8"]);
        // The client wrote 1.1.1.1 itself; 203.0.113.5 is what our edge proxy saw.
        let header = "1.1.1.1, 203.0.113.5, 10.0.0.2";
        assert_eq!(
            client_ip(ip("127.0.0.1"), [header], &trusted),
            ip("203.0.113.5")
        );
        // Several headers are one list, in order.
        assert_eq!(
            client_ip(
                ip("127.0.0.1"),
                ["1.1.1.1", "203.0.113.5, 10.0.0.2"],
                &trusted
            ),
            ip("203.0.113.5")
        );
    }

    #[test]
    fn trusted_peer_without_a_header_is_the_client() {
        let trusted = cidrs(&["127.0.0.1"]);
        assert_eq!(client_ip(ip("127.0.0.1"), [], &trusted), ip("127.0.0.1"));
        assert_eq!(client_ip(ip("127.0.0.1"), [""], &trusted), ip("127.0.0.1"));
    }

    #[test]
    fn malformed_header_falls_back_to_the_peer() {
        let trusted = cidrs(&["127.0.0.1"]);
        for header in [
            "unknown",
            "203.0.113.5, not-an-ip",
            "203.0.113.5,,",
            "_hidden",
        ] {
            assert_eq!(
                client_ip(ip("127.0.0.1"), [header], &trusted),
                ip("127.0.0.1"),
                "{header:?}"
            );
        }
    }

    #[test]
    fn ports_and_bracketed_ipv6_hops_are_understood() {
        let trusted = cidrs(&["127.0.0.1"]);
        assert_eq!(
            client_ip(ip("127.0.0.1"), ["203.0.113.5:4711"], &trusted),
            ip("203.0.113.5")
        );
        assert_eq!(
            client_ip(ip("127.0.0.1"), ["[2001:db8::1]:443"], &trusted),
            ip("2001:db8::1")
        );
        assert_eq!(
            client_ip(ip("127.0.0.1"), [" 2001:db8::2 "], &trusted),
            ip("2001:db8::2")
        );
    }

    #[test]
    fn all_trusted_hops_give_the_left_most() {
        let trusted = cidrs(&["10.0.0.0/8", "127.0.0.1"]);
        assert_eq!(
            client_ip(ip("127.0.0.1"), ["10.0.0.9, 10.0.0.2"], &trusted),
            ip("10.0.0.9")
        );
    }
}
