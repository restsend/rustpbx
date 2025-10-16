use anyhow::Result;
use get_if_addrs::get_if_addrs;
use std::net::IpAddr;

pub fn get_first_non_loopback_interface() -> Result<IpAddr> {
    for i in get_if_addrs()? {
        if !i.is_loopback() {
            match i.addr {
                get_if_addrs::IfAddr::V4(ref addr) => return Ok(std::net::IpAddr::V4(addr.ip)),
                _ => continue,
            }
        }
    }
    Err(anyhow::anyhow!("No IPV4 interface found"))
}
/// Check if an IP address is private (RFC 1918, RFC 4193, loopback, etc.)
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            // RFC 1918 private addresses
            // 10.0.0.0/8
            if ipv4.octets()[0] == 10 {
                return true;
            }
            // 172.16.0.0/12
            if ipv4.octets()[0] == 172 && (ipv4.octets()[1] & 0xf0) == 16 {
                return true;
            }
            // 192.168.0.0/16
            if ipv4.octets()[0] == 192 && ipv4.octets()[1] == 168 {
                return true;
            }
            // 127.0.0.0/8 (loopback)
            if ipv4.octets()[0] == 127 {
                return true;
            }
            // 169.254.0.0/16 (link-local)
            if ipv4.octets()[0] == 169 && ipv4.octets()[1] == 254 {
                return true;
            }
            false
        }
        IpAddr::V6(ipv6) => {
            // RFC 4193 unique local addresses (fc00::/7)
            if (ipv6.octets()[0] & 0xfe) == 0xfc {
                return true;
            }
            // loopback (::1)
            if ipv6.is_loopback() {
                return true;
            }
            // link-local (fe80::/10)
            if (ipv6.octets()[0] == 0xfe) && ((ipv6.octets()[1] & 0xc0) == 0x80) {
                return true;
            }
            false
        }
    }
}

/// Parse SDP and extract RTP addresses
pub fn extract_rtp_addresses_from_sdp(sdp: &str) -> Result<Vec<IpAddr>> {
    let mut addresses = Vec::new();

    for line in sdp.lines() {
        if line.starts_with("c=") {
            // Connection information: c=<nettype> <addrtype> <connection-address>
            let parts: Vec<&str> = line.splitn(4, ' ').collect();
            if parts.len() >= 3 {
                if let Ok(addr) = parts[2].parse::<IpAddr>() {
                    addresses.push(addr);
                }
            }
        }
    }

    Ok(addresses)
}

/// Check if SDP contains any private IP addresses
pub fn sdp_contains_private_ip(sdp: &str) -> Result<bool> {
    let addresses = extract_rtp_addresses_from_sdp(sdp)?;
    Ok(addresses.iter().any(is_private_ip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_is_private_ip() {
        // Private IPv4 addresses
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));

        // Public IPv4 addresses
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));

        // Private IPv6 addresses
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfc00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn test_extract_rtp_addresses_from_sdp() {
        let sdp = "v=0\r\n\
                   o=- 1234567890 1234567890 IN IP4 192.168.1.100\r\n\
                   s=Call\r\n\
                   c=IN IP4 192.168.1.100\r\n\
                   t=0 0\r\n\
                   m=audio 49170 RTP/AVP 0 8 97\r\n";

        let addresses = extract_rtp_addresses_from_sdp(sdp).unwrap();
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
    }

    #[test]
    fn test_sdp_contains_private_ip() {
        let private_sdp = "v=0\r\n\
                          o=- 1234567890 1234567890 IN IP4 192.168.1.100\r\n\
                          s=Call\r\n\
                          c=IN IP4 192.168.1.100\r\n\
                          t=0 0\r\n\
                          m=audio 49170 RTP/AVP 0 8 97\r\n";

        let public_sdp = "v=0\r\n\
                         o=- 1234567890 1234567890 IN IP4 8.8.8.8\r\n\
                         s=Call\r\n\
                         c=IN IP4 8.8.8.8\r\n\
                         t=0 0\r\n\
                         m=audio 49170 RTP/AVP 0 8 97\r\n";

        assert!(sdp_contains_private_ip(private_sdp).unwrap());
        assert!(!sdp_contains_private_ip(public_sdp).unwrap());
    }
}
