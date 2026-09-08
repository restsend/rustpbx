use crate::config::{ProxyConfig, SipContactConfig};
use ipnet::IpNet;
use rsipstack::sip::Host;
use rsipstack::sip::HostWithPort;
use rsipstack::sip::Transport;
use rsipstack::transport::SipAddr;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::Once;
use std::sync::OnceLock;
use tracing::warn;

pub use crate::config::{default_local_networks, parse_local_networks};

static LOCAL_INTERFACE_IP: OnceLock<Option<IpAddr>> = OnceLock::new();
static WILDCARD_CONTACT_WARN: Once = Once::new();

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

/// Two-stage replacement lookup: route probe toward the destination first,
/// then the process-wide interface IP. Production value for
/// [`ensure_advertisable_host_with`].
fn wildcard_replacement(destination: Option<IpAddr>) -> Option<IpAddr> {
    destination
        .and_then(detect_source_ip)
        .or_else(pick_local_interface_ip)
}

/// Returns the replacement host when `host` is an unspecified (wildcard)
/// address; `None` means the host can be advertised unchanged.
///
/// `probe` encapsulates replacement discovery and is injected so tests stay
/// deterministic; production callers pass [`wildcard_replacement`].
fn ensure_advertisable_host_with(
    host: &str,
    probe: impl Fn(Option<IpAddr>) -> Option<IpAddr>,
) -> Option<String> {
    let advertisable = match host.parse::<IpAddr>() {
        Ok(ip) => !ip.is_unspecified(),
        Err(_) => true,
    };
    if advertisable {
        return None;
    }
    probe(None).map(|ip| ip.to_string())
}

fn warn_wildcard_contact_once(host: &str, replacement: &str) {
    WILDCARD_CONTACT_WARN.call_once(|| {
        warn!(
            contact_host = %host,
            replacement = %replacement,
            "wildcard SIP bind address with no external_ip/sip_external_ip configured; \
             advertising the detected address in Contact instead. Configure sip_external_ip \
             (or external_ip) so in-dialog requests such as BYE can be routed back"
        );
    });
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
    contact_sip_addr_with_bind_ip(
        proxy,
        contact,
        rtp_external_ip,
        transport,
        port_override,
        destination,
        bind_ip,
        wildcard_replacement,
    )
}

#[allow(clippy::too_many_arguments)]
fn contact_sip_addr_with_bind_ip(
    proxy: &ProxyConfig,
    contact: &SipContactConfig,
    rtp_external_ip: Option<&str>,
    transport: Transport,
    port_override: Option<u16>,
    destination: Option<IpAddr>,
    bind_ip: &str,
    probe: fn(Option<IpAddr>) -> Option<IpAddr>,
) -> Option<SipAddr> {
    let listener = listener_sip_addr(proxy, transport, port_override)?;
    let host = resolve_contact_host(contact, bind_ip, rtp_external_ip, destination);
    let host = match ensure_advertisable_host_with(&host, probe) {
        Some(replacement) => {
            warn_wildcard_contact_once(&host, &replacement);
            replacement
        }
        None => host,
    };
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

    fn fake_probe(_: Option<IpAddr>) -> Option<IpAddr> {
        Some("192.0.2.77".parse().unwrap())
    }

    fn none_probe(_: Option<IpAddr>) -> Option<IpAddr> {
        None
    }

    #[test]
    fn unspecified_contact_host_replaced_by_probe() {
        let replaced = ensure_advertisable_host_with("0.0.0.0", fake_probe);
        assert_eq!(replaced.as_deref(), Some("192.0.2.77"));
    }

    #[test]
    fn unspecified_ipv6_contact_host_replaced_by_probe() {
        let replaced = ensure_advertisable_host_with("::", fake_probe);
        assert_eq!(replaced.as_deref(), Some("192.0.2.77"));
    }

    #[test]
    fn unspecified_contact_host_kept_when_probe_finds_nothing() {
        let replaced = ensure_advertisable_host_with("0.0.0.0", none_probe);
        assert_eq!(replaced, None);
    }

    #[test]
    fn hostname_and_concrete_contact_hosts_are_untouched() {
        for host in ["example.com", "192.168.1.10", "[2001:db8::5]"] {
            let replaced = ensure_advertisable_host_with(host, fake_probe);
            assert_eq!(replaced, None, "host {host} should stay unchanged");
        }
    }

    #[test]
    fn wildcard_bind_contact_uses_probed_ip() {
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

        let addr = contact_sip_addr_with_bind_ip(
            &proxy,
            &contact,
            None,
            Transport::Udp,
            None,
            Some("8.8.8.8".parse().unwrap()),
            "0.0.0.0",
            fake_probe,
        )
        .unwrap();

        assert_eq!(addr.addr.to_string(), "192.0.2.77:8060");
    }

    #[test]
    fn always_bind_wildcard_contact_still_replaced() {
        let proxy = ProxyConfig {
            addr: "0.0.0.0".to_string(),
            udp_port: Some(8060),
            ..ProxyConfig::default()
        };
        let contact = SipContactConfig {
            sip_contact_always_bind: true,
            ..Default::default()
        };

        let addr = contact_sip_addr_with_bind_ip(
            &proxy,
            &contact,
            None,
            Transport::Udp,
            None,
            None,
            "0.0.0.0",
            fake_probe,
        )
        .unwrap();

        assert_eq!(addr.addr.to_string(), "192.0.2.77:8060");
    }

    #[test]
    fn explicit_bind_contact_host_untouched_by_wildcard_guard() {
        let proxy = ProxyConfig {
            addr: "0.0.0.0".to_string(),
            udp_port: Some(8060),
            ..ProxyConfig::default()
        };

        let addr = contact_sip_addr_with_bind_ip(
            &proxy,
            &SipContactConfig::default(),
            None,
            Transport::Udp,
            None,
            Some("8.8.8.8".parse().unwrap()),
            "10.1.2.3",
            fake_probe,
        )
        .unwrap();

        assert_eq!(addr.addr.to_string(), "10.1.2.3:8060");
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
