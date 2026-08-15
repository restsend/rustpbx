use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiTokenConfig {
    pub token: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiConfig {
    #[serde(default = "default_rwi_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_rwi_max_calls_per_connection")]
    pub max_calls_per_connection: usize,
    #[serde(default = "default_orphan_hold_secs")]
    pub orphan_hold_secs: u32,
    #[serde(default = "default_originate_rate_limit")]
    pub originate_rate_limit: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tokens: Vec<RwiTokenConfig>,
}

impl Default for RwiConfig {
    fn default() -> Self {
        Self {
            max_connections: default_rwi_max_connections(),
            max_calls_per_connection: default_rwi_max_calls_per_connection(),
            orphan_hold_secs: default_orphan_hold_secs(),
            originate_rate_limit: default_originate_rate_limit(),
            tokens: Vec::new(),
        }
    }
}

fn default_rwi_max_connections() -> usize {
    2000
}

fn default_rwi_max_calls_per_connection() -> usize {
    200
}

fn default_orphan_hold_secs() -> u32 {
    30
}

fn default_originate_rate_limit() -> usize {
    10
}

impl RwiConfig {
    pub fn from_config(config: &Config) -> Option<&Self> {
        config.rwi.as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct RwiIdentity {
    pub token: String,
    pub scopes: Vec<String>,
}

pub struct RwiAuth {
    tokens: HashMap<String, RwiTokenConfig>,
}

impl RwiAuth {
    pub fn new(config: &RwiConfig) -> Self {
        let tokens = config
            .tokens
            .iter()
            .map(|t| (t.token.clone(), t.clone()))
            .collect();

        Self { tokens }
    }

    pub fn validate_token(&self, token: &str) -> Option<RwiIdentity> {
        self.tokens.get(token).map(|t| RwiIdentity {
            token: t.token.clone(),
            scopes: t.scopes.clone(),
        })
    }
}

pub type RwiAuthRef = Arc<RwLock<RwiAuth>>;

/// Rebuild the RWI auth (tokens/contexts) from a freshly loaded config and
/// swap it into the existing live `RwiAuthRef`, so existing WebSocket auth
/// checks observe the new credentials immediately.
pub async fn reload_rwi_auth(auth: &RwiAuthRef, config: &Config) {
    if let Some(cfg) = RwiConfig::from_config(config) {
        *auth.write().await = RwiAuth::new(cfg);
    }
}

pub fn create_rwi_auth(config: &Config) -> Option<RwiAuthRef> {
    RwiConfig::from_config(config).map(|cfg| Arc::new(RwLock::new(RwiAuth::new(cfg))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config() -> RwiConfig {
        RwiConfig {
            max_connections: 100,
            max_calls_per_connection: 50,
            orphan_hold_secs: 30,
            originate_rate_limit: 10,
            tokens: vec![
                RwiTokenConfig {
                    token: "token1".to_string(),
                    scopes: vec!["call.control".to_string()],
                },
                RwiTokenConfig {
                    token: "token2".to_string(),
                    scopes: vec!["call.control".to_string(), "supervisor.control".to_string()],
                },
            ],
        }
    }

    #[test]
    fn test_rwi_auth_validate_token_valid() {
        let config = create_test_config();
        let auth = RwiAuth::new(&config);

        let identity = auth.validate_token("token1");
        assert!(identity.is_some());
        let identity = identity.unwrap();
        assert_eq!(identity.token, "token1");
        assert_eq!(identity.scopes, vec!["call.control"]);
    }

    #[test]
    fn test_rwi_auth_validate_token_invalid() {
        let config = create_test_config();
        let auth = RwiAuth::new(&config);

        let identity = auth.validate_token("invalid-token");
        assert!(identity.is_none());
    }

    #[test]
    fn test_rwi_config_defaults() {
        let config = RwiConfig::default();
        assert_eq!(config.max_connections, 2000);
        assert_eq!(config.max_calls_per_connection, 200);
        assert_eq!(config.orphan_hold_secs, 30);
        assert_eq!(config.originate_rate_limit, 10);
        assert!(config.tokens.is_empty());
    }
}
