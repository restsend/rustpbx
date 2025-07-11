use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub mod matcher;
#[cfg(test)]
mod tests;

/// Routing state for managing stateful load balancing
#[derive(Debug)]
pub struct RoutingState {
    /// Round-robin counters for each destination group
    round_robin_counters: Arc<std::sync::Mutex<HashMap<String, AtomicUsize>>>,
}

impl RoutingState {
    pub fn new() -> Self {
        Self {
            round_robin_counters: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Get the next trunk index for round-robin selection
    pub fn next_round_robin_index(&self, destination_key: &str, trunk_count: usize) -> usize {
        if trunk_count == 0 {
            return 0;
        }

        let mut counters = self.round_robin_counters.lock().unwrap();
        let counter = counters
            .entry(destination_key.to_string())
            .or_insert_with(|| AtomicUsize::new(0));

        let current = counter.fetch_add(1, Ordering::SeqCst);
        current % trunk_count
    }
}

/// Original RouteConfig enum (backward compatibility)
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub struct RoutesConfig {
    #[serde(default)]
    pub routes: Option<Vec<RouteRule>>,
    #[serde(default)]
    pub includes: Option<Vec<String>>,
}

// /// Route trait
// pub trait Route: Send + Sync {
//     fn match_invite(&self, option: InviteOption) -> Result<RouteResult>;
// }

/// Trunk configuration container
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TrunksConfig {
    /// External configuration file includes
    #[serde(default)]
    pub includes: Vec<String>,

    /// Directly defined trunk configuration, using flatten to promote config items to current level
    #[serde(flatten)]
    pub trunks: HashMap<String, TrunkConfig>,
}

/// Single trunk configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TrunkConfig {
    pub name: String,
    pub dest: String,
    pub backup_dest: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub codec: Vec<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub max_calls: Option<u32>,
    #[serde(default)]
    pub max_cps: Option<u32>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub transport: Option<String>,
}

/// Routing configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RoutingConfig {
    /// External routing rules file includes
    #[serde(default)]
    pub includes: Vec<String>,

    /// Default routing strategy
    pub default: DefaultRoute,

    /// Directly defined routing rules
    #[serde(default)]
    pub rules: Vec<RouteRule>,
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

    #[serde(default = "default_enabled")]
    pub enabled: bool,
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
                } else if self.dest.is_some() {
                    ActionType::Forward
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

/// Trunk definitions in external files (without proxy.trunks prefix)
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ExternalTrunks(pub HashMap<String, TrunkConfig>);

/// Route rule definitions in external files
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ExternalRoutes {
    pub rules: Vec<RouteRule>,
}

/// Routing configuration loader
pub struct RoutingConfigLoader {
    base_path: std::path::PathBuf,
}

impl RoutingConfigLoader {
    pub fn new<P: AsRef<Path>>(base_path: P) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
        }
    }

    /// Load trunk configuration and process includes
    pub fn load_trunks_config<P: AsRef<Path>>(&self, config_path: P) -> Result<TrunksConfig> {
        let config_content =
            std::fs::read_to_string(config_path).context("Failed to read trunks config file")?;

        let mut config: TrunksConfig =
            toml::from_str(&config_content).context("Failed to parse trunks config file")?;

        self.load_trunk_includes(&mut config)?;
        Ok(config)
    }

    /// Load routing configuration and process includes
    pub fn load_routing_config<P: AsRef<Path>>(&self, config_path: P) -> Result<RoutingConfig> {
        let config_content =
            std::fs::read_to_string(config_path).context("Failed to read routing config file")?;

        let mut config: RoutingConfig =
            toml::from_str(&config_content).context("Failed to parse routing config file")?;

        self.load_routing_includes(&mut config)?;
        Ok(config)
    }

    /// Load trunk includes
    fn load_trunk_includes(&self, trunks_config: &mut TrunksConfig) -> Result<()> {
        for include_path in &trunks_config.includes {
            let full_path = self.base_path.join(include_path);
            let content = std::fs::read_to_string(&full_path)
                .with_context(|| format!("Failed to read trunk include file: {}", include_path))?;

            let external_trunks: ExternalTrunks = toml::from_str(&content)
                .with_context(|| format!("Failed to parse trunk include file: {}", include_path))?;

            // Merge into main config, main config takes priority
            for (name, trunk) in external_trunks.0 {
                trunks_config.trunks.entry(name).or_insert(trunk);
            }
        }

        Ok(())
    }

    /// Load routing includes
    fn load_routing_includes(&self, routing_config: &mut RoutingConfig) -> Result<()> {
        for include_path in &routing_config.includes {
            let full_path = self.base_path.join(include_path);
            let content = std::fs::read_to_string(&full_path).with_context(|| {
                format!("Failed to read routing include file: {}", include_path)
            })?;

            let external_routes: ExternalRoutes = toml::from_str(&content).with_context(|| {
                format!("Failed to parse routing include file: {}", include_path)
            })?;

            // Add to rule list
            routing_config.rules.extend(external_routes.rules);
        }

        // Sort by priority
        routing_config.rules.sort_by_key(|rule| rule.priority);

        Ok(())
    }
}

// Default value functions
fn default_enabled() -> bool {
    true
}
fn default_weight() -> u32 {
    100
}
fn default_select() -> String {
    "rr".to_string()
}
fn default_action() -> String {
    "forward".to_string()
}
