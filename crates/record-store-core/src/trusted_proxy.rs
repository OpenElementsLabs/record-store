//! Who is allowed to tell Record Store which client a request came from.
//!
//! `X-Forwarded-For` is written by whatever sent the request. Honouring it
//! unconditionally means an attacker chooses their own identity: they rotate
//! the header per request and every per-client limit — password attempts,
//! token probing — becomes a limit on a value they control, which is no limit
//! at all. It also means the address in the audit trail is whatever they typed.
//!
//! Ignoring it unconditionally is wrong too. Behind a reverse proxy the socket
//! address is the proxy for every visitor on earth, so the same limits collapse
//! onto one bucket and the trail records one address forever.
//!
//! The answer is neither default: an operator names the hops they run, and a
//! forwarding header is honoured only when the request actually arrived from
//! one of them. Nothing is trusted until it is named, so a deployment that has
//! not been configured is safe rather than convenient.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::CoreError;

/// The longest forwarding header this will look at.
///
/// A header is attacker-influenced even when the nearest hop is trusted — the
/// trusted hop appends to what it was given. Bounding the work keeps a long
/// chain from becoming a cheap way to spend server time.
const MAXIMUM_FORWARDED_BYTES: usize = 1_024;
/// The most hops walked while skipping trusted proxies.
const MAXIMUM_FORWARDED_HOPS: usize = 32;

/// One trusted address or CIDR block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Network {
    address: IpAddr,
    prefix: u8,
}

impl Network {
    fn parse(value: &str) -> Result<Self, CoreError> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => {
                let prefix: u8 = prefix.parse().map_err(|_| invalid(value))?;
                (address, Some(prefix))
            }
            None => (value, None),
        };
        let address: IpAddr = address.trim().parse().map_err(|_| invalid(value))?;
        let maximum = if address.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(maximum);
        if prefix > maximum {
            return Err(invalid(value));
        }
        Ok(Self { address, prefix })
    }

    fn contains(&self, candidate: IpAddr) -> bool {
        match (self.address, candidate) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                matches_prefix(&network.octets(), &candidate.octets(), self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                matches_prefix(&network.octets(), &candidate.octets(), self.prefix)
            }
            // An IPv4-mapped IPv6 peer is the same host as its IPv4 form, and a
            // proxy list written in one family must still match the other or an
            // operator's entry silently stops applying under a dual-stack
            // listener.
            (IpAddr::V4(_), IpAddr::V6(candidate)) => candidate
                .to_ipv4_mapped()
                .is_some_and(|mapped| self.contains(IpAddr::V4(mapped))),
            (IpAddr::V6(network), IpAddr::V4(candidate)) => {
                network.to_ipv4_mapped().is_some_and(|mapped| {
                    self.prefix >= 96
                        && matches_prefix(&mapped.octets(), &candidate.octets(), self.prefix - 96)
                })
            }
        }
    }
}

fn matches_prefix(network: &[u8], candidate: &[u8], prefix: u8) -> bool {
    let full = usize::from(prefix / 8);
    let remainder = prefix % 8;
    if network[..full] != candidate[..full] {
        return false;
    }
    if remainder == 0 {
        return true;
    }
    let mask = 0xFF_u8 << (8 - remainder);
    network[full] & mask == candidate[full] & mask
}

fn invalid(value: &str) -> CoreError {
    CoreError::InvalidIdentifier {
        kind: "trusted proxy",
        reason: format!("{value:?} is not an IP address or CIDR block"),
    }
}

/// The hops whose forwarding headers this deployment believes.
///
/// Empty by default, which means forwarding headers are ignored entirely and
/// the socket address is the client.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    networks: Vec<Network>,
}

impl TrustedProxies {
    /// Parses operator-supplied addresses and CIDR blocks.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> Result<Self, CoreError> {
        let networks = entries
            .iter()
            .map(|entry| Network::parse(entry.as_ref().trim()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { networks })
    }

    /// Returns whether any hop is trusted at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.networks.is_empty()
    }

    /// Returns whether one address is a hop this deployment runs.
    #[must_use]
    pub fn trusts(&self, address: IpAddr) -> bool {
        self.networks
            .iter()
            .any(|network| network.contains(address))
    }

    /// Resolves the address a request should be attributed to.
    ///
    /// The forwarding header is consulted only when the request arrived from a
    /// trusted hop. Within it the chain is walked from the right, discarding
    /// hops this deployment runs, because everything to the left of the last
    /// untrusted entry was written by somebody who could write anything.
    ///
    /// Returns `None` when there is nothing to attribute the request to, which
    /// a caller renders however it renders an unknown client.
    #[must_use]
    pub fn client_address(&self, peer: Option<IpAddr>, forwarded: Option<&str>) -> Option<IpAddr> {
        let peer = peer?;
        if self.networks.is_empty() || !self.trusts(peer) {
            return Some(peer);
        }
        let Some(forwarded) = forwarded.filter(|value| value.len() <= MAXIMUM_FORWARDED_BYTES)
        else {
            return Some(peer);
        };
        forwarded
            .split(',')
            .rev()
            .take(MAXIMUM_FORWARDED_HOPS)
            .filter_map(|hop| parse_hop(hop.trim()))
            .find(|hop| !self.trusts(*hop))
            .or(Some(peer))
    }
}

