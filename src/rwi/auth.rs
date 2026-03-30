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
pub struct RwiContextConfig {
    pub name: String,
    pub no_answer_timeout_secs: Option<u32>,
    pub no_answer_action: Option<String>,
    pub no_answer_transfer_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    #[serde(default = "default_transfer_refer_enabled")]
    pub refer_enabled: bool,
    #[serde(default = "default_transfer_attended_enabled")]
    pub attended_enabled: bool,
    #[serde(default = "default_transfer_3pcc_fallback_enabled")]
    pub three_pcc_fallback_enabled: bool,
    #[serde(default = "default_transfer_refer_timeout_secs")]
    pub refer_timeout_secs: u64,
    #[serde(default = "default_transfer_3pcc_timeout_secs")]
    pub three_pcc_timeout_secs: u64,
    #[serde(default = "default_max_concurrent_transfers")]
    pub max_concurrent_transfers: usize,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            refer_enabled: default_transfer_refer_enabled(),
            attended_enabled: default_transfer_attended_enabled(),
            three_pcc_fallback_enabled: default_transfer_3pcc_fallback_enabled(),
            refer_timeout_secs: default_transfer_refer_timeout_secs(),
            three_pcc_timeout_secs: default_transfer_3pcc_timeout_secs(),
            max_concurrent_transfers: default_max_concurrent_transfers(),
        }
    }
}

fn default_transfer_refer_enabled() -> bool {
    true
}

fn default_transfer_attended_enabled() -> bool {
    true
}

fn default_transfer_3pcc_fallback_enabled() -> bool {
    true
}

fn default_transfer_refer_timeout_secs() -> u64 {
    30
}

fn default_transfer_3pcc_timeout_secs() -> u64 {
    60
}

fn default_max_concurrent_transfers() -> usize {
    1000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiConfig {
    #[serde(default)]
    pub enabled: bool,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<RwiContextConfig>,
    #[serde(default)]
    pub transfer: TransferConfig,
}

impl Default for RwiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_connections: default_rwi_max_connections(),
            max_calls_per_connection: default_rwi_max_calls_per_connection(),
            orphan_hold_secs: default_orphan_hold_secs(),
            originate_rate_limit: default_originate_rate_limit(),
            tokens: Vec::new(),
            contexts: Vec::new(),
            transfer: TransferConfig::default(),
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

impl RwiIdentity {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    pub fn has_any_scope(&self, scopes: &[&str]) -> bool {
        scopes.iter().any(|s| self.has_scope(s))
    }
}

pub struct RwiAuth {
    tokens: HashMap<String, RwiTokenConfig>,
    contexts: HashMap<String, RwiContextConfig>,
}

impl RwiAuth {
    pub fn new(config: &RwiConfig) -> Self {
        let tokens = config
            .tokens
            .iter()
            .map(|t| (t.token.clone(), t.clone()))
            .collect();

        let contexts = config
            .contexts
            .iter()
            .map(|c| (c.name.clone(), c.clone()))
            .collect();

        Self { tokens, contexts }
    }

    pub fn validate_token(&self, token: &str) -> Option<RwiIdentity> {
        self.tokens.get(token).map(|t| RwiIdentity {
            token: t.token.clone(),
            scopes: t.scopes.clone(),
        })
    }

    pub fn get_context(&self, name: &str) -> Option<&RwiContextConfig> {
        self.contexts.get(name)
    }

    pub fn is_enabled(&self) -> bool {
        !self.tokens.is_empty()
    }
}

pub type RwiAuthRef = Arc<RwLock<RwiAuth>>;

pub fn create_rwi_auth(config: &Config) -> Option<RwiAuthRef> {
    RwiConfig::from_config(config).map(|cfg| Arc::new(RwLock::new(RwiAuth::new(cfg))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config() -> RwiConfig {
        RwiConfig {
            enabled: true,
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
            contexts: vec![
                RwiContextConfig {
                    name: "ctx1".to_string(),
                    no_answer_timeout_secs: Some(10),
                    no_answer_action: Some("hangup".to_string()),
                    no_answer_transfer_target: None,
                },
                RwiContextConfig {
                    name: "ctx2".to_string(),
                    no_answer_timeout_secs: Some(30),
                    no_answer_action: Some("transfer".to_string()),
                    no_answer_transfer_target: Some("sip:voicemail@local".to_string()),
                },
            ],
            transfer: TransferConfig::default(),
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
    fn test_rwi_auth_is_enabled() {
        let config = create_test_config();
        let auth = RwiAuth::new(&config);

        assert!(auth.is_enabled());
    }

    #[test]
    fn test_rwi_auth_get_context() {
        let config = create_test_config();
        let auth = RwiAuth::new(&config);

        let ctx = auth.get_context("ctx1");
        assert!(ctx.is_some());
        assert_eq!(ctx.unwrap().name, "ctx1");

        let ctx2 = auth.get_context("nonexistent");
        assert!(ctx2.is_none());
    }

    #[test]
    fn test_rwi_identity_has_scope() {
        let identity = RwiIdentity {
            token: "test".to_string(),
            scopes: vec!["call.control".to_string(), "queue.control".to_string()],
        };

        assert!(identity.has_scope("call.control"));
        assert!(identity.has_scope("queue.control"));
        assert!(!identity.has_scope("admin"));
    }

    #[test]
    fn test_rwi_identity_has_any_scope() {
        let identity = RwiIdentity {
            token: "test".to_string(),
            scopes: vec!["call.control".to_string()],
        };

        assert!(identity.has_any_scope(&["call.control", "admin"]));

        assert!(!identity.has_any_scope(&["admin", "superuser"]));
    }

    #[test]
    fn test_rwi_config_defaults() {
        let config = RwiConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_connections, 2000);
        assert_eq!(config.max_calls_per_connection, 200);
        assert_eq!(config.orphan_hold_secs, 30);
        assert_eq!(config.originate_rate_limit, 10);
        assert!(config.tokens.is_empty());
        assert!(config.contexts.is_empty());
        assert!(config.transfer.refer_enabled);
        assert!(config.transfer.attended_enabled);
        assert!(config.transfer.three_pcc_fallback_enabled);
    }
}
