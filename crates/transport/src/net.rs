use std::net::IpAddr;

/// True for addresses that can only belong to the local network: RFC 1918, CGNAT-free private
/// ranges, link-local, IPv6 unique-local, and loopback. The host refuses everything else.
pub fn is_local_network(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        for ok in ["192.168.1.20", "10.0.0.5", "172.16.3.4", "169.254.1.1", "127.0.0.1", "::1", "fd00::1", "fe80::1", "::ffff:192.168.0.2"] {
            assert!(is_local_network(ok.parse().unwrap()), "{ok}");
        }
        for bad in ["8.8.8.8", "172.32.0.1", "2001:4860::8888", "::ffff:1.1.1.1"] {
            assert!(!is_local_network(bad.parse().unwrap()), "{bad}");
        }
    }
}