/// Parses one forwarding-chain entry, which may carry a port.
fn parse_hop(value: &str) -> Option<IpAddr> {
    if let Ok(address) = value.parse::<IpAddr>() {
        return Some(address);
    }
    // `[2001:db8::1]:443` and `203.0.113.7:443` both appear in the wild.
    if let Some(rest) = value.strip_prefix('[') {
        let (literal, _) = rest.split_once(']')?;
        return literal.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    let (address, _) = value.rsplit_once(':')?;
    address.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().expect("address")
    }

    /// The default is the safe one: an unconfigured deployment attributes a
    /// request to the socket it arrived on, whatever headers say.
    #[test]
    fn nothing_is_trusted_until_an_operator_names_it() {
        let proxies = TrustedProxies::default();
        assert!(proxies.is_empty());
        assert_eq!(
            proxies.client_address(Some(ip("198.51.100.4")), Some("203.0.113.9")),
            Some(ip("198.51.100.4")),
            "a header from an untrusted peer must not choose the client"
        );
    }

    /// A request that did not arrive from a named hop is attributed to its
    /// socket, even when the deployment does trust some other hop.
    #[test]
    fn a_header_from_an_unnamed_hop_is_ignored() {
        let proxies = TrustedProxies::parse(&["10.0.0.1"]).expect("parse");
        assert_eq!(
            proxies.client_address(Some(ip("198.51.100.4")), Some("203.0.113.9")),
            Some(ip("198.51.100.4"))
        );
    }

    #[test]
    fn a_header_from_a_named_hop_names_the_client() {
        let proxies = TrustedProxies::parse(&["10.0.0.1"]).expect("parse");
        assert_eq!(
            proxies.client_address(Some(ip("10.0.0.1")), Some("203.0.113.9")),
            Some(ip("203.0.113.9"))
        );
    }

    /// The chain is walked from the right. Everything left of the last
    /// untrusted hop was written by somebody who could write anything, so a
    /// spoofed prefix must not be able to choose the answer.
    #[test]
    fn a_spoofed_prefix_cannot_choose_the_client() {
        let proxies = TrustedProxies::parse(&["10.0.0.0/8"]).expect("parse");
        assert_eq!(
            proxies.client_address(Some(ip("10.0.0.7")), Some("1.2.3.4, 203.0.113.9, 10.0.0.9")),
            Some(ip("203.0.113.9")),
            "the last untrusted hop is the client; anything further left is forgeable"
        );
    }

    /// A chain made entirely of trusted hops names nobody, so the request is
    /// attributed to the hop it arrived from rather than to an invented value.
    #[test]
    fn a_chain_of_only_trusted_hops_falls_back_to_the_peer() {
        let proxies = TrustedProxies::parse(&["10.0.0.0/8"]).expect("parse");
        assert_eq!(
            proxies.client_address(Some(ip("10.0.0.7")), Some("10.0.0.2, 10.0.0.3")),
            Some(ip("10.0.0.7"))
        );
    }

    #[test]
    fn garbage_and_oversized_headers_fall_back_to_the_peer() {
        let proxies = TrustedProxies::parse(&["10.0.0.1"]).expect("parse");
        for header in ["not an address", "", "   ", "<script>"] {
            assert_eq!(
                proxies.client_address(Some(ip("10.0.0.1")), Some(header)),
                Some(ip("10.0.0.1")),
                "{header:?}"
            );
        }
        let oversized = "203.0.113.9, ".repeat(200);
        assert_eq!(
            proxies.client_address(Some(ip("10.0.0.1")), Some(&oversized)),
            Some(ip("10.0.0.1"))
        );
    }

    #[test]
    fn hops_may_carry_ports_and_ipv6_literals() {
        let proxies = TrustedProxies::parse(&["10.0.0.1"]).expect("parse");
        assert_eq!(
            proxies.client_address(Some(ip("10.0.0.1")), Some("203.0.113.9:51234")),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            proxies.client_address(Some(ip("10.0.0.1")), Some("[2001:db8::5]:443")),
            Some(ip("2001:db8::5"))
        );
        assert_eq!(
            proxies.client_address(Some(ip("10.0.0.1")), Some("2001:db8::5")),
            Some(ip("2001:db8::5"))
        );
    }

    #[test]
    fn cidr_blocks_match_by_prefix_in_both_families() {
        let proxies = TrustedProxies::parse(&["172.16.0.0/12", "2001:db8::/32"]).expect("parse");
        assert!(proxies.trusts(ip("172.16.0.1")));
        assert!(proxies.trusts(ip("172.31.255.254")));
        assert!(!proxies.trusts(ip("172.32.0.1")));
        assert!(proxies.trusts(ip("2001:db8:1234::1")));
        assert!(!proxies.trusts(ip("2001:db9::1")));
    }

    /// A dual-stack listener reports an IPv4 peer as an IPv4-mapped IPv6
    /// address. An operator's IPv4 entry has to keep matching it, or the
    /// allowlist silently stops applying the day the listener changes.
    #[test]
    fn an_ipv4_mapped_peer_matches_its_ipv4_entry() {
        let proxies = TrustedProxies::parse(&["10.0.0.1"]).expect("parse");
        assert!(proxies.trusts(ip("::ffff:10.0.0.1")));
    }

    #[test]
    fn malformed_entries_are_refused_at_configuration_time() {
        for entry in ["", "nonsense", "10.0.0.1/33", "10.0.0.1/-1", "10.0.0.256"] {
            assert!(
                TrustedProxies::parse(&[entry]).is_err(),
                "accepted {entry:?}"
            );
        }
    }
}
