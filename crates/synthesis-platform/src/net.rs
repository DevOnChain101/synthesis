//! Which IP addresses an agent-driven connection must never reach (SSRF / egress defence).
//! One list for WebFetch, the browser and the egress proxy.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Loopback, private (RFC1918), CGNAT, link-local (incl. the 169.254.169.254 metadata address),
/// documentation, benchmarking, multicast, reserved and unspecified IPv4.
pub fn blocked_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 192 && o[1] == 0 && o[2] == 2) // documentation (TEST-NET-1)
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 6to4 relay anycast (deprecated)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100) // documentation (TEST-NET-2)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113) // documentation (TEST-NET-3)
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking
        || o[0] >= 240 // reserved
}

/// IPv6: loopback, ULA, link/site-local, multicast, Teredo, and the IPv4 forms that embed a
/// blocked IPv4 address (mapped, compatible, NAT64, 6to4).
pub fn blocked_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4() {
        return blocked_v4(v4); // ::ffff:a.b.c.d and ::a.b.c.d
    }
    let s = ip.segments();
    let embedded = |a: u16, b: u16| {
        blocked_v4(Ipv4Addr::new(
            (a >> 8) as u8,
            a as u8,
            (b >> 8) as u8,
            b as u8,
        ))
    };
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // ULA
        || (s[0] & 0xffc0) == 0xfe80 // link-local
        || (s[0] & 0xffc0) == 0xfec0 // site-local
        || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] && embedded(s[6], s[7])) // NAT64
        || (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1) // local-use NAT64 (64:ff9b:1::/48)
        || (s[0] == 0x100 && s[1..4] == [0; 3]) // discard-only (100::/64)
        || (s[0] == 0x2001 && s[1] == 0xdb8) // documentation (2001:db8::/32)
        || (s[0] == 0x2002 && embedded(s[1], s[2])) // 6to4
        || (s[0] == 0x2001 && s[1] == 0) // Teredo: embeds a v4 server and an obfuscated client
}

pub fn ip_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => blocked_v6(v6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        for (ip, blocked) in [
            ("10.0.0.1", true),
            ("172.16.0.1", true),
            ("192.168.1.1", true),
            ("100.64.0.1", true),
            ("169.254.169.254", true),
            ("127.0.0.1", true),
            ("192.0.2.1", true),
            ("0.0.0.0", true),
            ("8.8.8.8", false),
            ("1.1.1.1", false),
            ("::1", true),
            ("fd00::1", true),
            ("fe80::1", true),
            ("::ffff:127.0.0.1", true),
            ("::ffff:169.254.169.254", true),
            ("::ffff:8.8.8.8", false),
            ("64:ff9b::a00:1", true),
            ("64:ff9b::5db8:d822", false),
            ("2002:0a00:0001::1", true),
            ("2002:5db8:d822::1", false),
            ("2001:0:4136:e378:8000:63bf:3fff:fdd2", true),
            ("2001:db8::1", true),
            ("64:ff9b:1::1", true),
            ("100::1", true),
            ("192.88.99.1", true),
            ("100:0:0:1::1", false),
            ("2606:4700::1111", false),
        ] {
            assert_eq!(ip_blocked(ip.parse().unwrap()), blocked, "{ip}");
        }
    }
}
