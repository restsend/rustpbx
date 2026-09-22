use crate::config::{ProxyConfig, SipContactConfig};
use ipnet::IpNet;
use rsipstack::sip::Host;
use rsipstack::sip::HostWithPort;
use rsipstack::sip::Transport;
use rsipstack::transport::SipAddr;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::OnceLock;

pub use crate::config::{default_local_networks, parse_local_networks};

static LOCAL_INTERFACE_IP: OnceLock<Option<IpAddr>> = OnceLock::new();

pub fn is_local_destination(ip: IpAddr, networks: &[IpNet]) -> bool {
    networks.iter().any(|net| net.contains(&ip))
}

/// First non-loopback IP advertised by this host, resolved once per process.
///
/// Delegates to rsipstack's `resolve_bind_address` (getifaddrs based) so no
/// direct `if-addrs` dependency is required. On hosts without any real
/// interface rsipstack falls back to `127.0.0.1`, which is still more
/// routable than a wildcard address.
pub fn pick_local_interface_ip() -> Option<IpAddr> {
    *LOCAL_INTERFACE_IP.get_or_init(|| {
        let addr = rsipstack::transport::SipConnection::resolve_bind_address(SocketAddr::new(
            IpAddr::from([0, 0, 0, 0]),
            0,
        ));
        let ip = addr.ip();
        (!ip.is_unspecified()).then_some(ip)
    })
}

/// Source IP the OS would use to reach `destination`.
///
/// Uses a throwaway UDP socket: `connect` only performs a route lookup and
/// never sends packets.
pub fn detect_source_ip(destination: IpAddr) -> Option<IpAddr> {
    let bind_ip = if destination.is_ipv4() {
        IpAddr::from([0, 0, 0, 0])
    } else {
        IpAddr::from([0u8; 16])
    };
    let socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0)).ok()?;
    socket.connect(SocketAddr::new(destination, 9)).ok()?;
    socket
        .local_addr()
        .ok()
        .map(|addr| addr.ip())
        .filter(|ip| !ip.is_unspecified())
}

/// Resolve the host IP to advertise in SIP Contact for the given destination.
pub fn resolve_contact_host(
    contact: &SipContactConfig,
    bind_ip: &str,
    rtp_external_ip: Option<&str>,
    destination: Option<IpAddr>,
) -> String {
    if contact.sip_contact_always_bind {
        return bind_ip.to_string();
    }

    if let Some(dest) = destination
        && contact.contact_lan_use_bind
        && is_local_destination(dest, &contact.local_networks)
    {
        return bind_ip.to_string();
    }

    if let Some(sip_ext) = contact.sip_external_ip.as_deref().filter(|s| !s.is_empty()) {
        return sip_ext.to_string();
    }

    if let Some(rtp_ext) = rtp_external_ip.filter(|s| !s.is_empty()) {
        return rtp_ext.to_string();
    }

    bind_ip.to_string()
}

/// Build a listener address from configured `[proxy]` ports (never ephemeral outbound sockets).
pub fn listener_sip_addr(
    proxy: &ProxyConfig,
    transport: Transport,
) -> Option<SipAddr> {
    let port = listener_port_for_transport(proxy, transport)?;
    let host_with_port = format!("{}:{}", proxy.addr, port);
    Some(SipAddr {
        r#type: Some(transport),
        addr: HostWithPort::try_from(host_with_port.as_str()).ok()?,
    })
}

fn listener_port_for_transport(proxy: &ProxyConfig, transport: Transport) -> Option<u16> {
    match transport {
        Transport::Udp => proxy
            .udp_port
            .or_else(|| proxy.all_udp_ports().first().copied()),
        Transport::Tcp => proxy.tcp_port,
        Transport::Tls => proxy.tls_port,
        Transport::Ws | Transport::Wss => proxy.ws_port,
        _ => None,
    }
}

fn replace_contact_host(addr: &SipAddr, host: &str) -> SipAddr {
    let port = addr.addr.port.map(|p| p.0);
    let host_with_port = match port {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    };
    let mut updated = addr.clone();
    if let Ok(parsed) = HostWithPort::try_from(host_with_port.as_str()) {
        updated.addr = parsed;
    } else if let Ok(ip) = host.parse::<IpAddr>() {
        updated.addr.host = Host::IpAddr(ip);
    }
    updated
}

