use crate::{call::DialDirection, config::RecordingPolicy};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};
use tokio::net::lookup_host;

pub mod matcher;
#[cfg(test)]
mod tests;

/// Single trunk configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct TrunkConfig {
    pub dest: String,
    pub backup_dest: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codec: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<TrunkDirection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inbound_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingPolicy>,
}

impl TrunkConfig {
    pub async fn matches_inbound_ip(&self, addr: &IpAddr) -> bool {
        for host in &self.inbound_hosts {
            if candidate_matches(host, addr).await {
                return true;
            }
        }

        if candidate_matches(&self.dest, addr).await {
            return true;
        }

        if let Some(backup) = &self.backup_dest {
            if candidate_matches(backup, addr).await {
                return true;
            }
        }

        false
    }
}

/// Build a [`SourceTrunk`] instance when direction is allowed by trunk configuration.
pub fn build_source_trunk(
    name: String,
    config: &TrunkConfig,
    direction: &DialDirection,
) -> Option<SourceTrunk> {
    if let Some(trunk_direction) = config.direction {
        if !trunk_direction.allows(direction) {
            return None;
        }
    }

    Some(SourceTrunk {
        name,
        id: config.id,
        direction: config.direction,
    })
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TrunkDirection {
    Inbound,
    Outbound,
    Bidirectional,
}

impl TrunkDirection {
    pub fn allows(&self, direction: &DialDirection) -> bool {
        match self {
            TrunkDirection::Inbound => matches!(direction, DialDirection::Inbound),
            TrunkDirection::Outbound => matches!(direction, DialDirection::Outbound),
            TrunkDirection::Bidirectional => true,
        }
    }
}

impl From<crate::models::sip_trunk::SipTrunkDirection> for TrunkDirection {
    fn from(value: crate::models::sip_trunk::SipTrunkDirection) -> Self {
        match value {
            crate::models::sip_trunk::SipTrunkDirection::Inbound => TrunkDirection::Inbound,
            crate::models::sip_trunk::SipTrunkDirection::Outbound => TrunkDirection::Outbound,
            crate::models::sip_trunk::SipTrunkDirection::Bidirectional => {
                TrunkDirection::Bidirectional
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SourceTrunk {
    pub name: String,
    pub id: Option<i64>,
    pub direction: Option<TrunkDirection>,
}
/// Default route strategy
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DefaultRoute {
    pub dest: DestConfig,
    #[serde(default = "default_select")]
    pub select: String,
    #[serde(default = "default_action")]
    pub action: String,
}

/// Destination configuration (can be single or multiple trunks)
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum DestConfig {
    Single(String),
    Multiple(Vec<String>),
}

/// Route rule
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RouteRule {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub direction: RouteDirection,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_trunks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_trunk_ids: Vec<i64>,

    /// Match conditions
    #[serde(rename = "match")]
    pub match_conditions: MatchConditions,

    /// Rewrite rules
    #[serde(default)]
    pub rewrite: Option<RewriteRules>,

    /// Route action
    #[serde(flatten)]
    pub action: RouteAction,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

impl Default for RouteRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: None,
            priority: 0,
            direction: RouteDirection::Any,
            source_trunks: Vec::new(),
            source_trunk_ids: Vec::new(),
            match_conditions: MatchConditions::default(),
            rewrite: None,
            action: RouteAction::default(),
            disabled: None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RouteDirection {
    Any,
    Inbound,
    Outbound,
}

impl Default for RouteDirection {
    fn default() -> Self {
        RouteDirection::Any
    }
}

impl RouteDirection {
    pub fn matches(&self, direction: &DialDirection) -> bool {
        match self {
            RouteDirection::Any => true,
            RouteDirection::Inbound => matches!(direction, DialDirection::Inbound),
            RouteDirection::Outbound => matches!(direction, DialDirection::Outbound),
        }
    }
}

/// Match conditions
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct MatchConditions {
    /// From user part
    #[serde(rename = "from.user")]
    pub from_user: Option<String>,
    /// From host part
    #[serde(rename = "from.host")]
    pub from_host: Option<String>,
    /// To user part
    #[serde(rename = "to.user")]
    pub to_user: Option<String>,
    /// To host part
    #[serde(rename = "to.host")]
    pub to_host: Option<String>,
    /// To port
    #[serde(rename = "to.port")]
    pub to_port: Option<String>,
    /// Request URI user part
    #[serde(rename = "request_uri.user")]
    pub request_uri_user: Option<String>,
    /// Request URI host part
    #[serde(rename = "request_uri.host")]
    pub request_uri_host: Option<String>,
    /// Request URI port
    #[serde(rename = "request_uri.port")]
    pub request_uri_port: Option<String>,
    /// SIP header fields (starting with header.)
    #[serde(flatten)]
    pub headers: HashMap<String, String>,

    // Compatible simplified field names
    pub from: Option<String>,
    pub to: Option<String>,
    pub caller: Option<String>,
    pub callee: Option<String>,
}

/// Rewrite rules
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RewriteRules {
    /// Rewrite From user part
    #[serde(rename = "from.user")]
    pub from_user: Option<String>,
    /// Rewrite From host part
    #[serde(rename = "from.host")]
    pub from_host: Option<String>,
    /// Rewrite To user part
    #[serde(rename = "to.user")]
    pub to_user: Option<String>,
    /// Rewrite To host part
    #[serde(rename = "to.host")]
    pub to_host: Option<String>,
    /// Rewrite To port
    #[serde(rename = "to.port")]
    pub to_port: Option<String>,
    /// Rewrite Request URI user part
    #[serde(rename = "request_uri.user")]
    pub request_uri_user: Option<String>,
    /// Rewrite Request URI host part
    #[serde(rename = "request_uri.host")]
    pub request_uri_host: Option<String>,
    /// Rewrite Request URI port
    #[serde(rename = "request_uri.port")]
    pub request_uri_port: Option<String>,
    /// Add/modify header fields (starting with header.)
    #[serde(flatten)]
    pub headers: HashMap<String, String>,

    // Compatible simplified field names
    pub from: Option<String>,
    pub to: Option<String>,
    pub caller: Option<String>,
    pub callee: Option<String>,
}

/// Route action
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RouteAction {
    /// Explicit action type (optional, defaults to forward if dest is specified)
    #[serde(default)]
    pub action: Option<String>,

    /// Forward destination (when action is forward or unspecified)
    #[serde(default)]
    pub dest: Option<DestConfig>,

    /// Load balancing selection algorithm
    #[serde(default = "default_select")]
    pub select: String,

    /// Hash algorithm key
    #[serde(default)]
    pub hash_key: Option<String>,

    /// Reject configuration (when action is reject)
    #[serde(default)]
    pub reject: Option<RejectConfig>,
}

impl Default for RouteAction {
    fn default() -> Self {
        RouteAction {
            action: None,
            dest: Some(DestConfig::Single("default".to_string())),
            select: default_select(),
            hash_key: None,
            reject: None,
        }
    }
}

impl RouteAction {
    /// Get the actual action type
    pub fn get_action_type(&self) -> ActionType {
        match &self.action {
            Some(action) => match action.as_str() {
                "reject" => ActionType::Reject,
                "busy" => ActionType::Busy,
                _ => ActionType::Forward,
            },
            None => {
                // If no explicit action, infer from other fields
                if self.reject.is_some() {
                    ActionType::Reject
                } else {
                    ActionType::Forward
                }
            }
        }
    }
}

/// Action type enum
#[derive(Debug, Clone, PartialEq)]
pub enum ActionType {
    Forward,
    Reject,
    Busy,
}

/// Reject configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RejectConfig {
    pub code: u16,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn default_select() -> String {
    "rr".to_string()
}
fn default_action() -> String {
    "forward".to_string()
}

async fn candidate_matches(candidate: &str, addr: &IpAddr) -> bool {
    let trimmed = candidate.trim().trim_matches(|c| c == '<' || c == '>');
    if trimmed.is_empty() {
        return false;
    }

    if let Ok(network) = trimmed.parse::<IpNetwork>() {
        return network.contains(*addr);
    }

    if let Ok(socket) = trimmed.parse::<SocketAddr>() {
        return socket.ip() == *addr;
    }

    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return ip == *addr;
    }

    if let Ok(uri) = rsip::Uri::try_from(trimmed) {
        return host_matches(&uri.host_with_port.host.to_string(), addr).await;
    }

    if let Some((host, _)) = split_host_port(trimmed) {
        return host_matches(host, addr).await;
    }

    host_matches(trimmed, addr).await
}

/// Public helper to validate whether a candidate host definition resolves to the provided IP.
pub async fn candidate_matches_ip(candidate: &str, addr: &IpAddr) -> bool {
    candidate_matches(candidate, addr).await
}

async fn host_matches(host: &str, addr: &IpAddr) -> bool {
    let cleaned = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();

    if cleaned.is_empty() {
        return false;
    }

    if let Ok(network) = cleaned.parse::<IpNetwork>() {
        return network.contains(*addr);
    }

    if let Ok(socket) = cleaned.parse::<SocketAddr>() {
        return socket.ip() == *addr;
    }

    if let Ok(ip) = cleaned.parse::<IpAddr>() {
        return ip == *addr;
    }

    let lookup_target = match split_host_port(cleaned) {
        Some((host_part, _)) => host_part.to_string(),
        None => cleaned.to_string(),
    };

    match lookup_host((lookup_target.as_str(), 0)).await {
        Ok(addrs) => addrs.into_iter().any(|resolved| resolved.ip() == *addr),
        Err(_) => false,
    }
}

fn split_host_port(input: &str) -> Option<(&str, &str)> {
    if let Some(end) = input.find(']') {
        if input.starts_with('[') && input.len() > end + 1 && input[end + 1..].starts_with(':') {
            return Some((&input[1..end], &input[end + 2..]));
        }
    }

    if let Some(idx) = input.rfind(':') {
        if input[..idx].contains(':') {
            return None;
        }
        return Some((&input[..idx], &input[idx + 1..]));
    }

    None
}
