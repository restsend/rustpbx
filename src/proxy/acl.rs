use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::{TransactionCookie, TrunkContext};
use crate::{
    config::ProxyConfig,
    proxy::{
        routing::{
            extract_from_user, extract_to_user, source_addr_ip,
        },
    },
};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::transaction::transaction::Transaction;
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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
                        0xFFFF << (16 - bits)
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

struct DosPerIpData {
    recent: Vec<Instant>,
    concurrent: usize,
    blocked_until: Option<Instant>,
}

struct AclModuleInner {
    config: Arc<ProxyConfig>,
    server: Option<SipServerRef>,
    ua_white_list: HashSet<String>,
    ua_black_list: HashSet<String>,
    fallback_rules: Vec<String>,
    dos_data: Arc<RwLock<HashMap<IpAddr, DosPerIpData>>>,
}

#[derive(Clone)]
pub struct AclModule {
    inner: Arc<AclModuleInner>,
}

impl AclModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = AclModule::with_server(config, Some(server));
        Ok(Box::new(module))
    }

    fn with_server(config: Arc<ProxyConfig>, server: Option<SipServerRef>) -> Self {
        let fallback_rules = resolve_base_rules(&config);

        let ua_white_list = config
            .ua_white_list
            .as_ref()
            .map_or_else(HashSet::new, |list| list.iter().cloned().collect());

        let ua_black_list = config
            .ua_black_list
            .as_ref()
            .map_or_else(HashSet::new, |list| list.iter().cloned().collect());

        Self {
            inner: Arc::new(AclModuleInner {
                config,
                server,
                ua_white_list,
                ua_black_list,
                fallback_rules,
                dos_data: Arc::new(RwLock::new(HashMap::new())),
            }),
        }
    }

    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self::with_server(config, None)
    }

    // ── DoS protection helpers ─────────────────────────────────────

    fn extract_ip(tx: &Transaction) -> Option<IpAddr> {
        tx.connection
            .as_ref()
            .and_then(|conn| conn.get_remote_addr())
            .and_then(source_addr_ip)
    }

    async fn dos_check_and_track(&self, ip: IpAddr) -> Result<()> {
        let cfg = &self.inner.config;
        let now = Instant::now();
        let mut map = self.inner.dos_data.write().await;
        let entry = map.entry(ip).or_insert_with(|| DosPerIpData {
            recent: Vec::new(),
            concurrent: 0,
            blocked_until: None,
        });

        if let Some(until) = entry.blocked_until {
            if now < until {
                return Err(anyhow::anyhow!("IP blocked until {:?}", until));
            }
            entry.recent.clear();
            entry.concurrent = 0;
            entry.blocked_until = None;
        }

        let window = now - Duration::from_secs(1);
        entry.recent.retain(|t| *t > window);

        if entry.recent.len() >= cfg.dos_max_cps_per_ip as usize {
            entry.blocked_until = Some(now + Duration::from_secs(cfg.dos_scan_block_duration_secs));
            return Err(anyhow::anyhow!("CPS limit exceeded"));
        }

        if entry.concurrent >= cfg.dos_max_concurrent_per_ip as usize {
            return Err(anyhow::anyhow!("Concurrent limit exceeded"));
        }

        if entry.recent.len() >= cfg.dos_scan_probe_threshold as usize {
            entry.blocked_until = Some(now + Duration::from_secs(cfg.dos_scan_block_duration_secs));
            return Err(anyhow::anyhow!("Scan detected"));
        }

        entry.recent.push(now);
        entry.concurrent += 1;
        Ok(())
    }

    async fn dos_release(&self, ip: IpAddr) {
        if let Some(entry) = self.inner.dos_data.write().await.get_mut(&ip) {
            entry.concurrent = entry.concurrent.saturating_sub(1);
        }
    }

    // ── URI normalization check ────────────────────────────────────

    fn check_uri_normalization(&self, tx: &Transaction) -> Result<()> {
        let cfg = &self.inner.config;
        if !cfg.uri_reject_malformed {
            return Ok(());
        }

        let from = match tx.original.from_header() {
            Ok(f) => f,
            Err(e) => {
                warn!("Normalization: missing/malformed From: {}", e);
                return Err(anyhow::anyhow!("malformed From header"));
            }
        };

        match from.uri() {
            Ok(uri) => {
                if uri.to_string().len() > cfg.uri_max_length {
                    warn!("Normalization: From URI too long");
                    return Err(anyhow::anyhow!("From URI too long"));
                }
            }
            Err(e) => {
                warn!("Normalization: malformed From URI: {}", e);
                return Err(anyhow::anyhow!("malformed From URI"));
            }
        }
        Ok(())
    }

    pub fn is_from_trunk_context(
        &self,
        addr: &IpAddr,
        origin: &rsipstack::sip::Request,
    ) -> Option<TrunkContext> {
        let Some(server) = self.inner.server.as_ref() else {
            return None;
        };
        let inbound_trunks = server.data_context.acl_inbound_trunks.load();
        let source_network = ipnet::IpNet::from(*addr);
        let invite_users = if matches!(&origin.method, rsipstack::sip::Method::Invite) {
            Some((extract_from_user(origin), extract_to_user(origin)))
        } else {
            None
        };
        let mut matched = None;

        for trunks in inbound_trunks.cover_values(&source_network) {
            for name in trunks {
                let Some(trunk) = server.data_context.get_trunk(name) else {
                    continue;
                };
                if let Some((from_user, to_user)) = &invite_users
                    && !trunk.matches_incoming_user_prefixes(
                        from_user.as_deref(),
                        to_user.as_deref(),
                    )
                {
                    continue;
                }
                matched = Some(TrunkContext {
                    id: trunk.id,
                    name: name.clone(),
                    did_numbers: trunk.did_numbers,
                });
                break;
            }
        }
        matched
    }

    pub(crate) async fn is_ip_allowed(&self, addr: &IpAddr) -> bool {
        let rules = self.load_rules().await;
        for rule in rules {
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

    pub fn is_ua_allowed(&self, ua: &str) -> bool {
        if self.inner.ua_black_list.contains(ua) {
            return false;
        }
        if self.inner.ua_white_list.is_empty() {
            return true; // No whitelist means all UAs are allowed unless blacklisted
        }
        self.inner.ua_white_list.contains(ua)
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
                        0xFFFF << (16 - bits)
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
        let rules = self.load_rules().await;
        debug!("ACL module started with {} rules", rules.len());
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        debug!("ACL module stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        // 1. User-Agent check
        match tx.original.user_agent_header() {
            Some(ua_header) => {
                let ua = ua_header.value();
                if !self.is_ua_allowed(ua) {
                    info!(
                        method = tx.original.method().to_string(),
                        ua = ua,
                        "User-Agent is denied by acl module"
                    );
                    cookie.mark_as_spam(crate::call::cookie::SpamResult::UaBlacklist);
                    return Ok(ProxyAction::Abort);
                }
            }
            None => {
                if !self.inner.ua_white_list.is_empty() {
                    info!(
                        method = tx.original.method().to_string(),
                        "Missing User-Agent header, denied by acl module"
                    );
                    cookie.mark_as_spam(crate::call::cookie::SpamResult::Spam);
                    return Ok(ProxyAction::Abort);
                }
            }
        }

        // 2. URI normalization check (safety)
        if let Err(_e) = self.check_uri_normalization(tx) {
            return Ok(ProxyAction::Abort);
        }

        // 3. DoS protection (per-IP rate limiting, applies to all sources)
        if self.inner.config.dos_enabled {
            if let Some(ip) = Self::extract_ip(tx) {
                if let Err(e) = self.dos_check_and_track(ip).await {
                    warn!("DoS blocked {}: {}", ip, e);
                    return Ok(ProxyAction::Abort);
                }
            }
        }

        // 4. IP ACL check (allow/deny with trunk bypass)
        let from_addr = Self::extract_ip(tx)
            .ok_or_else(|| anyhow::anyhow!("missing transport source address"))?;
        if let Some(ctx) = self.is_from_trunk_context(&from_addr, &tx.original) {
            debug!(
                method = tx.original.method().to_string(),
                source_ip = %from_addr,
                trunk = %ctx.name,
                "IP is from trunk, bypassing acl check"
            );
            cookie.insert_extension(ctx);
            return Ok(ProxyAction::Continue);
        }

        if self.is_ip_allowed(&from_addr).await {
            return Ok(ProxyAction::Continue);
        }
        info!(
            method = tx.original.method().to_string(),
            source_ip = %from_addr,
            "IP is denied by acl module"
        );
        cookie.mark_as_spam(crate::call::cookie::SpamResult::IpBlacklist);
        Ok(ProxyAction::Abort)
    }

    async fn on_transaction_end(&self, tx: &mut Transaction) -> Result<()> {
        if self.inner.config.dos_enabled {
            if let Some(ip) = Self::extract_ip(tx) {
                self.dos_release(ip).await;
            }
        }
        Ok(())
    }
}

impl AclModule {
    async fn load_rules(&self) -> Vec<AclRule> {
        if let Some(server) = &self.inner.server {
            let snapshot = server.data_context.acl_rules_snapshot();
            return parse_rules(snapshot);
        }
        parse_rules(self.inner.fallback_rules.clone())
    }
}

fn resolve_base_rules(config: &ProxyConfig) -> Vec<String> {
    config
        .acl_rules
        .clone()
        .unwrap_or_else(|| vec!["allow all".to_string(), "deny all".to_string()])
}

fn parse_rules(rules: Vec<String>) -> Vec<AclRule> {
    rules.iter().filter_map(|rule| AclRule::new(rule)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::routing::{TrunkConfig, TrunkDirection};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_config,
    };
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn create_test_config(rules: Vec<String>) -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig {
            acl_rules: Some(rules),
            ..Default::default()
        })
    }

    fn create_dos_config(
        dos_enabled: bool,
        max_cps: u32,
        max_concurrent: u32,
        scan_threshold: u32,
        block_secs: u64,
    ) -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig {
            dos_enabled,
            dos_max_cps_per_ip: max_cps,
            dos_max_concurrent_per_ip: max_concurrent,
            dos_scan_probe_threshold: scan_threshold,
            dos_scan_block_duration_secs: block_secs,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn trunk_context_ignores_outbound_dest_same_ip() {
        let source_ip = IpAddr::V4(Ipv4Addr::new(43, 198, 217, 33));
        let mut config = ProxyConfig::default();

        config.trunks.insert(
            "outbound-same-ip".to_string(),
            TrunkConfig {
                id: Some(147),
                direction: Some(TrunkDirection::Outbound),
                dest: "43.198.217.33:5396".to_string(),
                ..Default::default()
            },
        );
        config.trunks.insert(
            "inbound-source".to_string(),
            TrunkConfig {
                id: Some(144),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec!["43.198.217.33".to_string()],
                ..Default::default()
            },
        );

        config.generated_dir = format!(
            "target/test-generated/acl-inbound-source-{}",
            std::process::id()
        );
        let (server, config) = create_test_server_with_config(config).await;
        let acl = AclModule::with_server(config, Some(server));
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "1000",
            None,
            "rustpbx.com",
            None,
        );
        let ctx = acl
            .is_from_trunk_context(&source_ip, &request)
            .expect("inbound trunk should match");

        assert_eq!(ctx.id, Some(144));
        assert_eq!(ctx.name, "inbound-source");
    }

    #[tokio::test]
    async fn trunk_context_matches_inbound_dest_only() {
        let source_ip = IpAddr::V4(Ipv4Addr::new(43, 198, 217, 33));
        let mut config = ProxyConfig::default();

        config.trunks.insert(
            "inbound-dest-only".to_string(),
            TrunkConfig {
                id: Some(144),
                direction: Some(TrunkDirection::Inbound),
                dest: "43.198.217.33:5060".to_string(),
                ..Default::default()
            },
        );

        config.generated_dir = format!(
            "target/test-generated/acl-inbound-dest-{}",
            std::process::id()
        );
        let (server, config) = create_test_server_with_config(config).await;
        let acl = AclModule::with_server(config, Some(server));
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "1000",
            None,
            "rustpbx.com",
            None,
        );

        let ctx = acl
            .is_from_trunk_context(&source_ip, &request)
            .expect("dest-only trunk should now match inbound source IP");
        assert_eq!(ctx.id, Some(144));
        assert_eq!(ctx.name, "inbound-dest-only");
    }

    #[tokio::test]
    async fn runtime_trunk_index_matches_cidr_and_invite_prefix() {
        let source_ip = IpAddr::V4(Ipv4Addr::new(43, 198, 217, 33));
        let mut config = ProxyConfig::default();
        config.generated_dir = format!(
            "target/test-generated/acl-trunk-index-{}",
            std::process::id()
        );
        config.trunks.insert(
            "broad-cidr".to_string(),
            TrunkConfig {
                id: Some(143),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec!["43.198.217.0/24".to_string()],
                incoming_to_user_prefix: Some("44".to_string()),
                ..Default::default()
            },
        );
        config.trunks.insert(
            "prefix-8614".to_string(),
            TrunkConfig {
                id: Some(144),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec!["43.198.217.33:5060".to_string()],
                incoming_to_user_prefix: Some("8614".to_string()),
                ..Default::default()
            },
        );
        config.trunks.insert(
            "prefix-86155".to_string(),
            TrunkConfig {
                id: Some(145),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec!["43.198.217.33".to_string()],
                incoming_to_user_prefix: Some("86155".to_string()),
                ..Default::default()
            },
        );
        config.trunks.insert(
            "cidr".to_string(),
            TrunkConfig {
                id: Some(146),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec!["198.51.100.0/24".to_string()],
                ..Default::default()
            },
        );
        config.trunks.insert(
            "ipv6-cidr".to_string(),
            TrunkConfig {
                id: Some(147),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec!["2001:db8::/32".to_string()],
                ..Default::default()
            },
        );

        let (server, config) = create_test_server_with_config(config).await;
        let acl = AclModule::with_server(config.clone(), Some(server.clone()));
        let matching_request = create_test_request(
            rsipstack::sip::Method::Invite,
            "861551234",
            None,
            "rustpbx.com",
            None,
        );
        let ctx = acl
            .is_from_trunk_context(&source_ip, &matching_request)
            .expect("matching prefix should select a trunk");
        assert_eq!(ctx.id, Some(145));
        assert_eq!(ctx.name, "prefix-86155");

        let broad_request = create_test_request(
            rsipstack::sip::Method::Invite,
            "441234",
            None,
            "rustpbx.com",
            None,
        );
        let ctx = acl
            .is_from_trunk_context(&source_ip, &broad_request)
            .expect("broader CIDR should match after exact candidates reject the prefix");
        assert_eq!(ctx.id, Some(143));

        let unmatched_request = create_test_request(
            rsipstack::sip::Method::Invite,
            "331234",
            None,
            "rustpbx.com",
            None,
        );
        assert!(acl
            .is_from_trunk_context(&source_ip, &unmatched_request)
            .is_none());

        let cidr_ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 25));
        assert_eq!(
            acl.is_from_trunk_context(&cidr_ip, &matching_request)
                .expect("IPv4 CIDR should match")
                .id,
            Some(146)
        );
        let ipv6_ip = IpAddr::V6(Ipv6Addr::from_str("2001:db8::1234").unwrap());
        assert_eq!(
            acl.is_from_trunk_context(&ipv6_ip, &matching_request)
                .expect("IPv6 CIDR should match")
                .id,
            Some(147)
        );

        let reloaded_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
        let mut reloaded_config = config.as_ref().clone();
        reloaded_config.trunks.clear();
        reloaded_config.trunks.insert(
            "reloaded".to_string(),
            TrunkConfig {
                id: Some(200),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec![reloaded_ip.to_string()],
                ..Default::default()
            },
        );
        server
            .data_context
            .reload_trunks(false, Some(Arc::new(reloaded_config)))
            .await
            .expect("runtime trunk reload should succeed");
        assert!(
            acl.is_from_trunk_context(&source_ip, &matching_request)
                .is_none(),
            "server-backed ACL must not fall back to stale startup trunks"
        );
        assert_eq!(
            acl.is_from_trunk_context(&reloaded_ip, &matching_request)
                .expect("reloaded trunk should match")
                .id,
            Some(200)
        );
    }

    #[tokio::test]
    async fn trunk_context_prefers_longest_incoming_user_prefix_matches() {
        let source_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 20));
        let mut config = ProxyConfig::default();
        config.generated_dir = format!(
            "target/test-generated/acl-trunk-prefix-order-{}",
            std::process::id()
        );
        config.trunks.insert(
            "short-caller-long-callee".to_string(),
            TrunkConfig {
                id: Some(301),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec![source_ip.to_string()],
                incoming_from_user_prefix: Some("12".to_string()),
                incoming_to_user_prefix: Some("123456".to_string()),
                ..Default::default()
            },
        );
        config.trunks.insert(
            "long-caller-no-callee".to_string(),
            TrunkConfig {
                id: Some(302),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec![source_ip.to_string()],
                incoming_from_user_prefix: Some("123".to_string()),
                ..Default::default()
            },
        );
        config.trunks.insert(
            "long-caller-long-callee".to_string(),
            TrunkConfig {
                id: Some(303),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec![source_ip.to_string()],
                incoming_from_user_prefix: Some("123".to_string()),
                incoming_to_user_prefix: Some("1234".to_string()),
                ..Default::default()
            },
        );
        config.trunks.insert(
            "same-match-higher-id".to_string(),
            TrunkConfig {
                id: Some(304),
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec![source_ip.to_string()],
                incoming_from_user_prefix: Some("123".to_string()),
                incoming_to_user_prefix: Some("1234".to_string()),
                ..Default::default()
            },
        );
        config.trunks.insert(
            "same-match-no-id".to_string(),
            TrunkConfig {
                direction: Some(TrunkDirection::Inbound),
                inbound_hosts: vec![source_ip.to_string()],
                incoming_from_user_prefix: Some("123".to_string()),
                incoming_to_user_prefix: Some("1234".to_string()),
                ..Default::default()
            },
        );

        let (server, config) = create_test_server_with_config(config).await;
        {
            let inbound_trunks = server.data_context.acl_inbound_trunks.load();
            let candidates = inbound_trunks
                .get(&ipnet::IpNet::from(source_ip))
                .expect("source IP should have inbound trunk candidates");
            assert_eq!(
                candidates.iter().map(String::as_str).collect::<Vec<_>>(),
                vec![
                    "long-caller-long-callee",
                    "same-match-higher-id",
                    "same-match-no-id",
                    "long-caller-no-callee",
                    "short-caller-long-callee",
                ]
            );
        }
        let acl = AclModule::with_server(config, Some(server));
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "123456789",
            None,
            "rustpbx.com",
            None,
        );

        let context = acl
            .is_from_trunk_context(&source_ip, &request)
            .expect("the most specific trunk should match");

        assert_eq!(context.id, Some(303));
        assert_eq!(context.name, "long-caller-long-callee");
    }

    #[test]
    fn test_parse_network() {
        let (net, prefix) = parse_network("192.168.1.0/24").unwrap();
        assert_eq!(net, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0)));
        assert_eq!(prefix, 24);

        let (net, prefix) = parse_network("192.168.1.1").unwrap();
        assert_eq!(net, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(prefix, 32);

        let (net, prefix) = parse_network("2001:db8::/32").unwrap();
        assert_eq!(net, IpAddr::V6(Ipv6Addr::from_str("2001:db8::").unwrap()));
        assert_eq!(prefix, 32);

        let (net, prefix) = parse_network("2001:db8::1").unwrap();
        assert_eq!(net, IpAddr::V6(Ipv6Addr::from_str("2001:db8::1").unwrap()));
        assert_eq!(prefix, 128);
    }

    #[tokio::test]
    async fn test_acl_rules() {
        let config = create_test_config(vec![
            "deny 192.168.1.100".to_string(),
            "allow 192.168.1.0/24".to_string(),
            "allow 10.0.0.0/8".to_string(),
            "deny all".to_string(),
        ]);

        let acl = AclModule::new(config);

        assert!(acl.is_ip_allowed(&"192.168.1.1".parse().unwrap()).await);
        assert!(acl.is_ip_allowed(&"10.2.3.4".parse().unwrap()).await);
        assert!(!acl.is_ip_allowed(&"192.168.1.100".parse().unwrap()).await);
        assert!(!acl.is_ip_allowed(&"172.16.1.1".parse().unwrap()).await);
    }

    #[tokio::test]
    async fn test_default_rules() {
        let config = Arc::new(ProxyConfig {
            acl_rules: None,
            ..Default::default()
        });
        let acl = AclModule::new(config);

        assert!(acl.is_ip_allowed(&"192.168.1.1".parse().unwrap()).await);
        assert!(acl.is_ip_allowed(&"10.0.0.1".parse().unwrap()).await);
    }

    // ── DoS protection unit tests ─────────────────────────────

    #[tokio::test]
    async fn test_dos_cps_limit() {
        let config = create_dos_config(true, 3, 100, 100, 60);
        let acl = AclModule::new(config);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        for _ in 0..3 {
            assert!(acl.dos_check_and_track(ip).await.is_ok());
        }
        assert!(acl.dos_check_and_track(ip).await.is_err());
    }

    #[tokio::test]
    async fn test_dos_concurrent_limit_and_release() {
        let config = create_dos_config(true, 100, 2, 100, 60);
        let acl = AclModule::new(config);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();

        assert!(acl.dos_check_and_track(ip).await.is_ok());
        assert!(acl.dos_check_and_track(ip).await.is_ok());
        assert!(acl.dos_check_and_track(ip).await.is_err());

        acl.dos_release(ip).await;
        assert!(acl.dos_check_and_track(ip).await.is_ok());
        assert!(acl.dos_check_and_track(ip).await.is_err());
    }

    #[tokio::test]
    async fn test_dos_scan_detection() {
        let config = create_dos_config(true, 100, 100, 3, 60);
        let acl = AclModule::new(config);
        let ip: IpAddr = "10.0.0.3".parse().unwrap();

        // First 3 requests pass (recent.len() grows: 0→1, 1→2, 2→3)
        assert!(acl.dos_check_and_track(ip).await.is_ok());
        assert!(acl.dos_check_and_track(ip).await.is_ok());
        assert!(acl.dos_check_and_track(ip).await.is_ok());
        // 4th: recent.len() == 3 >= threshold(3) → scan detected
        assert!(acl.dos_check_and_track(ip).await.is_err());
    }

    #[tokio::test]
    async fn test_dos_block_clears_after_window() {
        let config = create_dos_config(true, 1, 100, 100, 1); // 1 second block
        let acl = AclModule::new(config);
        let ip: IpAddr = "10.0.0.4".parse().unwrap();

        assert!(acl.dos_check_and_track(ip).await.is_ok());
        assert!(acl.dos_check_and_track(ip).await.is_err());

        tokio::time::sleep(Duration::from_secs(1)).await;
        // Block should have expired, window reset
        assert!(acl.dos_check_and_track(ip).await.is_ok());
    }

    #[tokio::test]
    async fn test_dos_independent_per_ip() {
        let config = create_dos_config(true, 1, 100, 100, 60);
        let acl = AclModule::new(config);
        let ip1: IpAddr = "10.0.0.5".parse().unwrap();
        let ip2: IpAddr = "10.0.0.6".parse().unwrap();

        assert!(acl.dos_check_and_track(ip1).await.is_ok());
        assert!(acl.dos_check_and_track(ip1).await.is_err()); // ip1 blocked

        assert!(acl.dos_check_and_track(ip2).await.is_ok()); // ip2 still allowed
    }
}