/// Build the SIP Contact address for a dialog leg.
pub fn build_contact_sip_addr(
    proxy: &ProxyConfig,
    contact: &SipContactConfig,
    rtp_external_ip: Option<&str>,
    transport: Transport,
    destination: Option<IpAddr>,
) -> Option<SipAddr> {
    let listener = listener_sip_addr(proxy, transport)?;
    let mut host = resolve_contact_host(contact, &proxy.addr, rtp_external_ip, destination);
    if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified()) {
        if let Some(replacement) = destination
            .and_then(detect_source_ip)
            .or_else(pick_local_interface_ip)
        {
            host = replacement.to_string();
        }
    }
    Some(replace_contact_host(&listener, &host))
}

/// Extract destination IP from a SIP host string (ignoring port).
pub fn ip_from_sip_host(host: &str) -> Option<IpAddr> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip bracketed IPv6
    let bare = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    bare.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;

    #[test]
    fn inbound_lan_and_wan_choose_different_contacts_with_public_listener() {
        let proxy = sample_proxy();
        for (peer, expected) in [("127.0.0.1", "192.168.1.10"), ("198.51.100.50", "203.0.113.10")] {
            let addr = build_contact_sip_addr(
                &proxy, &sample_contact(), Some("203.0.113.20"),
                Transport::Udp, Some(peer.parse().unwrap()),
            ).unwrap();
            assert_eq!(addr.addr.host.to_string(), expected);
            assert_eq!(addr.addr.port, Some(5060.into()));
        }
    }

    #[test]
    fn inbound_wildcard_uses_route_to_peer_instead_of_nat_advertisement() {
        let proxy = ProxyConfig { addr: "0.0.0.0".into(), ..sample_proxy() };
        let addr = build_contact_sip_addr(
            &proxy, &sample_contact(), Some("203.0.113.20"),
            Transport::Udp, Some("127.0.0.1".parse().unwrap()),
        ).unwrap();
        assert_eq!(addr.addr.to_string(), "127.0.0.1:5060");
    }

    fn sample_proxy() -> ProxyConfig {
        ProxyConfig {
            addr: "192.168.1.10".to_string(),
            udp_port: Some(5060),
            tls_port: Some(5061),
            ..ProxyConfig::default()
        }
    }

    fn sample_contact() -> SipContactConfig {
        SipContactConfig {
            sip_external_ip: Some("203.0.113.10".to_string()),
            local_networks: default_local_networks(),
            contact_lan_use_bind: true,
            ..Default::default()
        }
    }

    #[test]
    fn wildcard_bind_is_resolved_by_contact_builder() {
        let peer = "127.0.0.1".parse().unwrap();
        for always_bind in [true, false] {
            let contact = SipContactConfig {
                sip_contact_always_bind: always_bind,
                ..sample_contact()
            };
            assert_eq!(
                resolve_contact_host(&contact, "0.0.0.0", Some("203.0.113.20"), Some(peer)),
                "0.0.0.0",
            );
            let proxy = ProxyConfig { addr: "0.0.0.0".into(), ..sample_proxy() };
            let addr = build_contact_sip_addr(
                &proxy, &contact, Some("203.0.113.20"),
                Transport::Udp, Some(peer),
            ).unwrap();
            assert_eq!(addr.addr.to_string(), "127.0.0.1:5060");
            assert_eq!(
                resolve_contact_host(&contact, "192.0.2.10", None, Some(peer)),
                "192.0.2.10",
            );
        }
    }

    #[test]
    fn configured_local_networks_control_bind_selection() {
        let peer = "127.0.0.1".parse().unwrap();
        let mut contact = sample_contact();
        contact.local_networks = vec!["192.168.3.0/24".parse().unwrap()];
        assert_eq!(resolve_contact_host(&contact, "0.0.0.0", None, Some(peer)), "203.0.113.10");
        contact.local_networks.push("127.0.0.0/8".parse().unwrap());
        assert_eq!(resolve_contact_host(&contact, "0.0.0.0", None, Some(peer)), "0.0.0.0");
        contact.contact_lan_use_bind = false;
        assert_eq!(resolve_contact_host(&contact, "0.0.0.0", None, Some(peer)), "203.0.113.10");
        assert!(SipContactConfig::default().contact_lan_use_bind);
    }

    #[test]
    fn resolve_contact_host_uses_bind_for_lan_destination() {
        let contact = sample_contact();
        let host = resolve_contact_host(
            &contact,
            "192.168.1.10",
            Some("203.0.113.10"),
            Some("127.0.0.1".parse().unwrap()),
        );
        assert_eq!(host, "192.168.1.10");
    }

    #[test]
    fn resolve_contact_host_uses_sip_external_for_wan_destination() {
        let contact = sample_contact();
        let host = resolve_contact_host(
            &contact,
            "192.168.1.10",
            Some("203.0.113.10"),
            Some("8.8.8.8".parse().unwrap()),
        );
        assert_eq!(host, "203.0.113.10");
    }

    #[test]
    fn resolve_contact_host_uses_sip_external_without_destination() {
        let contact = sample_contact();
        let host = resolve_contact_host(&contact, "192.168.1.10", Some("203.0.113.10"), None);
        assert_eq!(host, "203.0.113.10");
    }

    #[test]
    fn resolve_contact_host_defaults_to_bind_without_destination_or_public_ip() {
        let contact = SipContactConfig {
            sip_external_ip: None,
            ..sample_contact()
        };
        let host = resolve_contact_host(&contact, "192.168.1.10", None, None);
        assert_eq!(host, "192.168.1.10");
    }

    #[test]
    fn resolve_contact_host_follows_rtp_when_sip_external_unset() {
        let contact = SipContactConfig {
            sip_external_ip: None,
            ..sample_contact()
        };
        let host = resolve_contact_host(
            &contact,
            "192.168.1.10",
            Some("203.0.113.10"),
            Some("8.8.8.8".parse().unwrap()),
        );
        assert_eq!(host, "203.0.113.10");
    }

    #[test]
    fn listener_sip_addr_uses_configured_tls_port() {
        let proxy = sample_proxy();
        let addr = listener_sip_addr(&proxy, Transport::Tls).unwrap();
        assert_eq!(addr.addr.to_string(), "192.168.1.10:5061");
        assert_eq!(addr.r#type, Some(Transport::Tls));
    }

    #[test]
    fn build_contact_sip_addr_lan_uses_bind_with_listener_port() {
        let proxy = sample_proxy();
        let contact = sample_contact();
        let addr = build_contact_sip_addr(
            &proxy,
            &contact,
            Some("203.0.113.10"),
            Transport::Tls,
            Some("127.0.0.1".parse().unwrap()),
        )
        .unwrap();
        assert_eq!(addr.addr.to_string(), "192.168.1.10:5061");
    }

    #[test]
    fn build_contact_sip_addr_wan_uses_public_ip_with_listener_port() {
        let proxy = sample_proxy();
        let contact = sample_contact();
        let addr = build_contact_sip_addr(
            &proxy,
            &contact,
            Some("203.0.113.10"),
            Transport::Tls,
            Some("8.8.8.8".parse().unwrap()),
        )
        .unwrap();
        assert_eq!(addr.addr.to_string(), "203.0.113.10:5061");
    }

    #[test]
    fn build_contact_sip_addr_uses_route_to_peer_for_wildcard_listener() {
        let proxy = ProxyConfig {
            addr: "0.0.0.0".to_string(),
            udp_port: Some(8060),
            ..ProxyConfig::default()
        };
        let contact = SipContactConfig {
            contact_lan_use_bind: true,
            ..Default::default()
        };

        let addr = build_contact_sip_addr(
            &proxy,
            &contact,
            None,
            Transport::Udp,
            Some("127.0.0.1".parse().unwrap()),
        )
        .unwrap();

        assert_eq!(addr.addr.to_string(), "127.0.0.1:8060");
    }

    #[test]
    fn build_contact_sip_addr_preserves_explicit_bind_ip() {
        let proxy = ProxyConfig {
            addr: "192.0.2.20".to_string(),
            udp_port: Some(8060),
            ..ProxyConfig::default()
        };
        let contact = SipContactConfig::default();

        let addr = build_contact_sip_addr(
            &proxy,
            &contact,
            None,
            Transport::Udp,
            None,
        )
        .unwrap();

        assert_eq!(addr.addr.to_string(), "192.0.2.20:8060");
    }

    #[test]
    fn detect_source_ip_for_loopback_destination_yields_loopback() {
        if let Some(ip) = detect_source_ip("127.0.0.1".parse().unwrap()) {
            assert_eq!(ip, IpAddr::from([127, 0, 0, 1]));
        }
    }

    #[test]
    fn pick_local_interface_ip_returns_advertisable_address() {
        if let Some(ip) = pick_local_interface_ip() {
            assert!(!ip.is_unspecified());
        }
    }
}
