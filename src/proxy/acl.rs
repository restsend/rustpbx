use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use rsip::prelude::HeadersExt;
use rsipstack::{transaction::transaction::Transaction, transport::SipConnection};
use std::{net::IpAddr, str::FromStr, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Debug, Clone)]
struct IpNetwork {
    network: IpAddr,
    prefix_len: u8,
}

impl IpNetwork {
    fn new(network: IpAddr, prefix_len: u8) -> Self {
        Self {
            network,
            prefix_len,
        }
    }

    fn contains(&self, ip: &IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                let mask = if self.prefix_len == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix_len)
                };
                (u32::from(network) & mask) == (u32::from(*ip) & mask)
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                let network_segments = network.segments();
                let ip_segments = ip.segments();
                let mut remaining_bits = self.prefix_len;

                for i in 0..8 {
                    if remaining_bits == 0 {
                        return true;
                    }
                    let bits = std::cmp::min(remaining_bits, 16);
                    let mask = if bits == 16 {
                        0xFFFF
                    } else {
                        (0xFFFF << (16 - bits)) & 0xFFFF
                    };
                    if (network_segments[i] & mask) != (ip_segments[i] & mask) {
                        return false;
                    }
                    if remaining_bits >= 16 {
                        remaining_bits -= 16;
                    } else {
                        break;
                    }
                }
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
enum AclAction {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
struct AclRule {
    action: AclAction,
    network: Option<IpNetwork>,
}

impl AclRule {
    fn new(rule: &str) -> Option<Self> {
        let parts: Vec<&str> = rule.split_whitespace().collect();
        if parts.len() < 2 {
            return None;
        }

        let action = match parts[0].to_lowercase().as_str() {
            "allow" => AclAction::Allow,
            "deny" => AclAction::Deny,
            _ => return None,
        };
        let network = if parts[1] == "all" {
            None
        } else {
            match parse_network(parts[1]) {
                Ok((network, prefix_len)) => Some(IpNetwork::new(network, prefix_len)),
                Err(_) => return None,
            }
        };

        Some(Self { action, network })
    }
}

struct AclModuleInner {
    rules: Vec<AclRule>,
}

#[derive(Clone)]
pub struct AclModule {
    inner: Arc<AclModuleInner>,
}

impl AclModule {
    pub fn create(_server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = AclModule::new(config);
        Ok(Box::new(module))
    }

    pub fn new(config: Arc<ProxyConfig>) -> Self {
        let rules = config.acl_rules.as_ref().map_or_else(
            || vec!["allow all".to_string(), "deny all".to_string()],
            |rules| rules.clone(),
        );

        let acl_rules: Vec<AclRule> = rules.iter().filter_map(|rule| AclRule::new(rule)).collect();

        let inner = Arc::new(AclModuleInner { rules: acl_rules });
        Self { inner }
    }

    pub fn is_allowed(&self, addr: &IpAddr) -> bool {
        for rule in &self.inner.rules {
            match &rule.network {
                Some(network) => {
                    if network.contains(addr) {
                        return matches!(rule.action, AclAction::Allow);
                    }
                }
                None => {
                    // "all" rule
                    return matches!(rule.action, AclAction::Allow);
                }
            }
        }
        false // Default deny if no rules match
    }
}

fn parse_network(addr: &str) -> Result<(IpAddr, u8)> {
    // If no mask is specified, treat as single IP
    if !addr.contains('/') {
        let ip = IpAddr::from_str(addr)?;
        let prefix_len = match ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        return Ok((ip, prefix_len));
    }

    // For CIDR notation, we'll use the network address
    let parts: Vec<&str> = addr.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!("invalid network address"));
    }

    let ip = IpAddr::from_str(parts[0])?;
    let prefix_len: u8 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid network address"))?;

    match ip {
        IpAddr::V4(ipv4) => {
            if prefix_len > 32 {
                return Err(anyhow::anyhow!("invalid network address prefix > 32"));
            }
            let mask = if prefix_len == 0 {
                0
            } else {
                u32::MAX << (32 - prefix_len)
            };
            let network = u32::from(ipv4) & mask;
            Ok((IpAddr::V4(network.into()), prefix_len))
        }
        IpAddr::V6(ipv6) => {
            if prefix_len > 128 {
                return Err(anyhow::anyhow!("invalid network address prefix > 128"));
            }
            let segments = ipv6.segments();
            let mut result = [0u16; 8];
            for i in 0..8usize {
                if prefix_len as usize > i * 16 {
                    let bits = std::cmp::min(prefix_len as usize - i * 16, 16);
                    let mask = if bits == 16 {
                        0xFFFF
                    } else {
                        (0xFFFF << (16 - bits)) & 0xFFFF
                    };
                    result[i] = segments[i] & mask;
                }
            }
            Ok((IpAddr::V6(result.into()), prefix_len))
        }
    }
}

