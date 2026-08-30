use crate::config::{ProxyConfig, SipContactConfig};
use ipnet::IpNet;
use rsipstack::sip::Host;
use rsipstack::sip::HostWithPort;
use rsipstack::sip::Transport;
use rsipstack::transport::SipAddr;
use std::net::IpAddr;

pub use crate::config::{default_local_networks, parse_local_networks};

pub fn is_local_destination(ip: IpAddr, networks: &[IpNet]) -> bool {
    networks.iter().any(|net| net.contains(&ip))
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
        return (*rtp_ext).to_string();
    }

    bind_ip.to_string()
}

/// Build a listener address from configured `[proxy]` ports (never ephemeral outbound sockets).
pub fn listener_sip_addr(
    proxy: &ProxyConfig,
    transport: Transport,
    port_override: Option<u16>,
) -> Option<SipAddr> {
    let port = port_override.or_else(|| listener_port_for_transport(proxy, transport))?;
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

/// True when `addr` matches a configured SIP listener port on this node.
pub fn is_configured_listener_addr(proxy: &ProxyConfig, addr: &SipAddr) -> bool {
    let Some(port) = addr.addr.port.map(|p| p.0) else {
        return false;
    };
    let transport = addr.r#type.unwrap_or(Transport::Udp);
    listener_port_for_transport(proxy, transport) == Some(port)
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
    port_override: Option<u16>,
    destination: Option<IpAddr>,
) -> Option<SipAddr> {
    build_contact_sip_addr_with_bind_ip(
        proxy,
        contact,
        rtp_external_ip,
        transport,
        port_override,
        destination,
        &proxy.addr,
    )
}

pub fn build_transaction_contact_sip_addr(
    proxy: &ProxyConfig,
    contact: &SipContactConfig,
    rtp_external_ip: Option<&str>,
    transport: Transport,
    port_override: Option<u16>,
    connection: &SipAddr,
) -> Option<SipAddr> {
    // Wildcard binds cannot be advertised; the accepted flow carries the concrete local host.
    let actual_bind_ip = proxy
        .addr
        .parse::<IpAddr>()
        .ok()
        .filter(IpAddr::is_unspecified)
        .map(|_| connection.addr.host.to_string());
    build_contact_sip_addr_with_bind_ip(
        proxy,
        contact,
        rtp_external_ip,
        transport,
        port_override,
        None,
        actual_bind_ip.as_deref().unwrap_or(&proxy.addr),
    )
}

fn build_contact_sip_addr_with_bind_ip(
    proxy: &ProxyConfig,
    contact: &SipContactConfig,
    rtp_external_ip: Option<&str>,
    transport: Transport,
    port_override: Option<u16>,
    destination: Option<IpAddr>,
    bind_ip: &str,
) -> Option<SipAddr> {
    let listener = listener_sip_addr(proxy, transport, port_override)?;
    let host = resolve_contact_host(contact, bind_ip, rtp_external_ip, destination);
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
    fn resolve_contact_host_uses_bind_for_lan_destination() {
        let contact = sample_contact();
        let host = resolve_contact_host(
            &contact,
            "192.168.1.10",
            Some("203.0.113.10"),
            Some("192.168.0.50".parse().unwrap()),
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
        let addr = listener_sip_addr(&proxy, Transport::Tls, None).unwrap();
        assert_eq!(addr.addr.to_string(), "192.168.1.10:5061");
        assert_eq!(addr.r#type, Some(Transport::Tls));
    }

    #[test]
    fn is_configured_listener_rejects_ephemeral_port() {
        let proxy = sample_proxy();
        let ephemeral = SipAddr {
            r#type: Some(Transport::Tls),
            addr: HostWithPort::try_from("192.168.1.10:43218").unwrap(),
        };
        assert!(!is_configured_listener_addr(&proxy, &ephemeral));
        let listener = listener_sip_addr(&proxy, Transport::Tls, None).unwrap();
        assert!(is_configured_listener_addr(&proxy, &listener));
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
            None,
            Some("192.168.0.50".parse().unwrap()),
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
            None,
            Some("8.8.8.8".parse().unwrap()),
        )
        .unwrap();
        assert_eq!(addr.addr.to_string(), "203.0.113.10:5061");
    }

    #[test]
    fn build_contact_sip_addr_uses_actual_bind_ip_for_wildcard_listener() {
        let proxy = ProxyConfig {
            addr: "0.0.0.0".to_string(),
            udp_port: Some(8060),
            ..ProxyConfig::default()
        };
        let contact = SipContactConfig {
            local_networks: default_local_networks(),
            contact_lan_use_bind: true,
            ..Default::default()
        };

        let connection = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("192.0.2.10:8060").unwrap(),
        };
        let addr = build_transaction_contact_sip_addr(
            &proxy,
            &contact,
            None,
            Transport::Udp,
            None,
            &connection,
        )
        .unwrap();

        assert_eq!(addr.addr.to_string(), "192.0.2.10:8060");
    }

    #[test]
    fn build_transaction_contact_sip_addr_preserves_explicit_bind_ip() {
        let proxy = ProxyConfig {
            addr: "192.0.2.20".to_string(),
            udp_port: Some(8060),
            ..ProxyConfig::default()
        };
        let contact = SipContactConfig::default();
        let connection = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("192.0.2.10:8060").unwrap(),
        };

        let addr = build_transaction_contact_sip_addr(
            &proxy,
            &contact,
            None,
            Transport::Udp,
            None,
            &connection,
        )
        .unwrap();

        assert_eq!(addr.addr.to_string(), "192.0.2.20:8060");
    }
}
