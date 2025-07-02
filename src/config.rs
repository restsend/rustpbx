use crate::{proxy::user::SipUser, useragent::RegisterOption};
use anyhow::Error;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const USER_AGENT: &str = "rustpbx";

#[derive(Parser, Debug)]
#[command(version)]
pub(crate) struct Cli {
    #[clap(long, default_value = "rustpbx.toml")]
    pub conf: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub http_addr: String,
    pub log_level: Option<String>,
    pub log_file: Option<String>,
    pub console: Option<ConsoleConfig>,
    pub ua: Option<UseragentConfig>,
    pub proxy: Option<ProxyConfig>,
    pub recorder_path: String,
    pub callrecord: Option<CallRecordConfig>,
    pub media_cache_path: String,
    pub llmproxy: Option<String>,
    pub ice_servers: Option<Vec<IceServerItem>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ConsoleConfig {
    pub prefix: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Deserialize, Default, Serialize)]
pub struct IceServerItem {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            prefix: "/console".to_string(),
            username: None,
            password: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct UseragentConfig {
    pub addr: String,
    pub udp_port: u16,
    pub external_ip: Option<String>,
    pub stun_server: Option<String>,
    pub rtp_start_port: Option<u16>,
    pub rtp_end_port: Option<u16>,
    pub useragent: Option<String>,
    pub register_users: Option<Vec<RegisterOption>>,
    pub graceful_shutdown: Option<bool>,
    pub handler: Option<InviteHandlerConfig>,
    pub accept_timeout: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type")]
pub enum InviteHandlerConfig {
    Webhook {
        url: String,
        method: Option<String>,
        headers: Option<Vec<(String, String)>>,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum UserBackendConfig {
    Memory {
        users: Option<Vec<SipUser>>,
    },
    Http {
        url: String,
        method: Option<String>,
        username_field: Option<String>,
        realm_field: Option<String>,
        headers: Option<HashMap<String, String>>,
    },
    Plain {
        path: String,
    },
    Database {
        url: String,
        table_name: Option<String>,
        id_column: Option<String>,
        username_column: Option<String>,
        password_column: Option<String>,
        enabled_column: Option<String>,
        realm_column: Option<String>,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum LocatorConfig {
    Memory,
    Http {
        url: String,
        method: Option<String>,
        username_field: Option<String>,
        expires_field: Option<String>,
        realm_field: Option<String>,
        headers: Option<HashMap<String, String>>,
    },
    Database {
        url: String,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum S3Vendor {
    Aliyun,
    Tencent,
    Minio,
    AWS,
    GCP,
    Azure,
    DigitalOcean,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum CallRecordConfig {
    Local {
        root: String,
    },
    S3 {
        vendor: S3Vendor,
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
        endpoint: String,
        root: String,
        with_media: Option<bool>,
    },
    Http {
        url: String,
        headers: Option<HashMap<String, String>>,
        with_media: Option<bool>,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(PartialEq)]
pub enum MediaProxyMode {
    /// Do not handle media proxy
    None,
    /// Only handle NAT (private IP addresses)
    NatOnly,
    /// All media goes through proxy
    All,
}

impl Default for MediaProxyMode {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct MediaProxyConfig {
    pub mode: MediaProxyMode,
    pub rtp_start_port: Option<u16>,
    pub rtp_end_port: Option<u16>,
    pub external_ip: Option<String>,
    pub force_proxy: Option<Vec<String>>, // List of IP addresses to always proxy
}

impl Default for MediaProxyConfig {
    fn default() -> Self {
        Self {
            mode: MediaProxyMode::None,
            rtp_start_port: Some(20000),
            rtp_end_port: Some(30000),
            external_ip: None,
            force_proxy: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub modules: Option<Vec<String>>,
    pub addr: String,
    pub external_ip: Option<String>,
    pub useragent: Option<String>,
    pub ssl_private_key: Option<String>,
    pub ssl_certificate: Option<String>,
    pub udp_port: Option<u16>,
    pub tcp_port: Option<u16>,
    pub tls_port: Option<u16>,
    pub ws_port: Option<u16>,
    pub acl_rules: Option<Vec<String>>,
    pub max_concurrency: Option<usize>,
    pub registrar_expires: Option<u32>,
    #[serde(default)]
    pub user_backend: UserBackendConfig,
    #[serde(default)]
    pub locator: LocatorConfig,
    #[serde(default)]
    pub media_proxy: MediaProxyConfig,
    #[serde(default)]
    pub realms: Option<Vec<String>>,
    #[serde(default)]
    pub enable_forwarding: Option<bool>,
    pub ws_handler: Option<String>,
}

impl ProxyConfig {
    pub fn normalize_realm(realm: &str) -> &str {
        if realm.is_empty() || realm == "*" || realm == "127.0.0.1" || realm == "::1" {
            "localhost"
        } else {
            realm
        }
    }
    pub fn is_same_realm(&self, callee_realm: &str) -> bool {
        match callee_realm {
            "localhost" | "127.0.0.1" | "::1" => true,
            _ => {
                if let Some(external_ip) = self.external_ip.as_ref() {
                    return external_ip.starts_with(callee_realm);
                }
                if let Some(realms) = self.realms.as_ref() {
                    for item in realms {
                        if item == callee_realm {
                            return true;
                        }
                    }
                }
                false
            }
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            acl_rules: Some(vec!["allow all".to_string(), "deny all".to_string()]),
            addr: "0.0.0.0".to_string(),
            modules: Some(vec![
                "acl".to_string(),
                "auth".to_string(),
                "registrar".to_string(),
                "call".to_string(),
            ]),
            external_ip: None,
            useragent: None,
            ssl_private_key: None,
            ssl_certificate: None,
            udp_port: Some(5060),
            tcp_port: None,
            tls_port: None,
            ws_port: None,
            max_concurrency: None,
            registrar_expires: Some(60),
            user_backend: UserBackendConfig::default(),
            locator: LocatorConfig::default(),
            media_proxy: MediaProxyConfig::default(),
            realms: Some(vec![]),
            enable_forwarding: Some(true),
            ws_handler: None,
        }
    }
}

impl Default for UserBackendConfig {
    fn default() -> Self {
        Self::Memory { users: None }
    }
}

impl Default for LocatorConfig {
    fn default() -> Self {
        Self::Memory
    }
}

impl Default for UseragentConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0".to_string(),
            udp_port: 25060,
            external_ip: None,
            rtp_start_port: Some(12000),
            rtp_end_port: Some(42000),
            stun_server: None,
            useragent: Some(USER_AGENT.to_string()),
            register_users: None,
            graceful_shutdown: Some(true),
            handler: None,
            accept_timeout: Some("50s".to_string()),
        }
    }
}

impl Default for CallRecordConfig {
    fn default() -> Self {
        Self::Local {
            #[cfg(target_os = "windows")]
            root: "./cdr".to_string(),
            #[cfg(not(target_os = "windows"))]
            root: "/tmp/cdr".to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: "0.0.0.0:8080".to_string(),
            log_level: Some("info".to_string()),
            log_file: None,
            console: Some(ConsoleConfig::default()),
            ua: Some(UseragentConfig::default()),
            proxy: None,
            #[cfg(target_os = "windows")]
            recorder_path: "./recorder".to_string(),
            #[cfg(not(target_os = "windows"))]
            recorder_path: "/tmp/recorder".to_string(),
            #[cfg(target_os = "windows")]
            media_cache_path: "./mediacache".to_string(),
            #[cfg(not(target_os = "windows"))]
            media_cache_path: "/tmp/mediacache".to_string(),
            callrecord: None,
            llmproxy: None,
            ice_servers: None,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Error> {
        let config = toml::from_str(
            &std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {}", e, path))?,
        )?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_load() {
        let config = Config::default();
        let config_str = toml::to_string(&config).unwrap();
        println!("{}", config_str);
    }
    #[test]
    fn test_config_dump() {
        let config = Config::default();
        let config_str = toml::to_string(&config).unwrap();
        println!("{}", config_str);
    }
}
