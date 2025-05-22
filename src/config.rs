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

#[derive(Debug, Deserialize)]
pub struct Config {
    pub http_addr: String,
    pub log_level: Option<String>,
    pub log_file: Option<String>,
    pub console: Option<ConsoleConfig>,
    pub ua: Option<UseragentConfig>,
    pub proxy: Option<ProxyConfig>,
    pub recorder_path: String,
    pub media_cache_path: String,
    pub llmproxy: Option<String>,
    pub ice_servers: Option<Vec<IceServerItem>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct UseragentConfig {
    pub addr: String,
    pub udp_port: u16,
    pub external_ip: Option<String>,
    pub stun_server: Option<String>,
    pub rtp_start_port: Option<u16>,
    pub rtp_end_port: Option<u16>,
    pub useragent: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum UserBackendConfig {
    Memory,
    Http {
        url: String,
        method: Option<String>,
        username_field: Option<String>,
        password_field: Option<String>,
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
        password_hash: Option<String>,
        password_salt: Option<String>,
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

#[derive(Clone, Debug, Deserialize)]
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
    pub allows: Option<Vec<String>>,
    pub denies: Option<Vec<String>>,
    pub max_concurrency: Option<usize>,
    pub registrar_expires: Option<u32>,
    pub user_backend: UserBackendConfig,
    pub locator: LocatorConfig,
}

#[derive(Debug, Deserialize)]
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

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0".to_string(),
            modules: Some(vec![
                "ban".to_string(),
                "auth".to_string(),
                "registrar".to_string(),
                "cdr".to_string(),
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
            allows: None,
            denies: None,
            max_concurrency: None,
            registrar_expires: Some(60),
            user_backend: UserBackendConfig::default(),
            locator: LocatorConfig::default(),
        }
    }
}

impl Default for UserBackendConfig {
    fn default() -> Self {
        Self::Memory
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
            proxy: Some(ProxyConfig::default()),
            #[cfg(target_os = "windows")]
            recorder_path: "./recorder".to_string(),
            #[cfg(not(target_os = "windows"))]
            recorder_path: "/tmp/recorder".to_string(),
            #[cfg(target_os = "windows")]
            media_cache_path: "./mediacache".to_string(),
            #[cfg(not(target_os = "windows"))]
            media_cache_path: "/tmp/mediacache".to_string(),
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