#[async_trait]
impl ProxyModule for AclModule {
    fn name(&self) -> &str {
        "acl"
    }

    async fn on_start(&mut self) -> Result<()> {
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        let via = tx.original.via_header()?;
        let target =
            SipConnection::parse_target_from_via(via).map_err(|e| anyhow::anyhow!("{}", e))?;
        if self.is_allowed(&target.host.try_into()?) {
            return Ok(ProxyAction::Continue);
        }
        info!(
            method = tx.original.method().to_string(),
            via = via.to_string(),
            "IP is denied by acl module"
        );
        tx.reply(rsip::StatusCode::Forbidden).await.ok();
        Ok(ProxyAction::Abort)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn create_test_config(rules: Vec<String>) -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig {
            acl_rules: Some(rules),
            ..Default::default()
        })
    }

    #[test]
    fn test_parse_network() {
        // Test IPv4
        let (net, prefix) = parse_network("192.168.1.0/24").unwrap();
        assert_eq!(net, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0)));
        assert_eq!(prefix, 24);

        // Test IPv4 without mask
        let (net, prefix) = parse_network("192.168.1.1").unwrap();
        assert_eq!(net, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(prefix, 32);

        // Test IPv6
        let (net, prefix) = parse_network("2001:db8::/32").unwrap();
        assert_eq!(net, IpAddr::V6(Ipv6Addr::from_str("2001:db8::").unwrap()));
        assert_eq!(prefix, 32);

        // Test IPv6 without mask
        let (net, prefix) = parse_network("2001:db8::1").unwrap();
        assert_eq!(net, IpAddr::V6(Ipv6Addr::from_str("2001:db8::1").unwrap()));
        assert_eq!(prefix, 128);
    }

    #[test]
    fn test_acl_rules() {
        let config = create_test_config(vec![
            "deny 192.168.1.100".to_string(),
            "allow 192.168.1.0/24".to_string(),
            "allow 10.0.0.0/8".to_string(),
            "deny all".to_string(),
        ]);

        let acl = AclModule::new(config);

        // Test allowed IPs
        assert!(acl.is_allowed(&"192.168.1.1".parse().unwrap()));
        assert!(acl.is_allowed(&"10.2.3.4".parse().unwrap()));

        // Test denied IPs
        assert!(!acl.is_allowed(&"192.168.1.100".parse().unwrap())); // Explicitly denied
        assert!(!acl.is_allowed(&"172.16.1.1".parse().unwrap())); // Denied by default
    }

    #[test]
    fn test_default_rules() {
        // Test with None (no ACL rules configured)
        let config = Arc::new(ProxyConfig {
            acl_rules: None,
            ..Default::default()
        });
        let acl = AclModule::new(config);

        // Test with default rules (allow all, deny all)
        // The first rule "allow all" matches all IPs, so they should be allowed
        assert!(acl.is_allowed(&"192.168.1.1".parse().unwrap()));
        assert!(acl.is_allowed(&"10.0.0.1".parse().unwrap()));
    }
}
