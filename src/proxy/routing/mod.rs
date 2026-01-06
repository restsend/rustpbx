use crate::{
    call::{DialDirection, DialStrategy, Location},
    config::RecordingPolicy,
};
use anyhow::{Result, anyhow};
use ipnetwork::IpNetwork;
use regex::Regex;
use rsip::{StatusCode, Uri};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::net::lookup_host;

pub mod http;
#[cfg(test)]
mod http_tests;
pub mod matcher;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigOrigin {
    Embedded,
    File(String),
}

impl ConfigOrigin {
    pub fn embedded() -> Self {
        Self::Embedded
    }

    pub fn from_file(path: impl Into<String>) -> Self {
        Self::File(path.into())
    }
}

impl Default for ConfigOrigin {
    fn default() -> Self {
        Self::Embedded
    }
}

/// Single trunk configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_from_user_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_to_user_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<crate::models::policy::PolicySpec>,
    #[serde(skip)]
    pub origin: ConfigOrigin,
}

impl Default for TrunkConfig {
    fn default() -> Self {
        Self {
            dest: String::new(),
            backup_dest: None,
            username: None,
            password: None,
            codec: Vec::new(),
            disabled: None,
            max_calls: None,
            max_cps: None,
            weight: None,
            transport: None,
            id: None,
            direction: None,
            inbound_hosts: Vec::new(),
            recording: None,
            incoming_from_user_prefix: None,
            incoming_to_user_prefix: None,
            country: None,
            policy: None,
            origin: ConfigOrigin::embedded(),
        }
    }
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

