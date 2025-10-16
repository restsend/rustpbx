use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
