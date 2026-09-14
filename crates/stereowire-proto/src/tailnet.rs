//! Recognising Tailscale addresses.
//!
//! `stereowire` sends plain UDP and carries no encryption of its own, because it runs
//! inside a WireGuard tunnel. The sender checks that rather than assuming it:
//! outside the tailnet the screen and audio go out in the clear, and the link
//! accepts packets from anyone who finds the port.

use std::net::{IpAddr, Ipv4Addr};

/// Tailscale assigns node addresses from the 100.64.0.0/10 shared-address
/// space reserved for carrier-grade NAT.
const TAILSCALE_V4: (Ipv4Addr, u32) = (Ipv4Addr::new(100, 64, 0, 0), 10);

/// Tailscale's IPv6 range.
const TAILSCALE_V6_PREFIX: u16 = 0xfd7a;

/// Whether an address looks like a Tailscale node, and so is inside the tunnel.
pub fn is_tailnet_address(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let (base, bits) = TAILSCALE_V4;
            let mask = u32::MAX << (32 - bits);
            v4.to_bits() & mask == base.to_bits() & mask
        }
        // Tailscale's ULA range begins fd7a:115c:a1e0.
        IpAddr::V6(v6) => v6.segments()[0] == TAILSCALE_V6_PREFIX,
    }
}

/// Loopback is exempt: it never leaves the machine, and is how the self-test
/// and any local experiment run.
pub fn is_protected(addr: IpAddr) -> bool {
    addr.is_loopback() || is_tailnet_address(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("valid address")
    }

    #[test]
    fn tailscale_addresses_are_recognised() {
        for addr in [
            "100.64.0.1",
            "100.100.100.100",
            "100.127.255.254",
            "100.79.3.21",
        ] {
            assert!(
                is_tailnet_address(ip(addr)),
                "{addr} is inside 100.64.0.0/10"
            );
        }
        assert!(is_tailnet_address(ip("fd7a:115c:a1e0::1")));
    }

    #[test]
    fn ordinary_addresses_are_not_mistaken_for_the_tailnet() {
        // 100.63.x and 100.128.x sit just outside the range, and are the
        // obvious places for an off-by-one to hide.
        for addr in [
            "100.63.255.255",
            "100.128.0.0",
            "8.8.8.8",
            "192.168.1.10",
            "10.0.0.1",
            "172.16.0.1",
            "203.0.113.5",
        ] {
            assert!(
                !is_tailnet_address(ip(addr)),
                "{addr} must not read as a tailnet address"
            );
        }
        assert!(!is_tailnet_address(ip("2001:4860:4860::8888")));
    }

    #[test]
    fn loopback_counts_as_protected_but_a_lan_address_does_not() {
        assert!(is_protected(ip("127.0.0.1")));
        assert!(is_protected(ip("::1")));
        // LAN traffic travels in the clear.
        assert!(!is_protected(ip("192.168.1.50")));
    }
}
