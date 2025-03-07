use anyhow::Error;
use clap::Parser;
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(version)]
pub(crate) struct Cli {
    #[clap(long)]
    pub conf: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub http_addr: String,
    pub log_level: Option<String>,
    pub log_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console: Option<ConsoleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sip: Option<SipConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyConfig>,
}

#[derive(Debug, Deserialize)]
pub struct SipConfig {
    pub addr: String,
    pub udp_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_port: Option<u16>,

    #[serde(skip_serializing_if = "Option::is_none")]
    /// Path to the private key file for TLS
    pub ssl_private_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Path to the certificate file for TLS
    pub ssl_certificate: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProxyConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub useragent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ConsoleConfig {
    pub prefix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
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
            useragent: Some("rustpbx".to_string()),
        }
    }
}

impl Default for SipConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0".to_string(),
            udp_port: 5060,
            external_ip: None,
            tcp_port: None,
            tls_port: None,
            ws_port: None,
            ssl_private_key: None,
            ssl_certificate: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: "0.0.0.0:8080".to_string(),
            log_level: None,
            log_file: None,
            console: Some(ConsoleConfig::default()),
            sip: Some(SipConfig::default()),
            proxy: Some(ProxyConfig::default()),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Error> {
        let config = toml::from_str(&std::fs::read_to_string(path)?)?;
        Ok(config)
    }
}
