use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use rsip::prelude::HeadersExt;
use rsipstack::{transaction::transaction::Transaction, transport::SipConnection};
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, str::FromStr, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AclConfig {
    pub order: Vec<String>,
    pub allows: Vec<String>,
    pub denies: Vec<String>,
}

impl Default for AclConfig {
    fn default() -> Self {
        Self {
            order: vec!["allow".to_string(), "deny".to_string()],
            allows: vec![],
            denies: vec![],
        }
    }
}

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

struct AclModuleInner {
    order: Vec<String>, // allow, deny
    allows: Vec<IpNetwork>,
    denies: Vec<IpNetwork>,
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
        let acl_config = match config.acl.as_ref() {
            Some(acl_config) => acl_config,
            None => &AclConfig::default(),
        };
        let mut allows = Vec::new();
        let mut denies = Vec::new();

        for allow in &acl_config.allows {
            if let Ok((network, prefix_len)) = parse_network(allow) {
                allows.push(IpNetwork::new(network, prefix_len));
            }
        }

        for deny in &acl_config.denies {
            if let Ok((network, prefix_len)) = parse_network(deny) {
                denies.push(IpNetwork::new(network, prefix_len));
            }
        }

        let mut order = acl_config.order.clone();
        order.retain(|x| x == "allow" || x == "deny");
        if order.is_empty() {
            order.push("allow".to_string());
            order.push("deny".to_string());
        }

        let inner = Arc::new(AclModuleInner {
            order: order.clone(),
            allows,
            denies,
        });
        Self { inner }
    }

    fn is_allowed(&self, addr: &IpAddr) -> bool {
        let mut result = false;
        for order in &self.inner.order {
            if order == "allow" {
                // Check if IP is in any allow list
                for allow in &self.inner.allows {
                    if allow.contains(addr) {
                        result = true;
                        break;
                    }
                }
            } else if order == "deny" {
                // Check if IP is in any deny list
                for deny in &self.inner.denies {
                    if deny.contains(addr) {
                        debug!("IP {} is denied by rule", addr);
                        return false;
                    }
                }
            }
        }
        // If no explicit allow rules exist, allow by default
        if self.inner.allows.is_empty() {
            result = true;
        }
        result
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

    fn create_test_config(allows: Vec<String>, denies: Vec<String>) -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig {
            acl: Some(AclConfig {
                allows,
                denies,
                ..Default::default()
            }),
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
    fn test_allow_deny_rules() {
        let config = create_test_config(
            vec![
                "192.168.1.0/24".to_string(),
                "10.0.0.0/8".to_string(),
                "2001:db8::/32".to_string(),
            ],
            vec!["192.168.1.100".to_string(), "10.1.1.0/24".to_string()],
        );

        let ban = AclModule::new(config);

        // Test allowed IPs
        assert!(ban.is_allowed(&"192.168.1.1".parse().unwrap()));
        assert!(ban.is_allowed(&"10.2.3.4".parse().unwrap()));
        assert!(ban.is_allowed(&"2001:db8::1".parse().unwrap()));

        // Test denied IPs
        assert!(!ban.is_allowed(&"192.168.1.100".parse().unwrap())); // Explicitly denied
        assert!(!ban.is_allowed(&"10.1.1.50".parse().unwrap())); // Denied subnet
        assert!(!ban.is_allowed(&"172.16.1.1".parse().unwrap())); // Not in allowed list
        assert!(!ban.is_allowed(&"2001:db9::1".parse().unwrap())); // Not in allowed list
    }

    #[test]
    fn test_default_allow() {
        let config = create_test_config(vec![], vec!["192.168.1.0/24".to_string()]);
        let ban = AclModule::new(config);

        // Test allowed IPs (everything except denied subnet)
        assert!(ban.is_allowed(&"10.0.0.1".parse().unwrap()));
        assert!(ban.is_allowed(&"172.16.1.1".parse().unwrap()));
        assert!(ban.is_allowed(&"2001:db8::1".parse().unwrap()));

        // Test denied IPs
        assert!(!ban.is_allowed(&"192.168.1.100".parse().unwrap()));
        assert!(!ban.is_allowed(&"192.168.1.1".parse().unwrap()));
    }
}