    pub fn matches_incoming_user_prefixes(
        &self,
        from_user: Option<&str>,
        to_user: Option<&str>,
    ) -> Result<bool> {
        if let Some(pattern) = &self.incoming_from_user_prefix {
            let candidate = from_user.unwrap_or_default();
            if pattern.trim().is_empty() {
                // Treat empty string as unset
            } else if !matches_user_prefix(pattern, candidate)? {
                return Ok(false);
            }
        }

        if let Some(pattern) = &self.incoming_to_user_prefix {
            let candidate = to_user.unwrap_or_default();
            if pattern.trim().is_empty() {
                // Treat empty string as unset
            } else if !matches_user_prefix(pattern, candidate)? {
                return Ok(false);
            }
        }

        Ok(true)
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

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codecs: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<crate::models::policy::PolicySpec>,
    #[serde(skip)]
    pub origin: ConfigOrigin,
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
            codecs: Vec::new(),
            disabled: None,
            policy: None,
            origin: ConfigOrigin::embedded(),
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

    /// Queue configuration file (when action is queue)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
}

impl Default for RouteAction {
    fn default() -> Self {
        RouteAction {
            action: None,
            dest: None,
            select: default_select(),
            hash_key: None,
            reject: None,
            queue: None,
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
                "queue" => ActionType::Queue,
                _ => ActionType::Forward,
            },
            None => {
                // If no explicit action, infer from other fields
                if self.queue.is_some() {
                    ActionType::Queue
                } else if self.reject.is_some() {
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
    Queue,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RouteQueueConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub accept_immediately: bool,
    #[serde(default)]
    pub passthrough_ringback: bool,
    #[serde(default)]
    pub hold: Option<RouteQueueHoldConfig>,
    #[serde(default)]
    pub fallback: Option<RouteQueueFallbackConfig>,
    #[serde(default)]
    pub strategy: RouteQueueStrategyConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RouteQueueHoldConfig {
    pub audio_file: Option<String>,
    #[serde(default = "RouteQueueHoldConfig::default_loop")]
    pub loop_playback: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RouteQueueStrategyConfig {
    #[serde(default = "QueueDialMode::default_mode")]
    pub mode: QueueDialMode,
    pub wait_timeout_secs: Option<u16>,
    #[serde(default)]
    pub targets: Vec<RouteQueueTargetConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RouteQueueTargetConfig {
    pub uri: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QueueDialMode {
    Sequential,
    Parallel,
}

impl QueueDialMode {
    pub fn default_mode() -> Self {
        QueueDialMode::Sequential
    }
}

impl Default for QueueDialMode {
    fn default() -> Self {
        Self::Sequential
    }
}

impl RouteQueueHoldConfig {
    fn default_loop() -> bool {
        true
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RouteQueueFallbackConfig {
    pub redirect: Option<String>,
    pub failure_code: Option<u16>,
    pub failure_reason: Option<String>,
    pub failure_prompt: Option<String>,
    pub queue_ref: Option<String>,
}

impl RouteQueueConfig {
    pub fn to_queue_plan(&self) -> Result<crate::call::QueuePlan> {
        let mut plan = crate::call::QueuePlan::default();
        plan.accept_immediately = self.accept_immediately;
        plan.passthrough_ringback = self.passthrough_ringback && self.accept_immediately;
        if let Some(hold) = &self.hold {
            let mut cfg = crate::call::QueueHoldConfig::default();
            if let Some(file) = &hold.audio_file {
                cfg = cfg.with_audio_file(file.clone());
            }
            cfg = cfg.with_loop_playback(hold.loop_playback);
            plan.hold = Some(cfg);
        }
        if let Some(fallback) = &self.fallback {
            plan.fallback = Some(fallback.to_action()?);
        }
        if let Some(strategy) = self.build_dial_strategy()? {
            plan.dial_strategy = Some(strategy);
        }
        if let Some(timeout) = self.strategy.wait_timeout_secs {
            if timeout > 0 {
                plan.ring_timeout = Some(Duration::from_secs(timeout as u64));
            }
        }
        Ok(plan)
    }

    fn build_dial_strategy(&self) -> Result<Option<DialStrategy>> {
        if self.strategy.targets.is_empty() {
            return Ok(None);
        }

        let mut locations = Vec::new();
        for target in &self.strategy.targets {
            let uri_text = target.uri.trim();
            if uri_text.is_empty() {
                continue;
            }
            let uri = Uri::try_from(uri_text)
                .map_err(|err| anyhow!("invalid queue target uri '{}': {}", uri_text, err))?;
            let mut location = Location::default();
            location.aor = uri.clone();
            location.contact_raw = Some(uri.to_string());
            locations.push(location);
        }

        if locations.is_empty() {
            return Ok(None);
        }

        let strategy = match self.strategy.mode {
            QueueDialMode::Parallel => DialStrategy::Parallel(locations),
            QueueDialMode::Sequential => DialStrategy::Sequential(locations),
        };
        Ok(Some(strategy))
    }
}

impl RouteQueueFallbackConfig {
    fn to_action(&self) -> Result<crate::call::QueueFallbackAction> {
        if let Some(queue) = self
            .queue_ref
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            return Ok(crate::call::QueueFallbackAction::Queue {
                name: queue.to_string(),
            });
        }
        if let Some(target) = &self.redirect {
            let uri = Uri::try_from(target.as_str())?;
            return Ok(crate::call::QueueFallbackAction::Redirect { target: uri });
        }
        if self.failure_code.is_some() || self.failure_prompt.is_some() {
            let status = match self.failure_code {
                Some(code) => {
                    if !(100..=699).contains(&code) {
                        return Err(anyhow!("invalid failure_code {}: must be 100-699", code));
                    }
                    StatusCode::from(code)
                }
                None => StatusCode::TemporarilyUnavailable,
            };

            let action = if let Some(prompt) = &self.failure_prompt {
                crate::call::FailureAction::PlayThenHangup {
                    audio_file: prompt.clone(),
                    use_early_media: false, // Use 200 OK for routing failures
                    status_code: status.clone(),
                    reason: self.failure_reason.clone(),
                }
            } else {
                crate::call::FailureAction::Hangup {
                    code: Some(status),
                    reason: self.failure_reason.clone(),
                }
            };
            return Ok(crate::call::QueueFallbackAction::Failure(action));
        }
        Err(anyhow!(
            "Queue fallback must specify redirect or failure action"
        ))
    }
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

fn matches_user_prefix(pattern: &str, value: &str) -> Result<bool> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Ok(true);
    }

    let mut is_regex = false;
    for ch in trimmed.chars() {
        match ch {
            '^' | '$' | '.' | '*' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '\\' => {
                is_regex = true;
                break;
            }
            _ => {}
        }
    }

    if !is_regex {
        return Ok(value.starts_with(trimmed));
    }

    let regex =
        Regex::new(trimmed).map_err(|err| anyhow!("invalid regex '{}': {}", trimmed, err))?;
    Ok(regex.is_match(value))
}
